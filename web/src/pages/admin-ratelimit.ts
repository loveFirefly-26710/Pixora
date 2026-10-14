/**
 * 频率限制（`#/admin/ratelimit`）。
 *
 * 三套额度分开配：API 调用、图片上传、并发与批次；全局默认加用户级覆盖。
 * 超限返回 429 + Retry-After，客户端据此退避。
 *
 * 两处必须说清楚，写在界面上而不是只写在文档里：
 *   - 「临时封禁」只掐 API，不影响该用户网页登录，跟「账号暂停」不是一回事；
 *   - 实时用量每 5 秒自己刷一次，页面切走要停掉轮询（不然会一直在后台打接口）。
 */

import { ApiError, request, requestPage } from "../core/api";
import { h, mount } from "../core/dom";
import { formatBytes, formatDateTime, formatNumber, parseSize, toGigabytes } from "../core/format";
import { confirmDialog, openModal } from "../core/modal";
import type { PageContext } from "../core/router";
import { toastError, toastOk } from "../core/toast";
import type { PolicyFields, RateLimitAdmin } from "../core/types";
import {
  badge,
  card,
  cardHead,
  cardNode,
  emptyState,
  field,
  note,
  pageHead,
  pagination,
  progressBar,
  qtip,
  responsiveTable,
  spinner,
  switchRow,
  textInput,
} from "../ui/widgets";

type UserRow = RateLimitAdmin["users"][number];
type RealtimeRow = RateLimitAdmin["realtime"][number];

/** 限流事件分页行。契约只说「来自审计日志」，字段按审计表取，缺了也不该崩。 */
interface RateLogRow {
  id?: number;
  uid?: string | null;
  email?: string | null;
  action?: string | null;
  target?: string | null;
  detail?: unknown;
  ip?: string | null;
  created_at?: string | null;
}

/** 可编辑的值集中在这一份里，保存时只读它。 */
interface RateDraft {
  apiPerMinute: HTMLInputElement;
  apiPerHour: HTMLInputElement;
  apiBurst: HTMLInputElement;
  apiReadPerMinute: HTMLInputElement;
  uploadPerMinute: HTMLInputElement;
  uploadPerHour: HTMLInputElement;
  uploadPerDay: HTMLInputElement;
  uploadBytesPerDay: HTMLInputElement;
  maxConcurrent: HTMLInputElement;
  maxBatch: HTMLInputElement;
  maxFileSize: HTMLInputElement;
  queueDepth: HTMLInputElement;
  onBreach: { respond: boolean; audit: boolean; tempBan: boolean };
  breachThreshold: HTMLInputElement;
  breachWindow: HTMLInputElement;
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

/** 容量输入：不带单位按 GB 解释，也可以写 20 MB 这种。 */
function sizeOf(input: HTMLInputElement, unit = "GB"): number {
  const text = input.value.trim();
  if (!text) return 0;
  return parseSize(/[a-z]$/i.test(text) ? text : `${text} ${unit}`) ?? 0;
}

function toMegabytes(bytes: number): string {
  return `${Math.round(bytes / 1024 / 1024)} MB`;
}

/** 表格单元格要 Node，纯文本用文本节点最省事（不额外套一层 span）。 */
function txt(value: string): Node {
  return document.createTextNode(value);
}

/** 带单位后缀的数值输入。 */
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
  root.append(loadingBlock("正在读取频率限制配置…"));
  const reload = (): void => void render(ctx);

  try {
    const data = await request<RateLimitAdmin>("/admin/ratelimit");
    mount(root, ...view(reload, root, data));
    // 带 ?tab=logs 进来直接打开日志，便于从告警邮件点回来
    if (ctx.query.get("tab") === "logs") void openLogs(1, "");
  } catch (err) {
    mount(root, errorBlock(err, reload));
  }
}

function view(reload: () => void, root: HTMLElement, data: RateLimitAdmin): Node[] {
  const global = data.global;
  const draft: RateDraft = {
    apiPerMinute: textInput({ value: String(global.api_per_minute), type: "number", mono: true }),
    apiPerHour: textInput({ value: String(global.api_per_hour), type: "number", mono: true }),
    apiBurst: textInput({ value: String(global.api_burst), type: "number", mono: true }),
    apiReadPerMinute: textInput({ value: String(global.api_read_per_minute), type: "number", mono: true }),
    uploadPerMinute: textInput({ value: String(global.upload_per_minute), type: "number", mono: true }),
    uploadPerHour: textInput({ value: String(global.upload_per_hour), type: "number", mono: true }),
    uploadPerDay: textInput({ value: String(global.upload_per_day), type: "number", mono: true }),
    uploadBytesPerDay: textInput({ value: `${toGigabytes(global.upload_bytes_per_day)} GB`, mono: true }),
    maxConcurrent: textInput({ value: String(global.max_concurrent_uploads), type: "number", mono: true }),
    maxBatch: textInput({ value: String(global.max_batch_size), type: "number", mono: true }),
    maxFileSize: textInput({ value: toMegabytes(global.max_file_size), mono: true }),
    queueDepth: textInput({ value: String(data.queue_depth.threshold), type: "number", mono: true }),
    onBreach: {
      respond: data.on_breach.respond,
      audit: data.on_breach.audit,
      tempBan: data.on_breach.temp_ban,
    },
    breachThreshold: textInput({ value: String(data.on_breach.threshold), type: "number", mono: true }),
    breachWindow: textInput({ value: String(data.on_breach.threshold_minutes), type: "number", mono: true }),
    banMinutes: textInput({ value: String(data.on_breach.ban_minutes), type: "number", mono: true }),
  };

  return [
    pageHead("频率限制", "全局默认策略加用户级覆盖。API 调用和图片上传分开限，超限返回 429 并带 Retry-After。"),
    h("div.row", { style: { marginBottom: "14px" } }, apiCard(draft), uploadCard(draft)),
    h("div.row", { style: { marginBottom: "14px" } }, concurrencyCard(draft, data), breachCard(draft)),
    overridesCard(reload, data),
    realtimeCard(root, data),
    h(
      "div.flexline",
      { style: { justifyContent: "flex-end", marginTop: "16px" } },
      h("button.btn", {
        type: "button",
        text: "查看超限日志",
        on: { click: () => void openLogs(1, "") },
      }),
      h("button.btn.pri", {
        type: "button",
        text: "保存设置",
        on: { click: (event) => void save(reload, draft, event.target as HTMLButtonElement) },
      }),
    ),
    note(
      "w",
      "计数实现要提前定：单实例用进程内令牌桶加滑动窗口，够快也够省内存。多实例部署时计数必须共享，走数据库计数表或可选接 Redis，否则每个实例各算各的，实际额度会被放大成 N 倍。",
    ),
  ];
}

/** API 调用频率（全局默认）。 */
function apiCard(draft: RateDraft): HTMLElement {
  return cardNode(
    cardHead("API 调用频率", badge("全局默认", "mute")),
    h(
      "div.row",
      null,
      field("次 / 分钟", draft.apiPerMinute),
      field("次 / 小时", draft.apiPerHour),
    ),
    h(
      "div.row",
      null,
      field("瞬时并发上限", draft.apiBurst, "同一秒内允许的在途请求数"),
      field("读接口单独限额", unitInput(draft.apiReadPerMinute, "次 / 分"), "列表、搜索这类读操作可以放宽"),
    ),
    h("div.tip", { text: "用户未单独设置时继承这套值；按 Token 维度计数，同一用户多个 Token 共享额度。" }),
  );
}

/** 图片上传频率（全局默认）。 */
function uploadCard(draft: RateDraft): HTMLElement {
  return cardNode(
    cardHead("图片上传频率", badge("全局默认", "mute")),
    h(
      "div.row",
      null,
      field("张 / 分钟", draft.uploadPerMinute),
      field("张 / 小时", draft.uploadPerHour),
    ),
    h(
      "div.row",
      null,
      field("张 / 天", draft.uploadPerDay),
      field("单日上传流量", draft.uploadBytesPerDay, "不带单位按 GB 计算，也可以写 500 MB"),
    ),
    h("div.tip", { text: "上传限额按「实际落盘的新文件」计数，被去重跳过的文件不占额度。" }),
  );
}

/** 并发与批次 + 队列深度阈值。 */
function concurrencyCard(draft: RateDraft, data: RateLimitAdmin): HTMLElement {
  return cardNode(
    cardHead("并发与批次"),
    h(
      "div.row",
      null,
      field("单用户并发上传数", draft.maxConcurrent, "客户端队列的并发上限，服务端也会校验"),
      field("单批次最大张数", draft.maxBatch),
    ),
    h(
      "div.row",
      null,
      field("单文件大小上限", draft.maxFileSize),
      field("队列深度阈值", draft.queueDepth, "超出后新请求直接 429"),
    ),
    h("div.tip", {
      text: `当前队列深度 ${formatNumber(data.queue_depth.current)}，阈值 ${formatNumber(data.queue_depth.threshold)}。`,
    }),
  );
}

/** 超限处理：三个开关 + 阈值与封禁时长。 */
function breachCard(draft: RateDraft): HTMLElement {
  return cardNode(
    cardHead("超限处理"),
    switchRow("返回 429 + Retry-After", "告诉客户端多久后可以重试，客户端据此退避", draft.onBreach.respond, (next) => {
      draft.onBreach.respond = next;
    }),
    switchRow("记录并通知管理员", "超限事件写入审计日志，可配置邮件提醒", draft.onBreach.audit, (next) => {
      draft.onBreach.audit = next;
    }),
    switchRow("连续超限自动临时封禁", "同一用户在窗口内反复触发则临时禁用 API", draft.onBreach.tempBan, (next) => {
      draft.onBreach.tempBan = next;
    }),
    h(
      "div.row",
      { style: { marginTop: "10px" } },
      field("触发次数阈值", unitInput(draft.breachThreshold, "次")),
      field("统计窗口", unitInput(draft.breachWindow, "分钟")),
      field("封禁时长", unitInput(draft.banMinutes, "分钟")),
    ),
    h(
      "div",
      { style: { marginTop: "6px" } },
      qtip("warn", null, "临时封禁只针对 API，不影响该用户网页登录。和「账号暂停」不是一回事，别混用。"),
    ),
  );
}

/** 用户级覆盖表：UID / 邮箱 / 三档额度 / 来源 / 操作。 */
function overridesCard(reload: () => void, data: RateLimitAdmin): HTMLElement {
  const all = data.users ?? [];
  const customCount = all.filter((row) => row.custom).length;
  const tbody = h("tbody");
  const search = textInput({ placeholder: "按 UID 或邮箱搜索…" });

  const fill = (keyword: string): void => {
    const key = keyword.trim().toLowerCase();
    const rows = all.filter(
      (row) => !key || row.uid.toLowerCase().includes(key) || row.email.toLowerCase().includes(key),
    );
    if (rows.length === 0) {
      mount(
        tbody,
        h(
          "tr",
          null,
          h("td", { colspan: 7 }, emptyState("🔍", all.length === 0 ? "还没有任何用户。" : "没有匹配的用户。")),
        ),
      );
      return;
    }
    mount(
      tbody,
      ...rows.map((row) => overrideRow(reload, data, row)),
    );
  };
  fill("");
  search.addEventListener("input", () => fill(search.value));

  const head = h(
    "div.flexline",
    { style: { padding: "12px 14px", borderBottom: "1px solid var(--border)" } },
    h("b", { style: { fontSize: "13.5px" }, text: "用户级覆盖" }),
    badge(`${customCount} 个用户已自定义`, customCount > 0 ? "info" : "mute"),
    h("span.spacer"),
    h("div", { style: { width: "200px" } }, search),
  );

  const table = h(
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
          h("th", { text: "UID" }),
          h("th", { text: "邮箱" }),
          h("th", { text: "API 次/分" }),
          h("th", { text: "上传 张/分" }),
          h("th", { text: "单日流量" }),
          h("th", { text: "来源" }),
          h("th", { text: "操作" }),
        ),
      ),
      tbody,
    ),
  );

  return cardNode(
    head,
    table,
    h(
      "div",
      { style: { padding: "12px 14px", borderTop: "1px solid var(--border)", background: "#fafafa" } },
      h("div.tip", {
        style: { margin: "0" },
        text: "改「全局默认」时，所有「继承默认」的用户立刻跟着变；标了「自定义」的用户不受影响。",
      }),
    ),
  );
}

function overrideRow(reload: () => void, data: RateLimitAdmin, row: UserRow): HTMLElement {
  const unlimited = row.unlimited;
  const source = row.role === "admin" && row.unlimited
    ? badge("管理员豁免", "pp")
    : row.custom
      ? badge("自定义", "info")
      : badge("继承默认", "mute");

  const valueCell = (value: number, formatter: (n: number) => string): Node =>
    unlimited ? badge("不限", "mute") : h("span", { text: formatter(value) });

  const actions = h(
    "div.act",
    null,
    h("button.btn.sm", {
      type: "button",
      text: "编辑",
      on: { click: () => void editOverride(reload, data, row) },
    }),
    row.custom && row.role !== "admin"
      ? h("button.btn.sm", {
          type: "button",
          text: "恢复默认",
          on: { click: () => void resetOverride(reload, row) },
        })
      : null,
  );

  return h(
    "tr",
    null,
    h("td", { "data-label": "UID" }, h("span.mono", { text: row.uid })),
    h("td", { "data-label": "邮箱" }, row.email),
    h("td", { "data-label": "API 次/分" }, valueCell(row.api_per_minute, (n) => String(n))),
    h("td", { "data-label": "上传 张/分" }, valueCell(row.upload_per_minute, (n) => String(n))),
    h("td", { "data-label": "单日流量" }, valueCell(row.upload_bytes_per_day, formatBytes)),
    h("td", { "data-label": "来源" }, source),
    h("td", { "data-label": "操作" }, actions),
  );
}

/** 用户覆盖的基线：有覆盖读覆盖，没有覆盖用行内数值补齐全局默认。 */
function baselineOf(data: RateLimitAdmin, row: UserRow): PolicyFields {
  const own = row.policy ?? null;
  if (own) return { ...own };
  const global = data.global;
  return {
    api_per_minute: row.api_per_minute,
    api_per_hour: global.api_per_hour,
    api_read_per_minute: global.api_read_per_minute,
    api_burst: global.api_burst,
    upload_per_minute: row.upload_per_minute,
    upload_per_hour: global.upload_per_hour,
    upload_per_day: row.upload_per_day,
    upload_bytes_per_day: row.upload_bytes_per_day,
    max_concurrent_uploads: global.max_concurrent_uploads,
    max_batch_size: global.max_batch_size,
    max_file_size: global.max_file_size,
    unlimited: row.unlimited,
    note: null,
  };
}

/** 编辑某个用户的覆盖额度。 */
function editOverride(reload: () => void, data: RateLimitAdmin, row: UserRow): void {
  const base = baselineOf(data, row);
  const inputs = {
    apiPerMinute: textInput({ value: String(base.api_per_minute), type: "number", mono: true }),
    apiPerHour: textInput({ value: String(base.api_per_hour), type: "number", mono: true }),
    apiBurst: textInput({ value: String(base.api_burst), type: "number", mono: true }),
    apiReadPerMinute: textInput({ value: String(base.api_read_per_minute), type: "number", mono: true }),
    uploadPerMinute: textInput({ value: String(base.upload_per_minute), type: "number", mono: true }),
    uploadPerHour: textInput({ value: String(base.upload_per_hour), type: "number", mono: true }),
    uploadPerDay: textInput({ value: String(base.upload_per_day), type: "number", mono: true }),
    uploadBytesPerDay: textInput({ value: `${toGigabytes(base.upload_bytes_per_day)} GB`, mono: true }),
    maxConcurrent: textInput({ value: String(base.max_concurrent_uploads), type: "number", mono: true }),
    maxBatch: textInput({ value: String(base.max_batch_size), type: "number", mono: true }),
    maxFileSize: textInput({ value: toMegabytes(base.max_file_size), mono: true }),
    note: textInput({ value: base.note ?? "", placeholder: "为什么给这个人单独放额度" }),
  };
  let unlimited = Boolean(base.unlimited);

  const submit = h("button.btn.pri", { type: "button", text: "保存" });
  const handle = openModal({
    title: `调整 u_${row.uid} 的额度`,
    body: [
      h("div.tip", { text: row.email }),
      switchRow("不限额度", "该用户不受全局默认限制（管理员默认豁免）", unlimited, (next) => {
        unlimited = next;
      }),
      cardHead("API 调用"),
      h("div.row", null, field("次 / 分钟", inputs.apiPerMinute), field("次 / 小时", inputs.apiPerHour)),
      h(
        "div.row",
        null,
        field("瞬时并发上限", inputs.apiBurst),
        field("读接口限额", unitInput(inputs.apiReadPerMinute, "次 / 分")),
      ),
      cardHead("图片上传"),
      h("div.row", null, field("张 / 分钟", inputs.uploadPerMinute), field("张 / 小时", inputs.uploadPerHour)),
      h(
        "div.row",
        null,
        field("张 / 天", inputs.uploadPerDay),
        field("单日流量", inputs.uploadBytesPerDay, "不带单位按 GB 计算"),
      ),
      cardHead("并发与批次"),
      h(
        "div.row",
        null,
        field("并发上传数", inputs.maxConcurrent),
        field("单批次最大张数", inputs.maxBatch),
        field("单文件上限", inputs.maxFileSize),
      ),
      field("备注", inputs.note, "只给后台看"),
    ],
    footer: [h("button.btn", { type: "button", text: "取消", on: { click: () => handle.close() } }), submit],
  });

  submit.addEventListener("click", async () => {
    const body = {
      unlimited,
      api_per_minute: intOf(inputs.apiPerMinute),
      api_per_hour: intOf(inputs.apiPerHour),
      api_read_per_minute: intOf(inputs.apiReadPerMinute),
      api_burst: intOf(inputs.apiBurst),
      upload_per_minute: intOf(inputs.uploadPerMinute),
      upload_per_hour: intOf(inputs.uploadPerHour),
      upload_per_day: intOf(inputs.uploadPerDay),
      upload_bytes_per_day: sizeOf(inputs.uploadBytesPerDay, "GB"),
      max_concurrent_uploads: intOf(inputs.maxConcurrent),
      max_batch_size: intOf(inputs.maxBatch),
      max_file_size: sizeOf(inputs.maxFileSize, "MB"),
      note: inputs.note.value.trim(),
    };
    submit.disabled = true;
    submit.textContent = "保存中…";
    try {
      // 路径里的 id 用 uid：列表里只有 uid 这个标识，后端按它找用户
      await request(`/admin/ratelimit/users/${encodeURIComponent(row.uid)}`, { method: "PUT", json: body });
      handle.close();
      toastOk("该用户的额度已更新");
      reload();
    } catch (err) {
      toastError(err);
      submit.disabled = false;
      submit.textContent = "保存";
    }
  });
}

/** 恢复默认：删掉覆盖行，重新跟着全局默认走。 */
async function resetOverride(reload: () => void, row: UserRow): Promise<void> {
  const ok = await confirmDialog({
    title: "恢复默认额度",
    message: `u_${row.uid} 的自定义额度会被删除，之后跟着全局默认走；全局默认一改，他立刻跟着变。`,
    confirmText: "恢复默认",
  });
  if (!ok) return;
  try {
    await request(`/admin/ratelimit/users/${encodeURIComponent(row.uid)}`, { method: "DELETE" });
    toastOk("已恢复默认");
    reload();
  } catch (err) {
    toastError(err);
  }
}

/** 实时用量：5 秒自己刷一次，只重画表格行，不动页面其它部分。 */
function realtimeCard(root: HTMLElement, data: RateLimitAdmin): HTMLElement {
  const tbody = h("tbody");
  const stamp = h("span.tip", { style: { margin: "0" }, text: "窗口：本分钟 / 今日 00:00 起" });

  const fill = (rows: RealtimeRow[]): void => {
    if (!rows || rows.length === 0) {
      mount(tbody, h("tr", null, h("td", { colspan: 6 }, emptyState("📈", "还没有活跃用户。"))));
      return;
    }
    mount(
      tbody,
      ...rows.map((row) =>
        h(
          "tr",
          null,
          h("td", { "data-label": "UID" }, h("span.mono", { text: row.uid })),
          h("td", { "data-label": "邮箱" }, row.email),
          h("td", { "data-label": "API 本分钟" }, metricCell(row.status, row.api_minute, row.api_minute_limit)),
          h("td", { "data-label": "上传 本分钟" }, metricCell(row.status, row.upload_minute, row.upload_minute_limit)),
          h("td", { "data-label": "上传 今日" }, metricCell(row.status, row.upload_day, row.upload_day_limit)),
          h(
            "td",
            { "data-label": "近 24 小时命中限制" },
            row.hits_24h > 0 ? badge(`${row.hits_24h} 次`, "warn") : badge("0 次", "mute"),
          ),
        ),
      ),
    );
  };
  fill(data.realtime);

  const head = h(
    "div.flexline",
    { style: { padding: "12px 14px", borderBottom: "1px solid var(--border)" } },
    h("b", { style: { fontSize: "13.5px" }, text: "实时用量" }),
    badge("5 秒自动刷新", "ok"),
    h("span.spacer"),
    stamp,
  );

  const table = h(
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
          h("th", { text: "UID" }),
          h("th", { text: "邮箱" }),
          h("th", { text: "API 本分钟" }),
          h("th", { text: "上传 本分钟" }),
          h("th", { text: "上传 今日" }),
          h("th", { text: "近 24 小时命中限制" }),
        ),
      ),
      tbody,
    ),
  );

  // 页面切走时路由会把 outlet 清空，节点一断开就把定时器停掉，
  // 否则切到别的页面后这里还在每 5 秒打一次接口。
  const timer = window.setInterval(() => {
    if (!root.isConnected) {
      window.clearInterval(timer);
      return;
    }
    void request<RateLimitAdmin>("/admin/ratelimit")
      .then((fresh) => {
        fill(fresh.realtime);
        stamp.textContent = `最后更新 ${new Date().toLocaleTimeString("zh-CN", { hour12: false })}`;
      })
      .catch(() => {
        // 单次轮询失败不打断：网络抖动很常见，下一轮会自己恢复
      });
  }, 5000);

  return cardNode(head, table);
}

/** 一格用量：账号不可用时显示原因，否则显示迷你进度条 + 当前值 / 上限。 */
function metricCell(status: string, value: number, limit: number): Node {
  if (status === "banned") return h("span", { style: { fontSize: "12px", color: "var(--faint)" }, text: "账号已封禁" });
  if (status === "suspended") return h("span", { style: { fontSize: "12px", color: "var(--faint)" }, text: "账号已暂停" });
  if (status === "pending") return h("span", { style: { fontSize: "12px", color: "var(--faint)" }, text: "未激活" });
  if (limit <= 0) return h("span", { style: { fontSize: "12px", color: "var(--faint)" }, text: "不限" });
  return h(
    "div.mini-metric",
    null,
    progressBar(value, limit),
    h("span", { text: `${formatNumber(value)} / ${formatNumber(limit)}` }),
  );
}

/** 超限日志：审计日志里的限流事件，分页看。 */
async function openLogs(page: number, uid: string): Promise<void> {
  const body = h("div");
  const foot = h("div", { style: { width: "100%" } });
  const uidInput = textInput({ value: uid, placeholder: "按 UID 过滤，留空看全部", mono: true });
  const query = textInput({ placeholder: "搜索 UID / 邮箱…" });

  const collect = (target: number): void => {
    void load(target, uidInput.value.trim());
  };

  const load = async (target: number, uidFilter: string): Promise<void> => {
    mount(body, loadingBlock("正在读取限流事件…"));
    try {
      const result = await requestPage<RateLogRow>("/admin/ratelimit/logs", {
        query: { uid: uidFilter || undefined, page: target },
      });
      if (result.items.length === 0) {
        mount(body, emptyState("📄", "这段时间没有限流事件。"));
        mount(foot);
        return;
      }
      mount(
        body,
        responsiveTable(
          ["时间", "用户", "动作", "详情", "来源 IP"],
          result.items.map((row) => [
            txt(formatDateTime(row.created_at ?? null)),
            txt(row.uid ? `u_${row.uid}` : (row.email ?? "—")),
            txt(row.action ?? "—"),
            txt(describeDetail(row)),
            h("span.mono", { text: row.ip ?? "—" }),
          ]),
        ),
      );
      const pager = pagination(result.meta, (next) => collect(next));
      mount(foot, pager ?? "");
    } catch (err) {
      mount(body, errorBlock(err, () => void load(target, uidFilter)));
      mount(foot);
    }
  };

  const handle = openModal({
    title: "超限日志",
    reading: true,
    body: [
      h(
        "div.flexline",
        { style: { marginBottom: "12px" } },
        h("div", { style: { flex: "0 0 200px" } }, uidInput),
        h(
          "button.btn.sm",
          { type: "button", text: "筛选", on: { click: () => collect(1) } },
        ),
        h("span.spacer"),
        h("div", { style: { flex: "0 0 200px" } }, query),
      ),
      body,
      foot,
    ],
    footer: [h("button.btn", { type: "button", text: "关闭", on: { click: () => handle.close() } })],
  });

  // 弹窗里的搜索框在前端过滤当前页，不额外打接口
  query.addEventListener("input", () => {
    const key = query.value.trim().toLowerCase();
    for (const tr of Array.from(body.querySelectorAll("tbody tr"))) {
      const text = (tr.textContent ?? "").toLowerCase();
      (tr as HTMLElement).style.display = !key || text.includes(key) ? "" : "none";
    }
  });

  void load(page, uid);
}

/** 审计详情是 JSON，挑几个常用键拼成人话显示。 */
function describeDetail(row: RateLogRow): string {
  const detail = row.detail;
  if (detail === null || detail === undefined) return row.target ?? "—";
  if (typeof detail === "string") return detail;
  if (typeof detail === "object") {
    const record = detail as Record<string, unknown>;
    const parts: string[] = [];
    for (const [key, value] of Object.entries(record)) {
      if (value === null || value === undefined || value === "") continue;
      parts.push(`${key}=${typeof value === "object" ? JSON.stringify(value) : String(value)}`);
    }
    return parts.length > 0 ? parts.join(" · ") : row.target ?? "—";
  }
  return String(detail);
}

/** 保存全局默认、超限处理，外加队列深度（队列深度归 /admin/settings 管）。 */
async function save(reload: () => void, draft: RateDraft, button: HTMLButtonElement): Promise<void> {
  const global = {
    api_per_minute: intOf(draft.apiPerMinute),
    api_per_hour: intOf(draft.apiPerHour),
    api_burst: intOf(draft.apiBurst),
    api_read_per_minute: intOf(draft.apiReadPerMinute),
    upload_per_minute: intOf(draft.uploadPerMinute),
    upload_per_hour: intOf(draft.uploadPerHour),
    upload_per_day: intOf(draft.uploadPerDay),
    upload_bytes_per_day: sizeOf(draft.uploadBytesPerDay, "GB"),
    max_concurrent_uploads: intOf(draft.maxConcurrent),
    max_batch_size: intOf(draft.maxBatch),
    max_file_size: sizeOf(draft.maxFileSize, "MB"),
  };
  const onBreach = {
    respond: draft.onBreach.respond,
    audit: draft.onBreach.audit,
    temp_ban: draft.onBreach.tempBan,
    threshold: intOf(draft.breachThreshold),
    threshold_minutes: intOf(draft.breachWindow),
    ban_minutes: intOf(draft.banMinutes),
  };
  const queueDepth = intOf(draft.queueDepth);

  if (global.max_file_size <= 0) {
    toastError("单文件大小上限填不对：可以写 20 MB、2 GB");
    return;
  }
  if (onBreach.threshold_minutes <= 0) {
    toastError("超限统计窗口至少 1 分钟");
    return;
  }

  button.disabled = true;
  button.textContent = "保存中…";
  try {
    await request("/admin/ratelimit/global", { method: "PUT", json: global });
    await request("/admin/ratelimit/on_breach", { method: "PUT", json: onBreach });
    // 队列深度阈值是上传策略的一部分，按契约走系统设置接口；
    // 放在这里只是为了跟「并发与批次」同屏编辑，值只写 upload 一个键。
    await request("/admin/settings", { method: "PUT", json: { upload: { queue_depth: queueDepth } } });
    toastOk("频率限制设置已保存");
    reload();
  } catch (err) {
    toastError(err);
    button.disabled = false;
    button.textContent = "保存设置";
  }
}
