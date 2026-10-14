/** 展示层格式化：字节、时间、尺寸、状态标签。 */

const UNITS = ["B", "KB", "MB", "GB", "TB", "PB"];

/** 2411724 → `2.4 MB`。与界面文案一致（一位小数，去掉多余的小数位）。 */
export function formatBytes(bytes: number | null | undefined): string {
  if (bytes === null || bytes === undefined || Number.isNaN(bytes)) return "—";
  if (bytes <= 0) return "0 B";
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), UNITS.length - 1);
  const value = bytes / Math.pow(1024, exponent);
  const digits = exponent === 0 ? 0 : value >= 100 ? 0 : value >= 10 ? 1 : 1;
  const text = value.toFixed(digits).replace(/\.0+$/, "");
  return `${text} ${UNITS[exponent]}`;
}

/** 严格按界面口径输出（2.4 GB / 10 GB），用于配额展示。 */
export function formatQuota(used: number, total: number): string {
  return `${formatBytes(used)} / ${formatBytes(total)}`;
}

export function percent(used: number, total: number): number {
  if (!total || total <= 0) return 0;
  return Math.min(100, Math.round((used / total) * 100));
}

/** 进度条色调：<70 主色、<90 黄、其余红。 */
export function barTone(ratio: number): "" | "w" | "d" {
  if (ratio >= 0.9) return "d";
  if (ratio >= 0.7) return "w";
  return "";
}

export function formatDimensions(width?: number | null, height?: number | null): string {
  if (!width || !height) return "—";
  return `${width}×${height}`;
}

function pad(value: number): string {
  return String(value).padStart(2, "0");
}

/** 相对时间：刚刚 / 12 分钟前 / 2 小时前 / 昨天 / 3 天前 / 2026-08-02。 */
export function relativeTime(input: string | null | undefined): string {
  if (!input) return "—";
  const then = new Date(input);
  if (Number.isNaN(then.getTime())) return "—";
  const now = Date.now();
  const diff = now - then.getTime();
  const minute = 60_000;
  const hour = 60 * minute;
  const day = 24 * hour;

  if (diff < 0) return formatDateTime(input);
  if (diff < minute) return "刚刚";
  if (diff < hour) return `${Math.floor(diff / minute)} 分钟前`;
  if (diff < day) return `${Math.floor(diff / hour)} 小时前`;
  if (diff < 2 * day) return "昨天";
  if (diff < 30 * day) return `${Math.floor(diff / day)} 天前`;
  return formatDate(input);
}

export function formatDate(input: string | null | undefined): string {
  if (!input) return "—";
  const date = new Date(input);
  if (Number.isNaN(date.getTime())) return "—";
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
}

/** 列表里用的紧凑格式：`10-14 12:31`。 */
export function formatShortDateTime(input: string | null | undefined): string {
  if (!input) return "—";
  const date = new Date(input);
  if (Number.isNaN(date.getTime())) return "—";
  return `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

/** 完整时间：`2026-10-14 12:31`。 */
export function formatDateTime(input: string | null | undefined): string {
  if (!input) return "—";
  const date = new Date(input);
  if (Number.isNaN(date.getTime())) return "—";
  return `${formatDate(input)} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

/** 表单用的本地时间串：`2026-10-15 00:00`。 */
export function toLocalInput(input: string | null | undefined): string {
  if (!input) return "";
  const date = new Date(input);
  if (Number.isNaN(date.getTime())) return "";
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

/** 把 `<input type="datetime-local">` 的值转成 RFC3339（UTC）。 */
export function fromLocalInput(value: string | null | undefined): string | null {
  if (!value) return null;
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return null;
  return date.toISOString().replace(/\.\d{3}Z$/, "Z");
}

/** 秒数 → `6 分 12 秒`。 */
export function formatDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return "0 秒";
  const minutes = Math.floor(seconds / 60);
  const rest = Math.round(seconds % 60);
  if (minutes <= 0) return `${rest} 秒`;
  if (minutes < 60) return `${minutes} 分 ${rest} 秒`;
  const hours = Math.floor(minutes / 60);
  return `${hours} 小时 ${minutes % 60} 分`;
}

/** 状态与标签的中文名，前后端只在这里对一次口径。 */
export const STATUS_LABEL: Record<string, string> = {
  pending: "待验证",
  active: "正常",
  suspended: "已暂停",
  banned: "已封禁",
};

export const ROLE_LABEL: Record<string, string> = {
  admin: "管理员",
  user: "普通用户",
};

export const VISIBILITY_LABEL: Record<string, string> = {
  public: "公开",
  private: "私有",
};

export const VISIBILITY_SCOPE_LABEL: Record<string, string> = {
  all: "全站",
  register: "仅注册",
  login: "仅登录",
  upload: "仅上传",
};

export function statusBadgeClass(status: string): string {
  switch (status) {
    case "active":
      return "ok";
    case "pending":
      return "warn";
    case "suspended":
      return "warn";
    case "banned":
      return "err";
    default:
      return "mute";
  }
}

/** 数字千分位：1284 → `1,284`。 */
export function formatNumber(value: number | null | undefined): string {
  if (value === null || value === undefined) return "—";
  return value.toLocaleString("zh-CN");
}

/** 把用户输入的口径（`10 GB`）解析成字节数。 */
export function parseSize(input: string): number | null {
  const match = input.trim().match(/^([\d.]+)\s*(B|KB|MB|GB|TB)?$/i);
  if (!match) {
    const plain = Number(input.trim());
    return Number.isFinite(plain) ? plain : null;
  }
  const value = Number(match[1]);
  if (!Number.isFinite(value)) return null;
  const unit = (match[2] ?? "B").toUpperCase();
  const index = UNITS.indexOf(unit);
  return Math.round(value * Math.pow(1024, index < 0 ? 0 : index));
}

/** 字节数转成表单里的 GB 字符串。 */
export function toGigabytes(bytes: number): string {
  const gb = bytes / 1024 / 1024 / 1024;
  return gb >= 10 ? gb.toFixed(0) : gb.toFixed(1).replace(/\.0$/, "");
}

/** 文件名截断，保留扩展名。 */
export function shortName(name: string, max = 22): string {
  if (name.length <= max) return name;
  const dot = name.lastIndexOf(".");
  const ext = dot > 0 ? name.slice(dot) : "";
  const stem = dot > 0 ? name.slice(0, dot) : name;
  const keep = Math.max(4, max - ext.length - 1);
  return `${stem.slice(0, keep)}…${ext}`;
}

/** 从 sha256 里取前 8 位，界面上显示成 `a3f9c1b8….webp`。 */
export function shortHash(sha256: string, ext?: string | null): string {
  const head = sha256.slice(0, 8);
  return ext ? `${head}….${ext}` : `${head}…`;
}

export function extOf(name: string): string {
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot + 1).toLowerCase() : "";
}

/** 中间截断的哈希，用于列表里的次要信息。 */
export function middleEllipsis(input: string, head = 8, tail = 4): string {
  if (input.length <= head + tail + 1) return input;
  return `${input.slice(0, head)}…${input.slice(-tail)}`;
}
