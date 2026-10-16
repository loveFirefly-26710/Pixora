/**
 * 系统日志：账号历史。
 *
 * 一个账号的一生都在这一页：什么时候注册的、换过哪几次邮箱、被谁暂停过、
 * 什么时候注销的，以及登录与访问记录。数据来自审计流水（以 UID 为目标），
 * 所以账号删掉之后这些记录仍然查得到——这正是它和用户详情页那张「处置记录」
 * 的区别。
 *
 * 每条记录都说明「是谁做的」：本人、管理员（带邮箱）还是系统。管理员代改的
 * 与用户自己改的分得清，翻历史才看得出「这次是本人操作还是别人动的」。
 */

import { ApiError, requestPage } from "../core/api";
import { h, mount } from "../core/dom";
import { formatDateTime } from "../core/format";
import { navigate, type PageContext } from "../core/router";
import {
  badge,
  card,
  emptyState,
  pageHead,
  pagination,
  qtip,
  responsiveTable,
  select,
  spinner,
  textInput,
} from "../ui/widgets";

interface AccountLogRow {
  id: number;
  action: string;
  action_label: string;
  target_uid: string | null;
  email: string | null;
  actor_kind: "self" | "admin" | "system" | "unknown";
  actor_label: string;
  actor_id: number | null;
  ip: string | null;
  detail: Record<string, unknown> | null;
  created_at: string;
}

/**
 * 筛选项。`value` 直接发给服务端（逗号分隔的一串动作名），与服务端
 * `account_log::ACCOUNT_ACTIONS` 里那些常量一一对应。
 */
const FILTERS: Array<{ value: string; label: string }> = [
  { value: "", label: "全部账号动作" },
  { value: "register", label: "注册账号" },
  { value: "account_email_change", label: "更换邮箱" },
  { value: "login_success,login_fail,login_blocked,logout", label: "登录与退出" },
  {
    value: "account_suspend,account_ban,account_restore,temp_ban",
    label: "处置（暂停 / 封禁 / 恢复）",
  },
  { value: "password_reset", label: "重置密码" },
  { value: "account_delete", label: "注销账号" },
];

export async function render(ctx: PageContext): Promise<void> {
  const pad = h("div.pad");
  ctx.outlet.append(pad);

  pad.append(
    pageHead(
      "系统日志",
      "账号历史：注册、换邮箱、处置、注销与登录记录。账号注销之后这些记录仍然保留，并标明每一步是本人还是管理员操作的。",
    ),
  );

  const uidFilter = ctx.query.get("uid") ?? "";
  const actionSelect = select(FILTERS, ctx.query.get("action") ?? "");
  const searchInput = textInput({
    value: ctx.query.get("query") ?? "",
    placeholder: "搜索 UID / 邮箱 / IP…",
  });
  const searchBtn = h("button.btn.sm", { type: "button", text: "查询" });

  const body = h("div");
  const foot = h("div");

  const current = (): { action: string; query: string; uid: string } => ({
    action: actionSelect.value,
    query: searchInput.value.trim(),
    uid: uidFilter,
  });

  /** 条件变了就重查第一页，并把条件写进地址栏——刷新、分享、从别处带着条件进来都对得上。 */
  const reload = (): void => {
    const filter = current();
    const params = new URLSearchParams();
    if (filter.action) params.set("action", filter.action);
    if (filter.query) params.set("query", filter.query);
    if (filter.uid) params.set("uid", filter.uid);
    const search = params.toString();
    navigate(`/admin/logs${search ? `?${search}` : ""}`, { replace: true });
    void load(1);
  };

  async function load(page: number): Promise<void> {
    const filter = current();
    mount(body, loadingBlock("正在读取账号历史…"));
    mount(foot);
    try {
      const result = await requestPage<AccountLogRow>("/admin/logs", {
        query: {
          action: filter.action || undefined,
          query: filter.query || undefined,
          uid: filter.uid || undefined,
          page,
        },
      });
      if (result.items.length === 0) {
        mount(body, emptyState("📄", "还没有账号历史记录。"));
        return;
      }
      mount(
        body,
        responsiveTable(
          ["时间", "账号", "动作", "操作人", "来源 IP", "说明"],
          result.items.map((row) => [
            h("span", { text: formatDateTime(row.created_at) }),
            accountCell(row),
            badge(row.action_label, actionTone(row.action)),
            actorCell(row),
            h("span.mono", { text: row.ip ?? "—" }),
            h("span", { text: describeDetail(row) }),
          ]),
        ),
      );
      mount(foot, pagination(result.meta, (next) => void load(next)) ?? "");
    } catch (err) {
      mount(body, errorBlock(err, () => void load(page)));
    }
  }

  searchBtn.addEventListener("click", () => reload());
  searchInput.addEventListener("keydown", (event) => {
    if ((event as KeyboardEvent).key === "Enter") reload();
  });
  actionSelect.addEventListener("change", () => reload());

  pad.append(
    h(
      "div.card",
      null,
      h("h4", null, "筛选"),
      h(
        "div.row",
        null,
        h("div.fld", null, h("label.f", { text: "动作" }), actionSelect),
        h("div.fld", null, h("label.f", { text: "搜索" }), searchInput),
      ),
      h("div.flexline", { style: { marginTop: "10px" } }, searchBtn),
      qtip(
        "info",
        null,
        "账号被注销后，这里的记录不会跟着消失——注销这件事本身就是要留档的。想看某个账号的全部历史，在用户管理里点它那一行的「操作日志」。",
      ),
    ),
    h("div", { style: { marginTop: "14px" } }, body),
    foot,
  );

  await load(1);
}

/** 账号那一格：UID 加能读到的邮箱；点上去看这个账号的历史。 */
function accountCell(row: AccountLogRow): HTMLElement {
  const uid = row.target_uid;
  if (!uid) return h("span", { text: "—" });
  return h(
    "div",
    null,
    h("button.btn.sm.gho", {
      type: "button",
      text: `u_${uid}`,
      on: { click: () => navigate(`/admin/logs?uid=${encodeURIComponent(uid)}`) },
    }),
    row.email ? h("div.mono", { style: { fontSize: "11.5px", color: "var(--muted)" }, text: row.email }) : null,
  );
}

/** 操作人那一格：本人 / 管理员（带邮箱）/ 系统，颜色区分开。 */
function actorCell(row: AccountLogRow): HTMLElement {
  const tone =
    row.actor_kind === "self" ? "info" : row.actor_kind === "admin" ? "warn" : row.actor_kind === "unknown" ? "err" : "mute";
  return badge(row.actor_label, tone);
}

function actionTone(action: string): "ok" | "warn" | "err" | "info" | "mute" {
  switch (action) {
    case "account_delete":
    case "account_ban":
      return "err";
    case "account_suspend":
    case "temp_ban":
    case "login_fail":
    case "login_blocked":
      return "warn";
    case "register":
      return "ok";
    case "login_success":
      return "info";
    default:
      return "mute";
  }
}

/** 说明列：把 detail 里最要紧的那两三件事读成人话。 */
function describeDetail(row: AccountLogRow): string {
  const detail = row.detail ?? {};
  const text = (key: string): string | null => {
    const value = detail[key];
    return typeof value === "string" && value ? value : null;
  };
  const parts: string[] = [];

  switch (row.action) {
    case "account_email_change": {
      const from = text("from");
      const to = text("to");
      parts.push(from && to ? `${from} → ${to}` : to ?? "邮箱已更新");
      break;
    }
    case "account_delete": {
      const files = detail["reclaimed_files"];
      const blobs = detail["blobs"];
      if (typeof files === "number") parts.push(`回收物理文件 ${files} 个`);
      if (typeof blobs === "number" && blobs > 0) parts.push(`待后台回收 ${blobs} 个`);
      if (typeof detail["used_bytes"] === "number") {
        parts.push(`释放用量 ${formatBytes(detail["used_bytes"] as number)}`);
      }
      break;
    }
    case "register": {
      const role = text("role");
      if (role) parts.push(role === "admin" ? "建成管理员" : "建成普通用户");
      if (text("via") === "admin") parts.push("后台创建");
      break;
    }
    default: {
      const reason = text("reason") ?? text("detail");
      if (reason) parts.push(reason);
      if (typeof detail["fail_count"] === "number") parts.push(`累计失败 ${detail["fail_count"]} 次`);
      if (typeof detail["revoked_sessions"] === "number") parts.push(`踢下线 ${detail["revoked_sessions"]} 个会话`);
      if (text("via")) parts.push(`途径 ${text("via")}`);
    }
  }
  return parts.length > 0 ? parts.join(" · ") : "—";
}

function formatBytes(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let index = 0;
  let size = value;
  while (size >= 1024 && index < units.length - 1) {
    size /= 1024;
    index += 1;
  }
  return `${size >= 10 || index === 0 ? Math.round(size) : size.toFixed(1)} ${units[index]}`;
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
