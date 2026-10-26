/**
 * 后台 · 会话记录（`#/admin/sessions`）。
 *
 * 会话记录就是浏览器里的登录 cookie：一条记录对应一台设备的一次登录。
 * 用户自己的「安全」页能管理自己的；这一页让管理员一次看到全站，
 * 可以按账号 / IP / 设备 / 备注搜，也可以删掉某条或清理已经结束的记录。
 *
 * 删除是彻底删行，不是「下线」：删掉后对应浏览器下一次请求就会回到登录页。
 * 正在被管理员自己使用的会话也允许删，但会提示一句——点错了不会锁死后台，
 * 重新登录即可。
 */

import { ApiError, request, requestPage } from "../core/api";
import { h, mount } from "../core/dom";
import { formatDateTime, relativeTime } from "../core/format";
import { confirmDialog } from "../core/modal";
import { navigate, type PageContext } from "../core/router";
import { toastError, toastOk } from "../core/toast";
import {
  badge,
  card,
  emptyState,
  pageHead,
  pagination,
  qtip,
  responsiveTable,
  spinner,
  textInput,
} from "../ui/widgets";

interface AdminSessionRow {
  id: number;
  user_id: number;
  uid: string;
  email: string;
  device: string | null;
  label: string | null;
  ip: string | null;
  created_at: string;
  last_seen_at: string;
  expires_at: string | null;
  revoked_at: string | null;
  state: "active" | "saved" | "expired" | "revoked";
  remember: boolean;
  current: boolean;
}

const STATE_LABEL: Record<AdminSessionRow["state"], string> = {
  active: "活跃",
  saved: "免登录",
  expired: "已过期",
  revoked: "已下线",
};

const STATE_TONE: Record<AdminSessionRow["state"], "ok" | "info" | "warn" | "mute"> = {
  active: "ok",
  saved: "info",
  expired: "warn",
  revoked: "mute",
};

export async function render(ctx: PageContext): Promise<void> {
  const pad = h("div.pad.admin-sessions-page");
  ctx.outlet.append(pad);
  pad.append(
    pageHead(
      "会话记录",
      "全站账号的会话 / cookie 记录。可按邮箱、UID、IP、设备或备注搜索，删除某条记录会让对应浏览器退出登录；「清理已结束记录」只删已下线与已过期的行。",
    ),
  );

  const searchInput = textInput({
    value: ctx.query.get("query") ?? "",
    placeholder: "搜索邮箱 / UID / IP / 设备 / 备注…",
  });
  const searchBtn = h("button.btn.sm", { type: "button", text: "查询" });
  const clearBtn = h("button.btn.sm.dgr", { type: "button", text: "清理已结束记录" });
  const body = h("div");
  const foot = h("div");

  const currentQuery = (): string => searchInput.value.trim();

  const reload = (): void => {
    const query = currentQuery();
    navigate(`/admin/sessions${query ? `?query=${encodeURIComponent(query)}` : ""}`, {
      replace: true,
    });
    void load(1);
  };

  async function load(page: number): Promise<void> {
    mount(body, loadingBlock("正在读取会话记录…"));
    mount(foot);
    try {
      const result = await requestPage<AdminSessionRow>("/admin/sessions", {
        query: { query: currentQuery() || undefined, page },
      });
      if (result.items.length === 0) {
        mount(body, emptyState("🔐", "没有匹配的会话记录。"));
        return;
      }
      mount(
        body,
        responsiveTable(
          ["账号", "设备 / 备注", "IP", "状态", "最近活动", "登录时间", "操作"],
          result.items.map((row) => [
            h(
              "div",
              null,
              h("button.btn.sm.gho", {
                type: "button",
                text: `u_${row.uid}`,
                on: { click: () => navigate(`/admin/users/${row.user_id}`) },
              }),
              h("div.mono", { style: { fontSize: "11.5px", color: "var(--muted)" }, text: row.email }),
            ),
            deviceCell(row),
            h("span.mono", { text: row.ip ?? "—" }),
            h(
              "span",
              { style: { display: "inline-flex", alignItems: "center", gap: "6px", flexWrap: "wrap" } },
              badge(STATE_LABEL[row.state], STATE_TONE[row.state]),
              row.remember ? badge("免登录", "info") : null,
              row.current ? badge("当前", "ok") : null,
            ),
            h("span", { text: relativeTime(row.last_seen_at) }),
            h("span", { text: formatDateTime(row.created_at) }),
            row.current
              ? h("span", { style: { color: "var(--faint)", fontSize: "12px" }, text: "当前会话" })
              : deleteButton(row, () => void load(page)),
          ]),
        ),
      );
      mount(foot, pagination(result.meta, (next) => void load(next)) ?? "");
    } catch (err) {
      mount(body, errorBlock(err, () => void load(page)));
    }
  }

  searchBtn.addEventListener("click", reload);
  searchInput.addEventListener("keydown", (event) => {
    if ((event as KeyboardEvent).key === "Enter") reload();
  });
  clearBtn.addEventListener("click", async () => {
    const ok = await confirmDialog({
      title: "清理已结束的会话记录",
      message:
        "全站已下线、已过期的会话记录会被删除。活跃会话与免登录记录不受影响；这一步不可撤销。",
      confirmText: "清理",
      danger: true,
    });
    if (!ok) return;
    clearBtn.disabled = true;
    try {
      const result = await request<{ deleted: number }>("/admin/sessions", { method: "DELETE" });
      toastOk(`已清理 ${result.deleted ?? 0} 条会话记录`);
      void load(1);
    } catch (err) {
      toastError(err);
    } finally {
      clearBtn.disabled = false;
    }
  });

  pad.append(
    card(
      "筛选",
      h(
        "div.row",
        null,
        h("div.fld", { style: { flex: "1 1 320px" } }, h("label.f", { text: "搜索" }), searchInput),
      ),
      h("div.flexline", { style: { marginTop: "10px" } }, searchBtn, clearBtn),
      qtip(
        "info",
        null,
        "「免登录」是用户勾选「保持登录」后留下的长期 cookie；它仍能恢复登录，但已经超过活动窗口。" +
          "删除记录等于让那台设备退出登录，用户下次打开需要重新输入密码。",
      ),
    ),
    h("div", { style: { marginTop: "14px" } }, body),
    foot,
  );

  await load(1);
}

function deviceCell(row: AdminSessionRow): HTMLElement {
  const label = row.label?.trim() ?? "";
  const primary = label || row.device || "未知设备";
  return h(
    "div",
    { style: { display: "flex", flexDirection: "column", gap: "2px" } },
    h("span", { text: primary }),
    label && row.device
      ? h("span", { style: { color: "var(--muted)", fontSize: "11.5px" }, text: row.device })
      : null,
  );
}

function deleteButton(row: AdminSessionRow, reload: () => void): HTMLElement {
  const button = h("button.btn.sm.dgr", { type: "button", text: "删除" });
  button.addEventListener("click", async () => {
    const ok = await confirmDialog({
      title: "删除会话记录",
      message:
        `${row.email} 在「${row.label?.trim() || row.device || "未知设备"}」上的这条记录会被删除。` +
        "如果它仍然有效，对方的浏览器会立刻退出登录。",
      confirmText: "删除",
      danger: true,
    });
    if (!ok) return;
    button.disabled = true;
    try {
      await request(`/admin/sessions/${row.id}`, { method: "DELETE" });
      toastOk("会话记录已删除");
      reload();
    } catch (err) {
      toastError(err);
      button.disabled = false;
    }
  });
  return button;
}

function loadingBlock(text: string): HTMLElement {
  return h(
    "div.card",
    { style: { display: "flex", alignItems: "center", gap: "8px", color: "var(--muted)", fontSize: "13px" } },
    spinner(),
    text,
  );
}

function errorBlock(err: unknown, onRetry: () => void): HTMLElement {
  const message = err instanceof ApiError ? err.message : err instanceof Error ? err.message : String(err);
  const detail = err instanceof ApiError ? err.detail : null;
  return card(
    null,
    qtip("err", "加载失败", detail ? `${message}（${detail}）` : message),
    h("button.btn", { type: "button", text: "重试", on: { click: onRetry } }),
  );
}
