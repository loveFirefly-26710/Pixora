//! 运行时配置：全部来自环境变量。
//!
//! 存储路径这类配置刻意只走环境变量，界面上只读展示（ADR-0003）。理由在
//! 设计稿里写得明白：容器里的挂载点改不了，改了也写不进去；而配置文件一旦
//! 可写，就等于给了一个远程改服务器目录的入口。
//!
//! 两个容易踩的点也在设计稿里点过名：
//!   - `PIXORA_BASE_URL` 在反向代理后面必须显式设置，否则自动探测出来的是容器内网地址，直链全错；
//!   - `PIXORA_TRUST_PROXY` 只信最靠近服务端的那一层，设成不限制等于让客户端
//!     伪造 `X-Forwarded-For` 绕过 IP 黑名单。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use pixora_crypto::MasterKey;
use pixora_repo::DbDriver;

use crate::error::{AppError, Result};

/// 主密钥的来源，界面上要如实展示给管理员看。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// 来自 `PIXORA_MASTER_KEY`
    Env,
    /// 来自 `PIXORA_MASTER_KEY_FILE`
    File(PathBuf),
    /// 首次启动自动生成并写入 `{data_dir}/master.key`
    Generated(PathBuf),
    /// 还没有（未安装且未生成）
    Missing,
}

impl KeySource {
    pub fn describe(&self) -> String {
        match self {
            KeySource::Env => "环境变量 PIXORA_MASTER_KEY".into(),
            KeySource::File(path) => format!("密钥文件 {}", path.display()),
            KeySource::Generated(path) => {
                format!("首次启动自动生成，已写入 {}", path.display())
            }
            KeySource::Missing => "未配置".into(),
        }
    }

    /// 安全提示：自动生成的密钥文件适合单机自托管，但不是最稳的做法。
    pub fn is_weak_setup(&self) -> bool {
        matches!(self, KeySource::Generated(_))
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub db_driver: DbDriver,
    pub db_dsn: String,
    pub db_max_connections: u32,
    pub data_dir: PathBuf,
    pub master_key: Option<MasterKey>,
    pub key_source: KeySource,
    pub workers: usize,
    /// 显式配的站点地址；为空时由请求头推断
    pub base_url: Option<String>,
    /// 可信代理层数。0 表示不信任任何转发头。
    pub trust_proxy: usize,
    /// 前端产物目录：给了就读盘（前端联调方便），否则用嵌进二进制的版本
    pub web_dir: Option<PathBuf>,
    /// Caddy / Nginx 的 X-Accel-Redirect 前缀，配了就让内核直接发文件
    pub accel_prefix: Option<String>,
    /// Cookie 是否带 Secure。默认按 base_url 是否 https 决定。
    pub secure_cookies: bool,
    /// 是否把邮箱验证码放进 API 响应里（仅供本机开发）。默认关。
    ///
    /// 默认关是有理由的：只要 SMTP 没配好（比如填了服务器却漏了发信地址），
    /// 服务端就发不出邮件——如果这时回显验证码，任何能调注册接口的人都能
    /// 用别人的邮箱把账号建出来。验证码始终会写进服务端日志，本机开发看日志即可。
    pub dev_return_codes: bool,
    pub log_level: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let db_driver = match env("PIXORA_DB_DRIVER") {
            Some(raw) => DbDriver::parse(&raw)?,
            None => DbDriver::Sqlite,
        };

        let db_dsn = match env("PIXORA_DB_DSN") {
            Some(dsn) => dsn,
            None => {
                // 没给 DSN 就落到数据目录里的单文件库——单机自托管的默认姿态
                let data_dir = env("PIXORA_DATA_DIR").unwrap_or_else(|| "./data".into());
                format!("{}/pixora.db", data_dir.trim_end_matches(['/', '\\']))
            }
        };

        let data_dir = PathBuf::from(env("PIXORA_DATA_DIR").unwrap_or_else(|| "./data".into()));

        let (master_key, key_source) = load_master_key(&data_dir)?;

        let bind = env("PIXORA_BIND")
            .unwrap_or_else(|| "0.0.0.0:8080".into())
            .parse()
            .map_err(|err| AppError::Config(format!("PIXORA_BIND 不是合法的监听地址：{err}")))?;

        let workers = env("PIXORA_WORKERS")
            .and_then(|raw| raw.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or_else(default_workers);

        let trust_proxy = env("PIXORA_TRUST_PROXY")
            .and_then(|raw| raw.parse::<usize>().ok())
            .unwrap_or(1);

        let base_url = env("PIXORA_BASE_URL").map(|raw| raw.trim_end_matches('/').to_string());
        let secure_cookies = match env("PIXORA_SECURE_COOKIES") {
            Some(raw) => matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
            None => base_url
                .as_deref()
                .map(|url| url.starts_with("https://"))
                .unwrap_or(false),
        };

        let db_max_connections = env("PIXORA_DB_MAX_CONNECTIONS")
            .and_then(|raw| raw.parse::<u32>().ok())
            .unwrap_or(if db_driver == DbDriver::Sqlite { 4 } else { 10 });

        Ok(Self {
            bind,
            db_driver,
            db_dsn,
            db_max_connections,
            data_dir,
            master_key,
            key_source,
            workers,
            base_url,
            trust_proxy,
            web_dir: env("PIXORA_WEB_DIR").map(PathBuf::from),
            accel_prefix: env("PIXORA_ACCEL_PREFIX"),
            secure_cookies,
            dev_return_codes: matches!(
                env("PIXORA_DEV_RETURN_CODES")
                    .unwrap_or_default()
                    .to_ascii_lowercase()
                    .as_str(),
                "1" | "true" | "yes"
            ),
            log_level: env("RUST_LOG").unwrap_or_else(|| "info".into()),
        })
    }

    /// 加密用的主密钥。需要加密凭据时调用——安装了之后一定拿得到，
    /// 因为在服务启动阶段就会把密钥文件准备好。
    pub fn require_master_key(&self) -> Result<&MasterKey> {
        self.master_key.as_ref().ok_or_else(|| {
            AppError::Config("缺少主密钥：请设置 PIXORA_MASTER_KEY 或让服务自动生成密钥文件".into())
        })
    }

    /// 兜底地址：没有配 base_url 时用请求推断（仅用于开发环境）。
    pub fn fallback_base_url(&self) -> String {
        format!("http://{}", self.bind)
    }

    pub fn upload_prefix_note(&self) -> String {
        format!(
            "数据目录来自环境变量 PIXORA_DATA_DIR（{}），界面上只读展示",
            self.data_dir.display()
        )
    }
}

fn default_workers() -> usize {
    std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(2)
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 主密钥的加载顺序：环境变量 → 密钥文件 → 自动生成到数据目录。
///
/// 自动生成这一步是为了让「一条命令起服务」这件事真的成立：
/// 没有它，首次启动就没有密钥可用，安装向导里的 SMTP 密码就存不进去。
/// 生成的同时会把来源记下来，界面上如实告诉管理员当前用的是哪种方式。
fn load_master_key(data_dir: &Path) -> Result<(Option<MasterKey>, KeySource)> {
    if let Some(raw) = env("PIXORA_MASTER_KEY") {
        let key = MasterKey::parse(&raw)?;
        return Ok((Some(key), KeySource::Env));
    }

    let key_path = env("PIXORA_MASTER_KEY_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("master.key"));

    if key_path.exists() {
        let raw = std::fs::read_to_string(&key_path)
            .map_err(|err| AppError::Config(format!("读取密钥文件失败：{err}")))?;
        let key = MasterKey::parse(&raw)?;
        return Ok((Some(key), KeySource::File(key_path)));
    }

    // 数据目录不可写时不要硬造密钥，让上层明确知道「没有密钥」
    if let Err(err) = std::fs::create_dir_all(data_dir) {
        tracing::warn!(dir = %data_dir.display(), error = %err, "数据目录不可用，跳过主密钥生成");
        return Ok((None, KeySource::Missing));
    }

    match MasterKey::generate() {
        Ok(key) => {
            match std::fs::write(&key_path, key.to_hex()) {
                Ok(()) => {
                    restrict_permissions(&key_path);
                    tracing::warn!(
                        path = %key_path.display(),
                        "未配置 PIXORA_MASTER_KEY，已自动生成主密钥文件。生产环境建议改用环境变量注入，并把该文件纳入备份。"
                    );
                    Ok((Some(key), KeySource::Generated(key_path)))
                }
                Err(err) => {
                    tracing::warn!(error = %err, "主密钥写入失败，本次运行不加密新凭据");
                    Ok((Some(key), KeySource::Missing))
                }
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "主密钥生成失败");
            Ok((None, KeySource::Missing))
        }
    }
}

/// 密钥文件只给属主读写。Windows 上不做额外处理（ACL 需要专门 API，
/// 而生产环境是 Linux 容器）。
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_source_descriptions_are_honest() {
        assert!(KeySource::Env.describe().contains("PIXORA_MASTER_KEY"));
        assert!(!KeySource::Env.is_weak_setup());
        assert!(KeySource::Generated(PathBuf::from("/data/master.key")).is_weak_setup());
        assert_eq!(KeySource::Missing.describe(), "未配置");
    }
}
