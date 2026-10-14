/**
 * 服务条款管理（`#/admin/terms`）。
 *
 * 状态决定能做什么，这张表就是规则本身：
 *   - 生效中：可编辑（改完另存为新草稿）、预览、删除（服务端会拒绝，页面上把原因原样带回来）；
 *   - 已归档：可查看、设为生效、删除；
 *   - 草稿：继续编辑、发布、删除。
 *
 * 两条后端约束要在这里体现出来，否则管理员会白点半天：
 *   1. `PATCH` 只接受草稿，所以编辑生效中的版本不能就地改，保存等于「另存为新草稿」；
 *   2. 生效中的版本删不掉（先把另一个版本设为生效才行）。删除按钮看起来是禁用的，
 *      但点下去仍然会请求服务端——不能删的理由只有服务端知道，前端不替它编。
 */

import { ApiError, request } from "../core/api";
import { h, mount } from "../core/dom";
import { formatDate, formatDateTime, formatNumber, fromLocalInput, toLocalInput } from "../core/format";
import { confirmDialog, openModal } from "../core/modal";
import type { PageContext } from "../core/router";
import { toastError, toastOk } from "../core/toast";
import type { TermsVersion } from "../core/types";
import {
  badge,
  card,
  cardHead,
  cardNode,
  emptyState,
  field,
  note,
  pageHead,
  qtip,
  select,
  spinner,
  switchRow,
  switchStatic,
  textarea,
  textInput,
} from "../ui/widgets";

/** 后台列表行：契约在版本本体之外还带同意人数与更新时间。 */
interface AdminTermsRow extends TermsVersion {
  updated_at?: string | null;
  require_reconsent?: boolean;
}

/** 页面状态：列表 + 当前编辑目标。 */
interface State {
  rows: AdminTermsRow[];
  /** 正在编辑的版本 id；null 表示新增 */
  editingId: number | null;
  editing: AdminTermsRow | null;
}

/** 三个重画入口，避免把「谁刷新谁」写成一串参数。 */
interface View {
  state: State;
  /** 只重画表格（搜索、删除后） */
  table: () => void;
  /** 只重画编辑区（切换编辑目标） */
  editor: () => void;
  /** 表格上方展示服务端拒绝原因的槽位 */
  reason: HTMLElement;
}

const STATUS_TONE: Record<string, "ok" | "warn" | "mute"> = {
  active: "ok",
  draft: "warn",
  archived: "mute",
};

const STATUS_TEXT: Record<string, string> = {
  active: "生效中",
  draft: "草稿",
  archived: "已归档",
};

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

function txt(value: string): Node {
  return document.createTextNode(value);
}

function statusOf(row: AdminTermsRow): string {
  return row.status ?? "archived";
}

function statusBadgeNode(row: AdminTermsRow): HTMLElement {
  const status = statusOf(row);
  return badge(row.status_label || STATUS_TEXT[status] || status, STATUS_TONE[status] ?? "mute");
}

/** 同意人数：草稿没有同意记录，显示破折号。 */
function consentText(row: AdminTermsRow): string {
  if (statusOf(row) === "draft") return "—";
  if (row.consent_count === undefined || row.user_total === undefined) return "—";
  return `同意 ${formatNumber(row.consent_count)} / 共 ${formatNumber(row.user_total)}`;
}

/** 下一个版本号的建议值：v2.1 → v2.2；解析不出来就不猜。 */
function suggestVersion(rows: AdminTermsRow[]): string {
  for (const row of rows) {
    const match = /^(.*?)(\d+)$/.exec(row.version);
    if (match) return `${match[1]}${Number(match[2]) + 1}`;
  }
  return "";
}

export async function render(ctx: PageContext): Promise<void> {
  const root = h("div.pad");
  mount(ctx.outlet, root);
  root.append(loadingBlock("正在读取条款版本…"));
  const reload = (): void => void render(ctx);

  try {
    const rows = await request<AdminTermsRow[]>("/admin/terms");
    const state: State = { rows, editingId: null, editing: null };
    // 默认对着最新一个草稿，没有草稿就是新增态
    const draft = rows.find((row) => statusOf(row) === "draft");
    if (draft) {
      state.editing = draft;
      state.editingId = draft.id;
    }
    paintPage(root, state);
  } catch (err) {
    mount(root, errorBlock(err, reload));
  }
}

function paintPage(root: HTMLElement, state: State): void {
  const tableBody = h("tbody");
  const editorHost = h("div");
  const reason = h("div", { style: { marginTop: "10px" } });
  const search = textInput({ placeholder: "搜索版本或摘要…" });
  const countBadge = badge(`${state.rows.length} 个版本`, "mute");

  const view: View = {
    state,
    reason,
    table: () => fillTable(view, tableBody, search.value, countBadge),
    editor: () => mount(editorHost, editorCard(view)),
  };

  const addBtn = h("button.btn.pri.sm", { type: "button", text: "＋ 新增条款" });
  addBtn.addEventListener("click", () => {
    state.editing = null;
    state.editingId = null;
    view.editor();
  });

  search.addEventListener("input", () => view.table());
  view.table();
  view.editor();

  const head = h(
    "div.flexline",
    { style: { padding: "12px 14px", borderBottom: "1px solid var(--border)" } },
    h("b", { style: { fontSize: "13.5px" }, text: "条款版本" }),
    countBadge,
    h("span.spacer"),
    h("div", { style: { width: "200px" } }, search),
    addBtn,
  );

  mount(
    root,
    pageHead("服务条款管理", "新增、修改、删除条款，存在数据库里，改完即时生效。发布新版本时可选择是否要求用户重新同意。"),
    cardNode(
      head,
      h(
        "div.table-wrap",
        null,
        h(
          "table.responsive",
          null,
          h(
            "thead",
            null,
            h(
              "tr",
              null,
              h("th", { text: "版本" }),
              h("th", { text: "更新摘要" }),
              h("th", { text: "生效日期" }),
              h("th", { text: "状态" }),
              h("th", { text: "同意人数" }),
              h("th", { text: "更新时间" }),
              h("th", { text: "操作" }),
            ),
          ),
          tableBody,
        ),
      ),
    ),
    editorHost,
    reason,
    h(
      "div",
      { style: { marginTop: "14px" } },
      qtip(
        "err",
        "删除限制",
        "当前生效中的版本不能直接删除，必须先把另一个版本设为生效。删除只是归档隐藏，用户已有的同意记录会保留，便于日后追溯。",
      ),
    ),
    note(
      "w",
      "条款存数据库这张表里：版本号、生效日期、正文、状态、更新摘要，外加一张同意记录表存「谁在什么时候同意了哪个版本、来自哪个 IP」。同意记录不随条款删除而消失。",
    ),
  );
}

function fillTable(view: View, tbody: HTMLElement, keyword: string, countBadge: HTMLElement): void {
  const key = keyword.trim().toLowerCase();
  const rows = view.state.rows.filter(
    (row) =>
      !key ||
      row.version.toLowerCase().includes(key) ||
      (row.summary ?? "").toLowerCase().includes(key),
  );
  countBadge.textContent = `${view.state.rows.length} 个版本`;
  if (rows.length === 0) {
    mount(tbody, h("tr", null, h("td", { colspan: 7 }, emptyState("📄", "没有匹配的条款版本。"))));
    return;
  }
  mount(tbody, ...rows.map((row) => rowNodes(view, row)));
}

/** 重新拉一次列表，并同步当前编辑目标（删掉的版本要退出编辑态）。 */
async function refetch(view: View): Promise<void> {
  const rows = await request<AdminTermsRow[]>("/admin/terms");
  view.state.rows = rows;
  if (view.state.editingId !== null) {
    view.state.editing = rows.find((row) => row.id === view.state.editingId) ?? null;
    if (!view.state.editing) view.state.editingId = null;
  }
  view.table();
  view.editor();
}

/** 一行规则，操作按状态给。 */
function rowNodes(view: View, row: AdminTermsRow): HTMLElement {
  const status = statusOf(row);
  const actions = h("div.act");

  const editBtn = (label: string): HTMLButtonElement =>
    h("button.btn.sm", {
      type: "button",
      text: label,
      on: {
        click: () => {
          view.state.editing = row;
          view.state.editingId = row.id;
          view.editor();
        },
      },
    });

  const previewBtn = (label: string): HTMLButtonElement =>
    h("button.btn.sm", { type: "button", text: label, on: { click: () => openPreview(row) } });

  const publishBtn = h("button.btn.sm.pri", {
    type: "button",
    text: "发布",
    on: { click: () => void publishRow(view, row) },
  });

  if (status === "active") {
    actions.append(editBtn("编辑"), previewBtn("预览"), deleteButton(view, row, true));
  } else if (status === "draft") {
    actions.append(editBtn("继续编辑"), publishBtn, deleteButton(view, row, false));
  } else {
    actions.append(previewBtn("查看"), hitEffectiveButton(view, row), deleteButton(view, row, false));
  }

  return h(
    "tr",
    null,
    h("td", { "data-label": "版本" }, h("b", { text: row.version })),
    h("td", { "data-label": "更新摘要" }, txt(row.summary || "—")),
    h("td", { "data-label": "生效日期" }, txt(row.effective_at ? formatDate(row.effective_at) : "—")),
    h("td", { "data-label": "状态" }, statusBadgeNode(row)),
    h("td", { "data-label": "同意人数" }, txt(consentText(row))),
    h("td", { "data-label": "更新时间" }, txt(formatDateTime(row.updated_at ?? row.published_at ?? null))),
    h("td", { "data-label": "操作" }, actions),
  );
}

/** 把已归档的版本重新设为生效。 */
function hitEffectiveButton(view: View, row: AdminTermsRow): HTMLButtonElement {
  const button = h("button.btn.sm", { type: "button", text: "设为生效" });
  button.addEventListener("click", async () => {
    const current = activeRow(view);
    const ok = await confirmDialog({
      title: `把 ${row.version} 设为生效`,
      message: current
        ? `当前生效的 ${current.version} 会被归档。如果这一版当初设置了「要求所有用户重新同意」，用户下次登录时会重新同意。`
        : "当前没有生效中的版本，这次设置之后条款立刻对用户生效。",
      confirmText: "设为生效",
    });
    if (!ok) return;
    await doPublish(view, row);
  });
  return button;
}

/**
 * 删除。生效中的版本按钮看起来禁用（服务端也会拒），但点下去仍然发请求：
 * 不能删的理由只有服务端知道，前端不替它编。
 */
function deleteButton(view: View, row: AdminTermsRow, looksDisabled: boolean): HTMLButtonElement {
  const button = h("button.btn.sm.dgr", {
    type: "button",
    text: "删除",
    "aria-disabled": looksDisabled ? "true" : "false",
    title: looksDisabled ? "生效中的版本不能直接删除，点一下看服务端说明" : "",
    style: looksDisabled ? { opacity: "0.55" } : undefined,
  });
  button.addEventListener("click", async () => {
    if (!looksDisabled) {
      const ok = await confirmDialog({
        title: `删除 ${row.version}`,
        message: "删除只是归档隐藏，用户已有的同意记录会保留，便于日后追溯。",
        confirmText: "删除",
        danger: true,
      });
      if (!ok) return;
    }
    button.disabled = true;
    try {
      await request(`/admin/terms/${row.id}`, { method: "DELETE" });
      toastOk(`${row.version} 已删除`);
      mount(view.reason);
      await refetch(view);
    } catch (err) {
      const message = err instanceof ApiError ? err.message : String(err);
      const detail = err instanceof ApiError ? err.detail : null;
      mount(view.reason, qtip("err", `${row.version} 没删掉`, detail ? `${message}（${detail}）` : message));
      toastError(err);
      button.disabled = false;
    }
  });
  return button;
}

function activeRow(view: View): AdminTermsRow | undefined {
  return view.state.rows.find((item) => statusOf(item) === "active");
}

async function doPublish(view: View, row: AdminTermsRow): Promise<void> {
  try {
    await request(`/admin/terms/${row.id}/publish`, { method: "POST" });
    toastOk(`${row.version} 已生效`);
    await refetch(view);
  } catch (err) {
    toastError(err);
  }
}

/** 发布前的确认：把「归档谁、要不要重新同意、多少人」说清楚。 */
async function publishRow(view: View, row: AdminTermsRow): Promise<void> {
  const current = activeRow(view);
  const parts = [
    `发布后 ${row.version} 立即生效。`,
    current ? `当前生效的 ${current.version} 会被归档。` : "当前没有生效中的版本。",
  ];
  if (row.require_reconsent) {
    parts.push(
      current?.user_total === undefined
        ? "这一版设置了「要求所有用户重新同意」，用户下次登录时需要重新同意。"
        : `这一版设置了「要求所有用户重新同意」，约 ${formatNumber(current.user_total)} 位用户下次登录时会被要求重新同意。`,
    );
  }
  const ok = await confirmDialog({
    title: `发布 ${row.version}`,
    message: parts.join(""),
    confirmText: "发布并生效",
  });
  if (!ok) return;
  await doPublish(view, row);
}

/** 预览：正文按纯文本摊开，和用户看到的一致。 */
function openPreview(row: AdminTermsRow): void {
  let handle: { close: () => void } | null = null;
  const body = h("div", {
    style: { whiteSpace: "pre-wrap", fontSize: "12.5px", lineHeight: "1.95", color: "#4b5563" },
  });
  body.textContent = row.body
    ? row.body
    : "列表接口没有带正文，请在编辑区查看或修改。";
  handle = openModal({
    title: `${row.version} · ${row.status_label || STATUS_TEXT[statusOf(row)] || ""}`,
    reading: true,
    body: [
      h("div.tip", {
        text: `${row.effective_at ? `生效日期 ${formatDate(row.effective_at)} · ` : ""}${consentText(row)}${row.summary ? ` · ${row.summary}` : ""}`,
      }),
      h("div", { style: { marginTop: "12px" } }, body),
    ],
    footer: [
      h("button.btn", { type: "button", text: "关闭", on: { click: () => handle?.close() } }),
    ],
  });
}

/** 编辑区：版本号、生效日期、状态、摘要、正文与两个开关。 */
function editorCard(view: View): HTMLElement {
  const state = view.state;
  const target = state.editing;
  const isDraft = target !== null && statusOf(target) === "draft";
  const controls = {
    version: textInput({ value: target?.version ?? suggestVersion(state.rows), placeholder: "v2.2", mono: true }),
    effective: textInput({ type: "datetime-local", value: target?.effective_at ? toLocalInput(target.effective_at) : "" }),
    status: select(
      [
        { value: "draft", label: "草稿" },
        { value: "now", label: "立即生效" },
        { value: "scheduled", label: "定时生效" },
      ],
      "draft",
    ),
    summary: textInput({ value: target?.summary ?? "", placeholder: "新增第七条：账号申诉流程说明" }),
    body: textarea({ value: target?.body ?? "", rows: 9, placeholder: "一、服务说明\n……" }),
    reconsent: { value: Boolean(target?.require_reconsent) },
  };

  const result = h("div", { style: { marginTop: "8px" } });
  const saveBtn = h("button.btn", {
    type: "button",
    text: isDraft || target === null ? "保存草稿" : "另存为新草稿",
  });
  const publishBtn = h("button.btn.pri", { type: "button", text: "发布并生效" });
  const previewBtn = h("button.btn", { type: "button", text: "预览用户视角" });

  previewBtn.addEventListener("click", () => {
    let handle: { close: () => void } | null = null;
    handle = openModal({
      title: `预览 ${controls.version.value.trim() || "新条款"}`,
      reading: true,
      body: [
        h("div.tip", {
          text: `${controls.summary.value.trim() || "（无更新摘要）"} · 纯文本渲染，与用户看到的形态一致`,
        }),
        h(
          "div",
          {
            style: {
              marginTop: "12px",
              whiteSpace: "pre-wrap",
              fontSize: "12.5px",
              lineHeight: "1.95",
              color: "#4b5563",
            },
          },
          txt(controls.body.value || "（正文为空）"),
        ),
      ],
      footer: [h("button.btn", { type: "button", text: "关闭", on: { click: () => handle?.close() } })],
    });
  });

  const submit = async (thenPublish: boolean): Promise<void> => {
    const version = controls.version.value.trim();
    if (!version) {
      toastError("版本号不能为空，例如 v2.2");
      return;
    }
    if (!controls.body.value.trim()) {
      toastError("条款正文不能为空");
      return;
    }
    const mode = controls.status.value;
    if (mode === "scheduled" && !controls.effective.value) {
      toastError("选了「定时生效」就要填生效日期");
      return;
    }
    const effectiveAt = mode === "scheduled" ? fromLocalInput(controls.effective.value) : null;
    const payload = {
      version,
      title: target?.title ?? "服务条款",
      body: controls.body.value,
      summary: controls.summary.value.trim() || null,
      status: "draft",
      require_reconsent: controls.reconsent.value,
      effective_at: effectiveAt,
    };

    saveBtn.disabled = true;
    publishBtn.disabled = true;
    saveBtn.textContent = "保存中…";
    try {
      let id: number;
      if (isDraft && target) {
        await request(`/admin/terms/${target.id}`, { method: "PATCH", json: payload });
        id = target.id;
      } else {
        // 后端只让改草稿：非草稿目标一律另存为新草稿，原版本不动
        const created = await request<AdminTermsRow>("/admin/terms", { method: "POST", json: payload });
        id = created?.id ?? 0;
        if (!id) throw new Error("服务端没有返回新版本 id");
        state.editingId = id;
      }

      if (thenPublish) {
        await request(`/admin/terms/${id}/publish`, { method: "POST" });
        toastOk(`${version} 已生效`);
      } else {
        toastOk(isDraft ? "草稿已保存" : `已另存为草稿 ${version}`);
      }
      await refetch(view);
    } catch (err) {
      saveBtn.disabled = false;
      publishBtn.disabled = false;
      saveBtn.textContent = isDraft || target === null ? "保存草稿" : "另存为新草稿";
      const message = err instanceof ApiError ? err.message : String(err);
      const detail = err instanceof ApiError ? err.detail : null;
      mount(result, qtip("err", "没保存成功", detail ? `${message}（${detail}）` : message));
      toastError(err);
    }
  };

  saveBtn.addEventListener("click", () => void submit(false));
  publishBtn.addEventListener("click", async () => {
    const current = activeRow(view);
    const version = controls.version.value.trim() || "新版本";
    const parts = [
      `发布后 ${version} 立即生效。`,
      current ? `当前生效的 ${current.version} 会被归档。` : "当前没有生效中的版本。",
    ];
    if (controls.reconsent.value) {
      parts.push(
        current?.user_total === undefined
          ? "已勾选「要求所有用户重新同意」，用户下次登录时需要重新同意。"
          : `已勾选「要求所有用户重新同意」，约 ${formatNumber(current.user_total)} 位用户下次登录时会被要求重新同意。`,
      );
    }
    const ok = await confirmDialog({ title: "发布并生效", message: parts.join(""), confirmText: "发布并生效" });
    if (!ok) return;
    await submit(true);
  });

  const hint = isDraft
    ? `正在编辑 ${target?.version}（草稿）`
    : target
      ? `正在编辑 ${target.version}（${target.status_label || STATUS_TEXT[statusOf(target)]}）：生效中的版本不能就地改，保存会另存为新草稿`
      : "新建条款：先落成草稿，确认无误再发布";

  return cardNode(
    cardHead("编辑条款", h("small", { text: hint })),
    !isDraft && target
      ? qtip(
          "warn",
          "这一版不能就地修改",
          "后端只允许改草稿：保存时会新建一个草稿版本，原版本保持原样；发布新草稿后才会替换它。",
        )
      : null,
    h(
      "div.row",
      null,
      field("版本号", controls.version),
      field("生效日期", controls.effective, "选「定时生效」时必填；留空表示发布时立即生效"),
      field("状态", controls.status),
    ),
    field("更新摘要（展示给用户，写进更新说明）", controls.summary),
    h(
      "div.fld",
      null,
      h("label.f", { text: "条款正文" }),
      controls.body,
      h("div.tip", { text: "支持纯文本，保留换行与段落。正文存数据库，不落文件，改完不用重启。" }),
    ),
    switchRow(
      "要求所有用户重新同意",
      "勾选后，用户下次登录会再次弹出通告，必须同意才能继续使用",
      controls.reconsent.value,
      (next) => {
        controls.reconsent.value = next;
      },
    ),
    switchStatic(
      "首次注册用户强制同意",
      "注册流程固定要求同意当前生效版本，这一项不可关闭，也不受上面的开关影响",
      true,
    ),
    h("div.flexline", { style: { justifyContent: "flex-end", marginTop: "14px" } }, previewBtn, saveBtn, publishBtn),
    result,
  );
}
