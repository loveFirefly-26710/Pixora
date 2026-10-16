/**
 * 系统设置（`#/admin/settings`）。
 *
 * 站点、上传策略与格式白名单、会话、频率限制与超限处理、缩略图策略、
 * 防盗链、内容审核、邮件与注册验证、外观、用户额度与用量。
 * 路径类配置已经移出这里（改由环境变量注入），别再往回加。
 *
 * 三条保存路径要分清：
 *   - 格式白名单改动走 `POST /admin/settings/formats`，勾选即生效，不参与「保存设置」；
 *   - 频率限制那几张卡片走 `/admin/ratelimit` 与 `/admin/ratelimit/on_breach`；
 *   - 其余都走 `PUT /admin/settings`，只提交页面上真的改过的子对象。
 * 后两条由同一个「保存设置」按钮触发。
 */

import { ApiError, request } from "../core/api";
import { absUrl } from "../core/config";
import { h, mount } from "../core/dom";
import { formatBytes, formatNumber, parseSize } from "../core/format";
import { confirmDialog, promptDialog } from "../core/modal";
import { openMailTemplates } from "./mail-templates";
import { loginMethodsCard } from "./login-methods-card";
import type { PageContext } from "../core/router";
import { toastError, toastOk } from "../core/toast";
import { currentUser, loadMe, loadSite } from "../core/state";
import { BUILTIN_AVATAR, refreshNav } from "../ui/shell";
import type { AdminSettings, RateLimitAdmin } from "../core/types";
import {
  apiRateCard,
  downloadRateCard,
  breachCard,
  rateLimitDraft,
  rateLogsButton,
  saveRateLimit,
  uploadRateCard,
  usersCard,
  validateRateLimit,
  type RateLimitDraft,
} from "./ratelimit-cards";
import {
  badge,
  card,
  cardHead,
  cardNode,
  field,
  note,
  pageHead,
  qtip,
  select,
  spinner,
  switchRow,
  textInput,
} from "../ui/widgets";

/**
 * 白名单里可选的格式，与后端的解码器一一对应。
 *
 * 服务端会在 `upload.format_options` 里给一份（含显示名），拿到就用它——
 * 清单的真相在后端，前端写死一份迟早会和新增的格式脱节。
 * JPEG 在库里叫 `jpg`（嗅探与存储都用这个名字），界面显示成 JPEG。
 */
const FALLBACK_FORMATS: Array<{ ext: string; label: string }> = [
  { ext: "jpg", label: "JPG" },
  { ext: "png", label: "PNG" },
  { ext: "gif", label: "GIF" },
  { ext: "webp", label: "WEBP" },
  { ext: "avif", label: "AVIF" },
  { ext: "heic", label: "HEIC" },
  { ext: "tiff", label: "TIFF" },
  { ext: "bmp", label: "BMP" },
  { ext: "apng", label: "APNG" },
  { ext: "svg", label: "SVG" },
  { ext: "jxl", label: "JXL" },
  { ext: "ico", label: "ICO" },
];

/** 广场每行几张图。列数越少，单张预览越大。 */
const GRID_COLUMN_OPTIONS = [
  { value: "2", label: "2 列 · 最大" },
  { value: "3", label: "3 列 · 推荐" },
  { value: "4", label: "4 列" },
  { value: "5", label: "5 列" },
  { value: "6", label: "6 列 · 最小" },
];

const SECURITY_OPTIONS = [
  { value: "ssl", label: "SSL（465，隐式加密）" },
  { value: "starttls", label: "STARTTLS（587）" },
  { value: "plain", label: "不加密（仅内网自建）" },
];

/** 页面上可编辑的值集中在这一份里。 */
interface SettingsDraft {
  siteName: HTMLInputElement;
  baseUrl: HTMLInputElement;
  plazaPublic: boolean;
  plazaColumns: HTMLSelectElement;
  recycleRetention: HTMLInputElement;
  requireLogin: boolean;
  registerEnabled: boolean;
  /** 允许注册的邮箱域名白名单，逗号分隔；空 = 不限 */
  emailDomains: HTMLInputElement;
  maxFileBytes: HTMLInputElement;
  maxBatchSize: HTMLInputElement;
  maxPixels: HTMLInputElement;
  maxWidth: HTMLInputElement;
  maxHeight: HTMLInputElement;
  queueDepth: HTMLInputElement;
  sessionIdle: HTMLInputElement;
  sessionAbsolute: HTMLInputElement;
  sessionRemember: HTMLInputElement;
  hotlinkEnabled: boolean;
  hotlinkAllowEmpty: boolean;
  hotlinkDomains: HTMLInputElement;
  moderationEnabled: boolean;
  moderationProvider: HTMLSelectElement;
  moderationThreshold: HTMLInputElement;
  moderationEndpoint: HTMLInputElement;
  moderationScoreField: HTMLInputElement;
  moderationRegion: HTMLInputElement;
  moderationAppId: HTMLInputElement;
  moderationAccessKey: HTMLInputElement;
  moderationSecretKey: HTMLInputElement;
  moderationToken: HTMLInputElement;
  moderationUserEnabled: boolean;
  moderationAdminEnabled: boolean;
  moderationBanAfter: HTMLInputElement;
  moderationBanMinutes: HTMLInputElement;
  moderationNotify: boolean;
  moderationFailOpen: boolean;
  thumbClient: boolean;
  thumbFallback: boolean;
  smtpHost: HTMLInputElement;
  smtpPort: HTMLInputElement;
  smtpSecurity: HTMLSelectElement;
  smtpUser: HTMLInputElement;
  smtpPassword: HTMLInputElement;
  fromName: HTMLInputElement;
  fromEmail: HTMLInputElement;
  appealEmail: HTMLInputElement;
  disposableCheck: boolean;
  mxCheck: boolean;
  thirdPartyCheck: boolean;
  background: HTMLInputElement;
  /** 站点默认头像地址（外链或上传那张的公开地址） */
  defaultAvatar: HTMLInputElement;
  /** 格式白名单改一处就提交一次，这里只是当前选中态 */
  formats: string[];
}

/** 加载态。 */
function loadingBlock(text: string): HTMLElement {
  return h(
    "div.card",
    { style: { display: "flex", alignItems: "center", gap: "8px", color: "var(--muted)", fontSize: "13px" } },
    spinner(),
    text,
  );
}

/** 错误态：服务端原话 + 重试。 */
function errorBlock(err: unknown, onRetry: () => void): HTMLElement {
  const message = err instanceof ApiError ? err.message : err instanceof Error ? err.message : String(err);
  const detail = err instanceof ApiError ? err.detail : null;
  return card(
    null,
    qtip("err", "加载失败", detail ? `${message}（${detail}）` : message),
    h("button.btn", { type: "button", text: "重试", on: { click: onRetry } }),
  );
}

function intOf(input: HTMLInputElement): number {
  const value = Number(input.value.trim());
  return Number.isFinite(value) ? Math.max(0, Math.round(value)) : 0;
}

/** 容量输入：不带单位按默认单位解释。 */
function sizeOf(input: HTMLInputElement, unit: string): number {
  const text = input.value.trim();
  if (!text) return 0;
  return parseSize(/[a-z]$/i.test(text) ? text : `${text} ${unit}`) ?? 0;
}

function unitInput(input: HTMLInputElement, unit: string): HTMLElement {
  return h(
    "div.row",
    null,
    input,
    h("span", { style: { flex: "0 0 60px", lineHeight: "34px", color: "var(--muted)" }, text: unit }),
  );
}

export async function render(ctx: PageContext): Promise<void> {
  const root = h("div.pad");
  mount(ctx.outlet, root);
  root.append(loadingBlock("正在读取系统设置…"));
  const reload = (): void => void render(ctx);

  try {
    // 频率限制那几张卡片也在这一页，两个接口一起拉：
    // 分两次渲染会让页面先闪一遍半成品
    const [data, rate] = await Promise.all([
      request<AdminSettings>("/admin/settings"),
      request<RateLimitAdmin>("/admin/ratelimit"),
    ]);
    mount(root, ...view(reload, data, rate));
  } catch (err) {
    mount(root, errorBlock(err, reload));
  }
}

function view(reload: () => void, data: AdminSettings, rate: RateLimitAdmin): Node[] {
  const draft: SettingsDraft = {
    siteName: textInput({ value: data.site.name }),
    baseUrl: textInput({ value: data.site.base_url, mono: true }),
    plazaPublic: data.site.plaza_public,
    plazaColumns: select(GRID_COLUMN_OPTIONS, String(data.site.plaza_grid_columns ?? 3)),
    recycleRetention: textInput({
      value: String(data.site.recycle_retention_days ?? 30),
      type: "number",
      mono: true,
      min: "0",
      max: "365",
    }),
    requireLogin: data.site.require_login_to_upload,
    registerEnabled: data.site.register_enabled,
    emailDomains: textInput({
      value: data.site.email_domains ?? "",
      mono: true,
      placeholder: "example.com, corp.example.com",
    }),
    maxFileBytes: textInput({ value: formatBytes(data.upload.max_file_bytes), mono: true }),
    maxBatchSize: textInput({ value: String(data.upload.max_batch_size), type: "number", mono: true }),
    maxPixels: textInput({ value: String(Math.round(data.upload.max_pixels / 10000)), type: "number", mono: true }),
    queueDepth: textInput({ value: String(data.upload.queue_depth), type: "number", mono: true }),
    thumbClient: data.thumb.client,
    thumbFallback: data.thumb.server_fallback,
    smtpHost: textInput({ value: data.mail.smtp_host, mono: true }),
    smtpPort: textInput({ value: String(data.mail.smtp_port), type: "number", mono: true }),
    smtpSecurity: select(SECURITY_OPTIONS, data.mail.smtp_security || "ssl"),
    smtpUser: textInput({ value: data.mail.smtp_user, mono: true }),
    smtpPassword: textInput({
      type: "password",
      placeholder: data.mail.password_set ? "已配置（留空表示不修改）" : "还没配置 SMTP 密码",
    }),
    fromName: textInput({ value: data.mail.from_name }),
    fromEmail: textInput({ value: data.mail.from_email, mono: true }),
    appealEmail: textInput({ value: data.mail.appeal_email, mono: true }),
    disposableCheck: data.mail.disposable_check,
    mxCheck: data.mail.mx_check,
    thirdPartyCheck: data.mail.third_party_check,
    background: textInput({ value: data.appearance.background ?? "", mono: true }),
    defaultAvatar: textInput({ value: data.appearance.default_avatar ?? "", mono: true, name: "default-avatar" }),
    formats: [...(data.upload.formats ?? [])],
    maxWidth: textInput({ value: String(data.upload.max_width ?? 0), type: "number", mono: true }),
    maxHeight: textInput({ value: String(data.upload.max_height ?? 0), type: "number", mono: true }),
    sessionIdle: textInput({
      value: String(data.session?.idle_minutes ?? 120),
      type: "number",
      mono: true,
      min: "0",
    }),
    sessionAbsolute: textInput({
      value: String(data.session?.absolute_days ?? 30),
      type: "number",
      mono: true,
      min: "0",
    }),
    sessionRemember: textInput({
      value: String(data.session?.remember_days ?? 30),
      type: "number",
      mono: true,
      min: "1",
    }),
    hotlinkEnabled: data.security?.hotlink_enabled ?? false,
    hotlinkAllowEmpty: data.security?.hotlink_allow_empty ?? true,
    hotlinkDomains: textInput({ value: data.security?.hotlink_domains ?? "", mono: true }),
    moderationEnabled: data.moderation?.enabled ?? false,
    moderationProvider: select(
      [
        { value: "aliyun", label: "阿里云内容安全" },
        { value: "tencent", label: "腾讯云图片内容安全" },
        { value: "baidu", label: "百度图像审核" },
        { value: "nsfwjs", label: "自建 NSFWJS 服务" },
      ],
      data.moderation?.provider ?? "aliyun",
    ),
    moderationThreshold: textInput({
      value: String(data.moderation?.threshold ?? 70),
      type: "number",
      mono: true,
      min: "1",
      max: "100",
    }),
    moderationEndpoint: textInput({
      value: data.moderation?.endpoint ?? "",
      mono: true,
      placeholder: "https://nsfw.example.com/check",
    }),
    moderationScoreField: textInput({ value: data.moderation?.score_field ?? "nsfw", mono: true }),
    moderationRegion: textInput({ value: data.moderation?.region ?? "", mono: true }),
    moderationAppId: textInput({ value: data.moderation?.app_id ?? "", mono: true }),
    moderationAccessKey: textInput({
      type: "password",
      placeholder: data.moderation?.access_key_set ? "已配置（留空表示不修改）" : "AccessKeyId / SecretId / API Key",
    }),
    moderationSecretKey: textInput({
      type: "password",
      placeholder: data.moderation?.secret_key_set ? "已配置（留空表示不修改）" : "AccessKeySecret / SecretKey",
    }),
    moderationToken: textInput({
      type: "password",
      placeholder: data.moderation?.token_set ? "已配置（留空表示不修改）" : "自建服务的 Bearer Token（可留空）",
    }),
    moderationUserEnabled: data.moderation?.user_enabled ?? true,
    moderationAdminEnabled: data.moderation?.admin_enabled ?? false,
    moderationBanAfter: textInput({
      value: String(data.moderation?.ban_after ?? 3),
      type: "number",
      mono: true,
      min: "0",
    }),
    moderationBanMinutes: textInput({
      value: String(data.moderation?.ban_minutes ?? 1440),
      type: "number",
      mono: true,
      min: "0",
    }),
    moderationNotify: data.moderation?.notify_admin ?? true,
    moderationFailOpen: data.moderation?.fail_open ?? true,
  };
  const rateDraft = rateLimitDraft(rate);
  // 登录方式那张卡自己拉配置：整页草稿里塞不下密钥这类东西
  const loginMethodsHost = h("div");
  void loginMethodsCard().then((node) => loginMethodsHost.replaceChildren(node));

  return [
    pageHead(
      "系统设置",
      "站点、上传策略、格式白名单、缩略图策略、会话、频率限制、内容审核、防盗链、邮件与反临时邮箱。" +
        "路径类配置已移出，改由环境变量注入。",
    ),
    // 卡片流：按高度自己排，短卡片不再被撑成长卡片那样高（见 components.css）
    h(
      "div.cardflow",
      null,
      siteCard(draft, data),
      sessionCard(draft),
      uploadCard(draft, data, rate, rateDraft),
      apiRateCard(rateDraft),
      uploadRateCard(rateDraft),
      downloadRateCard(rateDraft),
      breachCard(rateDraft),
      usersCard(reload, rate, data.upload.format_options?.length ? data.upload.format_options : FALLBACK_FORMATS),
      thumbCard(draft),
      defaultAvatarCard(reload, draft, data),
      securityCard(draft),
      moderationCard(draft),
      mailCard(draft),
      // 登录方式自己拉一次配置、自己提交（与 SMTP 那张卡不同：它不需要
      // 跟着整页草稿走，改了立刻生效更顺手）
      loginMethodsHost,
      appearanceCard(reload, draft, data),
    ),
    h(
      "div.flexline",
      { style: { justifyContent: "flex-end", marginTop: "16px" } },
      rateLogsButton(),
      h("button.btn.pri", {
        type: "button",
        text: "保存设置",
        on: {
          click: (event) => void save(reload, draft, data, rateDraft, event.target as HTMLButtonElement),
        },
      }),
    ),
    note(
      "w",
      "上传配额、注册默认配额、容量阈值不在这里——它们在「配额与容量」页统一算账。" +
        "这里只管「允不允许、多大、什么格式、多久一次」。",
    ),
  ];
}

/** 站点：基础 URL、名称与三个开关。 */
function siteCard(draft: SettingsDraft, data: AdminSettings): HTMLElement {
  return cardNode(
    cardHead("站点"),
    field("基础 URL", draft.baseUrl, "直链与邮件里的链接都以它为前缀，末尾不要带斜杠"),
    field("站点名称", draft.siteName),
    switchRow("广场开放浏览", "未登录用户可以看到公开图片", draft.plazaPublic, (next) => {
      draft.plazaPublic = next;
    }),
    field(
      "广场每行列数",
      draft.plazaColumns,
      "决定广场预览图的大小：列数越少，单张图越大。窄屏（手机）会自动少放几列，不受这里影响。",
    ),
    field(
      "回收站保留天数",
      draft.recycleRetention,
      "删除的图片与相册先留在回收站，这个天数内可恢复；到期由后台彻底删除。填 0 关闭回收站。",
    ),
    switchRow("拒绝游客上传", "上传必须登录", draft.requireLogin, (next) => {
      draft.requireLogin = next;
    }),
    switchRow(
      "允许用户注册",
      data.site.register_auto_closed
        ? "关闭后只能由管理员创建账号（当前由容量阈值自动关闭）"
        : "关闭后只能由管理员创建账号",
      draft.registerEnabled,
      (next) => {
        draft.registerEnabled = next;
      },
    ),
    // 开关被自动关闭替管理员翻过时，把原因写在旁边：
    // 不解释一句，管理员只会看到「开关自己关了」而不知道怎么恢复
    data.site.register_auto_closed
      ? qtip(
          "warn",
          "注册已被容量自动关闭",
          (data.site.register_closed_reason ?? "占用率超过了触发阈值") +
            "。把上面的开关重新打开即可恢复注册——你的手开会优先，容量恢复到恢复阈值以下之前不会再被自动关闭。",
        )
      : data.site.register_auto_close_override
        ? qtip(
            "warn",
            "容量自动关闭暂时不生效（你手动开启的）",
            (() => {
              const policy = data.site.register_auto_close;
              const head =
                policy && policy.percent != null
                  ? `占用已达 ${policy.percent.toFixed(1)}%（触发阈值 ${policy.on}%）：`
                  : "占用率已经在触发阈值之上：";
              return (
                head +
                "注册是你手动打开的，这一层不会把它按回去。" +
                (policy
                  ? `占用降到 ${policy.off}% 以下、之后再涨过 ${policy.on}% 时，才会重新触发自动关闭。`
                  : "")
              );
            })(),
          )
        : null,
    field(
      "允许注册的邮箱域名",
      draft.emailDomains,
      "逗号分隔，例如 example.com, corp.example.com；子域自动算命中。留空表示不限域名",
    ),
    h("div.tip", { text: "注册相关的默认配额和容量阈值在「配额与容量」里配。" }),
  );
}

/** 上传策略 + 格式白名单 chips（改动即时提交）。 */
function uploadCard(
  draft: SettingsDraft,
  data: AdminSettings,
  rate: RateLimitAdmin,
  rateDraft: RateLimitDraft,
): HTMLElement {
  const chips = h("div.flexline");
  const result = h("div", { style: { marginTop: "8px" } });
  const formatOptions =
    data.upload.format_options && data.upload.format_options.length > 0
      ? data.upload.format_options
      : FALLBACK_FORMATS;

  // 预览那句要留引用：它是 chips 的兄弟节点，用 querySelector 从 chips 往上找
  // 是找不到的（一开始就是这么写的，结果是勾选后预览一直停在初始清单）
  const preview = h("div.tip", { text: formatHintPreview(draft) });

  const paint = (): void => {
    const nodes = formatOptions.map((format) => {
      const on = draft.formats.includes(format.ext);
      const chip = h("button.chip", {
        type: "button",
        class: on ? "on" : "",
        text: format.label,
        "aria-pressed": String(on),
      });
      chip.addEventListener("click", () => void toggleFormat(format.ext, format.label));
      return chip;
    });
    mount(chips, ...nodes);
    preview.textContent = formatHintPreview(draft);
  };

  // 勾选即提交，连点会打出并发请求：前一个没回来之前先不受理新的点击
  let pending = false;

  const toggleFormat = async (format: string, label?: string): Promise<void> => {
    if (pending) return;
    pending = true;
    const previous = draft.formats;
    const next = previous.includes(format)
      ? previous.filter((item) => item !== format)
      : [...previous, format];
    if (next.length === 0) {
      toastError("至少要留一种格式，否则谁都没法上传");
      return;
    }
    // 白名单改完即生效：单独一个接口，不跟「保存设置」混在一起
    draft.formats = next;
    paint();
    try {
      await request("/admin/settings/formats", { method: "POST", json: { formats: next } });
      toastOk(`${label ?? format.toUpperCase()} 格式白名单已更新`);
      mount(result);
      // 上传页的「支持 …」读的是生效策略，改完立刻让它跟着变（不用刷新页面）
      await refreshIdentity();
    } catch (err) {
      // 失败就把界面上的选中态回滚，别让界面和服务端显示得不一样
      draft.formats = previous;
      paint();
      const message = err instanceof ApiError ? err.message : String(err);
      mount(result, qtip("err", "格式白名单没改成功", message));
    } finally {
      pending = false;
    }
  };

  paint();

  return cardNode(
    cardHead("上传策略"),
    h(
      "div.row",
      null,
      field("单文件大小上限", draft.maxFileBytes, "可以写 20 MB、2 GB"),
      field(
        "单批次最大张数",
        draft.maxBatchSize,
        "一次选择最多收多少张；上传页按它截断，服务端也在预检那一步卡条数。0 = 不限",
      ),
    ),
    h(
      "div.row",
      null,
      field("像素上限", unitInput(draft.maxPixels, "万"), "超出的图片直接拒绝，防止解压炸弹"),
      field("单用户并发上传数", rateDraft.maxConcurrent, "上传页同时传几张；与用户额度里的同一项"),
    ),
    h(
      "div.row",
      null,
      field("最大宽度", draft.maxWidth, "像素；0 表示不限"),
      field("最大高度", draft.maxHeight, "像素；0 表示不限"),
    ),
    field(
      "队列深度阈值",
      draft.queueDepth,
      "队列与在途任务的上限；超过回 429，客户端队列也按它限长。0 = 不限",
    ),
    h("div.tip", {
      text: `当前队列深度 ${formatNumber(rate.queue_depth.current)}，阈值 ${formatNumber(
        rate.queue_depth.threshold,
      )}。`,
    }),
    h("div.fld", null, h("label.f", { text: "支持的图片格式" }), chips),
    preview,
    result,
    h("div.tip", {
      text:
        "勾选即生效，不用保存也不用重启。JPG 即 JPEG。上传页那句「支持 …」按这份清单生成，改完立刻跟着变。",
    }),
  );
}

/** 「上传页会显示」的那句话，与面板里的 supportedHint 同一套写法。 */
function formatHintPreview(draft: SettingsDraft): string {
  const list = draft.formats.map((ext) => (ext === "jpg" ? "JPG" : ext.toUpperCase()));
  if (list.length === 0) return "上传页会显示：单张最大 " + draft.maxFileBytes.value;
  return `上传页会显示：支持 ${list.join(" · ")}；单张最大 ${draft.maxFileBytes.value}`;
}

/**
 * 会话有效期。
 *
 * 三个口径：闲置多久掉线（云控制台那种超时重登）、普通会话最长活多久、
 * 「记住我」活多久。填 0 表示那一项不限——自建服务里确实有人不想要超时。
 */
function sessionCard(draft: SettingsDraft): HTMLElement {
  return cardNode(
    cardHead("会话与登录"),
    h(
      "div.row",
      null,
      field(
        "闲置超时",
        unitInput(draft.sessionIdle, "分钟"),
        "多久没操作就要求重新登录；0 表示不按闲置算",
      ),
      field(
        "会话最长有效期",
        unitInput(draft.sessionAbsolute, "天"),
        "普通会话从建立起最长活这么久；0 表示不限",
      ),
    ),
    h(
      "div.row",
      null,
      field(
        "「记住我」有效期",
        unitInput(draft.sessionRemember, "天"),
        "登录时勾了「记住我」的会话活这么久，期间不看闲置时间",
      ),
    ),
    h("div.tip", {
      text: "超时后再访问会被送回登录页；「活跃会话」列表里也不会再出现已经掉线的那几台设备。",
    }),
  );
}

/** 防盗链：直链只允许来自白名单的来源站。 */
function securityCard(draft: SettingsDraft): HTMLElement {
  return cardNode(
    cardHead("防盗链"),
    switchRow(
      "启用防盗链",
      "直链只允许来自本站与白名单来源；别人站里引用你的图会拿到 403",
      draft.hotlinkEnabled,
      (next) => {
        draft.hotlinkEnabled = next;
      },
    ),
    switchRow(
      "允许空来源",
      "直接在地址栏打开、PicGo 这类客户端不带 Referer；关掉会连它们一起拒",
      draft.hotlinkAllowEmpty,
      (next) => {
        draft.hotlinkAllowEmpty = next;
      },
    ),
    field(
      "允许的来源",
      draft.hotlinkDomains,
      "逗号分隔，例如 blog.example.com, cdn.example.com；子域自动算命中。留空只放本站",
    ),
    h("div.tip", {
      text: "只影响直链与缩略图；站内页面的图片不受影响。开启前先确认没有第三方站正在引用你的图。",
    }),
  );
}

/** 内容审核：阿里云 / 腾讯云 / 百度 / 自建 NSFWJS。 */
function moderationCard(draft: SettingsDraft): HTMLElement {
  const providerTips = h("div.tip", { text: "" });
  const providerFields = h("div");

  /** 按服务商重建凭据字段：名字、说明都跟着换。 */
  const paintProviderFields = (): void => {
    const provider = draft.moderationProvider.value;
    const rows: Node[] = [];

    if (provider === "nsfwjs") {
      providerTips.textContent =
        "自建服务：向下面的接口 POST 一段 JSON，体为 { image: base64, mime }。" +
        "响应认 {nsfw: 0.9} 这类分数，也认 nsfwjs 原样的 predictions 数组。";
      rows.push(
        field("接口地址", draft.moderationEndpoint, "例如 https://nsfw.example.com/check"),
        h(
          "div.row",
          null,
          field("分数字段名", draft.moderationScoreField, "响应里哪个键是分数；默认 nsfw"),
          field("Bearer Token", draft.moderationToken, "需要鉴权时填；加密存储，留空表示不修改"),
        ),
      );
    } else if (provider === "baidu") {
      providerTips.textContent =
        "百度图像审核：在百度智能云创建应用后拿到 API Key 与 Secret Key 两项，填在下面。";
      rows.push(
        h(
          "div.row",
          null,
          field("API Key", draft.moderationAccessKey, "加密存储，留空表示不修改"),
          field("Secret Key", draft.moderationSecretKey, "加密存储，留空表示不修改"),
        ),
      );
    } else if (provider === "tencent") {
      providerTips.textContent =
        "腾讯云图片内容安全：填 SecretId 与 SecretKey；Region 留空默认 ap-guangzhou。";
      rows.push(
        h(
          "div.row",
          null,
          field("SecretId", draft.moderationAccessKey, "加密存储，留空表示不修改"),
          field("SecretKey", draft.moderationSecretKey, "加密存储，留空表示不修改"),
        ),
        h(
          "div.row",
          null,
          field("Region", draft.moderationRegion, "留空取 ap-guangzhou"),
          field("BizType", draft.moderationAppId, "控制台里配的策略组标识；留空用默认策略"),
        ),
      );
    } else {
      providerTips.textContent =
        "阿里云内容安全：填 AccessKeyId 与 AccessKeySecret；Region 留空默认 cn-shanghai。";
      rows.push(
        h(
          "div.row",
          null,
          field("AccessKeyId", draft.moderationAccessKey, "加密存储，留空表示不修改"),
          field("AccessKeySecret", draft.moderationSecretKey, "加密存储，留空表示不修改"),
        ),
        field("Region", draft.moderationRegion, "留空取 cn-shanghai"),
      );
    }

    // 已经配过的那几项在界面上永远是空的（不回显明文），给一句状态提示
    const configured: string[] = [];
    if (draft.moderationAccessKey.placeholder.includes("已配置")) configured.push("AccessKey / SecretId / API Key");
    if (draft.moderationSecretKey.placeholder.includes("已配置")) configured.push("Secret");
    if (draft.moderationToken.placeholder.includes("已配置")) configured.push("Token");
    if (configured.length > 0) {
      rows.push(h("div.tip", { text: `已配置：${configured.join("、")}。留空提交表示不改动。` }));
    }

    mount(providerFields, ...rows);
  };

  paintProviderFields();
  draft.moderationProvider.addEventListener("change", paintProviderFields);

  return cardNode(
    cardHead("内容审核", badge("上传前拦截", "info")),
    switchRow(
      "启用内容审核",
      "上传的图片先送审核服务，达到阈值直接拒绝并记一次违规",
      draft.moderationEnabled,
      (next) => {
        draft.moderationEnabled = next;
      },
    ),
    field("服务商", draft.moderationProvider),
    providerTips,
    // 各家的凭据项不一样，只摆当前这家要填的那几个：
    // 满屏都是别的服务商的输入框，谁也说不清该填哪个
    providerFields,
    h(
      "div.row",
      null,
      field("违规阈值", unitInput(draft.moderationThreshold, "分"), "0-100；达到或超过就拦。各家云厂商的「疑似」档在 70 附近"),
      field("累计几次自动暂停", unitInput(draft.moderationBanAfter, "次"), "24 小时内累计；0 表示不自动暂停"),
    ),
    h(
      "div.row",
      null,
      field("暂停时长", unitInput(draft.moderationBanMinutes, "分钟"), "到期自动恢复；默认 1440 分钟（一天）"),
    ),
    switchRow("普通用户过审", "不勾则普通用户的上传不送审", draft.moderationUserEnabled, (next) => {
      draft.moderationUserEnabled = next;
    }),
    switchRow("管理员也要过审", "默认不过：管理员改配置时不该被自己的图挡住", draft.moderationAdminEnabled, (next) => {
      draft.moderationAdminEnabled = next;
    }),
    switchRow("违规时邮件通知管理员", "第一例与触发自动暂停时各发一封，避免每张都发把通知刷成噪音", draft.moderationNotify, (next) => {
      draft.moderationNotify = next;
    }),
    switchRow(
      "审核服务不可用时放行",
      "推荐开启：第三方接口抖动不该让整站传不了图；关掉则宁可拒绝也不漏检",
      draft.moderationFailOpen,
      (next) => {
        draft.moderationFailOpen = next;
      },
    ),
    h("div.tip", {
      text: "密钥加密存储，界面只显示「已配置」。拦截与自动暂停都写审计日志（moderation_block / moderation_ban）。",
    }),
  );
}

/** 缩略图策略：客户端生成 + 服务端兜底。 */
function thumbCard(draft: SettingsDraft): HTMLElement {
  // 「把老缩略图按新尺寸重算一遍」在「存储管理」那一页（带进度），这里只管开关
  const state = h("div", { style: { marginTop: "10px" } });
  const refresh = (): void => {
    mount(
      state,
      draft.thumbFallback
        ? qtip(
            "warn",
            "服务端兜底已开启",
            "只有 API 上传这类没有缩略图的图片会在后台异步补生成。这会占用 CPU 与内存，请确认服务器扛得住再开。",
          )
        : qtip(
            "ok",
            null,
            "当前服务端不承担任何图像缩放，CPU 与内存压力接近零。缺缩略图的图片会回落到原图显示。",
          ),
    );
  };
  refresh();

  return cardNode(
    cardHead("缩略图策略"),
    switchRow(
      "客户端生成缩略图",
      "浏览器用 OffscreenCanvas 生成，随原图一起上传",
      draft.thumbClient,
      (next) => {
        draft.thumbClient = next;
      },
    ),
    switchRow(
      "服务端兜底生成",
      "仅对 API 上传等没有缩略图的场景，异步补生成",
      draft.thumbFallback,
      (next) => {
        draft.thumbFallback = next;
        refresh();
      },
    ),
    state,
  );
}

/** 邮件与注册验证：SMTP + 反临时邮箱三项 + 测试发信。 */
function mailCard(draft: SettingsDraft): HTMLElement {
  const result = h("div", { style: { marginTop: "8px" } });
  const testBtn = h("button.btn.sm", { type: "button", text: "测试发信" });
  const templateBtn = h("button.btn.sm", {
    type: "button",
    text: "管理邮件模板",
    on: { click: () => openMailTemplates() },
  });

  testBtn.addEventListener("click", async () => {
    const fallback = draft.fromEmail.value.trim() || currentUser()?.email || "";
    const to = await promptDialog({
      title: "测试发信",
      label: "收件地址",
      value: fallback,
      placeholder: "admin@example.com",
      confirmText: "发送",
      hint: "用的是已保存的 SMTP 配置：刚改过服务器或密码的话，先点「保存设置」再测。",
    });
    if (!to) return;
    testBtn.disabled = true;
    testBtn.textContent = "发送中…";
    try {
      const payload = await request<{ ok: boolean; message: string }>("/admin/settings/mail/test", {
        json: { to },
      });
      // 服务端返回的那句话原样展示
      mount(result, payload.ok ? qtip("ok", "发送成功", payload.message) : qtip("err", "发送失败", payload.message));
    } catch (err) {
      const message = err instanceof ApiError ? err.message : String(err);
      mount(result, qtip("err", "测试发信失败", message));
    } finally {
      testBtn.disabled = false;
      testBtn.textContent = "测试发信";
    }
  });

  return cardNode(
    cardHead("邮件与注册验证"),
    h("div.row", null, field("SMTP 服务器", draft.smtpHost), field("端口", draft.smtpPort)),
    h("div.row", null, field("安全方式", draft.smtpSecurity), field("发信账号", draft.smtpUser)),
    h("div.row", null, field("密码", draft.smtpPassword, "加密存储，留空表示不修改"), field("发信人名称", draft.fromName)),
    h("div.row", null, field("发信邮箱", draft.fromEmail), field("申诉联系邮箱", draft.appealEmail, "账号暂停通知里让用户联系的地址")),
    switchRow("反临时邮箱 · 内置域名清单", "随镜像打包，可在线更新", draft.disposableCheck, (next) => {
      draft.disposableCheck = next;
    }),
    switchRow(
      "反临时邮箱 · 收件域名校验",
      "查 MX 记录（没有 MX 回落到 A/AAAA）：注册、改邮箱、找回密码发信前都过一遍，" +
        "收不到的域名直接拒——退信最后都堆在发信邮箱里。内网邮件服务解析不出外网记录时关掉它",
      draft.mxCheck,
      (next) => {
        draft.mxCheck = next;
      },
    ),
    switchRow("反临时邮箱 · 第三方校验 API", "需在服务端配置密钥，命中即拒", draft.thirdPartyCheck, (next) => {
      draft.thirdPartyCheck = next;
    }),
    h("div.flexline", { style: { marginTop: "10px" } }, testBtn, templateBtn),
    result,
    h("div.tip", {
      text: "只有改动过才会提交，没配邮件也不影响其他设置。要让验证邮件真的发得出去，下面四项都要填对。",
    }),
  );
}

/**
 * 默认头像：没设过头像的账号（含新注册的）都用它。
 *
 * 与背景图同一条路：可以填地址，也可以直接上传（存进库、带版本号）。
 * 留空就是内置那张轮廓头像——所以这一栏空着也是「有效设置」，不是没配置。
 * 这一栏自带保存按钮：换了头像要立刻看到效果，不必回页面底部点「保存设置」。
 */
function defaultAvatarCard(reload: () => void, draft: SettingsDraft, data: AdminSettings): HTMLElement {
  const isUploaded = (data.appearance.default_avatar ?? "").startsWith("/api/v1/site/avatar");
  const preview = h("div.avatar-preview");
  const sync = (): void => {
    const url = absUrl(draft.defaultAvatar.value.trim()) || BUILTIN_AVATAR;
    mount(preview, h("img", { src: url, alt: "当前默认头像" }));
  };
  sync();
  draft.defaultAvatar.addEventListener("input", sync);

  const fileInput = h("input", {
    type: "file",
    accept: "image/*",
    style: { display: "none" },
    "aria-hidden": "true",
  });
  const uploadBtn = h("button.btn.sm.pri", { type: "button", text: "上传默认头像" });
  uploadBtn.addEventListener("click", () => fileInput.click());
  fileInput.addEventListener("change", () => {
    const file = fileInput.files?.[0];
    if (file) void uploadDefaultAvatar(file, uploadBtn, reload);
  });

  const saveBtn = h("button.btn.sm", { type: "button", text: "保存地址" });
  saveBtn.addEventListener("click", async () => {
    const value = draft.defaultAvatar.value.trim();
    saveBtn.disabled = true;
    saveBtn.textContent = "保存中…";
    try {
      await request("/admin/settings", {
        method: "PUT",
        json: { appearance: { default_avatar: value || null } },
      });
      toastOk(value ? "默认头像地址已生效" : "已清空，改用内置那张轮廓头像");
      await reloadSiteIdentity();
      reload();
    } catch (err) {
      toastError(err);
    } finally {
      saveBtn.disabled = false;
      saveBtn.textContent = "保存地址";
    }
  });

  const removeBtn = h("button.btn.sm.dgr", { type: "button", text: "移除默认头像" });
  removeBtn.disabled = !(data.appearance.default_avatar ?? "").trim();
  removeBtn.addEventListener("click", async () => {
    const ok = await confirmDialog({
      title: "移除默认头像",
      message: "清掉之后，没设过头像的账号显示内置那张轮廓头像；账号自己设过的头像不受影响。",
      confirmText: "移除",
      danger: true,
    });
    if (!ok) return;
    try {
      await request("/admin/settings", { method: "PUT", json: { appearance: { default_avatar: null } } });
      toastOk("默认头像已移除");
      await reloadSiteIdentity();
      reload();
    } catch (err) {
      toastError(err);
    }
  });

  return cardNode(
    cardHead("默认头像"),
    h(
      "div.row",
      { style: { alignItems: "flex-end" } },
      field(
        "默认头像地址",
        draft.defaultAvatar,
        "没设过头像的账号（含新注册的）都用它；留空就是内置那张轮廓头像。也可以直接上传一张。",
      ),
      h("div", { style: { flex: "0 0 76px", paddingBottom: "13px" } }, preview),
      h(
        "div",
        {
          style: {
            flex: "0 0 auto",
            paddingBottom: "13px",
            display: "flex",
            gap: "8px",
            // 窄屏三个按钮一行放不下：换行，别顶出卡片
            flexWrap: "wrap",
          },
        },
        saveBtn,
        uploadBtn,
        removeBtn,
      ),
    ),
    fileInput,
    h("div.tip", {
      text:
        (isUploaded ? "当前用的是上传进数据库的那张，也可以在这里换成外链地址。" : "") +
        "上传的图先在本机缩到 512px 再存进数据库，圆角与裁切由前端完成。",
    }),
  );
}

/** 外观：背景图（可以填地址，也可以直接上传一张）。 */
function appearanceCard(
  reload: () => void,
  draft: SettingsDraft,
  data: AdminSettings,
): HTMLElement {
  const preview = h("div.ph", { style: { flex: "0 0 200px", height: "64px" } });
  const sync = (): void => {
    const url = draft.background.value.trim();
    if (!url) {
      mount(preview, "未设置背景图");
      return;
    }
    if (!/^https?:|^\//.test(url)) {
      mount(preview, "地址看起来不对");
      return;
    }
    mount(preview, h("img", { src: absUrl(url), alt: "当前背景图" }));
  };
  sync();
  draft.background.addEventListener("input", sync);

  // 上传：先在本机把图缩到 1920px 再传，几百 KB 就够铺满任何屏幕，
  // 也不会把几十兆的原图塞进数据库
  const fileInput = h("input", {
    type: "file",
    accept: "image/*",
    style: { display: "none" },
    "aria-hidden": "true",
  });
  const uploadBtn = h("button.btn.sm.pri", { type: "button", text: "上传背景图" });
  uploadBtn.addEventListener("click", () => fileInput.click());
  fileInput.addEventListener("change", () => {
    const file = fileInput.files?.[0];
    if (!file) return;
    void uploadBackground(file, uploadBtn, reload);
  });

  const isUploaded = data.appearance.background?.startsWith("/api/v1/site/background") ?? false;


  const removeBtn = h("button.btn.sm.dgr", { type: "button", text: "移除背景图" });
  removeBtn.addEventListener("click", async () => {
    const ok = await confirmDialog({
      title: "移除背景图",
      message: "站点会回到没有背景图的样子，已经上传的图片文件不会被删除。",
      confirmText: "移除",
      danger: true,
    });
    if (!ok) return;
    try {
      await request("/admin/settings", { method: "PUT", json: { appearance: { background: null } } });
      toastOk("背景图已移除");
      reload();
    } catch (err) {
      toastError(err);
    }
  });

  return cardNode(
    cardHead("外观"),
    h(
      "div.row",
      { style: { alignItems: "flex-end" } },
      field(
        "背景图地址",
        draft.background,
        isUploaded
          ? "当前用的是上传进数据库的那张。也可以在这里换成外链地址。"
          : "填一个可公开访问的图片地址（本站直链也可以），或直接上传一张。",
      ),
      h("div", { style: { flex: "0 0 200px" } }, preview),
      h(
        "div",
        {
          style: {
            flex: "0 0 auto",
            paddingBottom: "13px",
            display: "flex",
            gap: "8px",
            // 同上：窄屏换行，别顶出卡片
            flexWrap: "wrap",
          },
        },
        uploadBtn,
        removeBtn,
      ),
    ),
    fileInput,
    h("div.tip", {
      text: "上传的图先在本机压到 1920px 再存进数据库，不落文件系统，上传完立刻生效。换了地址记得点「保存设置」。",
    }),
  );
}

/** 默认头像：同样先在本机缩小再上传，头像用不着大图。 */
async function uploadDefaultAvatar(
  file: File,
  button: HTMLButtonElement,
  reload: () => void,
): Promise<void> {
  if (file.size > 20 * 1024 * 1024) {
    toastError("头像图片不要超过 20 MiB，先压一下再传");
    return;
  }
  button.disabled = true;
  button.textContent = "处理中…";
  try {
    // 512px 足够任何显示尺寸；圆角与裁切交给 CSS 的 object-fit: cover
    const blob = await downscaleImage(file, 512);
    const form = new FormData();
    form.append("file", blob, "default-avatar.jpg");
    const payload = await request<{ ok: boolean; url: string; bytes: number }>(
      "/admin/settings/avatar",
      { method: "POST", form },
    );
    toastOk(`默认头像已上传（${formatBytes(payload.bytes)}）并生效`);
    await reloadSiteIdentity();
    reload();
  } catch (err) {
    toastError(err);
  } finally {
    button.disabled = false;
    button.textContent = "上传默认头像";
  }
}

/** 选好的文件 → 压到 1920px → 上传进库。 */
async function uploadBackground(
  file: File,
  button: HTMLButtonElement,
  reload: () => void,
): Promise<void> {
  if (file.size > 20 * 1024 * 1024) {
    toastError("背景图不要超过 20 MiB；铺满屏幕用不了太大的图，先压一下再传");
    return;
  }
  button.disabled = true;
  button.textContent = "处理中…";
  try {
    const blob = await downscaleImage(file, 1920);
    const form = new FormData();
    // 服务端认 file / image / background 三个字段名，这里用最直白的那个
    form.append("file", blob, "background.jpg");
    const payload = await request<{ ok: boolean; url: string; bytes: number }>(
      "/admin/settings/background",
      { method: "POST", form },
    );
    toastOk(`背景图已上传（${formatBytes(payload.bytes)}）并生效`);
    // 站名与背景图都来自 /site：重拉一次让品牌文字与背景立刻更新
    await reloadSiteIdentity();
    reload();
  } catch (err) {
    toastError(err);
  } finally {
    button.disabled = false;
    button.textContent = "上传背景图";
  }
}

/**
 * 本机缩放。
 *
 * 用 canvas 重画一遍再导出 JPEG：原图动辄几千像素宽、好几 MB，
 * 而背景图铺满屏幕用不了那么多像素，压完通常只有几百 KB——
 * 既省数据库空间，也省每次打开页面时的流量。
 * 没有 createImageBitmap 的环境（老 Safari）退回 <img> 加载。
 */
async function downscaleImage(file: File, maxWidth: number): Promise<Blob> {
  const bitmap: ImageBitmap | HTMLImageElement =
    typeof createImageBitmap === "function"
      ? await createImageBitmap(file)
      : await new Promise<HTMLImageElement>((resolve, reject) => {
          const image = new Image();
          const url = URL.createObjectURL(file);
          image.onload = () => resolve(image);
          image.onerror = () => reject(new Error("这张图读不出来，换一张试试"));
          image.src = url;
        });

  const width = bitmap instanceof HTMLImageElement ? bitmap.naturalWidth : bitmap.width;
  const height = bitmap instanceof HTMLImageElement ? bitmap.naturalHeight : bitmap.height;
  if (width <= 0 || height <= 0) throw new Error("这张图读不出来，换一张试试");

  const scale = Math.min(1, maxWidth / width);
  const canvas = document.createElement("canvas");
  canvas.width = Math.max(1, Math.round(width * scale));
  canvas.height = Math.max(1, Math.round(height * scale));
  const context = canvas.getContext("2d");
  if (!context) throw new Error("浏览器不支持本地压缩，请改用外链地址");
  context.drawImage(bitmap, 0, 0, canvas.width, canvas.height);
  if (bitmap instanceof ImageBitmap) bitmap.close();

  const blob = await new Promise<Blob | null>((resolve) =>
    canvas.toBlob(resolve, "image/jpeg", 0.85),
  );
  if (!blob) throw new Error("压缩失败，请改用外链地址");
  return blob;
}

/** 重新拉站点信息并刷新顶栏：站名与背景图改完要立刻看得到。 */
async function reloadSiteIdentity(): Promise<void> {
  await loadSite();
  refreshNav();
}

/**
 * 站点信息与「我的生效策略」一起重拉。
 *
 * 上传页那句「支持 JPG · PNG …；单张最大 …」读的是生效的上传策略
 * （`/me` 里那组 limits），白名单与单文件上限改完之后不重拉 `me`，
 * 那句话就还停在旧值——用户会以为改配置没生效。
 */
async function refreshIdentity(): Promise<void> {
  await reloadSiteIdentity();
  try {
    await loadMe();
  } catch {
    // 未登录或会话失效：站点信息已经刷新过，这里不值得报错
  }
}


/// 邮件那一栏动过没有。邮箱/密码这类字段为空是常态（还没配邮件），
/// 不能拿「填没填」当「改没改」——只有和加载时的值不一致才算动过。
function mailDirty(draft: SettingsDraft, loaded: AdminSettings): boolean {
  const mail = loaded.mail;
  return (
    draft.smtpHost.value.trim() !== mail.smtp_host ||
    draft.smtpPort.value.trim() !== String(mail.smtp_port) ||
    draft.smtpSecurity.value !== (mail.smtp_security || "ssl") ||
    draft.smtpUser.value.trim() !== mail.smtp_user ||
    draft.fromName.value.trim() !== mail.from_name ||
    draft.fromEmail.value.trim() !== mail.from_email ||
    draft.appealEmail.value.trim() !== mail.appeal_email ||
    draft.disposableCheck !== mail.disposable_check ||
    draft.mxCheck !== mail.mx_check ||
    draft.thirdPartyCheck !== mail.third_party_check ||
    // 密码框永远是空的；有内容就说明要改密码
    draft.smtpPassword.value.trim() !== ""
  );
}

/** 保存设置：只提交页面上这几个子对象；格式白名单走自己的接口，不在这里提交。 */
async function save(
  reload: () => void,
  draft: SettingsDraft,
  loaded: AdminSettings,
  rateDraft: RateLimitDraft,
  button: HTMLButtonElement,
): Promise<void> {
  const maxFile = sizeOf(draft.maxFileBytes, "MB");
  if (maxFile <= 0) {
    toastError("单文件大小上限填不对：可以写 20 MB、2 GB");
    return;
  }
  const pixels = intOf(draft.maxPixels) * 10000;
  if (pixels <= 0) {
    toastError("像素上限填不对：单位是万，填 8000 表示 8000 万像素");
    return;
  }
  const rateIssue = validateRateLimit(rateDraft);
  if (rateIssue) {
    toastError(rateIssue);
    return;
  }

  // 邮件段单独判断：只有真的动过才提交。
  // 以前是无条件校验「SMTP 服务器与发信邮箱都要填」，结果是邮件没配好的站
  // 连广场列数、上传上限这类无关设置都保存不了——报错还指向邮件字段，莫名其妙。
  const mailChanged = mailDirty(draft, loaded);
  if (mailChanged && (!draft.fromEmail.value.trim() || !draft.smtpHost.value.trim())) {
    toastError("要启用邮件功能，SMTP 服务器与发信邮箱都要填。只想改其他设置的话，别动邮件这一栏。");
    return;
  }

  const body: {
    site: Record<string, unknown>;
    upload: Record<string, unknown>;
    session: Record<string, unknown>;
    security: Record<string, unknown>;
    moderation: Record<string, unknown>;
    thumb: Record<string, unknown>;
    appearance: Record<string, unknown>;
    mail?: Record<string, unknown>;
  } = {
    site: {
      name: draft.siteName.value.trim(),
      base_url: draft.baseUrl.value.trim().replace(/\/+$/, ""),
      plaza_public: draft.plazaPublic,
      plaza_grid_columns: Number(draft.plazaColumns.value) || 3,
      recycle_retention_days: Math.min(365, Math.max(0, intOf(draft.recycleRetention))),
      require_login_to_upload: draft.requireLogin,
      register_enabled: draft.registerEnabled,
      email_domains: draft.emailDomains.value.trim(),
    },
    upload: {
      max_file_bytes: maxFile,
      max_batch_size: intOf(draft.maxBatchSize),
      max_pixels: pixels,
      max_width: intOf(draft.maxWidth),
      max_height: intOf(draft.maxHeight),
      queue_depth: intOf(draft.queueDepth),
    },
    session: {
      idle_minutes: Math.min(525600, intOf(draft.sessionIdle)),
      absolute_days: Math.min(365, intOf(draft.sessionAbsolute)),
      remember_days: Math.min(365, Math.max(1, intOf(draft.sessionRemember))),
    },
    security: {
      hotlink_enabled: draft.hotlinkEnabled,
      hotlink_allow_empty: draft.hotlinkAllowEmpty,
      hotlink_domains: draft.hotlinkDomains.value.trim(),
    },
    moderation: {
      enabled: draft.moderationEnabled,
      provider: draft.moderationProvider.value,
      threshold: Math.min(100, Math.max(1, intOf(draft.moderationThreshold))),
      score_field: draft.moderationScoreField.value.trim() || "nsfw",
      endpoint: draft.moderationEndpoint.value.trim(),
      region: draft.moderationRegion.value.trim(),
      app_id: draft.moderationAppId.value.trim(),
      user_enabled: draft.moderationUserEnabled,
      admin_enabled: draft.moderationAdminEnabled,
      ban_after: intOf(draft.moderationBanAfter),
      ban_minutes: intOf(draft.moderationBanMinutes),
      notify_admin: draft.moderationNotify,
      fail_open: draft.moderationFailOpen,
    },
    thumb: { client: draft.thumbClient, server_fallback: draft.thumbFallback },
    appearance: {
      background: draft.background.value.trim() || null,
      default_avatar: draft.defaultAvatar.value.trim() || null,
    },
  };

  // 密钥：留空＝不修改，所以只在填了的时候带上（服务端同样按「给了才写」处理）
  const moderationSecrets: Record<string, string> = {};
  if (draft.moderationAccessKey.value.trim()) {
    moderationSecrets.access_key = draft.moderationAccessKey.value.trim();
  }
  if (draft.moderationSecretKey.value.trim()) {
    moderationSecrets.secret_key = draft.moderationSecretKey.value.trim();
  }
  if (draft.moderationToken.value.trim()) {
    moderationSecrets.token = draft.moderationToken.value.trim();
  }
  if (Object.keys(moderationSecrets).length > 0) {
    Object.assign(body.moderation, moderationSecrets);
  }
  if (mailChanged) {
    body.mail = {
      smtp_host: draft.smtpHost.value.trim(),
      smtp_port: intOf(draft.smtpPort),
      smtp_security: draft.smtpSecurity.value,
      smtp_user: draft.smtpUser.value.trim(),
      from_name: draft.fromName.value.trim(),
      from_email: draft.fromEmail.value.trim(),
      appeal_email: draft.appealEmail.value.trim(),
      disposable_check: draft.disposableCheck,
      mx_check: draft.mxCheck,
      third_party_check: draft.thirdPartyCheck,
    };
    // 密码留空＝不修改：带上空字符串会把已存的密文覆盖掉
    const password = draft.smtpPassword.value.trim();
    if (password) body.mail.password = password;
  }

  button.disabled = true;
  button.textContent = "保存中…";
  try {
    await request("/admin/settings", { method: "PUT", json: body });
    // 频率限制那几张卡片走另外两个接口，跟这一次保存一起提交
    await saveRateLimit(rateDraft);
    toastOk("设置已保存");
    // 站名、背景图在 /site 里，单文件上限与格式在 /me 的生效策略里：
    // 两边都重拉一次，页面上的标题、背景与上传提示当场跟着变
    await refreshIdentity();
    reload();
  } catch (err) {
    toastError(err);
    button.disabled = false;
    button.textContent = "保存设置";
  }
}
