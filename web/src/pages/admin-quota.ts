/**
 * 配额与容量（`#/admin/quota`）。
 *
 * 页面围绕一条硬约束：`所有用户配额合计 + 管理员账户配额 ≤ 可分配上限`，
 * 上限 = 系统总额 − 系统占用（宿主机整盘已用）——磁盘上被别的数据先占住的
 * 额度分不出去，不扣掉就会出现「配额都发完了、上传却写不进盘」。
 * 校验由服务端兜底，前端负责把「还差多少」说清楚——改完才发现存不进去最让人烦。
 *
 * 系统总额的两种来源要分开看：本地盘用 statfs 的真实值，网络挂载（OSS 等）
 * 读到的是挂载伪值，必须由管理员手填，所以这里会把探测结果摊开讲。
 */

import { ApiError, request } from "../core/api";
import { h, mount } from "../core/dom";
import { formatBytes, formatNumber, parseSize, percent, toGigabytes } from "../core/format";
import { promptDialog } from "../core/modal";
import type { PageContext } from "../core/router";
import { toast, toastError, toastOk } from "../core/toast";
import type { AdminUserRow, QuotaAdmin, StorageAdmin } from "../core/types";
import {
  badge,
  card,
  cardHead,
  cardNode,
  emptyState,
  field,
  note,
  pageHead,
  progressBar,
  qtip,
  responsiveTable,
  roleBadge,
  select,
  spinner,
  statusBadge,
  switchRow,
  textInput,
} from "../ui/widgets";

/** 可编辑的值集中在这一份里，保存时只读它，不去 DOM 里反查。 */
interface QuotaDraft {
  mode: () => "auto" | "manual" | "hidden";
  manualTotal: HTMLInputElement;
  defaultQuota: HTMLInputElement;
  perUserMax: HTMLInputElement;
  autoEnabled: boolean;
  autoOn: HTMLInputElement;
  autoOff: HTMLInputElement;
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

/** 错误态：服务端原话 + 重试入口。 */
function errorBlock(err: unknown, onRetry: () => void): HTMLElement {
  const message = err instanceof ApiError ? err.message : err instanceof Error ? err.message : String(err);
  const detail = err instanceof ApiError ? err.detail : null;
  return card(
    null,
    qtip("err", "加载失败", detail ? `${message}（${detail}）` : message),
    h("button.btn", { type: "button", text: "重试", on: { click: onRetry } }),
  );
}

/**
 * 把服务端 detail 里的裸字节翻成人能读的量。
 * 契约里超额返回的是 `还差 2411724 字节`，原样甩给管理员看没有意义。
 */
function readableDetail(text: string): string {
  return text
    .replace(/(\d+)\s*(?:字节|bytes)/gi, (_match, digits: string) => formatBytes(Number(digits)))
    .replace(/\b(\d{6,})\b/g, (_match, digits: string) => formatBytes(Number(digits)));
}

/** 表单里的容量输入：不带单位就按 GB 解释（管理员填 50 就是 50 GB）。 */
function parseSizeInput(raw: string, unit = "GB"): number | null {
  const text = raw.trim();
  if (!text) return null;
  return parseSize(/[a-z]$/i.test(text) ? text : `${text} ${unit}`);
}

/** 十六进制 f_type：后端已格式化就用原值，给到数字就补 `0x`。 */
function hexFsType(raw: string): string {
  const text = String(raw ?? "").trim();
  if (!text) return "—";
  if (text.toLowerCase().startsWith("0x")) return text;
  const value = Number(text);
  return Number.isFinite(value) ? `0x${value.toString(16)}` : text;
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
    h("span", { style: { flex: "0 0 44px", lineHeight: "34px", color: "var(--muted)" }, text: unit }),
  );
}

export async function render(ctx: PageContext): Promise<void> {
  const root = h("div.pad");
  mount(ctx.outlet, root);
  root.append(loadingBlock("正在读取配额配置…"));
  const reload = (): void => void render(ctx);

  try {
    // 探测信息在存储页，但「为什么要求手填总额」得在这里讲，所以顺带拉一次；
    // 拉不到不影响本页主体功能，只是少了那段说明。
    const [quota, storage] = await Promise.all([
      request<QuotaAdmin>("/admin/quota"),
      request<StorageAdmin>("/admin/storage").catch(() => null),
    ]);
    // 保存接口会把保存后的整页生效值回传，就地用它重画：页面显示的一定是
    // 服务端确认过的数字，不再是一次额外的 GET 之后的猜测
    const paint = (fresh: QuotaAdmin): void => {
      mount(root, ...view(reload, paint, fresh, storage));
    };
    paint(quota);
  } catch (err) {
    mount(root, errorBlock(err, reload));
  }
}

function view(
  reload: () => void,
  paint: (fresh: QuotaAdmin) => void,
  data: QuotaAdmin,
  storage: StorageAdmin | null,
): Node[] {
  const draft: QuotaDraft = {
    mode: () => "auto",
    manualTotal: textInput({ value: toGigabytes(data.capacity.total ?? 0), mono: true }),
    defaultQuota: textInput({ value: `${toGigabytes(data.default_quota_bytes)} GB`, mono: true }),
    perUserMax: textInput({ value: `${toGigabytes(data.per_user_max)} GB`, mono: true }),
    autoEnabled: data.auto_close.enabled,
    autoOn: textInput({ value: String(data.auto_close.on), type: "number", mono: true }),
    autoOff: textInput({ value: String(data.auto_close.off), type: "number", mono: true }),
  };

  return [
    pageHead("配额与容量", "系统总额、默认注册配额、已分配合计的联动校验，以及容量触顶自动关闭注册。"),
    h("div.row", { style: { marginBottom: "14px" } }, capacityCard(draft, data, storage), allocationCard(data)),
    h("div.row", { style: { marginBottom: "14px" } }, defaultsCard(draft), autoCloseCard(draft, data)),
    topUsersCard(reload, data),
    note(
      "w",
      "改任何一个人的配额，服务端都会重算一遍可分配上限，超了直接拒绝保存并说明还差多少——不必等改完才发现。",
    ),
    h(
      "div.flexline",
      { style: { justifyContent: "flex-end", marginTop: "16px" } },
      h("button.btn.pri", {
        type: "button",
        text: "保存设置",
        on: { click: (event) => void save(reload, paint, draft, data, event.target as HTMLButtonElement) },
      }),
    ),
  ];
}

/** 系统总额：自动探测 / 手动配置。手动时给 GB 输入框，并解释为什么必须手填。 */
function capacityCard(draft: QuotaDraft, data: QuotaAdmin, storage: StorageAdmin | null): HTMLElement {
  const probe = storage?.capacity.probe ?? null;
  const modeSelect = select(
    [
      { value: "auto", label: "自动探测磁盘（本地盘 / 云盘）" },
      { value: "manual", label: "管理员手动配置（OSS 等网络挂载）" },
      { value: "hidden", label: "不显示磁盘容量" },
    ],
    data.capacity.mode,
  );
  draft.mode = () => (modeSelect.value === "manual" ? "manual" : modeSelect.value === "hidden" ? "hidden" : "auto");

  const totalWrap = field("系统总额", unitInput(draft.manualTotal, "GB"));
  const sync = (): void => {
    totalWrap.style.display = modeSelect.value === "manual" ? "" : "none";
  };
  sync();
  modeSelect.addEventListener("change", sync);

  const nodes: Node[] = [
    cardHead("系统总额"),
    field(
      "总额来源",
      modeSelect,
      `当前来源：${data.capacity.source}${data.capacity.note ? ` · ${data.capacity.note}` : ""}`,
    ),
    totalWrap,
  ];

  if (probe && !probe.trustworthy) {
    nodes.push(
      qtip(
        "warn",
        `检测到 ${probe.fs_name} 挂载`,
        h(
          "span",
          null,
          `statfs 读到的是挂载伪值，不是真实容量（f_type ${hexFsType(probe.fs_type)}）。`,
          "网络挂载的总额必须由管理员手动配置，页面上的占用率才准。",
        ),
      ),
    );
  }
  nodes.push(
    h("div.tip", {
      text: "本地磁盘选「自动探测」即可：占用率按宿主机整盘已用算。手填总额或网络挂载降级时只算本项目占用。",
    }),
  );
  return cardNode(null, ...nodes);
}

/** 分配汇总：总额、系统占用、可分配上限、已分配、剩余，进度条 + 百分比 + 校验规则。 */
function allocationCard(data: QuotaAdmin): HTMLElement {
  const total = data.capacity.total ?? 0;
  const hostUsed = data.capacity.host_used;
  // 上限由服务端给（总额 − 系统占用）；字段缺了就按同一口径推一遍
  const allocatable = data.allocatable ?? Math.max(0, total - (hostUsed ?? 0));
  const allocated = data.allocated ?? 0;
  const remaining = data.remaining ?? Math.max(0, allocatable - allocated);
  const ratio = data.allocated_percent ?? percent(allocated, allocatable || 1);

  const line = (label: string, value: Node | string, divided = false): HTMLElement =>
    h(
      "div.flexline",
      divided ? { style: { borderTop: "1px solid #f3f4f6", paddingTop: "6px" } } : null,
      h("span", { text: label }),
      h("span.spacer"),
      typeof value === "string" ? h("b", { text: value }) : value,
    );

  // 系统占用（整盘已用）必须先摆出来：它正是「可分配上限」比系统总额小的原因，
  // 只显示总额的话，下面的剩余额度会像算错了一样
  const rows: Node[] = [line("系统总额", formatBytes(total))];
  if (hostUsed != null) {
    rows.push(line("系统占用（宿主机整盘已用，不可分配）", formatBytes(hostUsed)));
    rows.push(line("可分配上限（系统总额 − 系统占用）", formatBytes(allocatable)));
  }
  rows.push(line("已分配配额合计（用户 + 管理员）", formatBytes(allocated)));
  rows.push(
    line(
      "剩余可分配",
      h("b", {
        text: formatBytes(remaining),
        style: { color: remaining > 0 ? "var(--success)" : "var(--danger)" },
      }),
      true,
    ),
  );

  const rule =
    hostUsed != null
      ? "所有用户配额合计 + 管理员账户配额 ≤ 系统总额 − 系统占用（盘上别的数据先占住的那部分分不出去）。超出的分配会被拒绝，并说明还差多少。"
      : "所有用户配额合计 + 管理员账户配额 ≤ 系统总额。超出的分配会被拒绝，并说明还差多少。";

  return card(
    "配额分配",
    h("div", { style: { fontSize: "13px", lineHeight: "2.4" } }, ...rows),
    h("div", { style: { marginTop: "12px" } }, progressBar(allocated, allocatable || 1)),
    h("div.tip", {
      text: `已分配 ${formatBytes(allocated)} / ${formatBytes(allocatable)}，占 ${Math.round(ratio)}%。`,
    }),
    h("div", { style: { marginTop: "12px" } }, qtip("err", "校验规则", rule)),
  );
}

/** 注册默认配额与单用户上限。 */
function defaultsCard(draft: QuotaDraft): HTMLElement {
  return card(
    "注册默认配额",
    h(
      "div.row",
      null,
      field("新用户默认配额", draft.defaultQuota),
      field("单用户配额上限", draft.perUserMax, "单个用户能分配到的最大值"),
    ),
    h("div.tip", {
      text: "注册时自动套用默认值，管理员可以事后单独调整。剩余可分配不足时，新用户会拿到剩余额度并提示管理员。",
    }),
  );
}

/** 容量触顶自动关闭注册：开关 + 触发/恢复阈值 + 当前状态。 */
function autoCloseCard(draft: QuotaDraft, data: QuotaAdmin): HTMLElement {
  const state = h("div", { style: { marginTop: "10px" } });

  // 占用率的口径与服务端一致：自动探测可信时按宿主机整盘已用算
  // （磁盘被谁占满都一样会停注册），手填/降级时退回本项目占用。
  const total = data.capacity.total ?? 0;
  const basis = data.capacity.host_used ?? data.capacity.used;
  const scope = data.capacity.host_used != null ? "宿主机磁盘占用" : "本项目占用";
  // 阈值是整数百分比，这里保留一位小数，免得显示「90%」却还没触发
  const usedRatio = total > 0 ? Math.min(100, (basis / total) * 100) : 0;
  const usedText = `${scope} ${formatBytes(basis)} / ${formatBytes(total)}，约 ${usedRatio.toFixed(1)}%`;

  const refreshState = (): void => {
    if (!draft.autoEnabled) {
      mount(
        state,
        qtip(
          "info",
          "未启用自动关闭",
          `${usedText}。注册入口由「系统设置」里的开关控制。`,
        ),
      );
      return;
    }
    // 占用率与阈值的比较用服务端给的那个数（口径与服务端判定一致），
    // 界面自己按 capacity 再算一遍迟早会和判定对不上
    const percent = data.auto_close.percent;
    const overThreshold = percent != null && percent >= Number(draft.autoOn.value);
    if (data.auto_close.register_open) {
      if (overThreshold || data.auto_close.manual_override) {
        // 「已超阈值但注册是开的」只有一种情形：管理员手动开过，这一层让位
        mount(
          state,
          qtip(
            "warn",
            "当前状态：注册开放（你手动开启的）",
            h(
              "span",
              null,
              `${usedText}，已超过 ${draft.autoOn.value}% 触发阈值，但注册是你手动打开的，自动关闭不会把它按回去。`,
              h("br"),
              `占用降到 ${draft.autoOff.value}% 以下、之后再涨过 ${draft.autoOn.value}% 时，才会重新触发自动关闭。`,
            ),
          ),
        );
        return;
      }
      mount(
        state,
        qtip(
          "ok",
          "当前状态：注册开放",
          h(
            "span",
            null,
            `${usedText}，未触及 ${draft.autoOn.value}% 阈值。`,
            h("br"),
            h("b", { text: "已注册用户完全不受影响" }),
            "，照常上传。",
          ),
        ),
      );
      return;
    }
    mount(
      state,
      qtip(
        "warn",
        "当前状态：注册已关闭",
        h(
          "span",
          null,
          data.auto_close.closed_reason || `${usedText}，已触及 ${draft.autoOn.value}% 阈值。`,
          h("br"),
          `降到 ${draft.autoOff.value}% 以下会自动重新开放；已注册用户不受影响。`,
        ),
      ),
    );
  };
  refreshState();
  draft.autoOn.addEventListener("input", refreshState);
  draft.autoOff.addEventListener("input", refreshState);

  return cardNode(
    cardHead("容量触顶自动关闭注册"),
    switchRow(
      "启用自动关闭",
      "自动探测模式下按宿主机磁盘占用判定（整盘写满就停），手填/降级时按本项目占用",
      data.auto_close.enabled,
      (next) => {
        draft.autoEnabled = next;
        refreshState();
      },
    ),
    h(
      "div.row",
      { style: { marginTop: "10px" } },
      field("触发阈值", unitInput(draft.autoOn, "%")),
      field("恢复阈值", unitInput(draft.autoOff, "%"), "降到这个值以下自动重新开放注册"),
    ),
    state,
  );
}

/** 分配明细：前 5 名占用，可逐个调配额。 */
function topUsersCard(reload: () => void, data: QuotaAdmin): HTMLElement {
  const users = data.top_users ?? [];
  if (users.length === 0) {
    return cardNode(cardHead("分配明细（前 5 名占用）"), emptyState("📊", "还没有用户占用配额。"));
  }

  const rows = users.map((user: AdminUserRow) => {
    const role: Node[] = [roleBadge(user.role)];
    if (user.status !== "active") role.push(statusBadge(user.status, user.status_label));
    const adjust = h("button.btn.sm", {
      type: "button",
      text: "调配额",
      on: { click: () => void adjustQuota(reload, user) },
    });
    return [
      h("span.mono", { text: user.uid }),
      txt(user.email),
      h("div.flexline", null, ...role),
      txt(formatBytes(user.quota_bytes)),
      txt(formatBytes(user.used_bytes)),
      h("div.mini-metric", null, progressBar(user.used_bytes, user.quota_bytes || 1)),
      adjust,
    ];
  });

  return cardNode(
    cardHead("分配明细（前 5 名占用）", badge(`${formatNumber(users.length)} 行`, "mute")),
    responsiveTable(["UID", "邮箱", "角色", "配额", "已用", "占比", "操作"], rows),
  );
}

/** 单个用户调配额：弹窗输入新配额，超额时把服务端的「还差多少」原样显示出来。 */
async function adjustQuota(reload: () => void, user: AdminUserRow): Promise<void> {
  const input = await promptDialog({
    title: `调整 u_${user.uid} 的配额`,
    label: "新配额（GB）",
    value: toGigabytes(user.quota_bytes),
    placeholder: "50",
    confirmText: "保存",
    hint: `当前 ${formatBytes(user.quota_bytes)}，已用 ${formatBytes(user.used_bytes)}。可以带单位，例如 50 GB、500 MB。`,
  });
  if (input === null) return;

  const quota = parseSizeInput(input);
  if (quota === null || quota <= 0) {
    toastError("容量格式不对：填数字即按 GB 计算，也可以写 500 MB");
    return;
  }
  try {
    await request(`/admin/users/${user.id}/quota`, { method: "POST", json: { quota_bytes: quota } });
    toastOk("配额已更新");
    reload();
  } catch (err) {
    if (err instanceof ApiError && err.detail) {
      toastError(`${err.message} · ${readableDetail(err.detail)}`);
      return;
    }
    toastError(err);
  }
}

/**
 * 保存整页配置。
 *
 * 服务端会重算一遍可分配上限（本地盘要问一次磁盘占用），偶尔要一两秒，
 * 所以按钮上先写清楚在干什么。保存完之后不管成功失败都必须有反馈：
 * 「点了没反应」是最难查的一类投诉——分不清是没保存上还是保存了但没变化。
 */
async function save(
  reload: () => void,
  paint: (fresh: QuotaAdmin) => void,
  draft: QuotaDraft,
  was: QuotaAdmin,
  button: HTMLButtonElement,
): Promise<void> {
  const defaultBytes = parseSizeInput(draft.defaultQuota.value);
  const maxBytes = parseSizeInput(draft.perUserMax.value);
  if (defaultBytes === null || defaultBytes <= 0) {
    toastError("新用户默认配额填不对：填数字即按 GB 计算");
    return;
  }
  if (maxBytes === null || maxBytes <= 0) {
    toastError("单用户配额上限填不对：填数字即按 GB 计算");
    return;
  }
  if (maxBytes < defaultBytes) {
    toastError("单用户配额上限不能小于新用户默认配额");
    return;
  }

  const body: Record<string, unknown> = {
    capacity_mode: draft.mode(),
    default_quota_bytes: defaultBytes,
    per_user_max: maxBytes,
    auto_close_enabled: draft.autoEnabled,
    auto_close_on: Number(draft.autoOn.value),
    auto_close_off: Number(draft.autoOff.value),
  };
  if (draft.mode() === "manual") {
    const manual = parseSizeInput(draft.manualTotal.value, "GB");
    if (manual === null || manual <= 0) {
      toastError("手动模式要填系统总额");
      return;
    }
    body.manual_total = manual;
  }

  // 与页面上加载到的值逐个比：什么都没改就直说，别让一次空点击看起来像坏了
  const changes = describeChanges(was, {
    capacity_mode: draft.mode(),
    default_quota_bytes: defaultBytes,
    per_user_max: maxBytes,
    auto_close_enabled: draft.autoEnabled,
    auto_close_on: Number(draft.autoOn.value),
    auto_close_off: Number(draft.autoOff.value),
    manual_total: typeof body.manual_total === "number" ? body.manual_total : null,
  });
  if (changes.length === 0) {
    toast("没有需要保存的改动");
    return;
  }

  button.disabled = true;
  button.textContent = "保存中…";
  // 超过两秒还没回来（多半在等磁盘探测）就把话说透，别让人以为卡死了
  const slow = window.setTimeout(() => {
    button.textContent = "重算容量中…";
  }, 2000);
  try {
    const fresh = await request<QuotaAdmin>("/admin/quota", { method: "PUT", json: body });
    toastOk(`已保存：${changes.join("；")}`);
    // 服务端把保存后的整页生效值回给我们了：直接用它重画，不必再拉一次
    paint(fresh);
  } catch (err) {
    if (err instanceof ApiError && err.detail) {
      // 契约保证超额时 detail 里带「还差多少」，这一句必须让人看见
      toastError(`${err.message} · ${readableDetail(err.detail)}`);
    } else {
      toastError(err);
    }
  } finally {
    window.clearTimeout(slow);
    button.disabled = false;
    button.textContent = "保存设置";
  }
}

/** 把「这次改了哪几项」写成一句人话，一项都没改就返回空数组。 */
function describeChanges(
  was: QuotaAdmin,
  next: {
    capacity_mode: string;
    default_quota_bytes: number;
    per_user_max: number;
    auto_close_enabled: boolean;
    auto_close_on: number;
    auto_close_off: number;
    manual_total: number | null;
  },
): string[] {
  const out: string[] = [];
  if (was.capacity.mode !== next.capacity_mode) {
    out.push(`容量口径改为「${MODE_LABEL[next.capacity_mode] ?? next.capacity_mode}」`);
  }
  if (was.default_quota_bytes !== next.default_quota_bytes) {
    out.push(`新用户默认配额 ${formatBytes(was.default_quota_bytes)} → ${formatBytes(next.default_quota_bytes)}`);
  }
  if (was.per_user_max !== next.per_user_max) {
    out.push(`单用户上限 ${formatBytes(was.per_user_max)} → ${formatBytes(next.per_user_max)}`);
  }
  if (was.auto_close.enabled !== next.auto_close_enabled) {
    out.push(`容量触顶自动关闭注册${next.auto_close_enabled ? "打开" : "关闭"}`);
  }
  if (was.auto_close.on !== next.auto_close_on) {
    out.push(`触发阈值 ${was.auto_close.on}% → ${next.auto_close_on}%`);
  }
  if (was.auto_close.off !== next.auto_close_off) {
    out.push(`恢复阈值 ${was.auto_close.off}% → ${next.auto_close_off}%`);
  }
  if (next.manual_total !== null && was.capacity.total !== next.manual_total) {
    out.push(`系统总额 ${next.manual_total ? formatBytes(next.manual_total) : "未填"}`);
  }
  return out;
}

/** 三种容量口径的界面说法，改了什么要能念出来。 */
const MODE_LABEL: Record<string, string> = {
  auto: "自动探测磁盘",
  manual: "管理员手动配置",
  hidden: "不显示磁盘容量",
};
