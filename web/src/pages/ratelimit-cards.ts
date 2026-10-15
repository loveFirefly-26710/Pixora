/**
 * 频率限制的几张卡片。
 *
 * 这些内容原来自成一页（`#/admin/ratelimit`），现在并进「系统设置」：
 * 页面骨架、保存按钮、接口合并都归那边，全局额度与超限处理跟着那一页的
 * 「保存设置」一起提交（`saveRateLimit`）。这里只出卡片和弹窗。
 *
 * 两处得写在界面上，而不是只写在文档里：
 *   - 「临时封禁」只掐 API，不影响该用户网页登录，跟「账号暂停」不是一回事；
 *   - 用户表的用量每 5 秒自己刷一次，节点一断开就停掉轮询。
 */

import { ApiError, request, requestPage } from "../core/api";
import { h, mount } from "../core/dom";
import { formatBytes, formatDateTime, formatNumber, parseSize, toGigabytes } from "../core/format";
import { confirmDialog, openModal } from "../core/modal";
import { toastError, toastOk } from "../core/toast";
import { CONCURRENCY_OPTIONS } from "../upload/queue";
import type { PolicyFields, RateLimitAdmin } from "../core/types";
import {
  badge,
  cardHead,
  cardNode,
  emptyState,
  field,
  pagination,
  progressBar,
  qtip,
  responsiveTable,
  select,
  spinner,
  statusBadge,
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

function intOf(input: HTMLInputElement): number {
  const value = Number(input.value.trim());
  return Number.isFinite(value) ? Math.max(0, Math.round(value)) : 0;
}

/** 容量输入：不带单位按默认单位解释。 */
function sizeOf(input: HTMLInputElement, unit: string): number {
  const text = input.value.trim();
  if (!text) return 0;
  return parseSize(/[a-z]$/i.test(text) ? text : `${text} ${unit}`) ?? 0;
}

function toMegabytes(bytes: number): string {
  return `${Math.round(bytes / 1024 / 1024)} MB`;
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

/** 表格单元格要 Node，纯文本用文本节点最省事（不额外套一层 span）。 */
function txt(value: string): Node {
  return document.createTextNode(value);
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
  return h(
    "div.card",
    null,
    qtip("err", "加载失败", detail ? `${message}（${detail}）` : message),
    h("button.btn", { type: "button", text: "重试", on: { click: onRetry } }),
  );
}

// ============================================================
// 全局额度与超限处理
// ============================================================

/**
 * 这一页可编辑的全局额度与超限处理。系统设置有自己的 draft，
 * 两边各管各的，保存时分别提交到不同的接口。
 */
export interface RateLimitDraft {
  apiPerMinute: HTMLInputElement;
  apiPerHour: HTMLInputElement;
  apiBurst: HTMLInputElement;
  apiReadPerMinute: HTMLInputElement;
  uploadPerMinute: HTMLInputElement;
  uploadPerHour: HTMLInputElement;
  uploadPerDay: HTMLInputElement;
  uploadBytesPerDay: HTMLInputElement;
  /** 单用户并发上传数：界面上摆在「上传策略」那张卡里，值属于全局策略 */
  maxConcurrent: HTMLInputElement;
  onBreach: { respond: boolean; audit: boolean; tempBan: boolean };
  breachThreshold: HTMLInputElement;
  breachWindow: HTMLInputElement;
  banMinutes: HTMLInputElement;
}

export function rateLimitDraft(data: RateLimitAdmin): RateLimitDraft {
  const global = data.global;
  return {
    apiPerMinute: textInput({ value: String(global.api_per_minute), type: "number", mono: true }),
    apiPerHour: textInput({ value: String(global.api_per_hour), type: "number", mono: true }),
    apiBurst: textInput({ value: String(global.api_burst), type: "number", mono: true }),
    apiReadPerMinute: textInput({ value: String(global.api_read_per_minute), type: "number", mono: true }),
    uploadPerMinute: textInput({ value: String(global.upload_per_minute), type: "number", mono: true }),
    uploadPerHour: textInput({ value: String(global.upload_per_hour), type: "number", mono: true }),
    uploadPerDay: textInput({ value: String(global.upload_per_day), type: "number", mono: true }),
    uploadBytesPerDay: textInput({ value: `${toGigabytes(global.upload_bytes_per_day)} GB`, mono: true }),
    maxConcurrent: textInput({ value: String(global.max_concurrent_uploads), type: "number", mono: true }),
    onBreach: {
      respond: data.on_breach.respond,
      audit: data.on_breach.audit,
      tempBan: data.on_breach.temp_ban,
    },
    breachThreshold: textInput({ value: String(data.on_breach.threshold), type: "number", mono: true }),
    breachWindow: textInput({ value: String(data.on_breach.threshold_minutes), type: "number", mono: true }),
    banMinutes: textInput({ value: String(data.on_breach.ban_minutes), type: "number", mono: true }),
  };
}

/** API 调用频率（全局默认）。 */
export function apiRateCard(draft: RateLimitDraft): HTMLElement {
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
export function uploadRateCard(draft: RateLimitDraft): HTMLElement {
  return cardNode(
    cardHead("上传频率", badge("全局默认", "mute")),
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

/** 超限处理：三个开关 + 阈值与封禁时长。 */
export function breachCard(draft: RateLimitDraft): HTMLElement {
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
    h("div.flexline", { style: { marginTop: "10px" } }, rateLogsButton()),
  );
}

/** 保存前的校验：返回一句能直接弹给管理员的话，没问题时返回 null。 */
export function validateRateLimit(draft: RateLimitDraft): string | null {
  if (intOf(draft.breachWindow) <= 0) return "超限统计窗口至少 1 分钟";
  return null;
}

/**
 * 提交全局额度与超限处理。
 *
 * 单文件上限、单批张数、像素、队列深度、格式白名单都不在这里——
 * 它们归系统设置的上传策略（`PUT /admin/settings`）。
 */
export async function saveRateLimit(draft: RateLimitDraft): Promise<void> {
  await request("/admin/ratelimit", {
    method: "PUT",
    json: {
      api_per_minute: intOf(draft.apiPerMinute),
      api_per_hour: intOf(draft.apiPerHour),
      api_burst: intOf(draft.apiBurst),
      api_read_per_minute: intOf(draft.apiReadPerMinute),
      upload_per_minute: intOf(draft.uploadPerMinute),
      upload_per_hour: intOf(draft.uploadPerHour),
      upload_per_day: intOf(draft.uploadPerDay),
      upload_bytes_per_day: sizeOf(draft.uploadBytesPerDay, "GB"),
      max_concurrent_uploads: intOf(draft.maxConcurrent),
    },
  });
  await request("/admin/ratelimit/on_breach", {
    method: "PUT",
    json: {
      respond: draft.onBreach.respond,
      audit: draft.onBreach.audit,
      temp_ban: draft.onBreach.tempBan,
      threshold: intOf(draft.breachThreshold),
      threshold_minutes: intOf(draft.breachWindow),
      ban_minutes: intOf(draft.banMinutes),
    },
  });
}

// ============================================================
// 用户额度与用量
// ============================================================

/** 会随轮询就地更新的格子：节点不动，只换里面的内容。 */
interface LiveCell {
  node: HTMLElement;
  update(row: RealtimeRow | undefined): void;
}

const DASH = "—";

/**
 * 用量格：迷你进度条 + 当前值 / 上限。
 *
 * 账号不可用时只显示一个破折号——原因在那行的状态徽标里，不必每格重复一遍。
 */
function usageCell(
  unlimited: boolean,
  initial: RealtimeRow | undefined,
  pick: (row: RealtimeRow) => [number, number],
): LiveCell {
  const node = h("div");
  const apply = (row: RealtimeRow | undefined): void => {
    if (!row || row.status !== "active") {
      mount(node, h("span", { style: { fontSize: "12px", color: "var(--faint)" }, text: DASH }));
      return;
    }
    const [value, limit] = pick(row);
    // 「不限」的账号也把当期用量显示出来，只是不画进度条：没有上限可对照
    if (unlimited || limit <= 0) {
      mount(
        node,
        h("span", { style: { fontSize: "12px", color: "var(--muted)" }, text: `${formatNumber(value)}（不限）` }),
      );
      return;
    }
    mount(
      node,
      h(
        "div.mini-metric",
        null,
        progressBar(value, limit),
        h("span", { text: `${formatNumber(value)} / ${formatNumber(limit)}` }),
      ),
    );
  };
  apply(initial);
  return { node, update: apply };
}

/** 命中格：近 24 小时被限流的次数。 */
function hitsCell(initial: RealtimeRow | undefined): LiveCell {
  const node = h("div");
  const apply = (row: RealtimeRow | undefined): void => {
    const count = row?.hits_24h ?? 0;
    mount(node, count > 0 ? badge(`${count} 次`, "warn") : badge("0 次", "mute"));
  };
  apply(initial);
  return { node, update: apply };
}

type LiveCells = { api: LiveCell; uploadMinute: LiveCell; uploadDay: LiveCell; hits: LiveCell };

/**
 * 用户额度与用量：一张表同时回答「额度是多少」和「这一刻用掉多少」。
 *
 * 额度那一列是编辑入口（继承默认 / 自定义 / 管理员豁免），用量每 5 秒自己刷。
 * 轮询只就地改格子里的内容，不重建行——重建会把「编辑」按钮一起换掉，
 * 正好点在那一刻的话，弹窗里打开的可能是另一个人的额度。
 */
export function usersCard(
  reload: () => void,
  data: RateLimitAdmin,
  formatOptions: Array<{ ext: string; label: string }>,
): HTMLElement {
  const all = data.users ?? [];
  const customCount = all.filter((row) => row.custom).length;
  const tbody = h("tbody");
  const search = textInput({ placeholder: "按 UID 或邮箱搜索…" });
  const stamp = h("span.tip", { style: { margin: "0" }, text: "窗口：本分钟 / 今日 00:00 起" });
  const live = new Map<string, LiveCells>();

  const fill = (keyword: string): void => {
    live.clear();
    const key = keyword.trim().toLowerCase();
    const byUid = new Map((data.realtime ?? []).map((row) => [row.uid, row]));
    const rows = all.filter(
      (row) => !key || row.uid.toLowerCase().includes(key) || row.email.toLowerCase().includes(key),
    );
    if (rows.length === 0) {
      mount(
        tbody,
        h(
          "tr",
          null,
          h("td", { colspan: 9 }, emptyState("🔍", all.length === 0 ? "还没有任何用户。" : "没有匹配的用户。")),
        ),
      );
      return;
    }
    mount(
      tbody,
      ...rows.map((row) => userRow(reload, data, formatOptions, row, byUid.get(row.uid), live)),
    );
  };
  fill("");
  search.addEventListener("input", () => fill(search.value));

  const head = h(
    "div.flexline",
    { style: { padding: "12px 14px", borderBottom: "1px solid var(--border)" } },
    h("b", { style: { fontSize: "13.5px" }, text: "用户额度与用量" }),
    badge(`${customCount} 个用户已自定义`, customCount > 0 ? "info" : "mute"),
    badge("用量 5 秒自动刷新", "ok"),
    h("span.spacer"),
    stamp,
    h("div", { style: { width: "200px" } }, search),
  );

  const card = cardNode(
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
            h("th", { text: "UID" }),
            h("th", { text: "邮箱" }),
            h("th", { text: "来源" }),
            h("th", { text: "API 次 / 分" }),
            h("th", { text: "上传 张 / 分" }),
            h("th", { text: "今日上传" }),
            h("th", { text: "单日流量" }),
            h("th", { text: "24 小时命中" }),
            h("th", { text: "操作" }),
          ),
        ),
        tbody,
      ),
    ),
    h(
      "div",
      { style: { padding: "12px 14px", borderTop: "1px solid var(--border)", background: "#fafafa" } },
      h("div.tip", {
        style: { margin: "0" },
        text: "改「全局默认」时，所有「继承默认」的用户立刻跟着变；标了「自定义」的用户不受影响。按人放宽或收紧，用这一行的「编辑」。",
      }),
    ),
  );

  // 页面切走时路由会把 outlet 清空，节点一断开就把定时器停掉，
  // 否则切到别的页面后这里还在每 5 秒打一次接口。
  // 第一次 tick 之前节点一定是连着的（卡片是同步挂上去的），所以
  // 「断开过」才停：单纯还没连上不算。
  let everConnected = false;
  const timer = window.setInterval(() => {
    if (card.isConnected) {
      everConnected = true;
    } else if (everConnected) {
      window.clearInterval(timer);
      return;
    }
    void request<RateLimitAdmin>("/admin/ratelimit")
      .then((fresh) => {
        const byUid = new Map((fresh.realtime ?? []).map((row) => [row.uid, row]));
        for (const [uid, cells] of live) {
          const row = byUid.get(uid);
          cells.api.update(row);
          cells.uploadMinute.update(row);
          cells.uploadDay.update(row);
          cells.hits.update(row);
        }
        stamp.textContent = `最后更新 ${new Date().toLocaleTimeString("zh-CN", { hour12: false })}`;
      })
      .catch(() => {
        // 单次轮询失败不打断：网络抖动很常见，下一轮会自己恢复
      });
  }, 5000);

  return card;
}

function userRow(
  reload: () => void,
  data: RateLimitAdmin,
  formatOptions: Array<{ ext: string; label: string }>,
  row: UserRow,
  realtime: RealtimeRow | undefined,
  live: Map<string, LiveCells>,
): HTMLElement {
  const unlimited = row.unlimited;
  const source =
    row.role === "admin" && unlimited
      ? badge("管理员豁免", "pp")
      : row.custom
        ? badge("自定义", "info")
        : badge("继承默认", "mute");

  const cells: LiveCells = {
    api: usageCell(unlimited, realtime, (r) => [r.api_minute, r.api_minute_limit]),
    uploadMinute: usageCell(unlimited, realtime, (r) => [r.upload_minute, r.upload_minute_limit]),
    uploadDay: usageCell(unlimited, realtime, (r) => [r.upload_day, r.upload_day_limit]),
    hits: hitsCell(realtime),
  };
  live.set(row.uid, cells);

  const actions = h(
    "div.act",
    null,
    h("button.btn.sm", {
      type: "button",
      text: "编辑",
      on: { click: () => void editOverride(reload, data, formatOptions, row) },
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
    h(
      "td",
      { "data-label": "来源" },
      h(
        "div.flexline",
        { style: { gap: "6px", flexWrap: "wrap" } },
        source,
        // 账号不可用时把原因摆一次，用量格就不再逐格重复
        row.status === "active" ? null : statusBadge(row.status),
      ),
    ),
    h("td", { "data-label": "API 次 / 分" }, cells.api.node),
    h("td", { "data-label": "上传 张 / 分" }, cells.uploadMinute.node),
    h("td", { "data-label": "今日上传" }, cells.uploadDay.node),
    h(
      "td",
      { "data-label": "单日流量" },
      unlimited ? badge("不限", "mute") : h("span", { text: formatBytes(row.upload_bytes_per_day) }),
    ),
    h("td", { "data-label": "24 小时命中" }, cells.hits.node),
    h("td", { "data-label": "操作" }, actions),
  );
}

/** 用户覆盖的基线：有覆盖读覆盖，没有覆盖用行内数值补齐全局默认。
 *  上传策略优先取后端解析好的生效值——它就是这个人当下真正被允许的量。 */
function baselineOf(data: RateLimitAdmin, row: UserRow): PolicyFields {
  const own = row.policy ?? null;
  const upload = row.upload ?? null;
  const global = data.global;
  const uploadFields = {
    max_batch_size: upload?.max_batch_size ?? own?.max_batch_size ?? global.max_batch_size,
    max_file_size: upload?.max_file_size ?? own?.max_file_size ?? global.max_file_size,
    max_pixels: upload?.max_pixels ?? own?.max_pixels ?? global.max_pixels,
    max_width: upload?.max_width ?? own?.max_width ?? global.max_width ?? 0,
    max_height: upload?.max_height ?? own?.max_height ?? global.max_height ?? 0,
    formats: upload?.formats ?? own?.formats ?? global.formats ?? [],
  };
  if (own) return { ...own, ...uploadFields };
  return {
    api_per_minute: row.api_per_minute,
    api_per_hour: global.api_per_hour,
    api_read_per_minute: global.api_read_per_minute,
    api_burst: global.api_burst,
    upload_per_minute: row.upload_per_minute,
    upload_per_hour: global.upload_per_hour,
    upload_per_day: row.upload_per_day,
    upload_bytes_per_day: row.upload_bytes_per_day,
    max_concurrent_uploads: upload?.max_concurrent_uploads ?? global.max_concurrent_uploads,
    ...uploadFields,
    unlimited: row.unlimited,
    note: null,
  };
}

/** 编辑某个用户的覆盖额度。 */
function editOverride(
  reload: () => void,
  data: RateLimitAdmin,
  formatOptions: Array<{ ext: string; label: string }>,
  row: UserRow,
): void {
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
    maxBatch: textInput({ value: String(base.max_batch_size), type: "number", mono: true }),
    maxFileSize: textInput({ value: toMegabytes(base.max_file_size), mono: true }),
    maxPixels: textInput({
      value: String(Math.round(base.max_pixels / 10000)),
      type: "number",
      mono: true,
    }),
    maxWidth: textInput({ value: String(base.max_width || 0), type: "number", mono: true }),
    maxHeight: textInput({ value: String(base.max_height || 0), type: "number", mono: true }),
    note: textInput({ value: base.note ?? "", placeholder: "为什么给这个人单独放额度" }),
  };
  let unlimited = Boolean(base.unlimited);

  // 图片格式：勾选框。语义与「系统设置」里那份白名单一致——
  // 一个都不勾就是「跟随全局默认」，服务端收到空数组会按「不覆盖」处理
  const ownFormats = new Set<string>(base.formats ?? []);
  const formatBoxes = new Map<string, HTMLInputElement>();
  const formatGrid = h("div.flexline", { style: { flexWrap: "wrap", gap: "10px 14px" } });
  for (const option of formatOptions) {
    const box = h("input", { type: "checkbox", checked: ownFormats.has(option.ext) });
    formatBoxes.set(option.ext, box);
    formatGrid.append(
      h(
        "label.flexline",
        { style: { gap: "4px", margin: "0", fontSize: "12.5px" } },
        box,
        h("span", { text: option.label }),
      ),
    );
  }

  // 并发数：与上传界面同一个下拉（客户端队列的并发上限）
  const concurrencySelect = select(
    CONCURRENCY_OPTIONS.map((value) => ({
      value: String(value),
      label: value === 1 ? "1（逐张，推荐）" : String(value),
    })),
    String(base.max_concurrent_uploads || 1),
  );

  /** 勾选框里勾了哪些（空 = 跟随全局）。 */
  const pickedFormats = (): string[] =>
    [...formatBoxes.entries()].filter(([, box]) => box.checked).map(([ext]) => ext);

  const submit = h("button.btn.pri", { type: "button", text: "保存" });
  const handle = openModal({
    title: `调整 u_${row.uid} 的额度`,
    body: [
      h("div.tip", { text: row.email }),
      switchRow("不限额度", "该用户的频率与流量不受限制（上传大小、格式仍按下面的策略走）", unlimited, (next) => {
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
      cardHead("上传策略", badge("按用户覆盖", "info")),
      h(
        "div.row",
        null,
        field("单文件大小上限", inputs.maxFileSize, "可以写 20 MB、2 GB；0 表示跟随全局默认"),
        field("单批次最大张数", inputs.maxBatch, "0 表示跟随全局默认"),
      ),
      h(
        "div.row",
        null,
        field("像素上限", unitInput(inputs.maxPixels, "万"), "超出的图片直接拒绝，防止解压炸弹"),
        field("最大宽度", inputs.maxWidth, "像素；0 表示不限"),
      ),
      h(
        "div.row",
        null,
        field("最大高度", inputs.maxHeight, "像素；0 表示不限"),
        field("并发上传数", concurrencySelect, "与上传界面同一个下拉：客户端队列的并发上限"),
      ),
      h(
        "div.fld",
        null,
        h("label.f", { text: "允许的图片格式" }),
        formatGrid,
        h("div.tip", { text: "一个都不勾 = 跟随全局默认；勾了就以这一份为准。" }),
      ),
      field("备注", inputs.note, "只给后台看"),
    ],
    footer: [
      // 恢复默认就在弹窗里：开着编辑框才想起来「这个人不该有覆盖」时，
      // 不用先取消再去表格里找那行
      ...(row.custom
        ? [
            h("button.btn.dgr.gho", {
              type: "button",
              text: "恢复默认",
              on: { click: () => void resetOverride(reload, row, handle) },
            }),
          ]
        : []),
      h("span.spacer"),
      h("button.btn", { type: "button", text: "取消", on: { click: () => handle.close() } }),
      submit,
    ],
  });

  submit.addEventListener("click", async () => {
    // 像素上限按「万」输入；0、空格式数组都表示「跟随上一层」
    const pixelsInWan = Number(inputs.maxPixels.value.trim());
    const formats = pickedFormats();
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
      max_concurrent_uploads: Number(concurrencySelect.value) || 1,
      max_batch_size: intOf(inputs.maxBatch),
      max_file_size: sizeOf(inputs.maxFileSize, "MB"),
      max_pixels: Number.isFinite(pixelsInWan) ? Math.max(0, Math.round(pixelsInWan)) * 10000 : 0,
      max_width: intOf(inputs.maxWidth),
      max_height: intOf(inputs.maxHeight),
      formats,
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
async function resetOverride(
  reload: () => void,
  row: UserRow,
  handle?: { close: () => void },
): Promise<void> {
  const ok = await confirmDialog({
    title: "恢复默认额度",
    message: `u_${row.uid} 的自定义额度会被删除，之后跟着全局默认走；全局默认一改，他立刻跟着变。`,
    confirmText: "恢复默认",
  });
  if (!ok) return;
  try {
    await request(`/admin/ratelimit/users/${encodeURIComponent(row.uid)}`, { method: "DELETE" });
    toastOk("已恢复默认");
    handle?.close();
    reload();
  } catch (err) {
    toastError(err);
  }
}

// ============================================================
// 超限日志
// ============================================================

/** 「查看超限日志」按钮。三处都开同一个弹窗，收在这里省得复制 onClick。 */
export function rateLogsButton(uid = "", small = false): HTMLButtonElement {
  return h("button.btn", {
    type: "button",
    class: small ? "sm" : "",
    text: "查看超限日志",
    on: { click: () => void openRateLogs(1, uid) },
  });
}

/** 超限日志：审计日志里的限流事件，分页看。 */
export async function openRateLogs(page: number, uid: string): Promise<void> {
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
        h("button.btn.sm", { type: "button", text: "筛选", on: { click: () => collect(1) } }),
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
