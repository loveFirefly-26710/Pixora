/**
 * 邮件模板管理（弹窗，从「系统设置 → 邮件与注册验证」进）。
 *
 * 内置文案在服务端，管理员改过的存库；改完发信立刻用新的，不用重启。
 * 两件事在这个弹窗里说清楚：
 *   - 可用变量随模板不同，写错了会当场被拒（拼错的占位符在渲染时会被
 *     静默清掉，信里少一段话还不报错）；
 *   - 「恢复默认」是把自定义那一行删掉，不是把内置文案拷进库里。
 */

import { ApiError, request } from "../core/api";
import { h, mount } from "../core/dom";
import { confirmDialog, openModal } from "../core/modal";
import { toastError, toastOk } from "../core/toast";
import { badge, emptyState, qtip, spinner, textarea, textInput } from "../ui/widgets";

interface MailTemplateRow {
  name: string;
  label: string;
  when: string;
  vars: string[];
  subject: string;
  body: string;
  customized: boolean;
  builtin_subject: string;
  builtin_body: string;
  updated_at: string | null;
}

/** 打开模板管理弹窗。 */
export function openMailTemplates(): void {
  const body = h("div");

  const list = async (): Promise<void> => {
    mount(body, h("div.card", { style: { display: "flex", alignItems: "center", gap: "8px", color: "var(--muted)", fontSize: "13px" } }, spinner(), "正在读取模板…"));
    try {
      const rows = await request<MailTemplateRow[]>("/admin/mail/templates");
      if (rows.length === 0) {
        mount(body, emptyState("📮", "没有可编辑的邮件模板。"));
        return;
      }
      mount(
        body,
        h("div.tip", { text: "改过的模板存库、立刻生效；没改的用内置文案。「恢复默认」会把自定义那一行删掉。" }),
        ...rows.map((row) =>
          h(
            "div.flexline",
            { style: { padding: "10px 0", borderBottom: "1px solid var(--border)", gap: "10px" } },
            h(
              "div",
              { style: { flex: "1", minWidth: "0" } },
              h("div", { style: { fontSize: "13px", fontWeight: "600" } }, row.label),
              h("div.tip", { style: { margin: "2px 0 0" }, text: row.when }),
            ),
            row.customized ? badge("已自定义", "info") : badge("内置", "mute"),
            h("button.btn.sm", {
              type: "button",
              text: "编辑",
              on: { click: () => void edit(row, list) },
            }),
          ),
        ),
      );
    } catch (err) {
      const message = err instanceof ApiError ? err.message : String(err);
      mount(body, qtip("err", "模板没读出来", message));
    }
  };

  const handle = openModal({
    title: "邮件模板",
    reading: true,
    body,
    footer: [h("button.btn", { type: "button", text: "关闭", on: { click: () => handle.close() } })],
  });

  void list();
}

/** 编辑一个模板：主题、正文、可用变量。 */
async function edit(row: MailTemplateRow, back: () => Promise<void>): Promise<void> {
  const subject = textInput({ value: row.subject });
  const bodyText = textarea({ rows: 14, value: row.body });
  bodyText.classList.add("mono");
  const error = h("div.tip", { style: { color: "var(--danger)", margin: "6px 0 0" } });

  const varsLine = h(
    "div.tip",
    null,
    "可用变量（写成双花括号）：",
    ...row.vars.map((name) =>
      h("code", { style: { margin: "0 4px" }, text: `{{${name}}}` }),
    ),
  );

  const body = h(
    "div",
    null,
    qtip("info", row.label, row.when),
    h("div.fld", { style: { marginTop: "12px" } }, h("label.f", { text: "主题" }), subject),
    h("div.fld", null, h("label.f", { text: "正文" }), bodyText),
    varsLine,
    error,
    h(
      "div.flexline",
      { style: { marginTop: "10px", justifyContent: "flex-end" } },
      row.customized
        ? h("button.btn.dgr.gho", {
            type: "button",
            text: "恢复默认文案",
            on: { click: () => void reset() },
          })
        : null,
      h("button.btn", { type: "button", text: "返回列表", on: { click: () => void back() } }),
      h("button.btn.pri", { type: "button", text: "保存", on: { click: () => void save() } }),
    ),
  );

  mount(document.querySelector(".modal .sheet-body")!, body);
  subject.focus();

  async function save(): Promise<void> {
    error.textContent = "";
    try {
      await request(`/admin/mail/templates/${encodeURIComponent(row.name)}`, {
        method: "PUT",
        json: { subject: subject.value, body: bodyText.value },
      });
      toastOk(`${row.label} 模板已保存，下一封邮件就用新文案`);
      await back();
    } catch (err) {
      // 服务端会指出哪个占位符用不上，原样显示
      error.textContent = err instanceof ApiError ? err.message : "保存失败，请稍后重试。";
    }
  }

  async function reset(): Promise<void> {
    const ok = await confirmDialog({
      title: "恢复默认文案",
      message: `${row.label} 的自定义文案会被删掉，回到内置的那一份。已经发出去的邮件不受影响。`,
      confirmText: "恢复默认",
    });
    if (!ok) return;
    try {
      await request(`/admin/mail/templates/${encodeURIComponent(row.name)}`, { method: "DELETE" });
      toastOk(`${row.label} 已回到内置文案`);
      await back();
    } catch (err) {
      toastError(err);
    }
  }
}
