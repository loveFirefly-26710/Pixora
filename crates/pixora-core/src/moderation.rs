//! 内容审核：上传的图片先过一遍内容安全服务，违规的挡在门外。
//!
//! 支持四家：阿里云内容安全、腾讯云图片内容安全、百度图像审核，以及
//! 自建的 NSFWJS 服务（`nsfwjs`）。四家都收敛到同一个 [`Verdict`]：
//! 「违规吗、多少分、判成什么标签」。上层只认这个结论，不问是哪家给的。
//!
//! 三条刻意的取舍：
//!
//!   1. **审核是增强，不是单点故障。** 审核服务超时或报错时默认放行
//!      （`moderation.fail_open`，默认开），但日志里留下痕迹。把上传的可用性
//!      绑在第三方接口的可用性上，出事时整站传不了图，代价比漏检一张大。
//!   2. **送审的是缩略图，不是原图。** 客户端生成的长边 400px 缩略图足够判断内容，
//!      而且四家都有体积上限（阿里云 10MB、腾讯云 5MB、百度 4MB），原图很容易超。
//!      只有在没有缩略图时才送原图，且同样有体积门槛。
//!   3. **密钥只进不出。** 凭据走 `Settings::set_secret` 加密入库，接口只回「配没配」。
//!
//! 签名实现按各家官方文档来：阿里云 RPC 风格 HMAC-SHA1，腾讯云 TC3-HMAC-SHA256，
//! 百度是先换 access_token 再表单提交。这些算法都能离线验证（见文件末尾的测试），
//! 但真实调用只有配上真凭据才能验证——所以每条错误路径都带着原始响应片段，
//! 出问题时日志里能直接看到对方说了什么。

use std::time::Duration;

use base64::Engine;

use pixora_crypto::hash::{hmac_sha256, sha256_hex};

use crate::error::{AppError, Result};
use crate::settings::ModerationSettings;
use crate::AppState;

/// 送审内容的体积上限（字节）。超过就不送原图，改用缩略图。
pub const MAX_SUBMIT_BYTES: usize = 5 * 1024 * 1024;
/// 各家接口的网络超时。审核慢一点没关系，但别把上传请求挂死。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);

/// 审核结论。
#[derive(Debug, Clone, Default)]
pub struct Verdict {
    /// 是否达到拦截阈值
    pub violation: bool,
    /// 违规分数（0-100）
    pub score: f64,
    /// 服务商给出的标签，如 porn / 色情
    pub label: String,
    /// 送审用的服务商
    pub provider: String,
    /// 原始结论片段，写进日志与审计
    pub detail: String,
}

impl Verdict {
    fn new(provider: &str, score: f64, label: &str, detail: String, threshold: i64) -> Self {
        Self {
            violation: score >= threshold as f64,
            score,
            label: label.to_string(),
            provider: provider.to_string(),
            detail,
        }
    }
}

/// 送审内容。
pub enum ScreenInput<'a> {
    /// 原图（或缩略图）字节与 MIME
    Image { bytes: &'a [u8], mime: &'a str },
}

impl ScreenInput<'_> {
    fn mime(&self) -> &str {
        match self {
            ScreenInput::Image { mime, .. } => mime,
        }
    }

    fn bytes(&self) -> &[u8] {
        match self {
            ScreenInput::Image { bytes, .. } => bytes,
        }
    }
}

/// 送审。`Err` 表示这次没审成（网络、凭据、响应看不懂），
/// 要不要放行由调用方按 `fail_open` 决定。
pub async fn screen(
    client: &reqwest::Client,
    settings: &ModerationSettings,
    input: ScreenInput<'_>,
) -> Result<Verdict> {
    if !settings.provider_ready() {
        return Err(AppError::Config(format!(
            "内容审核服务商 {} 的凭据没配齐",
            settings.provider
        )));
    }
    let bytes = input.bytes();
    if bytes.is_empty() {
        return Err(AppError::bad_request("送审内容为空"));
    }

    match settings.provider.as_str() {
        "aliyun" => aliyun(client, settings, bytes).await,
        "tencent" => tencent(client, settings, bytes).await,
        "baidu" => baidu(client, settings, bytes).await,
        _ => self_hosted(client, settings, bytes, input.mime()).await,
    }
}

/// 审核用的 HTTP 客户端。
///
/// 直接用 AppState 上那个共享客户端：连接池复用，省一次 TLS 握手。
/// 它的超时是 60 秒（给远端存储用的），审核这边要更短的——每个 provider
/// 自己的请求都带了 ，慢过头的审核不该拖住上传。
fn client(state: &AppState) -> reqwest::Client {
    (*state.http).clone()
}

// ============================================================
// 阿里云内容安全（ImageSyncScan，2018-05-09）
// ============================================================

/// 阿里云 RPC 风格的签名：HMAC-SHA1(secret + "&", stringToSign)。
///
/// `stringToSign = POST & %2F & percentEncode(规范化查询串)`。
/// 规范化查询串是把参数按字典序排好、逐个 percentEncode 后用 `&` 连接。
pub fn aliyun_string_to_sign(params: &[(String, String)]) -> String {
    let mut sorted: Vec<(String, String)> = params.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical = sorted
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!(
        "POST&{}&{}",
        percent_encode("/"),
        percent_encode(&canonical)
    )
}

/// 阿里云要求的百分号编码：只留 `A-Za-z0-9-_.~`。
/// 空格必须是 `%20`（不能是 `+`），`*` 必须是 `%2A`。
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// HMAC-SHA1 签名后 base64。
pub fn aliyun_signature(secret: &str, string_to_sign: &str) -> String {
    let key = format!("{secret}&");
    let digest = pixora_crypto::hash::hmac_sha1(key.as_bytes(), string_to_sign.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(digest)
}

async fn aliyun(
    client: &reqwest::Client,
    settings: &ModerationSettings,
    bytes: &[u8],
) -> Result<Verdict> {
    let region = if settings.region.trim().is_empty() {
        "cn-shanghai".to_string()
    } else {
        settings.region.trim().to_string()
    };
    let host = format!("green.{region}.aliyuncs.com");
    let url = format!("https://{host}/");

    let tasks = serde_json::json!([{
        "imageBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
        "dataId": format!("pixora-{}", chrono::Utc::now().timestamp_millis()),
    }])
    .to_string();

    let nonce = pixora_crypto::token::new_csrf_token()
        .map_err(|err| AppError::internal(format!("无法生成签名随机串：{err}")))?;
    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let mut params: Vec<(String, String)> = vec![
        ("AccessKeyId".into(), settings.access_key.clone()),
        ("Action".into(), "ImageSyncScan".into()),
        ("Format".into(), "JSON".into()),
        ("RegionId".into(), region),
        ("Scenes".into(), r#"["porn"]"#.into()),
        ("SignatureMethod".into(), "HMAC-SHA1".into()),
        ("SignatureNonce".into(), nonce),
        ("SignatureVersion".into(), "1.0".into()),
        ("Tasks".into(), tasks),
        ("Timestamp".into(), timestamp),
        ("Version".into(), "2018-05-09".into()),
    ];
    let signature = aliyun_signature(&settings.secret_key, &aliyun_string_to_sign(&params));
    params.push(("Signature".into(), signature));

    let body = params
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&");

    let response = client
        .post(&url)
        .timeout(REQUEST_TIMEOUT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|err| AppError::internal(format!("阿里云内容安全请求失败：{err}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| AppError::internal(format!("读取阿里云内容安全响应失败：{err}")))?;
    if !status.is_success() {
        return Err(AppError::internal(format!(
            "阿里云内容安全返回 {status}：{}",
            truncate(&text)
        )));
    }

    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| AppError::internal(format!("阿里云内容安全响应不是 JSON：{err}")))?;
    let data = value
        .get("data")
        .and_then(|item| item.as_array())
        .and_then(|list| list.first())
        .ok_or_else(|| {
            AppError::internal(format!("阿里云内容安全响应缺少 data：{}", truncate(&text)))
        })?;
    let results = data
        .get("results")
        .and_then(|item| item.as_array())
        .cloned()
        .unwrap_or_default();

    // 取分数最高的一条：多场景时以最严重的为准
    let mut best = Verdict::new("aliyun", 0.0, "pass", truncate(&text), settings.threshold);
    for item in results {
        let rate = item.get("rate").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let label = item
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let blocked = item
            .get("suggestion")
            .and_then(|v| v.as_str())
            .map(|value| value.eq_ignore_ascii_case("block"))
            .unwrap_or(false);
        // 服务商直接判 block 就按满分处理，不跟本地阈值较劲
        let score = if blocked { 100.0 } else { rate };
        if score >= best.score {
            best = Verdict::new("aliyun", score, &label, truncate(&text), settings.threshold);
        }
    }
    Ok(best)
}

// ============================================================
// 腾讯云图片内容安全（ImageModeration，2020-12-29）
// ============================================================

/// TC3-HMAC-SHA256 的签名。返回 `Authorization` 头的值。
///
/// 步骤照官方文档：派生三级密钥（日期 → 服务 → 请求），
/// 再对 `StringToSign` 做 HMAC-SHA256 并十六进制编码。
pub fn tencent_authorization(
    secret_id: &str,
    secret_key: &str,
    host: &str,
    action: &str,
    timestamp: i64,
    payload: &str,
) -> String {
    let date = chrono::DateTime::from_timestamp(timestamp, 0)
        .unwrap_or_else(chrono::Utc::now)
        .format("%Y-%m-%d")
        .to_string();
    let canonical_headers = format!(
        "content-type:application/json; charset=utf-8\nhost:{host}\nx-tc-action:{}\n",
        action.to_ascii_lowercase()
    );
    let signed_headers = "content-type;host;x-tc-action";
    let hashed_payload = sha256_hex(payload.as_bytes());
    let canonical_request = format!(
        "POST\n/\n\n{canonical_headers}\n{signed_headers}\n{hashed_payload}"
    );
    let scope = format!("{date}/ims/tc3_request");
    let string_to_sign = format!(
        "TC3-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let secret_date =
        hmac_sha256(format!("TC3{secret_key}").as_bytes(), date.as_bytes());
    let secret_service = hmac_sha256(&secret_date, b"ims");
    let secret_signing = hmac_sha256(&secret_service, b"tc3_request");
    let signature =
        hex::encode(hmac_sha256(&secret_signing, string_to_sign.as_bytes()));

    format!(
        "TC3-HMAC-SHA256 Credential={secret_id}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    )
}

async fn tencent(
    client: &reqwest::Client,
    settings: &ModerationSettings,
    bytes: &[u8],
) -> Result<Verdict> {
    let host = "ims.tencentcloudapi.com";
    let region = if settings.region.trim().is_empty() {
        "ap-guangzhou".to_string()
    } else {
        settings.region.trim().to_string()
    };
    let timestamp = chrono::Utc::now().timestamp();

    let mut payload = serde_json::json!({
        "FileContent": base64::engine::general_purpose::STANDARD.encode(bytes),
        "Interval": 0,
        "MaxFrames": 1,
    });
    // BizType 在控制台配置，留空用默认策略
    if !settings.app_id.trim().is_empty() {
        payload["BizType"] = serde_json::Value::String(settings.app_id.trim().to_string());
    }
    let payload = payload.to_string();

    let authorization = tencent_authorization(
        &settings.access_key,
        &settings.secret_key,
        host,
        "ImageModeration",
        timestamp,
        &payload,
    );

    let response = client
        .post(format!("https://{host}/"))
        .timeout(REQUEST_TIMEOUT)
        .header("Content-Type", "application/json; charset=utf-8")
        .header("Host", host)
        .header("X-TC-Action", "ImageModeration")
        .header("X-TC-Version", "2020-12-29")
        .header("X-TC-Timestamp", timestamp.to_string())
        .header("X-TC-Region", region)
        .header("Authorization", authorization)
        .body(payload)
        .send()
        .await
        .map_err(|err| AppError::internal(format!("腾讯云内容安全请求失败：{err}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| AppError::internal(format!("读取腾讯云内容安全响应失败：{err}")))?;
    if !status.is_success() {
        return Err(AppError::internal(format!(
            "腾讯云内容安全返回 {status}：{}",
            truncate(&text)
        )));
    }

    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| AppError::internal(format!("腾讯云内容安全响应不是 JSON：{err}")))?;
    let root = value.get("Response").unwrap_or(&value);
    if let Some(error) = root.get("Error") {
        return Err(AppError::internal(format!(
            "腾讯云内容安全报错：{}",
            truncate(&error.to_string())
        )));
    }

    let mut best = Verdict::new("tencent", 0.0, "pass", truncate(&text), settings.threshold);
    if let Some(list) = root.get("ResultSet").and_then(|item| item.as_array()) {
        for item in list {
            let suggestion = item
                .get("Suggestion")
                .and_then(|v| v.as_str())
                .unwrap_or("Pass");
            let label = item
                .get("Label")
                .and_then(|v| v.as_str())
                .unwrap_or("Pass")
                .to_string();
            let raw = item.get("Score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            // 服务商的 Score 是 0-100
            let score = if suggestion.eq_ignore_ascii_case("Block") {
                100.0
            } else {
                raw
            };
            if score >= best.score {
                best = Verdict::new("tencent", score, &label, truncate(&text), settings.threshold);
            }
        }
    }
    Ok(best)
}

// ============================================================
// 百度图像审核
// ============================================================

/// 百度的分数是 0-1 的概率，统一换算成 0-100 与其他家对齐。
fn baidu_score(probability: f64) -> f64 {
    if probability <= 1.0 {
        probability * 100.0
    } else {
        probability
    }
}

async fn baidu(
    client: &reqwest::Client,
    settings: &ModerationSettings,
    bytes: &[u8],
) -> Result<Verdict> {
    let token = baidu_token(client, settings).await?;
    let form = format!(
        "image={}",
        percent_encode(&base64::engine::general_purpose::STANDARD.encode(bytes))
    );

    let response = client
        .post(format!(
            "https://aip.baidubce.com/rest/2.0/solution/v1/img_censor/v2/user_defined?access_token={token}"
        ))
        .timeout(REQUEST_TIMEOUT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form)
        .send()
        .await
        .map_err(|err| AppError::internal(format!("百度图像审核请求失败：{err}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| AppError::internal(format!("读取百度图像审核响应失败：{err}")))?;
    if !status.is_success() {
        return Err(AppError::internal(format!(
            "百度图像审核返回 {status}：{}",
            truncate(&text)
        )));
    }

    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| AppError::internal(format!("百度图像审核响应不是 JSON：{err}")))?;
    if let Some(code) = value.get("error_code") {
        return Err(AppError::internal(format!(
            "百度图像审核报错 {code}：{}",
            value
                .get("error_msg")
                .and_then(|v| v.as_str())
                .unwrap_or("未知原因")
        )));
    }

    // conclusionType：1 合规 / 2 不合规 / 3 疑似
    let conclusion = value
        .get("conclusionType")
        .and_then(|v| v.as_i64())
        .unwrap_or(1);
    let mut best = Verdict::new("baidu", 0.0, "pass", truncate(&text), settings.threshold);
    if let Some(list) = value.get("data").and_then(|item| item.as_array()) {
        for item in list {
            let probability = item
                .get("probability")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let kind = item
                .get("type")
                .and_then(|v| v.as_i64())
                .map(|value| value.to_string())
                .or_else(|| item.get("msg").and_then(|v| v.as_str()).map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string());
            let msg = item
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let score = if conclusion == 2 {
                baidu_score(probability).max(100.0)
            } else {
                baidu_score(probability)
            };
            if score >= best.score {
                best = Verdict::new(
                    "baidu",
                    score,
                    &format!("{kind}{msg}"),
                    truncate(&text),
                    settings.threshold,
                );
            }
        }
    } else if conclusion == 2 {
        // 有结论没有明细：按不合规处理
        best = Verdict::new("baidu", 100.0, "不合规", truncate(&text), settings.threshold);
    }
    Ok(best)
}

/// 换 access_token。百度这一步单独一个接口，每次审图都换一遍太浪费，
/// 缓存到进程内存里（有效期 30 天，这里按 25 天提前换）。
async fn baidu_token(client: &reqwest::Client, settings: &ModerationSettings) -> Result<String> {
    use std::sync::Mutex;
    use std::time::Instant;

    static CACHE: Mutex<Option<(String, Instant)>> = Mutex::new(None);
    let key = format!("{}:{}", settings.access_key, settings.secret_key);

    if let Ok(guard) = CACHE.lock() {
        if let Some((token, saved_at)) = guard.as_ref() {
            if saved_at.elapsed() < Duration::from_secs(25 * 24 * 3600) {
                // 密钥换过就要重新换 token：把密钥指纹一起缓存
                if let Some((_, fingerprint)) = token.rsplit_once('|') {
                    if fingerprint == fingerprint_of(&key) {
                        return Ok(token.split('|').next().unwrap_or(token).to_string());
                    }
                }
            }
        }
    }

    let response = client
        .post("https://aip.baidubce.com/oauth/2.0/token")
        .timeout(REQUEST_TIMEOUT)
        .query(&[
            ("grant_type", "client_credentials"),
            ("client_id", settings.access_key.as_str()),
            ("client_secret", settings.secret_key.as_str()),
        ])
        .send()
        .await
        .map_err(|err| AppError::internal(format!("百度 access_token 请求失败：{err}")))?;
    let text = response
        .text()
        .await
        .map_err(|err| AppError::internal(format!("读取百度 access_token 响应失败：{err}")))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| AppError::internal(format!("百度 access_token 响应不是 JSON：{err}")))?;
    let Some(token) = value.get("access_token").and_then(|v| v.as_str()) else {
        return Err(AppError::internal(format!(
            "百度未能签发 access_token：{}",
            truncate(&text)
        )));
    };

    let token = token.to_string();
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some((format!("{token}|{}", fingerprint_of(&key)), Instant::now()));
    }
    Ok(token)
}

/// 缓存键的短指纹：不存密钥本身，只存它的哈希前缀。
fn fingerprint_of(key: &str) -> String {
    sha256_hex(key.as_bytes())[..16].to_string()
}

// ============================================================
// 自建 NSFWJS 服务
// ============================================================

/// 自建服务的响应解析。两种形态都认：
///
///   - 直接给分数：`{"nsfw": 0.92}`（字段名可配）。0-1 与 0-100 都按各自量纲处理；
///   - 原样转发 nsfwjs 的预测数组：`[{"className":"Porn","probability":0.9}, …]`，
///     这时把 Porn / Hentai / Sexy 三项的概率取最大——nsfwjs 的常规用法。
///
/// 也认 `{"result": true}` 这种布尔结论。
pub fn parse_self_hosted(body: &serde_json::Value, score_field: &str, threshold: i64) -> Verdict {
    let detail = truncate(&body.to_string());

    // 1) 预测数组
    let predictions = body
        .get("predictions")
        .and_then(|item| item.as_array())
        .or_else(|| body.as_array());
    if let Some(list) = predictions {
        let mut best = 0.0f64;
        let mut label = String::from("pass");
        for item in list {
            let Some(name) = item.get("className").and_then(|v| v.as_str()) else {
                continue;
            };
            let probability = item
                .get("probability")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "porn" | "hentai" | "sexy"
            ) && probability > best
            {
                best = probability;
                label = name.to_string();
            }
        }
        return Verdict::new(
            "nsfwjs",
            if best <= 1.0 { best * 100.0 } else { best },
            &label,
            detail,
            threshold,
        );
    }

    // 2) 布尔结论
    if let Some(flag) = body.get("result").and_then(|v| v.as_bool()) {
        let score = if flag { 100.0 } else { 0.0 };
        return Verdict::new("nsfwjs", score, if flag { "nsfw" } else { "pass" }, detail, threshold);
    }

    // 3) 数值字段：先按配置的字段名找，找不到再试常见名字
    let field = if score_field.trim().is_empty() {
        "nsfw"
    } else {
        score_field.trim()
    };
    let candidates = [
        field,
        "nsfw",
        "porn",
        "sexy",
        "hentai",
        "score",
        "probability",
        "rate",
        "confidence",
    ];
    for name in candidates {
        if let Some(value) = body.get(name).and_then(|v| v.as_f64()) {
            // 0-1 是概率，0-100 已经是分数
            let score = if value <= 1.0 { value * 100.0 } else { value };
            return Verdict::new(
                "nsfwjs",
                score,
                if score >= threshold as f64 { "nsfw" } else { "pass" },
                detail,
                threshold,
            );
        }
    }

    Verdict::new("nsfwjs", 0.0, "pass", detail, threshold)
}

async fn self_hosted(
    client: &reqwest::Client,
    settings: &ModerationSettings,
    bytes: &[u8],
    mime: &str,
) -> Result<Verdict> {
    let endpoint = settings.endpoint.trim();
    if endpoint.is_empty() {
        return Err(AppError::Config("自建审核服务没配接口地址".into()));
    }

    let payload = serde_json::json!({
        "image": base64::engine::general_purpose::STANDARD.encode(bytes),
        "mime": mime,
    });

    let mut request = client.post(endpoint).timeout(REQUEST_TIMEOUT).json(&payload);
    if !settings.token.trim().is_empty() {
        request = request.header("Authorization", format!("Bearer {}", settings.token.trim()));
    }
    let response = request
        .send()
        .await
        .map_err(|err| AppError::internal(format!("自建审核服务请求失败：{err}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| AppError::internal(format!("读取自建审核服务响应失败：{err}")))?;
    if !status.is_success() {
        return Err(AppError::internal(format!(
            "自建审核服务返回 {status}：{}",
            truncate(&text)
        )));
    }

    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| AppError::internal(format!("自建审核服务响应不是 JSON：{err}")))?;
    Ok(parse_self_hosted(&value, &settings.score_field, settings.threshold))
}

/// 日志与审计里只留一小段原始响应，别把整份 JSON 塞进库。
fn truncate(input: &str) -> String {
    const LIMIT: usize = 400;
    if input.chars().count() <= LIMIT {
        return input.to_string();
    }
    let mut out: String = input.chars().take(LIMIT).collect();
    out.push('…');
    out
}

// ============================================================
// 与上传流程的接合
// ============================================================

impl AppState {
    /// 上传前的内容审核。
    ///
    /// `Ok(())` 放行；`Err(ContentRejected)` 拦下——这条路径上审计已经写好，
    /// 该发信、该停号也都做完了。审核没开、或这个角色不过审时直接放行，
    /// 不产生任何外部请求。
    pub async fn review_upload(
        &self,
        user: &pixora_repo::model::User,
        image: ScreenInput<'_>,
        original_name: &str,
        ip: Option<&str>,
    ) -> Result<()> {
        let settings = self.settings.moderation().await?;
        if !settings.applies_to(user.is_admin()) {
            return Ok(());
        }
        if !settings.provider_ready() {
            tracing::warn!(
                provider = %settings.provider,
                "内容审核已开启但凭据没配齐，本次上传跳过审核"
            );
            return Ok(());
        }

        let verdict = match screen(&client(self), &settings, image).await {
            Ok(verdict) => verdict,
            Err(err) => {
                // 审核服务不可用时按配置决定放行还是拒绝。默认放行：
                // 让第三方接口的抖动掐掉整站上传，代价比漏一张大
                if settings.fail_open {
                    tracing::warn!(error = %err, "内容审核失败，按「放行」处理（moderation.fail_open）");
                    return Ok(());
                }
                return Err(AppError::StorageUnavailable(format!(
                    "内容审核服务暂时不可用，请稍后重试（{err}）"
                )));
            }
        };

        if !verdict.violation {
            return Ok(());
        }

        self.audit_detail(
            crate::audit::actions::MODERATION_BLOCK,
            Some(user.id),
            original_name.to_string(),
            serde_json::json!({
                "provider": verdict.provider,
                "label": verdict.label,
                "score": verdict.score,
                "threshold": settings.threshold,
                "response": verdict.detail,
            }),
            ip,
        )
        .await;

        // 24 小时内的违规次数：判「要不要自动停号」和「该不该再发一封信」都看它
        let since = chrono::Utc::now() - chrono::Duration::hours(24);
        let hits = self
            .repo
            .count_audit_action_since(
                Some(user.id),
                crate::audit::actions::MODERATION_BLOCK,
                since,
            )
            .await
            .unwrap_or(1);

        let will_ban =
            settings.ban_after > 0 && settings.ban_minutes > 0 && hits >= settings.ban_after;

        // 上报策略：第一例必报（新出现的违规者要有人知道），触发自动停号时报一次。
        // 中间每张都发信只会让管理员把通知静音掉，等于没报。
        if settings.notify_admin && (hits == 1 || will_ban) {
            self.notify_admins(user, &verdict, &settings, hits, original_name)
                .await;
        }

        if will_ban {
            self.suspend_for_moderation(user, &settings, hits, ip).await;
        }

        Err(AppError::ContentRejected {
            label: verdict.label,
            score: verdict.score,
            provider: verdict.provider,
        })
    }

    /// 违规上报：给每个活跃管理员发一封信。
    async fn notify_admins(
        &self,
        user: &pixora_repo::model::User,
        verdict: &Verdict,
        settings: &ModerationSettings,
        hits: i64,
        original_name: &str,
    ) {
        let admins = match self.repo.list_active_admins().await {
            Ok(admins) => admins,
            Err(err) => {
                tracing::warn!(error = %err, "读取管理员列表失败，内容审核上报没能发出");
                return;
            }
        };
        let vars = serde_json::json!({
            "email": user.email,
            "uid": user.uid,
            "label": verdict.label,
            "score": format!("{:.0}", verdict.score),
            "threshold": settings.threshold,
            "provider": verdict.provider,
            "name": original_name,
            "time": chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string(),
            "count": hits,
            "ban_after": settings.ban_after,
        });
        for admin in admins {
            if admin.id == user.id {
                // 自己违规自己收信没有意义
                continue;
            }
            if let Err(err) = self
                .dispatch_mail(
                    &admin.email,
                    pixora_mail::template::names::MODERATION_ALERT,
                    vars.clone(),
                )
                .await
            {
                tracing::warn!(error = %err, to = %admin.email, "内容审核上报邮件没能发出");
            }
        }
    }

    /// 违规累计到阈值：临时暂停账号，到期自动恢复。
    async fn suspend_for_moderation(
        &self,
        user: &pixora_repo::model::User,
        settings: &ModerationSettings,
        hits: i64,
        ip: Option<&str>,
    ) {
        let now = chrono::Utc::now();
        let until = now + chrono::Duration::minutes(settings.ban_minutes);
        let reason = format!(
            "24 小时内 {hits} 次上传未通过内容审核，账号已自动暂停，{}后自动恢复",
            describe_minutes(settings.ban_minutes)
        );

        if let Err(err) = self
            .repo
            .update_user_status_until(
                user.id,
                pixora_repo::model::status::SUSPENDED,
                Some(&reason),
                None,
                now,
                Some(until),
            )
            .await
        {
            tracing::warn!(error = %err, "自动暂停账号失败");
            return;
        }
        let _ = self
            .repo
            .insert_user_action(
                user.id,
                None,
                "auto_suspend_moderation",
                Some(&reason),
                &serde_json::json!({ "hits": hits, "until": until.to_rfc3339() }),
                false,
                now,
            )
            .await;
        // 暂停立即生效：把已登录的会话踢掉，否则手上那个会话还能接着传
        let _ = self.repo.revoke_all_sessions(user.id, now).await;

        self.audit_detail(
            crate::audit::actions::MODERATION_BAN,
            Some(user.id),
            &user.uid,
            serde_json::json!({ "hits": hits, "until": until.to_rfc3339(), "reason": reason }),
            ip,
        )
        .await;

        let appeal = self
            .settings
            .mail()
            .await
            .map(|mail| {
                if mail.appeal_email.trim().is_empty() {
                    mail.from_email
                } else {
                    mail.appeal_email
                }
            })
            .unwrap_or_default();
        if let Err(err) = self
            .dispatch_mail(
                &user.email,
                pixora_mail::template::names::SUSPEND_NOTICE,
                serde_json::json!({
                    "email": user.email,
                    "uid": user.uid,
                    "reason": reason,
                    "time": now.format("%Y-%m-%d %H:%M UTC").to_string(),
                    "requirements": "无需提交材料：这是内容审核触发的自动暂停，到期会自动恢复",
                    "appeal_email": appeal,
                }),
            )
            .await
        {
            tracing::warn!(error = %err, "自动暂停通知邮件没能发出");
        }
    }

    /// 到期的临时处置：把账号放回 active。
    ///
    /// 每个鉴权入口都会先调一次，所以「到期」这个动作不需要另外的定时任务——
    /// 用户下次访问就是恢复的时刻。管理员手填了时长的暂停/封禁与内容审核的
    /// 自动暂停走同一条路；没填时长的（`auto_unsuspend_at` 为空）是永久处置，
    /// 只能人工解封。
    pub async fn lift_expired_suspension(
        &self,
        user: &pixora_repo::model::User,
    ) -> Option<pixora_repo::model::User> {
        use pixora_repo::model::status;
        let until = user.auto_unsuspend_at?;
        if user.status != status::SUSPENDED && user.status != status::BANNED {
            return None;
        }
        if until > chrono::Utc::now() {
            return None;
        }

        let now = chrono::Utc::now();
        if let Err(err) = self
            .repo
            .update_user_status_until(
                user.id,
                pixora_repo::model::status::ACTIVE,
                None,
                None,
                now,
                None,
            )
            .await
        {
            tracing::warn!(error = %err, "自动解除暂停失败");
            return None;
        }
        let _ = self
            .repo
            .insert_user_action(
                user.id,
                None,
                "auto_unsuspend",
                Some("处置到期，账号已自动恢复"),
                &serde_json::json!({ "until": until.to_rfc3339() }),
                false,
                now,
            )
            .await;
        self.audit_detail(
            crate::audit::actions::ACCOUNT_RESTORE,
            Some(user.id),
            &user.uid,
            serde_json::json!({ "auto": true, "reason": "处置到期" }),
            None,
        )
        .await;
        tracing::info!(uid = %user.uid, "临时处置已到期，账号已恢复");

        self.repo.find_user_by_id(user.id).await.ok().flatten()
    }
}

/// 「1440 分钟」说成人话，通知邮件里要读得懂。
fn describe_minutes(minutes: i64) -> String {
    if minutes > 0 && minutes % 1440 == 0 {
        format!("{} 天", minutes / 1440)
    } else if minutes > 0 && minutes % 60 == 0 {
        format!("{} 小时", minutes / 60)
    } else {
        format!("{minutes} 分钟")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encoding_matches_aliyun_rules() {
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("a+b"), "a%2Bb");
        assert_eq!(percent_encode("a*b"), "a%2Ab");
        assert_eq!(percent_encode("a~b"), "a~b");
        assert_eq!(percent_encode("/"), "%2F");
    }

    #[test]
    fn aliyun_string_to_sign_is_sorted_and_encoded() {
        let params = vec![
            ("b".to_string(), "2".to_string()),
            ("a".to_string(), "1 2".to_string()),
        ];
        // 排序后 a 在前；值里的空格按 %20 编码，整串再编码一次（% 变 %25）
        let signed = aliyun_string_to_sign(&params);
        assert!(signed.starts_with("POST&%2F&a%3D1%25202%26b%3D2"));
    }

    #[test]
    fn aliyun_signature_is_deterministic() {
        let first = aliyun_signature("secret", "POST&%2F&a%3D1");
        let second = aliyun_signature("secret", "POST&%2F&a%3D1");
        assert_eq!(first, second);
        assert_ne!(first, aliyun_signature("other", "POST&%2F&a%3D1"));
        // base64 的 SHA1 摘要固定 20 字节 → 28 个字符带一个 =
        assert_eq!(first.len(), 28);
    }

    #[test]
    fn tencent_authorization_has_expected_shape() {
        let header = tencent_authorization(
            "AKIDEXAMPLE",
            "SECRET",
            "ims.tencentcloudapi.com",
            "ImageModeration",
            1_700_000_000,
            "{}",
        );
        assert!(header.starts_with("TC3-HMAC-SHA256 Credential=AKIDEXAMPLE/2023-11-14/ims/tc3_request"));
        assert!(header.contains("SignedHeaders=content-type;host;x-tc-action"));
        assert!(header.contains("Signature="));
    }

    #[test]
    fn self_hosted_parses_score_and_predictions() {
        let score = parse_self_hosted(&serde_json::json!({"nsfw": 0.85}), "nsfw", 70);
        assert!(score.violation);
        assert!((score.score - 85.0).abs() < 0.01);

        // 已经是 0-100 的分数按原样
        let percent = parse_self_hosted(&serde_json::json!({"rate": 20}), "nsfw", 70);
        assert!(!percent.violation);

        // nsfwjs 的原始预测数组：取三项里最高的那个概率
        let predictions = parse_self_hosted(
            &serde_json::json!({"predictions": [
                {"className": "Neutral", "probability": 0.7},
                {"className": "Porn", "probability": 0.1},
                {"className": "Hentai", "probability": 0.9},
            ]}),
            "nsfw",
            70,
        );
        assert!(predictions.violation);
        assert!((predictions.score - 90.0).abs() < 0.01);
        assert_eq!(predictions.label, "Hentai");

        // 布尔结论
        let blocked = parse_self_hosted(&serde_json::json!({"result": true}), "nsfw", 70);
        assert!(blocked.violation);

        // 看不懂的响应按「没违规」处理，别误伤
        let unknown = parse_self_hosted(&serde_json::json!({"foo": "bar"}), "nsfw", 70);
        assert!(!unknown.violation);
    }

    /// 自动暂停与手动暂停的区别：到点自动放人，手动处置不受影响。
    #[tokio::test]
    async fn expired_auto_suspension_lifts_but_manual_ones_do_not() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = crate::tests_support::test_state(&dir).await;
        let user = crate::tests_support::make_user(&state, "moderation@example.com", "2001").await;

        // 手工暂停（没有到期时间）：不该被自动恢复
        state
            .repo
            .update_user_status(
                user.id,
                pixora_repo::model::status::SUSPENDED,
                Some("管理员手工暂停"),
                None,
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        let reloaded = state.repo.find_user_by_id(user.id).await.unwrap().unwrap();
        assert!(
            state.lift_expired_suspension(&reloaded).await.is_none(),
            "手工暂停没有到期时间，不该被自动解除"
        );

        // 自动暂停，但还没到期：也不动
        let later = chrono::Utc::now() + chrono::Duration::minutes(30);
        state
            .repo
            .update_user_status_until(
                user.id,
                pixora_repo::model::status::SUSPENDED,
                Some("内容审核自动暂停"),
                None,
                chrono::Utc::now(),
                Some(later),
            )
            .await
            .unwrap();
        let reloaded = state.repo.find_user_by_id(user.id).await.unwrap().unwrap();
        assert!(
            state.lift_expired_suspension(&reloaded).await.is_none(),
            "还没到期就恢复，等于封禁没生效"
        );

        // 到期了：放人
        let past = chrono::Utc::now() - chrono::Duration::minutes(1);
        state
            .repo
            .update_user_status_until(
                user.id,
                pixora_repo::model::status::SUSPENDED,
                Some("内容审核自动暂停"),
                None,
                chrono::Utc::now(),
                Some(past),
            )
            .await
            .unwrap();
        let reloaded = state.repo.find_user_by_id(user.id).await.unwrap().unwrap();
        let restored = state
            .lift_expired_suspension(&reloaded)
            .await
            .expect("到点要放人");
        assert_eq!(restored.status, pixora_repo::model::status::ACTIVE);
        assert!(restored.auto_unsuspend_at.is_none(), "恢复后清掉到期时间");
        state.ensure_loginable(&restored).expect("恢复后应当能登录");
    }

    #[test]
    fn baidu_probability_scales_to_percent() {
        assert!((baidu_score(0.93) - 93.0).abs() < 0.01);
        assert!((baidu_score(93.0) - 93.0).abs() < 0.01);
    }
}
