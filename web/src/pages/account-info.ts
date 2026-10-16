/**
 * 账号信息
 *
 * 只读页：邮箱 / UID / 角色 / 状态 / 用量 / 限流参数。
 *
 * 每次进入都重新拉一次 `/me`，而不是复用启动时缓存的那一份——
 * 管理员刚调完配额或处置状态，用户刷新页面就要看到新值；
 * 页面拿的是「此刻生效的值」，不是「登录那一刻的快照」。
 */

import { ApiError, request } from "../core/api";
import { h, mount } from "../core/dom";
import {
  formatBytes,
  formatDate,
  formatDateTime,
  formatNumber,
  percent,
} from "../core/format";
import { confirmDialog } from "../core/modal";
import type { PageContext } from "../core/router";
import { loadMe, updateUser } from "../core/state";
import { avatarUrl, navGroups, navHref, refreshNav } from "../ui/shell";
import { toastError, toastOk } from "../core/toast";
import type { UserLimits, UserProfile } from "../core/types";
import {
  badge,
  card,
  field,
  pageHead,
  progressBar,
  qtip,
  roleBadge,
  statusBadge,
  textInput,
} from "../ui/widgets";

export async function render(ctx: PageContext): Promise<void> {
  const pad = h("div.pad");
  ctx.outlet.append(pad);
  pad.append(
    pageHead("账号信息", "邮箱、UID、角色、状态与用量。频率限制由管理员配置，这里只读展示。"),
  );

  const body = h("div");
  pad.append(body);

  async function load(): Promise<void> {
    body.replaceChildren(skeletonBlock(2));
    try {
      const payload = await request<{ user: UserProfile }>("/me");
      // 顺手同步全局状态：外壳上的头像与「管理后台」入口都读这份数据
      updateUser(payload.user);
      body.replaceChildren(
        h(
          "div.row",
          { style: { marginBottom: "14px" } },
          avatarCard(payload.user, () => void load()),
          identityCard(payload.user),
        ),
        h(
          "div.row",
          { style: { marginBottom: "14px" } },
          usageCard(payload.user),
          limitsCard(payload.user.limits),
        ),
        // 窄屏没有侧边栏，后台与学生那些页面得从这里进
        mobileNavCard(),
      );
    } catch (err) {
      body.replaceChildren(errorBlock(err, () => void load()));
    }
  }

  await load();
}

/**
 * 头像卡：上传一张，或填一个图片地址。
 *
 * 上传走的是普通上传接口（落进默认相册、打上 `avatar` 标签），所以头像也是
 * 图库里的正经一张图：能在相册里看到、占配额、走内容审核。带 `reuse_existing`
 * 的意思是「这份内容我图库里已经有就复用它」——同一张图反复设头像时，相册里
 * 不会并排多出两张一模一样的。
 */
function avatarCard(user: UserProfile, reload: () => void): HTMLElement {
  const preview = h("div.avatar-preview");
  // 地址挂了（失效、对方不允许外链、路径过期）时说清是哪一种情况：
  // 「头像没变」和「地址坏了」在界面上长得一样，不点明用户只能反复试
  const brokenNotice = h("div.tip", { style: { display: "none", color: "var(--danger)" } });
  const paint = (url: string): void => {
    if (url) {
      const img = h("img", { src: url, alt: "当前头像" });
      img.addEventListener("error", () => {
        // 换成默认头像，并把原因写在地址输入框下面
        img.replaceWith(h("img", { src: avatarUrl(null), alt: "默认头像" }));
        brokenNotice.textContent =
          "这个头像地址加载不出图片（可能已失效，或对方不允许外链），当前显示的是默认头像。换一个地址或上传一张图片即可。";
        brokenNotice.style.display = "";
      });
      mount(preview, img);
    } else {
      mount(preview, h("span", { text: (user.email[0] ?? "U").toUpperCase() }));
    }
  };
  // 没设过自己的头像时显示的是「站点默认 / 内置默认」那张，
  // 下方的说明会讲清楚这一张是从哪来的
  // 与外层的头像同一套回落：自己的 → 站点默认 → 首字母方块
  paint(avatarUrl(user));

  const urlInput = textInput({
    value: user.avatar && /^https?:/.test(user.avatar) ? user.avatar : "",
    mono: true,
    placeholder: "https://example.com/me.png",
  });

  const fileInput = h("input", { type: "file", accept: "image/*", style: { display: "none" }, "aria-hidden": "true" });
  const uploadBtn = h("button.btn.sm.pri", { type: "button", text: "上传图片" });
  uploadBtn.addEventListener("click", () => fileInput.click());
  fileInput.addEventListener("change", () => {
    const file = fileInput.files?.[0];
    if (file) void uploadAvatar(file, uploadBtn, reload);
  });

  const saveUrl = h("button.btn.sm", { type: "button", text: "应用地址" });
  const urlField = field("图片地址", urlInput);
  urlField.append(brokenNotice);
  saveUrl.addEventListener("click", async () => {
    const value = urlInput.value.trim();
    if (!value) {
      toastError(new Error("请填写图片地址，或上传一张图片"));
      urlInput.focus();
      return;
    }
    await saveAvatar(saveUrl, value, reload);
    brokenNotice.style.display = "none";
  });

  const remove = h("button.btn.sm.dgr", { type: "button", text: "移除头像" });
  remove.addEventListener("click", async () => {
    const ok = await confirmDialog({
      title: "移除头像",
      message:
        "移除后回到站点默认头像。当初上传的那张头像图会一并清理（同一份内容在别处还有引用时只解这条引用）。",
      confirmText: "移除",
      danger: true,
    });
    if (!ok) return;
    await saveAvatar(remove, "", reload);
    brokenNotice.style.display = "none";
  });

  return card(
    "头像",
    h(
      "div.flexline",
      { style: { alignItems: "center", gap: "14px", flexWrap: "wrap" } },
      preview,
      h(
        "div",
        { style: { flex: "1 1 220px", minWidth: "200px" } },
        h("div", { text: "推荐使用 URL 地址；不设置时采用站点默认头像。" }),
        h("div.tip", { style: { marginTop: "6px" }, text: "上传的图会存进「默认相册」，并标记 avatar 标签。" }),
      ),
      h("div", { style: { flex: "0 0 auto", display: "flex", gap: "8px", flexWrap: "wrap" } }, uploadBtn, remove),
      fileInput,
    ),
    h(
      "div.row",
      { style: { marginTop: "10px", alignItems: "flex-end" } },
      h("div", { style: { flex: "1 1 260px" } }, urlField),
      h("div", { style: { flex: "0 0 auto", paddingBottom: "13px" } }, saveUrl),
    ),
  );
}

/**
 * 窄屏用的入口目录。
 *
 * 侧边栏在 1024px 以下隐藏，而底部标签栏只有广场 / 相册 / 上传 / 我的四个——
 * 没有这张表的话，安全、API Token、回收站与整个管理后台在手机上根本进不去。
 * 宽屏不摆：那里侧边栏一直在旁边。
 */
function mobileNavCard(): HTMLElement | null {
  if (!window.matchMedia("(max-width: 1023px)").matches) return null;
  const groups = navGroups().filter(([, items]) => items.length > 0);
  const list = h("div.navlist");
  for (const [title, items] of groups) {
    list.append(h("div.grp", { text: title }));
    for (const item of items) {
      list.append(
        h(
          "a.navitem",
          { href: navHref(item.path) },
          h("span.ic", { text: item.icon }),
          h("span", { text: item.label }),
        ),
      );
    }
  }
  return card("全部入口", list);
}

/** 保存头像地址。空串表示移除。 */
async function saveAvatar(button: HTMLButtonElement, value: string, reload: () => void): Promise<void> {
  button.disabled = true;
  const label = button.textContent ?? "";
  button.textContent = "保存中…";
  try {
    const payload = await request<{ avatar: string | null }>("/me", {
      method: "PATCH",
      json: { avatar: value },
    });
    updateUser({ avatar: payload.avatar });
    // 顶栏的头像跟着换：`updateUser` 会通知订阅者，但页面自己那块预览要重画才动
    refreshNav();
    toastOk(payload.avatar ? "头像已更新" : "头像已移除");
    reload();
  } catch (err) {
    toastError(err);
  } finally {
    button.disabled = false;
    button.textContent = label;
  }
}

/**
 * 上传头像：先在本机压到 256px 再传，然后拿回来的直链设为头像。
 *
 * 走的是普通上传接口，所以头像和相册里任何一张图是同一类东西：同一个默认
 * 相册、配额一起算、内容审核一样过。相同内容再传一次不会重复占盘。
 */
async function uploadAvatar(file: File, button: HTMLButtonElement, reload: () => void): Promise<void> {
  if (file.size > 20 * 1024 * 1024) {
    toastError(new Error("头像图片不得超过 20 MiB，请压缩后重试"));
    return;
  }
  button.disabled = true;
  button.textContent = "处理中…";
  try {
    const blob = await downscaleToSquare(file, 256);
    const form = new FormData();
    form.append("file", blob, "avatar.jpg");
    // 不指定相册 = 落到用户的默认相册；标签固定成 avatar
    form.append("tags", "avatar");
    form.append("visibility", "private");
    form.append("original_name", "avatar.jpg");
    // 同一张图设过头像就不必在图库里再留第二份：服务端直接引用已有的那张
    form.append("reuse_existing", "1");
    const uploaded = await request<{ url: string; dedup: boolean; reused: boolean }>("/upload", {
      method: "POST",
      form,
    });
    await request("/me", { method: "PATCH", json: { avatar: uploaded.url } });
    await loadMe();
    toastOk(
      uploaded.reused
        ? "这张图已经在你的图库里，已直接引用为头像"
        : uploaded.dedup
          ? "图库里已有相同文件，已引用为头像"
          : "头像已上传并应用",
    );
    reload();
  } catch (err) {
    toastError(err);
  } finally {
    button.disabled = false;
    button.textContent = "上传图片";
  }
}

/** 本机裁剪成正方形并缩到 `size` 像素：头像是圆的，长方形留着也是被裁掉。 */
async function downscaleToSquare(file: File, size: number): Promise<Blob> {
  const bitmap: ImageBitmap | HTMLImageElement =
    typeof createImageBitmap === "function"
      ? await createImageBitmap(file)
      : await new Promise<HTMLImageElement>((resolve, reject) => {
          const image = new Image();
          const url = URL.createObjectURL(file);
          image.onload = () => resolve(image);
          image.onerror = () => reject(new Error("图片无法读取，请更换后重试"));
          image.src = url;
        });

  const width = bitmap instanceof HTMLImageElement ? bitmap.naturalWidth : bitmap.width;
  const height = bitmap instanceof HTMLImageElement ? bitmap.naturalHeight : bitmap.height;
  if (width <= 0 || height <= 0) throw new Error("图片无法读取，请更换后重试");

  const side = Math.min(width, height);
  const canvas = document.createElement("canvas");
  canvas.width = size;
  canvas.height = size;
  const context = canvas.getContext("2d");
  if (!context) throw new Error("浏览器不支持本地压缩，请改用图片地址");
  context.drawImage(bitmap, (width - side) / 2, (height - side) / 2, side, side, 0, 0, size, size);
  if (bitmap instanceof ImageBitmap) bitmap.close();

  const blob = await new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, "image/jpeg", 0.88));
  if (!blob) throw new Error("压缩失败，请改用图片地址");
  return blob;
}

/** 身份卡：邮箱 / UID / 角色 / 状态 / 注册与最近登录时间。 */
function identityCard(user: UserProfile): HTMLElement {
  return card(
    "账号信息",
    h(
      "div",
      { style: { fontSize: "13px", lineHeight: "2" } },
      h(
        "div",
        null,
        "邮箱 ",
        h("b", { text: user.email }),
        " ",
        // 验证状态决定「通知邮件收不收得到」，单独给个徽章
        badge(user.email_verified ? "已验证" : "未验证", user.email_verified ? "ok" : "warn"),
      ),
      h("div", null, "UID ", h("span.mono", { text: user.uid })),
      h("div", null, "角色 ", roleBadge(user.role)),
      h("div", null, "状态 ", statusBadge(user.status)),
      h("div", { text: `注册于 ${formatDate(user.created_at)}` }),
      h("div", {
        text: `最后登录 ${formatDateTime(user.last_login_at)}${
          user.last_login_ip ? ` · ${user.last_login_ip}` : ""
        }`,
      }),
    ),
    // 被处置过的账号会带原因，登录后第一时间就该知道为什么被限制
    user.status !== "active" && user.status_reason
      ? h(
          "div",
          { style: { marginTop: "10px" } },
          qtip(
            "warn",
            "账号当前受限",
            `${user.status_reason}（${formatDateTime(user.status_changed_at)}）`,
          ),
        )
      : null,
    h("div.tip", { text: "邮箱和密码由管理员在后台也能改，改完会给你发通知邮件。" }),
  );
}

/** 用量卡：进度条 + 统计明细。 */
function usageCard(user: UserProfile): HTMLElement {
  const ratio = user.quota_bytes > 0 ? user.used_bytes / user.quota_bytes : 0;
  // 占用到 70% 就该提醒，到 90% 基本马上传不动了
  const tone = ratio >= 0.9 ? "剩余空间已不足 10%" : ratio >= 0.7 ? "已用超过 70%" : "";
  return card(
    "存储用量",
    h(
      "div",
      { style: { fontSize: "13px", marginBottom: "8px" } },
      "已用 ",
      h("b", { text: formatBytes(user.used_bytes) }),
      " / 配额 ",
      h("b", { text: formatBytes(user.quota_bytes) }),
    ),
    progressBar(user.used_bytes, user.quota_bytes),
    h("div.tip", {
      text: `已用 ${percent(user.used_bytes, user.quota_bytes)}%${
        tone ? ` · ${tone}` : ""
      }。配额由管理员分配，接近上限时会提前提示。`,
    }),
    h("div", {
      style: {
        marginTop: "12px",
        paddingTop: "12px",
        borderTop: "1px solid #f3f4f6",
        fontSize: "12.5px",
        color: "var(--muted)",
      },
      text:
        `相册 ${formatNumber(user.stats.album_count)} · 图片 ${formatNumber(user.stats.image_count)}` +
        ` · 其中公开 ${formatNumber(user.stats.public_count)} · 标签 ${formatNumber(user.stats.tag_count)}`,
    }),
  );
}

/** 限流参数卡：全部只读，改不了就不做成可输入的样子。 */
function limitsCard(limits: UserLimits): HTMLElement {
  const unlimited = limits.unlimited;
  const count = (value: number): string => (unlimited ? "不限" : formatNumber(value));
  const size = (value: number): string => (unlimited ? "不限" : formatBytes(value));
  return card(
    "频率限制（只读）",
    h(
      "div.row",
      null,
      readonlyField("API 次 / 分钟", count(limits.api_per_minute)),
      readonlyField("上传 张 / 分钟", count(limits.upload_per_minute)),
      readonlyField("上传 张 / 天", count(limits.upload_per_day)),
      readonlyField("单日流量", size(limits.upload_bytes_per_day)),
    ),
    h(
      "div.row",
      { style: { marginTop: "12px" } },
      readonlyField("单文件上限", size(limits.max_file_size)),
      readonlyField("单批张数", count(limits.max_batch_size)),
    ),
    unlimited
      ? h(
          "div",
          { style: { marginTop: "12px" } },
          qtip("info", null, "当前账号被标记为「不限」，上面这些限额对你不生效。"),
        )
      : null,
    h("div.tip", { text: "由管理员在后台配置，用户自己改不了。需要调整请联系管理员。" }),
  );
}

function readonlyField(label: string, value: string): HTMLElement {
  const input = textInput({ value, readonly: true });
  return h("div.fld", { style: { margin: "0" } }, h("label.f", { text: label }), input);
}

/** 加载态：两块高度接近真实卡片的骨架，避免数据到达前是一片空白。 */
function skeletonBlock(count: number): HTMLElement {
  return h(
    "div",
    null,
    ...Array.from({ length: count }, () =>
      h("div.skel", { style: { height: "110px", marginBottom: "14px" } }),
    ),
  );
}

/** 错误态：给出原因和重试入口，不要只丢一句「加载失败」。 */
function errorBlock(err: unknown, retry: () => void): HTMLElement {
  const message =
    err instanceof ApiError
      ? err.detail
        ? `${err.message}（${err.detail}）`
        : err.message
      : err instanceof Error
        ? err.message
        : String(err);
  return h(
    "div.card",
    null,
    qtip("err", "账号信息加载失败", message),
    h(
      "div",
      { style: { marginTop: "10px" } },
      h("button.btn", { type: "button", text: "重试", on: { click: retry } }),
    ),
  );
}
