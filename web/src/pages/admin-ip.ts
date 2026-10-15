/**
 * IP 黑名单（`#/admin/ip`）。
 *
 * 手动规则与风控自动加的临时规则在同一张表里，但能做的事不一样：
 *   - 手动规则可以编辑、移除；
 *   - 自动规则是风控按阈值加的，只能「提前解除」，不该被顺手改掉理由。
 *
 * 注意一处契约缺口：后端只有「新建 / 删除 / 改策略」，没有单条修改接口，
 * 所以「编辑」是「先删再建」。这件事必须写在界面上，
 * 否则管理员会以为命中次数为什么突然归零。
 *
 * 白名单优先于黑名单：同一条地址同时命中两种规则时放行赢，页面底部有说明。
 */

import { ApiError, request } from "../core/api";
import { h, mount } from "../core/dom";
import {
  VISIBILITY_SCOPE_LABEL,
  formatDateTime,
  formatNumber,
  fromLocalInput,
  toLocalInput,
} from "../core/format";
import { confirmDialog, openModal } from "../core/modal";
import type { PageContext } from "../core/router";
import { toastError, toastOk } from "../core/toast";
import type { IpAdmin } from "../core/types";
import {
  badge,
  card,
  cardHead,
  cardNode,
  emptyState,
  field,
  pageHead,
  qtip,
  select,
  spinner,
  switchRow,
  textInput,
} from "../ui/widgets";

type IpRuleRow = IpAdmin["rules"][number];

const SCOPES = [
  { value: "all", label: "全站拦截" },
  { value: "register", label: "仅拦截注册" },
  { value: "login", label: "仅拦截登录" },
  { value: "upload", label: "仅拦截上传" },
];

/** 策略里可编辑的那几个值，保存时只读这一份。 */
interface PolicyDraft {
  enabled: boolean;
  trustProxy: boolean;
  autoBan: boolean;
  proxyHops: HTMLInputElement;
  failThreshold: HTMLInputElement;
  failWindow: HTMLInputElement;
  banMinutes: HTMLInputElement;
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

/** 本地格式校验：只挡手滑（少一位、多空格），真正的判定仍在服务端。 */
function looksLikeCidr(text: string): boolean {
  const value = text.trim();
  if (!value || /\s/.test(value)) return false;
  const [address, mask] = value.split("/");
  if (mask !== undefined && !/^\d{1,3}$/.test(mask)) return false;
  if (address.includes(":")) return /^[0-9a-fA-F:]+$/.test(address); // IPv6
  return /^\d{1,3}(\.\d{1,3}){3}$/.test(address); // IPv4
}

function scopeBadge(row: IpRuleRow): HTMLElement {
  if (row.is_auto) return badge("自动临时", "mute");
  if (row.kind === "allow") return badge("白名单", "ok");
  return row.scope === "all"
    ? badge(row.scope_label || "全站", "err")
    : badge(row.scope_label || VISIBILITY_SCOPE_LABEL[row.scope] || row.scope, "warn");
}

/** 文本框回退：纯文本单元格用文本节点最省事。 */
function txt(value: string): Node {
  return document.createTextNode(value);
}

export async function render(ctx: PageContext): Promise<void> {
  const root = h("div.pad");
  mount(ctx.outlet, root);
  root.append(loadingBlock("正在读取 IP 规则…"));
  const reload = (): void => void render(ctx);

  try {
    const data = await request<IpAdmin>("/admin/ip");
    mount(root, ...view(reload, data));
  } catch (err) {
    mount(root, errorBlock(err, reload));
  }
}

function view(reload: () => void, data: IpAdmin): Node[] {
  const policy: PolicyDraft = {
    enabled: data.policy.enabled,
    trustProxy: data.policy.trust_proxy,
    autoBan: data.policy.auto_ban,
    proxyHops: textInput({ value: String(data.policy.proxy_hops), type: "number", mono: true }),
    failThreshold: textInput({ value: String(data.policy.fail_threshold), type: "number", mono: true }),
    failWindow: textInput({ value: String(data.policy.fail_window_minutes), type: "number", mono: true }),
    banMinutes: textInput({ value: String(data.policy.ban_minutes), type: "number", mono: true }),
  };

  return [
    pageHead("IP 黑名单", "封掉恶意来源。支持单个 IP 和 CIDR 网段，命中后按配置的范围拦截。"),
    h("div.row", { style: { marginBottom: "14px" } }, addRuleCard(reload), policyCard(reload, policy)),
    rulesCard(reload, data),
    h(
      "div",
      { style: { marginTop: "14px" } },
      qtip(
        "info",
        null,
        "建议同时配白名单：把自己或公司的固定 IP 加白，被自动风控误伤时也进得来。两者冲突时白名单优先。",
      ),
    ),
  ];
}

/** 添加规则：IP/CIDR、范围、备注、到期时间（留空永久）。 */
function addRuleCard(reload: () => void): HTMLElement {
  const cidrInput = textInput({ placeholder: "203.0.113.42 或 203.0.113.0/24", mono: true });
  const scopeSelect = select(SCOPES, "all");
  const noteInput = textInput({ placeholder: "为什么拉黑，方便以后判断要不要解封" });
  const expiresInput = textInput({ type: "datetime-local" });
  const submit = h("button.btn.pri", { type: "button", text: "加入黑名单" });

  submit.addEventListener("click", async () => {
    const cidr = cidrInput.value.trim();
    if (!cidr) {
      toastError("请填写要拦截的 IP 或网段");
      return;
    }
    if (!looksLikeCidr(cidr)) {
      toastError("看起来不是合法的 IP 或 CIDR，例如 203.0.113.42 或 2001:db8::/32");
      return;
    }
    submit.disabled = true;
    submit.textContent = "提交中…";
    try {
      await request("/admin/ip/rules", {
        method: "POST",
        json: {
          cidr,
          kind: "deny",
          scope: scopeSelect.value,
          note: noteInput.value.trim() || null,
          // 留空＝永久
          expires_at: fromLocalInput(expiresInput.value),
        },
      });
      toastOk(`${cidr} 已加入黑名单`);
      reload();
    } catch (err) {
      toastError(err);
      submit.disabled = false;
      submit.textContent = "加入黑名单";
    }
  });

  return cardNode(
    cardHead("添加规则"),
    h("div.row", null, field("IP 或 CIDR 网段", cidrInput), field("拦截范围", scopeSelect)),
    field("备注", noteInput),
    h(
      "div.row",
      null,
      field(
        "生效时间",
        textInput({ value: formatDateTime(new Date().toISOString()), readonly: true, mono: true }),
        "提交后立即生效",
      ),
      field("到期时间", expiresInput, "留空表示永久"),
    ),
    submit,
    h("div.tip", {
      text: "支持 IPv4 与 IPv6 网段，例如 203.0.113.0/24、2001:db8::/32。命中即返回 403，不进业务逻辑。",
    }),
  );
}

/** 策略设置：总开关、代理信任、自动临时封禁。 */
function policyCard(reload: () => void, policy: PolicyDraft): HTMLElement {
  const submit = h("button.btn.sm.pri", { type: "button", text: "保存策略" });
  submit.addEventListener("click", async () => {
    submit.disabled = true;
    submit.textContent = "保存中…";
    try {
      await request("/admin/ip/policy", {
        method: "PUT",
        json: {
          enabled: policy.enabled,
          trust_proxy: policy.trustProxy,
          proxy_hops: intOf(policy.proxyHops),
          auto_ban: policy.autoBan,
          fail_threshold: intOf(policy.failThreshold),
          fail_window_minutes: intOf(policy.failWindow),
          ban_minutes: intOf(policy.banMinutes),
        },
      });
      toastOk("策略已保存");
      reload();
    } catch (err) {
      toastError(err);
      submit.disabled = false;
      submit.textContent = "保存策略";
    }
  });

  return cardNode(
    cardHead("策略设置"),
    switchRow("启用黑名单", "总开关，关掉后所有规则都不生效", policy.enabled, (next) => {
      policy.enabled = next;
    }),
    switchRow("信任反向代理头", "读取 X-Forwarded-For 判断真实 IP，容器部署必须开", policy.trustProxy, (next) => {
      policy.trustProxy = next;
    }),
    field(
      "可信代理层数",
      policy.proxyHops,
      "只信任最靠近服务端的一层，防止客户端伪造 X-Forwarded-For 绕过黑名单",
    ),
    switchRow("自动临时封禁", "同 IP 短时间内登录失败或上传失败过多，自动临时封禁", policy.autoBan, (next) => {
      policy.autoBan = next;
    }),
    h(
      "div.row",
      { style: { marginTop: "10px" } },
      field("失败次数阈值", policy.failThreshold),
      field("统计窗口（分钟）", policy.failWindow),
      field("封禁时长（分钟）", policy.banMinutes),
    ),
    h("div", { style: { marginTop: "12px" } }, submit),
  );
}

/** 已生效规则表：搜索过滤 + 按是否为自动规则给不同操作。 */
function rulesCard(reload: () => void, data: IpAdmin): HTMLElement {
  const all = data.rules ?? [];
  const tbody = h("tbody");
  const search = textInput({ placeholder: "搜索 IP 或备注…" });

  const fill = (keyword: string): void => {
    const key = keyword.trim().toLowerCase();
    const rows = all.filter(
      (row) =>
        !key ||
        row.cidr.toLowerCase().includes(key) ||
        (row.note ?? "").toLowerCase().includes(key),
    );
    if (rows.length === 0) {
      mount(tbody, h("tr", null, h("td", { colspan: 6 }, emptyState("🛡", "没有匹配的规则。"))));
      return;
    }
    mount(tbody, ...rows.map((row) => ruleRow(reload, row)));
  };
  fill("");
  search.addEventListener("input", () => fill(search.value));

  const head = h(
    "div.flexline",
    { style: { padding: "12px 14px", borderBottom: "1px solid var(--border)" } },
    h("b", { style: { fontSize: "13.5px" }, text: "已生效规则" }),
    badge(`${formatNumber(data.count ?? all.length)} 条`, "mute"),
    h("span.spacer"),
    h("div", { style: { width: "200px" } }, search),
  );

  return cardNode(
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
            h("th", { text: "IP / 网段" }),
            h("th", { text: "范围" }),
            h("th", { text: "备注" }),
            h("th", { text: "命中次数" }),
            h("th", { text: "添加时间" }),
            h("th", { text: "操作" }),
          ),
        ),
        tbody,
      ),
    ),
  );
}

function ruleRow(reload: () => void, row: IpRuleRow): HTMLElement {
  const noteCell = h(
    "div",
    null,
    txt(row.note ?? "—"),
    row.expires_at
      ? h("div", { style: { fontSize: "11.5px", color: "var(--muted)" }, text: `到期 ${formatDateTime(row.expires_at)}` })
      : null,
  );

  const actions = h(
    "div.act",
    null,
    row.is_auto
      ? h("button.btn.sm.dgr", {
          type: "button",
          text: "提前解除",
          on: { click: () => void liftRule(reload, row) },
        })
      : h("button.btn.sm", {
          type: "button",
          text: "编辑",
          on: { click: () => openRuleModal(reload, row) },
        }),
    row.is_auto
      ? null
      : h("button.btn.sm.dgr", {
          type: "button",
          text: "移除",
          on: { click: () => void removeRule(reload, row) },
        }),
  );

  return h(
    "tr",
    null,
    h("td", { "data-label": "IP / 网段" }, h("span.mono", { text: row.cidr })),
    h("td", { "data-label": "范围" }, scopeBadge(row)),
    h("td", { "data-label": "备注" }, noteCell),
    h("td", { "data-label": "命中次数" }, txt(formatNumber(row.hits))),
    h("td", { "data-label": "添加时间" }, txt(formatDateTime(row.created_at))),
    h("td", { "data-label": "操作" }, actions),
  );
}

/** 移除手动规则。 */
async function removeRule(reload: () => void, row: IpRuleRow): Promise<void> {
  const ok = await confirmDialog({
    title: "移除这条规则",
    message: `${row.cidr} 会立刻停止拦截（${row.note ? row.note : "无备注"}）。已经拦下的请求不会追溯。`,
    confirmText: "移除",
    danger: true,
  });
  if (!ok) return;
  try {
    await request(`/admin/ip/rules/${row.id}`, { method: "DELETE" });
    toastOk("规则已移除");
    reload();
  } catch (err) {
    toastError(err);
  }
}

/** 提前解除自动临时规则：也是删除，但说法要按「解除」来。 */
async function liftRule(reload: () => void, row: IpRuleRow): Promise<void> {
  const ok = await confirmDialog({
    title: "提前解除封禁",
    message: `${row.cidr} 是风控自动加的临时规则（${row.note ?? "自动封禁"}）。解除后这个来源可以立刻访问，如果它继续触发阈值会被再次封禁。`,
    confirmText: "提前解除",
  });
  if (!ok) return;
  try {
    await request(`/admin/ip/rules/${row.id}`, { method: "DELETE" });
    toastOk("已提前解除");
    reload();
  } catch (err) {
    toastError(err);
  }
}

/**
 * 编辑手动规则。
 * 契约里没有单条修改接口，所以保存 = 删除旧的 + 按新值新建，
 * 代价是规则 id 与命中计数会归零——这一点必须提前说清。
 */
function openRuleModal(reload: () => void, row: IpRuleRow): void {
  const cidrInput = textInput({ value: row.cidr, mono: true });
  const scopeSelect = select(SCOPES, row.scope);
  const noteInput = textInput({ value: row.note ?? "" });
  const expiresInput = textInput({ type: "datetime-local", value: toLocalInput(row.expires_at) });

  const submit = h("button.btn.pri", { type: "button", text: "保存" });
  const handle = openModal({
    title: `编辑规则 ${row.cidr}`,
    body: [
      h(
        "div",
        { style: { marginBottom: "12px" } },
        qtip(
          "warn",
          "保存方式",
          "没有单条修改接口：保存会先删掉这条再按新值重建，编号会变、命中次数归零。想保留计数就别动它。",
        ),
      ),
      field("IP 或 CIDR 网段", cidrInput),
      field("拦截范围", scopeSelect),
      field("备注", noteInput),
      field("到期时间", expiresInput, "留空表示永久"),
    ],
    footer: [h("button.btn", { type: "button", text: "取消", on: { click: () => handle.close() } }), submit],
  });

  submit.addEventListener("click", async () => {
    const cidr = cidrInput.value.trim();
    if (!looksLikeCidr(cidr)) {
      toastError("看起来不是合法的 IP 或 CIDR");
      return;
    }
    submit.disabled = true;
    submit.textContent = "保存中…";
    try {
      await request(`/admin/ip/rules/${row.id}`, { method: "DELETE" });
      await request("/admin/ip/rules", {
        method: "POST",
        json: {
          cidr,
          kind: "deny",
          scope: scopeSelect.value,
          note: noteInput.value.trim() || null,
          expires_at: fromLocalInput(expiresInput.value),
        },
      });
      handle.close();
      toastOk("规则已更新（命中计数从 0 重新开始）");
      reload();
    } catch (err) {
      toastError(err);
      submit.disabled = false;
      submit.textContent = "保存";
    }
  });
}
