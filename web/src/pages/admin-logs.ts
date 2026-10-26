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
  /** account = 账户信息，system = 系统设置 */
  category: "account" | "system";
  category_label: string;
  target_uid: string | null;
  /** 审计原始目标：账号 UID，或系统配置标识（settings / cidr / backend:id） */
  target?: string | null;
  email: string | null;
  actor_kind: "self" | "admin" | "system" | "unknown";
  actor_label: string;
  actor_id: number | null;
  ip: string | null;
  detail: Record<string, unknown> | null;
  created_at: string;
}

/** 用户管理相关动作：归到「账户信息」日志。 */
const ACCOUNT_INFO_ACTIONS = [
  "register",
  "account_update",
  "account_email_change",
  "account_delete",
  "account_suspend",
  "account_ban",
  "account_restore",
  "temp_ban",
  "password_reset",
  "quota_change",
  "login_success",
  "login_fail",
  "login_blocked",
  "logout",
].join(",");

/** 其余后台配置动作：归到「系统设置」日志。 */
const SYSTEM_SETTINGS_ACTIONS = [
  "settings_change",
  "ip_rule_change",
  "storage_change",
  "terms_publish",
].join(",");

/**
 * 筛选项。`value` 直接发给服务端（逗号分隔的一串动作名），与服务端
 * `account_log::ACCOUNT_ACTIONS` 里那些常量一一对应。
 */
const FILTERS: Array<{ value: string; label: string }> = [
  { value: "", label: "全部操作" },
  { value: ACCOUNT_INFO_ACTIONS, label: "账户信息" },
  { value: SYSTEM_SETTINGS_ACTIONS, label: "系统设置" },
];


export async function render(ctx: PageContext): Promise<void> {
  const pad = h("div.pad.admin-logs-page");
  ctx.outlet.append(pad);

  pad.append(
    pageHead(
      "系统日志",
      "账号与系统操作历史：用户管理相关的注册、资料、额度、处置、注销等记录归入「账户信息」，系统设置、IP 黑白名单、存储与条款等变更归入「系统设置」。账号注销之后这些记录仍然保留，并标明每一步是本人还是管理员操作的。",
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
    mount(body, loadingBlock("正在读取系统日志…"));
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
        mount(body, emptyState("📄", "还没有系统日志记录。"));
        return;
      }
      mount(
        body,
        responsiveTable(
          ["时间", "类型", "账号", "动作", "操作人", "来源 IP", "说明"],
          result.items.map((row) => [
            h("span", { text: formatDateTime(row.created_at) }),
            badge(row.category_label, row.category === "system" ? "mute" : "info"),
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
        "这里同时记录管理员修改系统设置、IP 黑白名单、存储配置等操作。账号被注销后，相关记录不会跟着消失——注销这件事本身就是要留档的。想看某个账号的全部历史，在用户管理里点它那一行的「操作日志」。",
      ),
    ),
    h("div", { style: { marginTop: "14px" } }, body),
    foot,
  );

  await load(1);
}

/** 账号那一格：UID 加能读到的邮箱；点上去看这个账号的历史。 */
function accountCell(row: AccountLogRow): HTMLElement {
  if (row.category === "system") {
    return h("span", {
      style: { fontSize: "12px", color: "var(--muted)" },
      text: systemTargetLabel(row),
    });
  }
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

/** 系统配置动作的「账号」列：显示被修改的对象，而不是一个假的用户 UID。 */
function systemTargetLabel(row: AccountLogRow): string {
  const target = (row.target ?? "").trim();
  switch (row.action) {
    case "settings_change":
      if (target === "settings") return "系统设置";
      if (target === "ratelimit:global") return "全局频率限制";
      if (target === "ratelimit:on_breach") return "超限处理策略";
      if (target === "capacity") return "容量阈值";
      if (target === "mail_test") return "邮件测试";
      if (target === "upload.formats") return "上传格式";
      if (target === "login-methods") return "登录方式";
      if (target === "config:background") return "背景图";
      if (target === "config:default_avatar") return "默认头像";
      if (row.detail?.["scope"] === "mail_template") return "邮件模板";
      return target || "系统设置";
    case "ip_rule_change":
      return target === "policy" ? "IP 访问策略" : target ? `IP 规则 ${target}` : "IP 黑白名单";
    case "storage_change":
      if (target === "thumb.rebuild") return "缩略图重算";
      if (target === "upload_prefix") return "上传前缀";
      if (target === "storage:gc") return "存储回收";
      if (target.startsWith("default:")) return "默认存储后端";
      if (target.startsWith("backend:")) return "存储后端";
      return target || "存储配置";
    case "terms_publish":
      return target.startsWith("terms:") ? "服务条款" : target || "服务条款";
    default:
      return target || "系统配置";
  }
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
    case "account_update":
    case "quota_change":
    case "temp_ban":
    case "login_fail":
    case "login_blocked":
      return "warn";
    case "register":
      return "ok";
    case "login_success":
      return "info";
    case "ip_rule_change":
      return "warn";
    case "settings_change":
    case "storage_change":
    case "terms_publish":
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
    case "account_update": {
      if (text("scope") === "rate_limit") {
        parts.push(detail["custom"] === true ? "设置个人频率限制" : "恢复继承默认频率限制");
        break;
      }
      const role = text("role");
      if (role) parts.push(role === "admin" ? "角色改为管理员" : "角色改为普通用户");
      if (typeof detail["password_reset"] === "number") {
        parts.push(`重置密码，踢下线 ${detail["password_reset"]} 个会话`);
      }
      if (detail["must_change_password"] === true) parts.push("要求下次登录修改密码");
      const email = text("email");
      if (email) parts.push(`邮箱改为 ${email}`);
      break;
    }
    case "quota_change": {
      const from = detail["from"];
      const to = detail["to"];
      if (typeof from === "number" && typeof to === "number") {
        parts.push(`额度 ${formatBytes(from)} → ${formatBytes(to)}`);
      } else if (typeof to === "number") {
        parts.push(`额度改为 ${formatBytes(to)}`);
      }
      break;
    }
    case "settings_change": {
      const target = row.target ?? "";
      if (target === "settings") {
        const keys = Array.isArray(detail["keys"]) ? detail["keys"] : [];
        const names = keys.filter((key): key is string => typeof key === "string");
        parts.push(
          names.length > 0
            ? `更新 ${names.length} 项设置：${names.slice(0, 4).join("、")}${names.length > 4 ? "…" : ""}`
            : "更新系统设置",
        );
      } else if (target === "ratelimit:global") {
        parts.push("更新全局频率限制");
      } else if (target === "ratelimit:on_breach") {
        parts.push("更新超限处理策略");
      } else if (target === "capacity") {
        parts.push("更新容量阈值");
      } else if (target === "mail_test") {
        parts.push("发送测试邮件");
      } else if (text("scope") === "mail_template") {
        parts.push(
          text("action") === "reset"
            ? `恢复默认邮件模板 ${target}`
            : `修改邮件模板 ${target}`,
        );
      } else if (target === "upload.formats") {
        const formats = Array.isArray(detail["formats"]) ? detail["formats"] : [];
        const names = formats.filter((item): item is string => typeof item === "string");
        parts.push(
          names.length > 0
            ? `更新上传格式白名单：${names.join(" · ")}`
            : "更新上传格式白名单",
        );
      } else if (target === "login-methods") {
        parts.push("更新登录方式");
      } else if (target === "config:background") {
        parts.push("更新站点背景图");
      } else if (target === "config:default_avatar") {
        parts.push("更新默认头像");
      } else {
        parts.push(target ? `更新 ${target}` : "更新系统配置");
      }
      break;
    }
    case "ip_rule_change": {
      const action = text("action");
      const kind = text("kind");
      const scope = text("scope");
      if ((row.target ?? "") === "policy") {
        parts.push("更新 IP 访问策略");
      } else if (action === "create") {
        parts.push(`新增 IP 规则 ${row.target ?? ""}${kind ? `（${kind}）` : ""}`);
        if (scope) parts.push(`作用域 ${scope}`);
      } else if (action === "delete") {
        parts.push(`删除 IP 规则 ${row.target ?? ""}`);
      } else {
        parts.push(row.target ? `修改 IP 规则 ${row.target}` : "修改 IP 黑白名单");
      }
      break;
    }
    case "storage_change": {
      const target = row.target ?? "";
      const action = text("action");
      if (target === "thumb.rebuild") {
        parts.push("触发缩略图重算");
        if (typeof detail["total"] === "number") parts.push(`共 ${detail["total"]} 张`);
      } else if (target === "upload_prefix") {
        parts.push("修改上传前缀");
        const from = text("from");
        const to = text("to");
        if (from && to) parts.push(`${from || "（空）"} → ${to}`);
      } else if (target === "storage:gc") {
        parts.push("执行存储回收");
        if (typeof detail["removed"] === "number") parts.push(`清理 ${detail["removed"]} 个`);
      } else if (target.startsWith("default:")) {
        parts.push("切换默认存储后端");
      } else if (target.startsWith("backend:")) {
        parts.push(
          action === "create"
            ? "新增存储后端"
            : action === "delete"
              ? "删除存储后端"
              : "修改存储后端",
        );
        if (text("driver")) parts.push(`驱动 ${text("driver")}`);
      } else {
        parts.push("修改存储配置");
      }
      break;
    }
    case "terms_publish": {
      const target = row.target ?? "";
      if (target.startsWith("terms:")) {
        const archived = text("archived");
        parts.push(archived ? `发布新版本服务条款（归档 ${archived}）` : "发布新版本服务条款");
      } else {
        parts.push(target ? `发布服务条款 ${target}` : "发布服务条款");
      }
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
