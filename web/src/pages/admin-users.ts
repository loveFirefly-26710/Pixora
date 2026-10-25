/**
 * 用户管理（管理后台）
 *
 * 指标卡 + 筛选 + 列表 + 批量操作。
 *
 * 两个刻意的取舍：
 * 1. 筛选变化只重建列表区，不走路由重渲染——搜索框是连续输入的，
 *    整页重建会把光标和输入内容一起丢掉；地址栏用 replaceState 同步，刷新也能还原。
 * 2. 批量操作逐个调接口再汇总：批量没有事务，一个用户失败不该连累其它用户，
 *    失败原因也要能一条条摆给管理员看（超额还差多少、邮箱冲突之类）。
 */

import { ApiError, request, requestPage, type ApiMeta } from "../core/api";
import { debounce, h } from "../core/dom";
import { describeMinutes, formatBytes, formatDate, formatNumber } from "../core/format";
import { confirmDialog, openModal } from "../core/modal";
import { navigate } from "../core/router";
import type { PageContext } from "../core/router";
import { accountLabel, canOperateAccount, getState } from "../core/state";
import { toast, toastError, toastOk } from "../core/toast";
import type { AdminStats, AdminUserRow } from "../core/types";
import {
  emptyState,
  field,
  pageHead,
  pagination,
  passwordControl,
  progressBar,
  qtip,
  roleBadge,
  select,
  statCard,
  statusBadge,
  switchRow,
  textInput,
  textarea,
} from "../ui/widgets";

const PER_PAGE = 20;

interface Filters {
  query: string;
  role: string;
  status: string;
  page: number;
}

/** 处置接口的返回体。 */
interface StatusResult {
  status: string;
  revoked_sessions: number;
  mail_sent: boolean;
}

interface BatchFailure {
  who: string;
  message: string;
}

export async function render(ctx: PageContext): Promise<void> {
  const filters: Filters = {
    query: ctx.query.get("q")?.trim() ?? "",
    role: ctx.query.get("role") ?? "",
    status: ctx.query.get("status") ?? "",
    page: Math.max(1, Number(ctx.query.get("page") ?? "1") || 1),
  };
  /** 已勾选的用户 id（按筛选变化清空，避免批量操作打到看不见的行上） */
  const selected = new Set<number>();
  let rows: AdminUserRow[] = [];
  let meta: ApiMeta = { page: filters.page, per_page: PER_PAGE, total: 0, total_pages: 1 };

  const pad = h("div.pad");
  ctx.outlet.append(pad);
  pad.append(
    h(
      "div.flexline",
      { style: { justifyContent: "space-between", alignItems: "flex-start" } },
      pageHead("用户管理", "配额度、改邮箱密码、暂停或封禁违规账号，以及进入某个用户的相册视图。"),
      h("button.btn.pri", {
        type: "button",
        text: "新建用户",
        on: { click: () => openCreateUserDialog() },
      }),
    ),
  );

  const statsSlot = h("div.stat", { style: { marginBottom: "14px" } });
  const listSlot = h("div");

  /* ---------------- 工具栏 ---------------- */

  const search = textInput({ placeholder: "搜索邮箱或 UID…" });
  search.value = filters.query;
  search.style.maxWidth = "240px";
  search.addEventListener(
    "input",
    debounce(() => {
      filters.query = search.value.trim();
      filters.page = 1;
      resetSelection();
      syncUrl();
      void loadUsers();
    }, 300),
  );

  const roleSelect = select(
    [
      { value: "", label: "全部角色" },
      { value: "admin", label: "管理员" },
      { value: "user", label: "普通用户" },
    ],
    filters.role,
  );
  roleSelect.style.maxWidth = "130px";
  roleSelect.addEventListener("change", () => {
    filters.role = roleSelect.value;
    filters.page = 1;
    resetSelection();
    syncUrl();
    void loadUsers();
  });

  const statusSelect = select(
    [
      { value: "", label: "全部状态" },
      { value: "active", label: "正常" },
      { value: "pending", label: "待验证" },
      { value: "suspended", label: "已暂停" },
      { value: "banned", label: "已封禁" },
    ],
    filters.status,
  );
  statusSelect.style.maxWidth = "130px";
  statusSelect.addEventListener("change", () => {
    filters.status = statusSelect.value;
    filters.page = 1;
    resetSelection();
    syncUrl();
    void loadUsers();
  });

  const quotaButton = h("button.btn.sm", { type: "button", text: "批量设配额", disabled: true });
  const suspendButton = h("button.btn.sm", { type: "button", text: "批量暂停", disabled: true });
  const clearButton = h("button.btn.sm.gho", { type: "button", text: "清除选择", disabled: true });

  const toolbar = h(
    "div.toolbar",
    null,
    search,
    roleSelect,
    statusSelect,
    h("span.spacer"),
    quotaButton,
    suspendButton,
  );

  const selectionText = h("span", { text: "已选 0 个用户" });
  const selectionBar = h(
    "div.toolbar.sel",
    { style: { display: "none" } },
    selectionText,
    h("span.spacer"),
    clearButton,
  );

  const bottomNote = h("div.note", {
    text:
      "「暂停」与「封禁」都会让网页登录和 API 同时失效，确认后自动发通知邮件给用户。" +
      "管理员账号不能被处置，免得把自己锁在外面；管理员也不能改自己的角色，换角色要另一位管理员动手。" +
      "「新建用户」建的账号邮箱直接是已验证状态，密码由你告诉对方。",
  });

  pad.append(statsSlot, toolbar, selectionBar, listSlot, bottomNote);

  quotaButton.addEventListener("click", () => {
    const targets = selectedRows();
    if (targets.length === 0) return;
    openBatchQuotaDialog(targets);
  });
  suspendButton.addEventListener("click", () => {
    const targets = selectedRows();
    if (targets.length === 0) return;
    openDispositionDialog(targets, "suspend");
  });
  clearButton.addEventListener("click", () => {
    resetSelection();
    renderList();
  });

  /* ---------------- 数据加载 ---------------- */

  async function loadAll(): Promise<void> {
    await Promise.all([loadStats(), loadUsers()]);
  }

  async function loadStats(): Promise<void> {
    statsSlot.replaceChildren(...skeletonCards(4));
    try {
      const stats = await request<AdminStats>("/admin/stats");
      statsSlot.replaceChildren(
        statCard("用户总数", formatNumber(stats.user_total)),
        statCard("本月新增", formatNumber(stats.user_new_this_month)),
        statCard("已分配配额", formatBytes(stats.allocated_quota)),
        statCard("实际占用", formatBytes(stats.used_bytes)),
      );
    } catch (err) {
      // 指标拿不到不该拖垮整页：列表才是这一页的主体
      statsSlot.replaceChildren(
        h("div.card", { style: { gridColumn: "1 / -1" } }, qtip("err", "指标加载失败", errorText(err))),
      );
    }
  }

  async function loadUsers(): Promise<void> {
    listSlot.replaceChildren(skeletonRows(4));
    try {
      const page = await requestPage<AdminUserRow>("/admin/users", {
        query: {
          query: filters.query || undefined,
          role: filters.role || undefined,
          status: filters.status || undefined,
          page: filters.page,
          per_page: PER_PAGE,
        },
      });
      rows = page.items;
      meta = page.meta;
      // 页码越界（例如期间删了账号）就退回第一页，而不是给个空表格
      if (rows.length === 0 && meta.total > 0 && filters.page > 1) {
        filters.page = 1;
        syncUrl();
        await loadUsers();
        return;
      }
      renderList();
    } catch (err) {
      listSlot.replaceChildren(errorBlock(err, () => void loadUsers()));
    }
  }

  /* ---------------- 列表渲染 ---------------- */

  function renderList(): void {
    if (rows.length === 0) {
      listSlot.replaceChildren(
        h(
          "div.card",
          null,
          emptyState(
            "👥",
            filters.query || filters.role || filters.status
              ? "没有匹配的用户。换个关键词，或清空筛选条件。"
              : "还没有用户。",
            h("button.btn.sm", {
              type: "button",
              text: "清空筛选",
              on: {
                click: () => {
                  search.value = "";
                  roleSelect.value = "";
                  statusSelect.value = "";
                  filters.query = "";
                  filters.role = "";
                  filters.status = "";
                  filters.page = 1;
                  resetSelection();
                  syncUrl();
                  void loadUsers();
                },
              },
            }),
          ),
        ),
      );
      return;
    }
    const paginator = buildPagination();
    listSlot.replaceChildren(...[buildTable(), paginator].filter((node): node is HTMLElement => node !== null));
  }

  function buildPagination(): HTMLElement | null {
    return pagination(meta, (page) => {
      filters.page = page;
      resetSelection();
      syncUrl();
      void loadUsers();
    });
  }

  function buildTable(): HTMLElement {
    const allSelected = rows.length > 0 && rows.every((row) => selected.has(row.id));
    const selectAll = h("input", { type: "checkbox", "aria-label": "全选本页" });
    selectAll.checked = allSelected;
    selectAll.addEventListener("change", () => {
      for (const row of rows) {
        if (selectAll.checked) selected.add(row.id);
        else selected.delete(row.id);
      }
      // 全选要一次性更新所有行的勾选态与高亮，重建表格最省心
      renderList();
      updateSelectionBar();
    });

    const thead = h(
      "thead",
      null,
      h(
        "tr",
        null,
        h("th", { style: { width: "36px" } }, selectAll),
        h("th", { style: { width: "70px" }, text: "UID" }),
        h("th", { text: "邮箱" }),
        h("th", { style: { width: "92px" }, text: "角色" }),
        h("th", { style: { width: "92px" }, text: "状态" }),
        h("th", { style: { width: "200px" }, text: "用量 / 配额" }),
        // 注册时间在 768–1023px 属于次要列，按响应式规范收起来
        h("th", { class: "hide-md", style: { width: "104px" }, text: "注册时间" }),
        h("th", { style: { width: "196px" }, text: "操作" }),
      ),
    );

    const table = h("table.responsive", null, thead, h("tbody", null, ...rows.map(rowNode)));
    return h("div.table-wrap", null, table);
  }

  function rowNode(row: AdminUserRow): HTMLElement {
    const tr = h("tr", { class: selected.has(row.id) ? "pick" : "" });

    const checkbox = h("input", { type: "checkbox", "aria-label": `选择 ${row.email}` });
    checkbox.checked = selected.has(row.id);
    checkbox.addEventListener("change", () => {
      if (checkbox.checked) selected.add(row.id);
      else selected.delete(row.id);
      // 只改这一行：整表重建会把滚动位置顶掉
      tr.classList.toggle("pick", checkbox.checked);
      updateSelectionBar();
    });

    tr.append(
      h("td", { "data-label": "选择" }, checkbox),
      h("td", { "data-label": "UID" }, h("span.mono", { text: row.uid })),
      h(
        "td",
        { "data-label": "邮箱" },
        h("span", { text: accountLabel(row) }),
        // 访客账号在列表里叫「访客」：它没有主人，邮箱又是保留域，
        // 把真实邮箱折成一行小字放在下面，需要时仍看得到
        row.guest
          ? h("div.mono", { style: { fontSize: "11px", color: "var(--faint)" }, text: row.email })
          : null,
        row.rate_custom
          ? h("span.tag", { title: "该账号单独设置了频率限制", text: "限流自定义" })
          : null,
      ),
      h("td", { "data-label": "角色" }, roleBadge(row.role)),
      h("td", { "data-label": "状态" }, statusBadge(row.status, row.status_label)),
      h("td", { "data-label": "用量 / 配额" }, usageCell(row)),
      h("td", { class: "hide-md", "data-label": "注册时间", text: formatDate(row.created_at) }),
      h("td", { "data-label": "操作" }, actionsCell(row)),
    );

    return tr;
  }

  /**
   * 免登录切换到某个用户。
   *
   * 服务端会为目标用户签发一个会话，并把管理员自己的会话令牌记在
   * `pixora_impersonator` Cookie 里——所以刷新之后仍能切回来。
   * 切过去之后页面按对方账号重载一次，避免界面上留着上一个人的缓存。
   */
  async function impersonate(row: AdminUserRow, button: HTMLButtonElement): Promise<void> {
    const ok = await confirmDialog({
      title: `以 u_${row.uid} 的身份登录`,
      message:
        `不需要对方密码，会直接以 ${row.email} 的身份进入站点；期间的所有操作都算在对方名下。` +
        "<br>页面顶部会显示提示条，点「切回管理员」即可恢复；本次切换会写入审计日志。",
      confirmText: "切换登录",
    });
    if (!ok) return;
    button.disabled = true;
    try {
      await request(`/admin/users/${row.id}/impersonate`, { method: "POST" });
      toast(`已切换到 u_${row.uid}，可以随时切回`);
      // 身份换了，整页重载最干净：缓存里的用户、配额、相册都要按新身份重取
      window.location.reload();
    } catch (err) {
      toastError(err);
      button.disabled = false;
    }
  }

  function usageCell(row: AdminUserRow): HTMLElement {
    if (!row.quota_bytes || row.quota_bytes <= 0) {
      return h("span", { style: { fontSize: "12px", color: "var(--faint)" }, text: "—" });
    }
    return h(
      "div",
      null,
      progressBar(row.used_bytes, row.quota_bytes),
      h("span", {
        style: { fontSize: "12px", color: "var(--muted)" },
        text: `${formatBytes(row.used_bytes)} / ${formatBytes(row.quota_bytes)}`,
      }),
    );
  }

  function actionsCell(row: AdminUserRow): HTMLElement {
    const box = h("div.flexline", { style: { gap: "6px", flexWrap: "wrap" } });
    box.append(
      h("button.btn.sm", {
        type: "button",
        text: "详情",
        on: { click: () => navigate(`/admin/users/${row.id}`) },
      }),
      h("button.btn.sm", {
        type: "button",
        text: "查看相册",
        on: { click: () => navigate(`/albums?uid=${encodeURIComponent(row.uid)}`) },
      }),
      // 免登录切换：不用知道对方密码，直接以他的身份进站，随时能切回来。
      // 访客账号也切不得——那是给一个不接受登录的账号发登录凭据（见 core/guest.rs）
      (() => {
        const allowed = canOperateAccount(row.uid) && !row.guest;
        const button = h("button.btn.sm", {
          type: "button",
          text: "切换登录",
          disabled: !allowed,
          title: row.guest
            ? "访客账号是系统账号，不能切换登录"
            : allowed
              ? `以 u_${row.uid} 的身份登录（不需要对方密码，可随时切回）`
              : "安装时创建的管理员，不能切进去用",
          on: {
            click: (event) => {
              if (allowed) void impersonate(row, event.currentTarget as HTMLButtonElement);
            },
          },
        });
        return button;
      })(),
    );

    if (!canOperateAccount(row.uid)) {
      box.append(
        h("span", {
          style: { fontSize: "11.5px", color: "var(--faint)" },
          text: "安装账号不可操作",
        }),
      );
      return box;
    }

    // 删除账号：安装账号（上面已返回）、访客账号、自己都不能删，其余可以。
    // 管理员账号也允许删（与「用户详情」页的危险操作一致），但要照着邮箱确认。
    if (!row.guest && row.id !== getState().user?.id) {
      const remove = h("button.btn.sm.dgr", {
        type: "button",
        text: "删除",
        title: "删除该账号及其相册、图片与会话",
      });
      remove.addEventListener("click", () => void deleteUser(row, remove));
      box.append(remove);
    }

    if (row.role === "admin") {
      // 管理员不显示处置按钮：把自己关在门外是最难恢复的运维事故。
      // 不用 title 藏说明——触屏上悬停不出来，说明必须看得见。
      box.append(
        h("span", {
          style: { fontSize: "11.5px", color: "var(--faint)" },
          text: "管理员不可处置",
        }),
      );
      return box;
    }

    if (row.status === "suspended" || row.status === "banned") {
      const label = row.status === "banned" ? "解封" : "恢复";
      const button = h("button.btn.sm", { type: "button", text: label });
      button.addEventListener("click", () => void restoreUser(row, button, label));
      box.append(button);
    } else {
      const button = h("button.btn.sm.wrn", { type: "button", text: "暂停" });
      button.addEventListener("click", () => openDispositionDialog([row], "suspend"));
      box.append(button);
    }
    // 访客账号是系统账号：不能登录，也就没有提权这回事。处置按钮留着——
    // 暂停或封禁它正是按住游客上传的手段（见 core/guest.rs）
    if (row.guest) {
      box.append(
        h("span", {
          style: { fontSize: "11.5px", color: "var(--faint)" },
          text: "访客账号不可登录",
        }),
      );
      return box;
    }

    // 提权放在最后：它比暂停少见，也不该挨着暂停误点
    const promote = h("button.btn.sm", { type: "button", text: "设为管理员" });
    promote.addEventListener("click", () => void promoteUser(row, promote));
    box.append(promote);
    return box;
  }

  /* ---------------- 批量与处置 ---------------- */

  function selectedRows(): AdminUserRow[] {
    return rows.filter((row) => selected.has(row.id));
  }

  function resetSelection(): void {
    selected.clear();
    updateSelectionBar();
  }

  function updateSelectionBar(): void {
    const count = selected.size;
    selectionText.textContent = `已选 ${count} 个用户`;
    // 选中数固定在最左，操作在右：管理员扫一眼就知道这批操作会作用到几个账号
    selectionBar.style.display = count > 0 ? "" : "none";
    quotaButton.disabled = count === 0;
    suspendButton.disabled = count === 0;
    clearButton.disabled = count === 0;
  }

  async function restoreUser(row: AdminUserRow, button: HTMLButtonElement, label: string): Promise<void> {
    const ok = await confirmDialog({
      title: `${label}账号`,
      message: `${row.email}（u_${row.uid}）恢复后可以重新登录并使用 API。是否继续？`,
      confirmText: label,
    });
    if (!ok) return;
    button.disabled = true;
    try {
      const result = await request<StatusResult>(`/admin/users/${row.id}/status`, {
        method: "POST",
        json: {
          action: "restore",
          reason: "管理员在用户列表手动恢复",
          duration_minutes: null,
          requirements: null,
          appeal_email: getState().site?.appeal_email ?? null,
          notify: true,
        },
      });
      toastOk(
        `已恢复 ${row.email} · 踢下线 ${result.revoked_sessions ?? 0} 个会话${
          result.mail_sent ? " · 通知邮件已发送" : ""
        }`,
      );
      await loadAll();
    } catch (err) {
      toastError(err);
      button.disabled = false;
    }
  }

  /**
   * 删除账号（列表里的快捷入口）。
   *
   * 与详情页同一套口径：不可逆，所以要求照着邮箱确认；服务端还会再校验一次。
   * 删完重新拉列表与统计，页码越界由 loadUsers 自己回退。
   */
  async function deleteUser(row: AdminUserRow, button: HTMLButtonElement): Promise<void> {
    if (row.id === getState().user?.id) {
      toastError(new Error("不能删除自己的账号"));
      return;
    }
    const ok = await confirmDialog({
      title: "删除账号",
      message:
        `删除后 ${row.email}（u_${row.uid}）的相册、图片、令牌与会话一起消失，` +
        "不再被别人引用的文件当场从磁盘上删掉，且无法撤销。" +
        "<br>照着输入邮箱确认。",
      confirmText: "删除账号",
      danger: true,
      requireText: row.email,
    });
    if (!ok) return;
    button.disabled = true;
    try {
      await request(`/admin/users/${row.id}?confirm=${encodeURIComponent(row.email)}`, {
        method: "DELETE",
      });
      toastOk(`${row.email} 及它的数据已删除`);
      selected.delete(row.id);
      await loadAll();
    } catch (err) {
      toastError(err);
      button.disabled = false;
    }
  }

  /** 批量设配额：弹窗收集参数，逐个调接口，最后汇总。 */
  function openBatchQuotaDialog(targets: AdminUserRow[]): void {
    const input = textInput({ placeholder: "例如 20（= 20 GB）或 500 MB" });
    const errorLine = h("div.tip", { style: { color: "var(--danger)" } });
    const progress = h("div.tip");
    const cancel = h("button.btn", { type: "button", text: "取消" });
    const confirm = h("button.btn.pri", { type: "button", text: `设置 ${targets.length} 个账号` });

    const handle = openModal({
      title: `批量设置配额（${targets.length} 个账号）`,
      body: [
        qtip(
          "warn",
          "会覆盖这些账号现有的配额",
          "设置后立即生效。若合计超过系统总容量，服务端会拒绝该账号并说明还差多少。",
        ),
        h(
          "div.fld",
          { style: { marginTop: "12px" } },
          h("label.f", { text: "每人配额" }),
          input,
          h("div.tip", { text: "不写单位时按 GB 计（填 20 就是 20 GB），也可写 500 MB。" }),
        ),
        h("ul", {
          style: { margin: "8px 0 0", paddingLeft: "20px", fontSize: "12px", color: "var(--muted)" },
          html: targets
            .slice(0, 12)
            .map((row) => `<li>${escapeText(row.email)}（u_${escapeText(row.uid)}）</li>`)
            .join("") + (targets.length > 12 ? `<li>…等共 ${targets.length} 个</li>` : ""),
        }),
        errorLine,
        progress,
      ],
      footer: [cancel, confirm],
    });

    cancel.addEventListener("click", () => handle.close());
    confirm.addEventListener("click", async () => {
      const bytes = parseQuota(input.value);
      if (bytes === null || bytes <= 0) {
        errorLine.textContent = "请填写配额：填「20」表示 20 GB，也可以写「500 MB」这种带单位的写法。";
        input.focus();
        return;
      }
      errorLine.textContent = "";
      confirm.disabled = true;
      cancel.disabled = true;
      const failures = await runBatch(
        targets,
        async (target) => {
          await request(`/admin/users/${target.id}/quota`, {
            method: "POST",
            json: { quota_bytes: bytes },
          });
        },
        (done, total) => {
          progress.textContent = `正在处理 ${done}/${total}…`;
        },
      );
      handle.close();
      // 批量做完就把勾选清掉：留着选中态，下一次点批量按钮很容易误伤同一批人
      resetSelection();
      await loadAll();
      showBatchResult({
        title: "批量设置配额",
        okCount: targets.length - failures.length,
        failures,
        extra: `目标配额：${formatBytes(bytes)} / 人`,
      });
    });

    input.focus();
  }

  /**
   * 处置确认框（暂停 / 封禁）：单个用户和批量共用同一套参数，
   * 少一套表单就少一处口径不一致。
   */
  /** 提权：管理员权限很大，先确认一次再发请求。 */
  async function promoteUser(row: AdminUserRow, button: HTMLButtonElement): Promise<void> {
    const ok = await confirmDialog({
      title: "设为管理员",
      message:
        `把 ${row.email}（u_${row.uid}）设为管理员？` +
        "管理员可以看到所有用户的内容与统计、改系统设置、处置账号，且不能被暂停或封禁。",
      confirmText: "设为管理员",
    });
    if (!ok) return;
    button.disabled = true;
    try {
      await request(`/admin/users/${row.id}`, { method: "PATCH", json: { role: "admin" } });
      toastOk(`${row.email} 已设为管理员`);
      await loadUsers();
    } catch (err) {
      toastError(err);
      button.disabled = false;
    }
  }

  /** 后台直接建号：邮箱直接激活，密码由管理员告诉对方，首次登录后自行修改。 */
  function openCreateUserDialog(): void {
    const email = textInput({ value: "", mono: true, placeholder: "user@example.com" });
    const password = passwordControl({ placeholder: "至少 10 位，要能通过强度校验" });
    const role = select([
      { value: "user", label: "普通用户" },
      { value: "admin", label: "管理员" },
    ]);
    const errorLine = h("div", { style: { color: "var(--danger)", fontSize: "12.5px", minHeight: "16px" } });
    let submitting = false;

    const cancel = h("button.btn", { type: "button", text: "取消" });
    const confirm = h("button.btn.pri", { type: "button", text: "创建" });
    const handle = openModal({
      title: "新建用户",
      body: h(
        "div",
        null,
        field("邮箱", email, "创建后即为已验证状态，不会再发验证邮件"),
        field("初始密码", password.node, "告诉对方后请提醒尽快在「安全」页改掉"),
        field("角色", role, "管理员权限很大：能看全部用户的内容与统计、改系统设置"),
        errorLine,
      ),
      footer: [cancel, confirm],
    });
    cancel.addEventListener("click", () => handle.close());
    confirm.addEventListener("click", async () => {
      if (submitting) return;
      const mail = email.value.trim();
      if (!mail || !password.input.value) {
        errorLine.textContent = "邮箱与密码都要填。";
        return;
      }
      submitting = true;
      confirm.disabled = true;
      confirm.textContent = "创建中…";
      try {
        await request("/admin/users", {
          method: "POST",
          json: { email: mail, password: password.input.value, role: role.value },
        });
        toastOk(`已创建 ${mail}`);
        handle.close();
        await loadUsers();
      } catch (err) {
        errorLine.textContent = err instanceof ApiError ? err.message : String(err);
      } finally {
        submitting = false;
        confirm.disabled = false;
        confirm.textContent = "创建";
      }
    });
  }

  function openDispositionDialog(targets: AdminUserRow[], preset: "suspend" | "ban"): void {
    let kind: "suspend" | "ban" = preset;
    const disposables = targets.filter((row) => row.role !== "admin");
    const skipped = targets.length - disposables.length;

    const makeKindChip = (value: "suspend" | "ban", label: string): HTMLButtonElement => {
      const chip = h("button.chip", {
        type: "button",
        class: kind === value ? "on" : "",
        text: label,
      });
      chip.addEventListener("click", () => {
        kind = value;
        for (const other of kindChips.children) other.classList.toggle("on", other === chip);
        refreshConfirmLabel();
      });
      return chip;
    };
    const kindChips = h(
      "div.flexline",
      null,
      makeKindChip("suspend", "暂停使用（禁用）"),
      makeKindChip("ban", "封禁账号"),
    );

    // 生效时长自己填，留空就是永久。单位给小时与天两档
    const durationValue = textInput({
      type: "number",
      mono: true,
      placeholder: "留空 = 永久",
      min: "1",
    });
    const durationUnit = select(
      [
        { value: "days", label: "天" },
        { value: "hours", label: "小时" },
      ],
      "days",
    );
    /** 手填时长换算成分钟；空、0、非数字一律按「永久」处理。 */
    const durationMinutes = (): number | null => {
      const value = Number(durationValue.value.trim());
      if (!Number.isFinite(value) || value <= 0) return null;
      return Math.max(1, Math.round(value * (durationUnit.value === "hours" ? 60 : 1440)));
    };
    const appeal = textInput({ value: getState().site?.appeal_email ?? "", mono: true });
    const reason = textarea({
      rows: 3,
      placeholder: "例如：上传违规内容，违反服务条款第 3 条",
    });
    const requirements = textarea({
      rows: 4,
      placeholder: "写进通知邮件的「如何解封」部分，例如：本人手持身份证的照片 + 手写承诺书…",
    });
    let notify = true;
    const notifyRow = switchRow(
      "发送通知邮件",
      `由站点发信邮箱发出，收件人：${
        targets.length === 1 ? targets[0].email : `${disposables.length} 个被处置的账号`
      }`,
      true,
      (next) => {
        notify = next;
      },
    );

    // 访客账号单独一套：只填时长与备注。它登不进来、邮箱是保留域（发出去必退信），
    // 申诉邮箱、解封要求、通知邮件这几项对它没有意义
    const guestTarget = targets.length === 1 && targets[0].guest === true;
    if (guestTarget) notify = false;
    const guestNote = h("div.tip", {
      text: "访客账号不发通知邮件：它的邮箱收不到信，账号本身也登不进来。暂停它就是按住游客上传，恢复即重新开放。",
    });

    const errorLine = h("div.tip", { style: { color: "var(--danger)" } });
    const progress = h("div.tip");
    const cancel = h("button.btn", { type: "button", text: "取消" });
    const confirm = h("button.btn", { type: "button" });

    function refreshConfirmLabel(): void {
      const ban = kind === "ban";
      confirm.textContent = ban
        ? `确认封禁${targets.length > 1 ? ` ${disposables.length} 个账号` : ""}`
        : `确认暂停${targets.length > 1 ? ` ${disposables.length} 个账号` : ""}`;
      confirm.className = `btn ${ban ? "dgr" : "wrn"}`;
    }
    refreshConfirmLabel();

    const body: Node[] = [
      h("div.fld", null, h("label.f", { text: "处置类型" }), kindChips),
      h(
        "div.row",
        null,
        h(
          "div.fld",
          null,
          h("label.f", { text: "生效时长" }),
          h(
            "div.row",
            { style: { margin: "0", alignItems: "center" } },
            durationValue,
            h("div", { style: { flex: "0 0 90px" } }, durationUnit),
          ),
          h("div.tip", { text: "留空表示永久，只能手动解封；填了就到期自动恢复。" }),
        ),
        ...(guestTarget
          ? []
          : [
              h(
                "div.fld",
                null,
                h("label.f", { text: "申诉接收邮箱" }),
                appeal,
                h("div.tip", { text: "默认取注册发信邮箱，可改" }),
              ),
            ]),
      ),
      h(
        "div.fld",
        null,
        h("label.f", { text: guestTarget ? "备注（写进账号历史）" : "处置原因（展示给用户，也写进邮件）" }),
        reason,
      ),
      ...(guestTarget
        ? [guestNote]
        : [field("解封要求（写进通知邮件的「如何解封」部分）", requirements), notifyRow]),
      h(
        "div",
        { style: { marginTop: "12px" } },
        qtip(
          "warn",
          null,
          "确认后立刻生效：所有会话踢下线，API Token 进入暂停；解封时自动恢复，不用重新签发。",
        ),
      ),
    ];
    if (skipped > 0) {
      body.push(
        h("div.tip", { text: `已跳过 ${skipped} 个管理员账号：管理员不能被暂停或封禁。` }),
      );
    }
    body.push(errorLine, progress);

    const handle = openModal({
      title:
        targets.length === 1
          ? `处置 ${accountLabel(targets[0])}`
          : `批量处置 ${disposables.length} 个账号`,
      body,
      footer: [cancel, confirm],
    });

    cancel.addEventListener("click", () => handle.close());
    confirm.addEventListener("click", async () => {
      if (disposables.length === 0) {
        errorLine.textContent = "没有可处置的账号：管理员账号不能被暂停或封禁。";
        return;
      }
      if (!reason.value.trim()) {
        errorLine.textContent = guestTarget
          ? "请填写备注：它会写进账号历史。"
          : "请填写处置原因：它会展示给用户，也会写进通知邮件。";
        reason.focus();
        return;
      }
      errorLine.textContent = "";
      confirm.disabled = true;
      cancel.disabled = true;

      // 先算一次：批量里每个人用的都是同一个时长，请求与结果文案共用
      const minutes = durationMinutes();
      let sessions = 0;
      let mails = 0;
      const failures = await runBatch(
        disposables,
        async (target) => {
          const result = await request<StatusResult>(`/admin/users/${target.id}/status`, {
            method: "POST",
            json: {
              action: kind,
              reason: reason.value.trim(),
              duration_minutes: minutes,
              requirements: requirements.value.trim() || null,
              appeal_email: appeal.value.trim() || null,
              notify,
            },
          });
          sessions += result.revoked_sessions ?? 0;
          if (result.mail_sent) mails += 1;
        },
        (done, total) => {
          progress.textContent = `正在处理 ${done}/${total}…`;
        },
      );

      handle.close();
      resetSelection();
      await loadAll();
      // 会话数与邮件状态是管理员最关心的两个副作用，用提示条再说一遍
      toastOk(
        `已踢下线 ${sessions} 个会话、通知邮件${mails > 0 ? `已发送 ${mails} 封` : "未发送"}`,
      );
      showBatchResult({
        title: kind === "ban" ? "批量封禁结果" : "批量暂停结果",
        okCount: disposables.length - failures.length,
        failures,
        skipped,
        extra: `共踢下线 ${sessions} 个会话${
          minutes ? ` · 生效 ${describeMinutes(minutes)}` : " · 永久生效"
        }`,
      });
    });

    reason.focus();
  }

  /* ---------------- 状态块 ---------------- */

  function skeletonCards(count: number): HTMLElement[] {
    return Array.from({ length: count }, () =>
      h("div.card", null, h("div.skel", { style: { height: "38px" } })),
    );
  }

  function skeletonRows(count: number): HTMLElement {
    return h(
      "div",
      null,
      ...Array.from({ length: count }, () =>
        h("div.skel", { style: { height: "52px", marginBottom: "8px" } }),
      ),
    );
  }

  function errorBlock(err: unknown, retry: () => void): HTMLElement {
    return h(
      "div.card",
      null,
      qtip("err", "用户列表加载失败", errorText(err)),
      h(
        "div",
        { style: { marginTop: "10px" } },
        h("button.btn", { type: "button", text: "重试", on: { click: retry } }),
      ),
    );
  }

  /**
   * 地址栏同步用 replaceState 而不是路由的 updateQuery：
   * updateQuery 会触发整页重渲染，搜索框会被重建，光标与输入内容都会丢。
   */
  function syncUrl(): void {
    const params = new URLSearchParams();
    if (filters.query) params.set("q", filters.query);
    if (filters.role) params.set("role", filters.role);
    if (filters.status) params.set("status", filters.status);
    if (filters.page > 1) params.set("page", String(filters.page));
    const query = params.toString();
    history.replaceState(null, "", `#/admin/users${query ? `?${query}` : ""}`);
  }

  await loadAll();
}

/* ------------------------------------------------------------------ */
/* 批量执行与汇总                                                      */
/* ------------------------------------------------------------------ */

/**
 * 顺序执行批量操作。
 * 刻意不并发：几个请求同时打过去，失败顺序、限流触发都会变得难以复现；
 * 管理后台的批量操作数量有限，顺序执行更可控，也更容易显示进度。
 */
async function runBatch(
  targets: AdminUserRow[],
  action: (target: AdminUserRow) => Promise<void>,
  onProgress: (done: number, total: number) => void,
): Promise<BatchFailure[]> {
  const failures: BatchFailure[] = [];
  for (let index = 0; index < targets.length; index += 1) {
    onProgress(index, targets.length);
    const target = targets[index];
    try {
      await action(target);
    } catch (err) {
      failures.push({ who: `${target.email}（u_${target.uid}）`, message: errorText(err) });
    }
  }
  onProgress(targets.length, targets.length);
  return failures;
}

/** 批量结果汇总：成功了几个、失败的是谁、为什么失败。 */
function showBatchResult(options: {
  title: string;
  okCount: number;
  failures: BatchFailure[];
  skipped?: number;
  extra?: string;
}): void {
  const body: Node[] = [
    options.failures.length === 0
      ? qtip("ok", null, `全部成功：${options.okCount} 个账号已处理。`)
      : qtip("warn", null, `成功 ${options.okCount} 个，失败 ${options.failures.length} 个。`),
  ];
  if (options.extra) body.push(h("div.tip", { text: options.extra }));
  if (options.skipped) {
    body.push(
      h("div.tip", { text: `跳过 ${options.skipped} 个管理员账号：管理员不能被暂停或封禁。` }),
    );
  }
  if (options.failures.length > 0) {
    body.push(
      h(
        "div.table-wrap",
        { style: { marginTop: "12px" } },
        h(
          "table.responsive",
          null,
          h(
            "thead",
            null,
            h("tr", null, h("th", { text: "账号" }), h("th", { text: "失败原因" })),
          ),
          h(
            "tbody",
            null,
            ...options.failures.map((failure) =>
              h(
                "tr",
                null,
                h("td", { "data-label": "账号", text: failure.who }),
                h("td", { "data-label": "失败原因", text: failure.message }),
              ),
            ),
          ),
        ),
      ),
    );
  }

  const handle = openModal({
    title: options.title,
    body,
    footer: [h("button.btn.pri", { type: "button", text: "知道了", on: { click: () => handle.close() } })],
  });
}

/* ------------------------------------------------------------------ */
/* 小工具                                                              */
/* ------------------------------------------------------------------ */

/** 配额输入解析：带单位按单位算；不带单位一律按 GB 算（口径与详情页一致）。 */
function parseQuota(input: string): number | null {
  const text = input.trim();
  if (!text) return null;
  const match = text.match(/^([\d.]+)\s*(B|KB|MB|GB|TB)?$/i);
  if (!match) return null;
  const value = Number(match[1]);
  if (!Number.isFinite(value) || value <= 0) return null;
  const units = ["B", "KB", "MB", "GB", "TB"];
  const index = units.indexOf((match[2] ?? "GB").toUpperCase());
  return Math.round(value * Math.pow(1024, index < 0 ? 0 : index));
}

function errorText(err: unknown): string {
  if (err instanceof ApiError) return err.detail ? `${err.message}（${err.detail}）` : err.message;
  return err instanceof Error ? err.message : String(err);
}

/** 只给弹窗里的静态列表用，避免邮箱里的特殊字符破坏结构。 */
function escapeText(input: string): string {
  return input
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}
