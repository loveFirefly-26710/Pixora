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
import { h } from "../core/dom";
import {
  formatBytes,
  formatDate,
  formatDateTime,
  formatNumber,
  percent,
} from "../core/format";
import type { PageContext } from "../core/router";
import { updateUser } from "../core/state";
import type { UserLimits, UserProfile } from "../core/types";
import {
  badge,
  card,
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
          identityCard(payload.user),
          usageCard(payload.user),
        ),
        limitsCard(payload.user.limits),
      );
    } catch (err) {
      body.replaceChildren(errorBlock(err, () => void load()));
    }
  }

  await load();
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
