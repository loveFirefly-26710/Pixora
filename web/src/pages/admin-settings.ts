/**
 * 系统设置（`#/admin/settings`）。
 *
 * 站点、上传策略与格式白名单、缩略图策略、邮件与注册验证、外观。
 * 路径类配置已经移出这里（改由环境变量注入），别再往回加。
 *
 * 两条保存路径要分清：
 *   - 格式白名单改动走 `POST /admin/settings/formats`，勾选即生效，不参与「保存设置」；
 *   - 其余都走 `PUT /admin/settings`，只提交页面上真的改过的子对象。
 */

import { ApiError, request } from "../core/api";
import { h, mount } from "../core/dom";
import { formatBytes, parseSize } from "../core/format";
import { confirmDialog, promptDialog } from "../core/modal";
import type { PageContext } from "../core/router";
import { toastError, toastOk } from "../core/toast";
import { currentUser } from "../core/state";
import type { AdminSettings } from "../core/types";
import {
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

/** 白名单里可选的格式，与后端的解码器一一对应。 */
const ALL_FORMATS = ["jpg", "png", "gif", "webp", "avif", "heic", "tiff", "bmp", "svg", "jxl", "ico"];

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
  requireLogin: boolean;
  registerEnabled: boolean;
  maxFileBytes: HTMLInputElement;
  maxBatchSize: HTMLInputElement;
  maxPixels: HTMLInputElement;
  queueDepth: HTMLInputElement;
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
    const data = await request<AdminSettings>("/admin/settings");
    mount(root, ...view(reload, data));
  } catch (err) {
    mount(root, errorBlock(err, reload));
  }
}

function view(reload: () => void, data: AdminSettings): Node[] {
  const draft: SettingsDraft = {
    siteName: textInput({ value: data.site.name }),
    baseUrl: textInput({ value: data.site.base_url, mono: true }),
    plazaPublic: data.site.plaza_public,
    plazaColumns: select(GRID_COLUMN_OPTIONS, String(data.site.plaza_grid_columns ?? 3)),
    requireLogin: data.site.require_login_to_upload,
    registerEnabled: data.site.register_enabled,
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
    formats: [...(data.upload.formats ?? [])],
  };

  return [
    pageHead(
      "系统设置",
      "站点、上传策略、格式白名单、缩略图策略、邮件与反临时邮箱。路径类配置已移出，改由环境变量注入。",
    ),
    h("div.row", { style: { marginBottom: "14px" } }, siteCard(draft), uploadCard(draft)),
    h("div.row", { style: { marginBottom: "14px" } }, thumbCard(draft), mailCard(draft)),
    appearanceCard(reload, draft),
    h(
      "div.flexline",
      { style: { justifyContent: "flex-end", marginTop: "16px" } },
      h("button.btn", {
        type: "button",
        text: "重新生成缩略图",
        on: { click: (event) => void rebuildThumbs(event.target as HTMLButtonElement) },
      }),
  h("button.btn.pri", {
        type: "button",
        text: "保存设置",
        on: { click: (event) => void save(reload, draft, data, event.target as HTMLButtonElement) },
      }),
    ),
    note(
      "w",
      "上传配额、注册默认配额、容量阈值不在这里——它们在「配额与容量」页统一算账。这里只管「允不允许、多大、什么格式」。",
    ),
  ];
}

/** 站点：基础 URL、名称与三个开关。 */
function siteCard(draft: SettingsDraft): HTMLElement {
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
    switchRow("拒绝游客上传", "上传必须登录", draft.requireLogin, (next) => {
      draft.requireLogin = next;
    }),
    switchRow("允许用户注册", "关闭后只能由管理员创建账号", draft.registerEnabled, (next) => {
      draft.registerEnabled = next;
    }),
    h("div.tip", { text: "注册相关的默认配额和容量阈值在「配额与容量」里配。" }),
  );
}

/** 上传策略 + 格式白名单 chips（改动即时提交）。 */
function uploadCard(draft: SettingsDraft): HTMLElement {
  const chips = h("div.flexline");
  const result = h("div", { style: { marginTop: "8px" } });

  const paint = (): void => {
    const nodes = ALL_FORMATS.map((format) => {
      const on = draft.formats.includes(format);
      const chip = h("button.chip", {
        type: "button",
        class: on ? "on" : "",
        text: format.toUpperCase(),
        "aria-pressed": String(on),
      });
      chip.addEventListener("click", () => void toggleFormat(format));
      return chip;
    });
    mount(chips, ...nodes);
  };

  // 勾选即提交，连点会打出并发请求：前一个没回来之前先不受理新的点击
  let pending = false;

  const toggleFormat = async (format: string): Promise<void> => {
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
      toastOk(`${format.toUpperCase()} 格式白名单已更新`);
      mount(result);
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
      field("单批最大张数", draft.maxBatchSize),
    ),
    h(
      "div.row",
      null,
      field("像素上限", unitInput(draft.maxPixels, "万"), "超出的图片直接拒绝，防止解压炸弹"),
      field("队列深度阈值", draft.queueDepth, "超出返回 429"),
    ),
    h("div.fld", null, h("label.f", { text: "支持的图片格式" }), chips),
    result,
    h("div.tip", {
      text: "SVG 入库前会剥离脚本与外部引用，响应加沙箱头。勾选即生效，无需重启，也不用点保存。",
    }),
  );
}

/** 缩略图策略：客户端生成 + 服务端兜底。 */
function thumbCard(draft: SettingsDraft): HTMLElement {
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
    switchRow("反临时邮箱 · MX 记录校验", "域名解析不出邮件服务器则拒绝", draft.mxCheck, (next) => {
      draft.mxCheck = next;
    }),
    switchRow("反临时邮箱 · 第三方校验 API", "需在服务端配置密钥，命中即拒", draft.thirdPartyCheck, (next) => {
      draft.thirdPartyCheck = next;
    }),
    h("div.flexline", { style: { marginTop: "10px" } }, testBtn),
    result,
    h("div.tip", {
      text: "这一栏只有改动过才会提交：没配邮件也不影响保存其他设置。要让邮箱验证真正发得出去，SMTP 服务器、发信账号、密码、发信邮箱四项都要填对。",
    }),
  );
}

/** 外观：背景图地址。 */
function appearanceCard(reload: () => void, draft: SettingsDraft): HTMLElement {
  const preview = h("div.ph", { style: { flex: "0 0 200px", height: "64px" } });
  const sync = (): void => {
    const url = draft.background.value.trim();
    mount(preview, url && /^https?:|^\//.test(url) ? h("img", { src: url, alt: "当前背景图" }) : url ? "地址看起来不对" : "未设置背景图");
  };
  sync();
  draft.background.addEventListener("input", sync);

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
        "填一个可公开访问的图片地址（本站直链也可以）。契约里没有背景图上传接口，所以这里收地址不收文件。",
      ),
      h("div", { style: { flex: "0 0 200px" } }, preview),
      h("div", { style: { flex: "0 0 auto", paddingBottom: "13px" } }, removeBtn),
    ),
    h("div.tip", { text: "地址改完要点下面的「保存设置」；「移除背景图」会立刻生效。" }),
  );
}

/** 重新生成缩略图：服务端兜底没开就如实说 0 张进队。 */
async function rebuildThumbs(button: HTMLButtonElement): Promise<void> {
  const ok = await confirmDialog({
    title: "重新生成缩略图",
    message: "会给缺少缩略图的图片排一次后台任务。服务端没开兜底生成时不会有任何任务入队。",
    confirmText: "开始",
  });
  if (!ok) return;
  button.disabled = true;
  button.textContent = "提交中…";
  try {
    const payload = await request<{ queued: number; enabled: boolean }>("/admin/thumbnails/rebuild", {
      method: "POST",
    });
    if (!payload.enabled) {
      // 如实显示：兜底关着就是 0 张，别让人以为在跑
      toastError(`服务端兜底生成未启用：${payload.queued} 张进队。要补缩略图请先打开「服务端兜底生成」。`);
    } else if (payload.queued === 0) {
      toastOk("没有缺缩略图的图片，0 张进队");
    } else {
      toastOk(`已排入 ${payload.queued} 张`);
    }
  } catch (err) {
    toastError(err);
  } finally {
    button.disabled = false;
    button.textContent = "重新生成缩略图";
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
    thumb: Record<string, unknown>;
    appearance: Record<string, unknown>;
    mail?: Record<string, unknown>;
  } = {
    site: {
      name: draft.siteName.value.trim(),
      base_url: draft.baseUrl.value.trim().replace(/\/+$/, ""),
      plaza_public: draft.plazaPublic,
      plaza_grid_columns: Number(draft.plazaColumns.value) || 3,
      require_login_to_upload: draft.requireLogin,
      register_enabled: draft.registerEnabled,
    },
    upload: {
      max_file_bytes: maxFile,
      max_batch_size: intOf(draft.maxBatchSize),
      max_pixels: pixels,
      queue_depth: intOf(draft.queueDepth),
    },
    thumb: { client: draft.thumbClient, server_fallback: draft.thumbFallback },
    appearance: { background: draft.background.value.trim() || null },
  };
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
    toastOk("设置已保存");
    reload();
  } catch (err) {
    toastError(err);
    button.disabled = false;
    button.textContent = "保存设置";
  }
}
