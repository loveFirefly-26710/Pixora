/**
 * 上传队列引擎。
 *
 * 一次选 1000 张要能扛住，所以任务拆成四段，全部在 Worker 里做重活：
 *   1. 算 SHA-256（hash.worker）
 *   2. 批量 POST /blobs/exists 预检，服务器已有的直接跳过，不发字节
 *   3. 只对 missing 的生成 WebP 缩略图（thumb.worker）
 *   4. 按并发逐张 POST /upload，失败指数退避，最多 3 次；429 读 Retry-After 尊重服务端节奏
 *
 * 队列状态通过事件广播出去，UI 只订阅不轮询。
 * 队列本身不跨页面刷新保留：文件本体浏览器存不下，而只留「已完成」的记录
 * 会在下次打开时堆在待上传预览里，得一张张点叉才能清掉。刷新即从空队列开始，
 * 上传设置（相册、标签、可见性、并发）另存在 core/store.ts。
 */

import { ApiError, request } from "../core/api";
import { extOf, normalizeExt } from "../core/format";
import { getState, loadSite } from "../core/state";
import {
  loadUploadSettings,
  loadLocalUploadSettings,
  saveUploadSettings,
  type StoredUploadSettings,
} from "../core/store";
import type { HashCheck, UploadResult } from "../core/types";

/**
 * 上传归属：管理员在用户视图里代目标用户上传时由页面设置。
 * 普通页面为 null，服务端按登录账号或访客账号处理。
 */
let ownerUid: string | null = null;

export function setUploadOwnerUid(uid: string | null): void {
  ownerUid = uid;
}

/** 并发数只允许 1 / 2 / 4：再大对单账号上传限额和服务器都没有好处。 */
export type Concurrency = 1 | 2 | 4;
/**
 * 并发档位的上限：策略里的「单用户并发上传数」。
 *
 * 用户可选 1..上限，而不是在一组固定的档位里挑——管理员配 5，就该能选到 5。
 * 拿不到策略时按 1 走（服务端本来也会按生效策略拦）。
 */
export function concurrencyCap(): number {
  const state = getState();
  const value =
    state.user?.limits?.max_concurrent_uploads ??
    state.site?.guest_upload?.max_concurrent_uploads ??
    1;
  return Number.isFinite(value) && value >= 1 ? Math.floor(value) : 1;
}

/**
 * 下拉里列出来的档位。档位多的时候不必全列（32 个选项没人看），
 * 这种情况下由面板换成数字输入，见 `panel.ts`。
 */
export function concurrencyOptions(cap = concurrencyCap()): number[] {
  const top = Math.max(1, Math.min(cap, 32));
  return Array.from({ length: top }, (_, index) => index + 1);
}

export type QueueStatus =
  | "hashing"
  | "ready"
  | "uploading"
  | "done"
  | "skipped"
  | "failed"
  | "needs-reselect";

export interface QueueItem {
  id: string;
  /** 刷新后恢复的项没有文件，靠 status=needs-reselect 提示用户重选 */
  file: File | null;
  name: string;
  size: number;
  mime: string;
  sha256: string | null;
  thumb: Blob | null;
  /** 缩略图没带上的原因（格式解不了、浏览器不支持 WebP 编码等） */
  thumbNote: string | null;
  /** 本地预览用的 objectURL（原文件），移除时 revoke */
  previewUrl: string | null;
  status: QueueStatus;
  progress: number;
  error: string | null;
  note: string | null;
  retries: number;
  addedAt: number;
  folderTags: string[];
  result: UploadResult | null;
  durationMs: number | null;
  /** 预检已完成（或不需要预检），可以进入上传 */
  checked: boolean;
  /** 缩略图已尝试过（成败都算），避免失败项反复重试缩略图 */
  thumbTried: boolean;
  /** 退避到期时间，未到期不派发 */
  retryAt: number | null;
}

export interface QueueStats {
  total: number;
  success: number;
  skipped: number;
  waiting: number;
  failed: number;
  needsReselect: number;
  bytesTotal: number;
  bytesDone: number;
  percent: number;
}

export type QueueEvent =
  | { kind: "items" }
  | { kind: "item"; id: string }
  | { kind: "state" }
  | { kind: "notice"; tone: "info" | "ok" | "warn" | "err"; text: string }
  | { kind: "error"; error: unknown }
  | { kind: "finished"; stats: QueueStats };

export type QueueListener = (event: QueueEvent) => void;

export interface IncomingFile {
  file: File;
  /** 选择文件夹/拖拽目录时保留的相对路径，用于推导标签 */
  path?: string;
}

const MAX_RETRIES = 3;
/**
 * 网络类失败的次数。以前给到 8 次（服务端重启要等半分钟），但审核服务不可用
 * 也会落进这一类：用户看着它重试八轮、弹一屏提醒，实际恢复不了。统一最多
 * 3 次重试，失败后明确提示一次，把选择权交回给用户。
 */
const MAX_NETWORK_RETRIES = 3;
/** 预检一批最多发多少个哈希。契约上限 1000，留出余量防止哈希串太长；
 *  实际分块还受「单批次最大张数」约束，取两者的小值。 */
const PREFLIGHT_BATCH = 400;
/** Worker 单任务超时：超过就认定 Worker 坏了，terminate 后走降级路径。 */
const WORKER_TIMEOUT = 30_000;

// ---------- 纯函数（可单测） ----------

/** 分片切分，预检/批量请求都用它。 */
export function chunk<T>(list: T[], size: number): T[][] {
  const out: T[][] = [];
  if (size <= 0) return out;
  for (let i = 0; i < list.length; i += size) out.push(list.slice(i, i + size));
  return out;
}

/**
 * 重试延迟：客户端指数退避 0.8s → 1.6s → 3.2s（封顶 8s），
 * 服务端给了 Retry-After 就取两者更大值——限流时说 30 秒就等 30 秒，
 * 抢跑只会再吃一个 429。
 */
export function backoffDelay(attempt: number, retryAfterSeconds?: number | null): number {
  const base = Math.min(800 * 2 ** Math.max(0, attempt - 1), 8000);
  const server = retryAfterSeconds && retryAfterSeconds > 0 ? retryAfterSeconds * 1000 : 0;
  return Math.max(base, server);
}

/** 永久性错误：重试多少次都是一样的结果，直接判失败并说明原因。 */
const PERMANENT_CODES = new Set<string>([
  "BAD_REQUEST",
  "UNAUTHORIZED",
  "FORBIDDEN",
  "NOT_FOUND",
  "QUOTA_EXCEEDED",
  "FILE_TOO_LARGE",
  "UNSUPPORTED_FORMAT",
  "INVALID_IMAGE",
  "CONTENT_REJECTED",
  "TERMS_REQUIRED",
  "ACCOUNT_PENDING",
  "ACCOUNT_SUSPENDED",
  "ACCOUNT_BANNED",
]);

export function isPermanentError(err: unknown): boolean {
  if (err instanceof ApiError) {
    if (PERMANENT_CODES.has(err.code)) return true;
    return !err.isTransient;
  }
  return false;
}

/**
 * 给用户看的上传失败文案。
 *
 * 服务端的 detail/message 里可能带着审核服务地址、上游响应、请求路径之类的
 * 排查信息，这些不该出现在提示里。这里按错误码换成一句能行动的话；只有
 * 「文件太大」「格式不对」这类本来就是给用户看的业务错误才原样保留。
 */
export function friendlyUploadError(err: unknown): string {
  if (!(err instanceof ApiError)) return "上传失败，请稍后重试。";
  switch (err.code) {
    case "CONTENT_REJECTED":
      return "图片未通过内容审核，请更换图片后再试。";
    case "FILE_TOO_LARGE":
      // 服务端消息里带着实际大小与上限，本来就是给用户看的
      return err.message;
    case "UNSUPPORTED_FORMAT":
      return err.message;
    case "INVALID_IMAGE":
      return invalidImageMessage(err.message);
    case "BAD_REQUEST":
      if (err.message.includes("读取上传内容失败")) {
        return "图片上传中断，请重新选择文件后再试。";
      }
      return err.message || "这张图片无法上传，请调整后重试。";
    case "STORAGE_UNAVAILABLE":
      return "上传服务暂时不可用（内容审核或存储服务可能未就绪），请稍后重试。";
    case "NETWORK":
      return "网络连接中断，上传没有完成，请检查网络后重试。";
    case "INTERNAL":
      return "服务端暂时无法完成上传，请稍后重试。";
    case "RATE_LIMITED":
    case "QUEUE_FULL":
      // 服务端已经给出了等待时长，照搬比笼统的「稍后重试」有用
      return err.message;
    default:
      return err.message || "上传失败，请稍后重试。";
  }
}

/** 像素数转成人能一眼读懂的量级（8000 万、1.2 亿）。 */
function readablePixels(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return "";
  if (value >= 100_000_000) return `${(value / 100_000_000).toFixed(1).replace(/\.0$/, "")} 亿`;
  if (value >= 10_000) return `${Math.round(value / 10_000)} 万`;
  return String(value);
}

/**
 * 尺寸类错误单独说清楚：像素上限、长宽上限是站点的明确规定，
 * 重试不会变好，用户需要知道的是「图太大，缩一下再传」。
 */
function invalidImageMessage(message: string): string {
  const text = message.replace(/^文件不是有效的图片[:：]\s*/, "");
  const pixels = text.match(/像素数\s*(\d+)\s*超出上限\s*(\d+)(?:（(\d+)×(\d+)）)?/);
  if (pixels) {
    const actual = Number(pixels[1]);
    const limit = Number(pixels[2]);
    const dims = pixels[3] && pixels[4] ? `（${pixels[3]}×${pixels[4]}）` : "";
    return `图片分辨率过大：${readablePixels(actual)}像素${dims}，超过本站上限 ${readablePixels(limit)}像素。请缩小图片后再试。`;
  }
  const width = text.match(/宽度\s*(\d+)\s*超出上限\s*(\d+)/);
  if (width) {
    return `图片宽度太大：${width[1]} 像素，本站上限 ${width[2]} 像素。请缩小图片后再试。`;
  }
  const height = text.match(/高度\s*(\d+)\s*超出上限\s*(\d+)/);
  if (height) {
    return `图片高度太大：${height[1]} 像素，本站上限 ${height[2]} 像素。请缩小图片后再试。`;
  }
  if (text.includes("文件为空")) return "文件是空的，请重新选择。";
  if (text.includes("无法识别") || text.includes("格式异常") || text.includes("SVG")) {
    return "无法识别这张图片的格式，请换一张图片。";
  }
  return "这张图片不符合本站要求，请换一张图片后再试。";
}

/** 面板只负责提示，不接触原始错误里的 detail；用于 toast 的副本去掉它们。 */
function publicUploadError(err: unknown): unknown {
  if (!(err instanceof ApiError)) return new Error(friendlyUploadError(err));
  return new ApiError(err.status, {
    code: err.code,
    message: friendlyUploadError(err),
  }, err.retryAfter);
}

/** 四类统计口径：成功 / 跳过 / 等待中 / 失败；跳过单独算一类，它没消耗带宽也没占盘。 */
export function computeStats(items: QueueItem[]): QueueStats {
  let success = 0;
  let skipped = 0;
  let waiting = 0;
  let failed = 0;
  let needsReselect = 0;
  let bytesTotal = 0;
  let bytesDone = 0;
  for (const item of items) {
    bytesTotal += item.size;
    switch (item.status) {
      case "done":
        success += 1;
        bytesDone += item.size;
        break;
      case "skipped":
        skipped += 1;
        bytesDone += item.size;
        break;
      case "failed":
        failed += 1;
        break;
      case "needs-reselect":
        needsReselect += 1;
        waiting += 1;
        break;
      default:
        waiting += 1;
        bytesDone += Math.round((item.size * item.progress) / 100);
        break;
    }
  }
  const total = items.length;
  const settled = success + skipped + failed;
  return {
    total,
    success,
    skipped,
    waiting,
    failed,
    needsReselect,
    bytesTotal,
    bytesDone,
    percent: total > 0 ? Math.round((settled / total) * 100) : 0,
  };
}

/** 预计剩余时间：用最近成功上传的耗时的平均值估算；样本不足时保守取 2.5 秒/张。 */
export function estimateRemainingMs(
  samples: number[],
  pending: number,
  concurrency: number,
): number {
  if (pending <= 0) return 0;
  const usable = samples.filter((value) => Number.isFinite(value) && value > 0).slice(-20);
  const average =
    usable.length > 0 ? usable.reduce((sum, value) => sum + value, 0) / usable.length : 2500;
  return (average * pending) / Math.max(1, concurrency);
}

/** 标签合并：设置里的共用标签 + 文件夹路径推出的标签，去重且限长。 */
export function mergeTags(base: string, extra: string[] = []): string {
  const out: string[] = [];
  const push = (value: string): void => {
    const tag = value.trim();
    if (tag && !out.includes(tag) && out.length < 20) out.push(tag);
  };
  base.split(",").forEach(push);
  extra.forEach(push);
  return out.join(",");
}

/**
 * 从相对路径推导标签：丢掉文件名与根目录名（根目录是用户挑的那个文件夹本身，
 * 做成标签没有信息量），最多取最后两级，再多标签就没有分类意义了。
 */
export function folderTagsFromPath(relativePath: string | undefined): string[] {
  if (!relativePath) return [];
  const parts = relativePath.split(/[\\/]/).filter(Boolean);
  return parts.slice(1, -1).slice(-2);
}

// ---------- Worker 桥 ----------

/**
 * Worker 的 URL 用拼接而不是字面量：两个 Worker 由 build.mjs 单独产出到 dist/workers/，
 * 写成字面量会让 esbuild 把 `new URL("./x.js", import.meta.url)` 当资源再去打包一遍。
 */
const WORKER_BASE: string = import.meta.url;

function spawnWorker(file: string): Worker {
  return new Worker(new URL(`./workers/${file}`, WORKER_BASE), { type: "module" });
}

interface BridgeTask {
  resolve: (message: { type: string } & Record<string, unknown>) => void;
  reject: (error: Error) => void;
  onProgress?: (message: { type: string } & Record<string, unknown>) => void;
  timer: number;
}

/** 极简的 Worker 请求/响应桥：按 id 派发，超时即认定 Worker 失效并整体作废。 */
class WorkerBridge {
  private worker: Worker | null = null;
  private failed = false;
  private tasks = new Map<string, BridgeTask>();
  private counter = 0;

  constructor(private readonly file: string) {}

  get unavailable(): boolean {
    return this.failed;
  }

  private ensure(): Worker | null {
    if (this.failed) return null;
    if (this.worker) return this.worker;
    try {
      const worker = spawnWorker(this.file);
      worker.addEventListener("message", (event: MessageEvent) => {
        const data = event.data as { type?: unknown; id?: unknown } | null;
        if (!data || typeof data.id !== "string" || typeof data.type !== "string") return;
        const task = this.tasks.get(data.id);
        if (!task) return;
        if (data.type === "progress") {
          task.onProgress?.(data as { type: string } & Record<string, unknown>);
          return;
        }
        this.tasks.delete(data.id);
        window.clearTimeout(task.timer);
        task.resolve(data as { type: string } & Record<string, unknown>);
      });
      worker.addEventListener("error", () => this.invalidate("Worker 无法加载"));
      this.worker = worker;
      return worker;
    } catch {
      this.failed = true;
      return null;
    }
  }

  /** 发送任务。resolve 的是 Worker 的最终消息（done 或 error），progress 走回调。 */
  send(
    task: Record<string, unknown>,
    onProgress?: (message: { type: string } & Record<string, unknown>) => void,
    timeoutMs = WORKER_TIMEOUT,
  ): Promise<{ type: string } & Record<string, unknown>> {
    const worker = this.ensure();
    if (!worker) return Promise.reject(new Error("Worker 不可用"));
    this.counter += 1;
    const id = `t${this.counter}`;
    return new Promise((resolve, reject) => {
      const timer = window.setTimeout(() => {
        this.tasks.delete(id);
        this.invalidate("Worker 响应超时");
      }, timeoutMs);
      this.tasks.set(id, { resolve, reject, onProgress, timer });
      worker.postMessage({ ...task, id });
    });
  }

  private invalidate(reason: string): void {
    this.failed = true;
    try {
      this.worker?.terminate();
    } catch {
      /* 已经终止 */
    }
    this.worker = null;
    for (const task of this.tasks.values()) {
      window.clearTimeout(task.timer);
      task.reject(new Error(reason));
    }
    this.tasks.clear();
  }
}

// ---------- 引擎 ----------

function createId(index: number): string {
  return `q${Date.now().toString(36)}${index.toString(36)}${Math.random().toString(36).slice(2, 6)}`;
}

/**
 * 当前访问者能不能上传。
 *
 * 登录用户按自己的账号走；未登录的人只在站点开着「允许游客上传」时能传，
 * 依据是 /site 下发的 `guest_upload`（关着时它是 null）。
 *
 * 这一道拦在入队口，是为了不替未授权的人白干活：算 SHA-256、生成缩略图、
 * 预检、上传全都要花本机与服务器的资源，而「选中文件」这一步不该触发其中任何一环。
 * 真正的许可仍在服务端——关着游客上传时 `/blobs/exists` 与 `/upload` 直接回 401，
 * 文件哈希到不了服务端。
 */
export function uploadAllowed(): boolean {
  const state = getState();
  if (state.user) return true;
  return state.site?.guest_upload?.enabled === true;
}

/** 服务端格式白名单优先；拿不到站点信息时退回常见图片扩展名，宁可多收也别把 HEIC 挡在外面。
 *  登录后以「我的生效策略」为准——管理员可以按用户单独放宽或收紧格式白名单。 */
function isLikelyImage(file: File): boolean {
  const state = getState();
  const formats =
    state.user?.limits?.formats ?? state.site?.guest_upload?.formats ?? state.site?.formats ?? [];
  // 两边都归一：服务端给的白名单只有 jpg / tiff / heic 这几个名字，
  // 用户文件却常见 .jpeg / .tif / .heif。不归一的话这些文件会被当成
  // 「非图片」直接丢掉，而服务端其实完全收得下
  const ext = normalizeExt(extOf(file.name));
  if (formats.length > 0) return formats.map(normalizeExt).includes(ext);
  if (file.type.startsWith("image/")) return true;
  return ["jpg", "png", "gif", "webp", "avif", "heic", "tiff", "bmp", "svg"].includes(ext);
}

/**
 * 单批次最大张数：一次选择最多收多少张。
 *
 * 与「队列深度阈值」不是一回事：队列深度管的是「浏览器里同时堆着多少
 * 待传项」，单批张数管的是「这一次选择收多少」。一次选择（选文件、拖文件夹）
 * 就是一批，超出的部分不加入，得再选一次。
 *
 * 服务端也在同一处卡：批量预检一次收的哈希数超过它就直接 400，
 * 因为上传是逐张发的，预检是唯一能按批拦住的地方。
 * 登录后按我的生效策略（可能被按用户覆盖），没登录回落到站点全局值。
 * 0 表示不限。
 */
function effectiveBatchLimit(): number {
  const state = getState();
  const value =
    state.user?.limits?.max_batch_size ??
    state.site?.guest_upload?.max_batch_size ??
    state.site?.max_batch_size ??
    1000;
  if (!Number.isFinite(value) || value <= 0) return 0;
  return Math.floor(value);
}

/**
 * 队列深度阈值：队列里最多堆多少张。
 *
 * 服务端用同一个值卡两处：在途任务满了就回 429，预检一次收的哈希数超了
 * 就回 400。所以本地队列也必须按它限长，不然会出现「本地排了 900 张、
 * 第一张预检就被服务端拒掉」的错位——用户看到的是「上传失败」，
 * 而不是「你一次选太多了」。
 * 0 表示不限。
 */
function effectiveQueueDepth(): number {
  const state = getState();
  const value =
    state.user?.limits?.queue_depth ??
    state.site?.guest_upload?.queue_depth ??
    state.site?.queue_depth ??
    0;
  if (!Number.isFinite(value) || value <= 0) return 0;
  return Math.floor(value);
}



class UploadQueue {
  private items: QueueItem[] = [];
  private settings: StoredUploadSettings = {
    albumId: null,
    tags: "",
    visibility: "private",
    concurrency: 1,
  };
  private listeners = new Set<QueueListener>();
  private uploadedHooks = new Set<(result: UploadResult) => void>();

  private running = false;
  private paused = false;
  private active = new Map<string, AbortController>();
  private durations: number[] = [];
  private retryTimer: number | undefined;
  private hashBusy = false;
  private thumbBusy = false;
  private preflightBusy = false;
  /** 连续网络失败计数：用来判断「服务端是不是不在了」 */
  private networkFailures = 0;
  private hydrated = false;
  private seq = 0;
  private hashBridge = new WorkerBridge("hash.worker.js");
  private thumbBridge = new WorkerBridge("thumb.worker.js");
  private warnedNoHash = false;
  private warnedNoThumb = false;
  /** 「游客上传被关掉」这条路只走一次：一批请求会连着撞上同一个 401 */
  private guestClosed = false;

  // ---- 订阅与广播 ----

  subscribe(listener: QueueListener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  /** 每张成功上传后回调，广场页用来把新图插进列表。 */
  onUploaded(hook: (result: UploadResult) => void): () => void {
    this.uploadedHooks.add(hook);
    return () => this.uploadedHooks.delete(hook);
  }

  private emit(event: QueueEvent): void {
    for (const listener of [...this.listeners]) listener(event);
  }

  private emitItem(id: string): void {
    this.emit({ kind: "item", id });
  }

  // ---- 读状态 ----

  snapshot(): QueueItem[] {
    return this.items;
  }

  getSettings(): StoredUploadSettings {
    return { ...this.settings };
  }

  isRunning(): boolean {
    return this.running;
  }

  isPaused(): boolean {
    return this.paused;
  }

  stats(): QueueStats {
    return computeStats(this.items);
  }

  /** 可立即派发的张数：已预检、缩略图已处理、不在退避窗口里。 */
  readyCount(): number {
    return this.items.filter((item) => item.status === "ready" && item.file && item.checked && !item.retryAt).length;
  }

  /**
   * 还会被上传的张数：算哈希中、等待中、上传中都算。
   * 「开始上传」的判断不能只看 readyCount——刚选完文件时全在算哈希，
   * 那一刻 readyCount 是 0，但队列里有活要干。
   */
  queuedCount(): number {
    return this.items.filter(
      (item) => item.status === "hashing" || item.status === "ready" || item.status === "uploading",
    ).length;
  }

  failedItems(): QueueItem[] {
    return this.items.filter((item) => item.status === "failed");
  }

  /** 已成功（含去重命中）的直链列表，导出清单用。 */
  successLinks(): string[] {
    return this.items
      .filter((item) => item.status === "done" && item.result?.url)
      .map((item) => item.result!.url);
  }

  estimatedRemainingMs(): number {
    const pending = this.items.filter(
      (item) => item.status === "ready" || item.status === "uploading",
    ).length;
    return estimateRemainingMs(this.durations, pending, this.settings.concurrency);
  }

  // ---- 入队 ----

  addFiles(incoming: IncomingFile[]): number {
    // 入队口先判许可：未登录且站点没开游客上传时，文件连队列都不进，
    // 后面的算哈希、预检、上传自然都不会发生。放在最前面，是因为这几步
    // 一旦跑起来就会动本机 CPU、也会向服务端发请求
    if (!uploadAllowed()) {
      this.emit({
        kind: "notice",
        tone: "warn",
        text: "本站未开放游客上传，文件没有加入队列。登录后即可上传。",
      });
      return 0;
    }

    const accepted: IncomingFile[] = [];
    let rejected = 0;
    for (const entry of incoming) {
      if (isLikelyImage(entry.file)) accepted.push(entry);
      else rejected += 1;
    }
    if (accepted.length === 0) {
      if (rejected > 0) {
        this.emit({
          kind: "notice",
          tone: "warn",
          text: `${rejected} 个文件不在图片格式白名单内，已忽略。`,
        });
      }
      return 0;
    }

    // 单批次最大张数：一次选择收多少张。先按它截断，再按队列容量截断——
    // 两个上限管的是不同的事，谁先谁后只影响提示语里报哪个数
    const batchLimit = effectiveBatchLimit();
    let overBatch = 0;
    if (batchLimit > 0 && accepted.length > batchLimit) {
      overBatch = accepted.length - batchLimit;
      accepted.length = batchLimit;
    }

    // 队列深度阈值：算这一批能进来多少张，超出部分直接不收。
    // 不截断的话，浏览器会老老实实排 900 张，然后第一张预检就被服务端拒掉——
    // 用户看到的是「上传失败」，而不是「你一次选太多了」
    const depth = effectiveQueueDepth();
    let skipped = 0;
    if (depth > 0 && this.items.length + accepted.length > depth) {
      const room = Math.max(0, depth - this.items.length);
      skipped = accepted.length - room;
      accepted.length = room;
      if (accepted.length === 0) {
        this.emit({
          kind: "notice",
          tone: "warn",
          text: `队列已有 ${this.items.length} 张（上限 ${depth} 张），请先完成或清空后再添加。`,
        });
        return 0;
      }
    }

    for (const entry of accepted) {
      this.seq += 1;
      const file = entry.file;
      this.items.push({
        id: createId(this.seq),
        file,
        name: file.name,
        size: file.size,
        mime: file.type || extOf(file.name),
        sha256: null,
        thumb: null,
        thumbNote: null,
        // 预览直接用原文件的 objectURL：省掉「等缩略图生成」的空窗期
        previewUrl: typeof URL !== "undefined" ? URL.createObjectURL(file) : null,
        status: "hashing",
        progress: 0,
        error: null,
        note: null,
        retries: 0,
        addedAt: Date.now(),
        folderTags: folderTagsFromPath(entry.path),
        result: null,
        durationMs: null,
        checked: false,
        thumbTried: false,
        retryAt: null,
      });
    }

    if (overBatch > 0) {
      this.emit({
        kind: "notice",
        tone: "warn",
        text:
          `单批次上限 ${batchLimit} 张，本次多选的 ${overBatch} 张未加入。` +
          "可再次选择以继续上传。",
      });
    }
    if (skipped > 0) {
      this.emit({
        kind: "notice",
        tone: "warn",
        text:
          `队列上限为 ${depth} 张（队列深度阈值），超出的 ${skipped} 张未加入。` +
          "请先完成本批上传，再继续添加。",
      });
    }
    if (rejected > 0) {
      this.emit({
        kind: "notice",
        tone: "warn",
        text: `已添加 ${accepted.length} 张；另有 ${rejected} 个非图片文件已忽略。`,
      });
    }
    // 能收下这一批就说明许可又回来了（比如刚登录，或者管理员重新开了游客上传）
    this.guestClosed = false;
    this.emit({ kind: "items" });
    this.pump();
    return accepted.length;
  }

  removeItem(id: string): void {
    const index = this.items.findIndex((item) => item.id === id);
    if (index < 0) return;
    const [item] = this.items.splice(index, 1);
    this.active.get(id)?.abort();
    this.active.delete(id);
    if (item?.previewUrl) URL.revokeObjectURL(item.previewUrl);
    this.emit({ kind: "items" });
  }

  /** 清空：只留成功与跳过项，已完成的进度和直链不丢。 */
  clearPending(): void {
    for (const item of this.items) {
      if (item.status !== "done" && item.status !== "skipped" && item.previewUrl) {
        URL.revokeObjectURL(item.previewUrl);
      }
    }
    this.items = this.items.filter((item) => item.status === "done" || item.status === "skipped");
    for (const [id, controller] of this.active) {
      controller.abort();
      this.active.delete(id);
    }
    this.running = false;
    this.paused = false;
    this.clearRetryTimer();
    this.emit({ kind: "items" });
    this.emit({ kind: "state" });
  }

  clearAll(): void {
    for (const item of this.items) {
      if (item.previewUrl) URL.revokeObjectURL(item.previewUrl);
    }
    for (const [, controller] of this.active) controller.abort();
    this.active.clear();
    this.items = [];
    this.running = false;
    this.paused = false;
    this.clearRetryTimer();
    this.emit({ kind: "items" });
    this.emit({ kind: "state" });
  }

  // ---- 设置 ----

  setSettings(patch: Partial<StoredUploadSettings>): void {
    const next: StoredUploadSettings = { ...this.settings, ...patch };
    // 并发落在 1..策略上限 之间：管理员按用户把上限压到 1 时，客户端选的 4
    // 不能越过它；反过来也不能低于 1
    const cap = concurrencyCap();
    const wanted = Number.isFinite(next.concurrency) ? Math.floor(next.concurrency) : 1;
    const concurrency = Math.max(1, Math.min(wanted, cap)) as Concurrency;
    this.settings = {
      albumId: typeof next.albumId === "number" ? next.albumId : null,
      tags: typeof next.tags === "string" ? next.tags : "",
      visibility: next.visibility === "public" ? "public" : "private",
      concurrency,
    };
    this.emit({ kind: "state" });
    // 并发数调大后立刻把等待项铺开，不用等下一次进度事件
    this.pump();
    void saveUploadSettings(this.settings);
  }

  // ---- 生命周期 ----

  start(): void {
    if (this.running) {
      this.paused = false;
      this.emit({ kind: "state" });
      this.pump();
      return;
    }
    if (this.queuedCount() === 0) {
      // 没有「能在流水线里往前走」的项时不要进入运行态，否则会立刻收到一次假的「队列完成」
      const failed = this.failedItems().length;
      const needReselect = this.stats().needsReselect;
      this.emit({
        kind: "notice",
        tone: "info",
        text:
          failed > 0
            ? `还有 ${failed} 张失败项，用「重试全部失败项」重新排队。`
            : needReselect > 0
              ? `有 ${needReselect} 张需要重新选择文件后才能继续。`
              : "没有可上传的图片：先把图片加进来。",
      });
      return;
    }
    this.running = true;
    this.paused = false;
    this.durations = [];
    this.emit({ kind: "state" });
    this.pump();
  }

  pause(): void {
    if (!this.running || this.paused) return;
    this.paused = true;
    this.emit({ kind: "state" });
    this.emit({ kind: "notice", tone: "info", text: "已暂停：正在上传的那张会传完，后面的不再派发。" });
  }

  resume(): void {
    if (!this.running) {
      this.start();
      return;
    }
    this.paused = false;
    this.emit({ kind: "state" });
    this.pump();
  }

  /** 取消：中止在飞的请求，等待项保留在队列里，随时可以重新开始。 */
  stop(): void {
    const hadActive = this.active.size > 0;
    for (const [, controller] of this.active) controller.abort();
    this.active.clear();
    for (const item of this.items) {
      if (item.status === "uploading") {
        item.status = "ready";
        item.progress = 0;
        item.retryAt = null;
      }
    }
    this.running = false;
    this.paused = false;
    this.clearRetryTimer();
    this.emit({ kind: "items" });
    this.emit({ kind: "state" });
    if (hadActive) this.emit({ kind: "notice", tone: "info", text: "已取消本次上传。" });
  }

  retry(id: string): void {
    const item = this.items.find((entry) => entry.id === id);
    if (!item) return;
    if (!item.file) {
      item.status = "needs-reselect";
      item.note = "需要重新选择这个文件";
      this.emitItem(id);
      return;
    }
    item.retries = 0;
    item.retryAt = null;
    item.error = null;
    item.progress = 0;
    item.status = "ready";
    this.emitItem(id);
    if (!this.running) this.start();
    else this.pump();
  }

  retryFailed(): void {
    const failed = this.failedItems();
    if (failed.length === 0) return;
    for (const item of failed) {
      item.retries = 0;
      item.retryAt = null;
      item.error = null;
      item.progress = 0;
      item.status = item.file ? "ready" : "needs-reselect";
      if (!item.file) item.note = "需要重新选择这个文件";
      this.emitItem(item.id);
    }
    this.emit({ kind: "notice", tone: "info", text: `已重新排队 ${failed.length} 张失败项。` });
    if (!this.running) this.start();
    else this.pump();
  }

  /** 从 IndexedDB 恢复：只有面板挂载时调一次。 */
  /** 一键移除所有失败项（上一轮的失败留在面板上只会碍事）。 */
  clearFailed(): number {
    const before = this.items.length;
    this.items = this.items.filter((item) => item.status !== "failed");
    this.networkFailures = 0;
    const removed = before - this.items.length;
    if (removed > 0) {
      this.emit({ kind: "items" });
      }
    return removed;
  }

  async hydrate(): Promise<void> {
    if (this.hydrated) return;
    this.hydrated = true;

    // 只恢复上传设置（相册、标签、可见性、并发），不恢复上一次的条目：
    // 待上传预览是「这次要传什么」的清单，刷新之后就该是空的。
    // 传完的记录留在那里只会让人一张张点叉；成功的图本来也已经在相册里。
    const settings = await loadUploadSettings();
    const resolved = settings ?? loadLocalUploadSettings();
    if (resolved) {
      this.settings = { ...this.settings, ...resolved };
      this.emit({ kind: "state" });
    }
  }

  // ---- 调度 ----

  private pump(): void {
    void this.pumpHash();
    void this.pumpPreflight();
    void this.pumpThumbs();
    this.pumpUpload();
  }

  private async pumpHash(): Promise<void> {
    if (this.hashBusy) return;
    const next = this.items.find((item) => item.status === "hashing" && item.file);
    if (!next || !next.file) return;
    this.hashBusy = true;
    const file = next.file;
    try {
      if (this.hashBridge.unavailable) throw new Error("Worker 不可用");
      const message = await this.hashBridge.send(
        { type: "hash", file },
        (progress) => {
          const loaded = Number(progress.loaded ?? 0);
          const total = Number(progress.total ?? file.size) || file.size;
          // 哈希进度只反映在说明文字上，不占主进度条（主进度条是上传进度）
          next.note = `计算哈希 ${Math.round((loaded / total) * 100)}%`;
          this.emitItem(next.id);
        },
      );
      if (message.type === "done" && typeof message.sha256 === "string") {
        next.sha256 = message.sha256;
        next.status = "ready";
        next.note = null;
      } else {
        throw new Error(typeof message.message === "string" ? message.message : "哈希计算失败");
      }
    } catch {
      // 算不出哈希就放弃预检，直接上传：服务端会自己算并去重，功能不中断
      next.sha256 = null;
      next.status = "ready";
      next.checked = true;
      next.note = "未计算哈希，上传时由服务器判重";
      if (!this.warnedNoHash) {
        this.warnedNoHash = true;
        this.emit({
          kind: "notice",
          tone: "warn",
          text: "本机无法在后台计算哈希，将直接上传。",
        });
      }
    } finally {
      this.hashBusy = false;
      this.emitItem(next.id);
        this.pump();
    }
  }

  private async pumpPreflight(): Promise<void> {
    if (this.preflightBusy) return;
    // 等这一批哈希都算完再一次预检，避免每来一张发一次请求
    if (this.items.some((item) => item.status === "hashing" && item.file)) return;
    const candidates = this.items.filter((item) => item.status === "ready" && !item.checked && item.sha256);
    if (candidates.length === 0) return;

    this.preflightBusy = true;
    try {
      const hashes = [...new Set(candidates.map((item) => item.sha256!))];
      // 分块大小不能超过服务端的单批上限：一次预检请求就是服务端眼里的「一批」，
      // 切得比它大就会被 400 挡回来
      const batchLimit = effectiveBatchLimit();
      const chunkSize = batchLimit > 0 ? Math.min(PREFLIGHT_BATCH, batchLimit) : PREFLIGHT_BATCH;
      const exist = new Set<string>();
      for (const batch of chunk(hashes, chunkSize)) {
        const result = await request<HashCheck>("/blobs/exists", {
          json: { hashes: batch },
          query: ownerUid ? { uid: ownerUid } : undefined,
        });
        for (const hash of result.exist ?? []) exist.add(hash);
      }
      for (const item of candidates) {
        item.checked = true;
        if (item.sha256 && exist.has(item.sha256)) {
          item.status = "skipped";
          item.progress = 100;
          // 不在界面上暴露「服务器已有相同文件」：那是服务端去重信息
          item.note = "上传完成";
          this.emitItem(item.id);
        }
      }
      this.emit({
        kind: "notice",
        tone: "info",
        text: `预检完成：${candidates.length} 张图片已就绪。`,
      });
    } catch (err) {
      // 未登录却撞上 401：站点多半刚把游客上传关掉，重拉站点信息后停队，
      // 别继续逐张试（本机白算哈希，服务端白收一堆请求）
      if (this.handleGuestRevoked(err)) return;
      // 预检失败不阻塞主流程：照常上传，服务端靠 dedup 自己判重
      for (const item of candidates) item.checked = true;
      this.emit({ kind: "notice", tone: "warn", text: "预检失败，将逐张上传。" });
      this.emit({ kind: "error", error: publicUploadError(err) });
    } finally {
      this.preflightBusy = false;
        this.pump();
    }
  }

  private async pumpThumbs(): Promise<void> {
    if (this.thumbBusy) return;
    const next = this.items.find(
      (item) => item.status === "ready" && item.checked && !item.thumbTried && item.file,
    );
    if (!next || !next.file) return;
    this.thumbBusy = true;
    next.thumbTried = true;
    const file = next.file;
    try {
      if (this.thumbBridge.unavailable) throw new Error("Worker 不可用");
      const message = await this.thumbBridge.send({ type: "thumb", file });
      if (message.type === "done") {
        const blob = message.blob;
        if (blob instanceof Blob && blob.size > 0) {
          next.thumb = blob;
          next.thumbNote = null;
        } else {
          next.thumbNote =
            typeof message.reason === "string" && message.reason ? message.reason : "本次不带缩略图";
        }
      } else {
        next.thumbNote = typeof message.message === "string" ? message.message : "缩略图生成失败";
      }
    } catch (err) {
      next.thumbNote = err instanceof Error ? err.message : "缩略图生成失败";
      if (!this.warnedNoThumb) {
        this.warnedNoThumb = true;
        this.emit({
          kind: "notice",
          tone: "warn",
          text: "本机没能生成缩略图，上传会照常进行，列表页将用原图缩放显示。",
        });
      }
    } finally {
      this.thumbBusy = false;
      this.emitItem(next.id);
      this.pump();
    }
  }

  private pumpUpload(): void {
    if (!this.running || this.paused) {
      this.maybeFinish();
      return;
    }
    const now = Date.now();
    let earliestRetry = 0;
    for (const item of this.items) {
      if (item.retryAt !== null && item.retryAt <= now) item.retryAt = null;
      if (item.retryAt !== null) {
        earliestRetry = earliestRetry === 0 ? item.retryAt : Math.min(earliestRetry, item.retryAt);
      }
    }

    while (this.active.size < this.settings.concurrency) {
      const next = this.items.find(
        (item) =>
          item.status === "ready" &&
          item.checked &&
          item.thumbTried &&
          item.file !== null &&
          item.retryAt === null &&
          !this.active.has(item.id),
      );
      if (!next) break;
      void this.uploadItem(next);
    }

    if (earliestRetry > 0 && this.retryTimer === undefined) {
      this.retryTimer = window.setTimeout(
        () => {
          this.retryTimer = undefined;
          this.pump();
        },
        Math.min(Math.max(earliestRetry - now, 0) + 120, 30_000),
      );
    }
    this.maybeFinish();
  }

  private async uploadItem(item: QueueItem): Promise<void> {
    const file = item.file;
    if (!file) return;
    const controller = new AbortController();
    // 这一段必须在第一个 await 之前完成：pumpUpload 的并发计数靠它同步生效
    this.active.set(item.id, controller);
    item.status = "uploading";
    item.progress = 0;
    item.error = null;
    item.note = null;
    this.emitItem(item.id);

    const startedAt = Date.now();
    try {
      const form = new FormData();
      form.append("file", file, item.name);
      if (item.sha256) form.append("sha256", item.sha256);
      if (item.thumb && item.sha256) form.append("thumb", item.thumb, `${item.sha256}.webp`);
      const settings = this.settings;
      if (settings.albumId !== null) form.append("album_id", String(settings.albumId));
      const tags = mergeTags(settings.tags, item.folderTags);
      if (tags) form.append("tags", tags);
      form.append("visibility", settings.visibility);
      form.append("original_name", item.name);

      const result = await request<UploadResult>("/upload", {
        form,
        query: ownerUid ? { uid: ownerUid } : undefined,
        signal: controller.signal,
        onProgress: (loaded, total) => {
          // 留 1% 给服务端处理阶段，避免进度条早早停在 100% 干等
          item.progress = total > 0 ? Math.min(99, Math.round((loaded / total) * 100)) : 50;
          this.emitItem(item.id);
        },
      });

      item.result = result;
      item.progress = 100;
      this.networkFailures = 0;
      item.durationMs = Date.now() - startedAt;
      this.durations.push(item.durationMs);
      if (result.dedup) {
        // 去重是服务端内部行为；对用户统一显示「已完成」，不暴露库里已有这份文件
        item.status = "skipped";
        item.note = "上传完成";
      } else {
        item.status = "done";
        item.note = item.thumb ? "客户端已转 WebP 缩略图" : item.thumbNote;
      }
      if (result.fallback_message) {
        // 远端后端失败时文件实际落在本地，用户需要知道；上游错误细节不往界面带
        this.emit({
          kind: "notice",
          tone: "warn",
          text: `「${item.name}」远端存储暂时不可用，文件已保存到本地存储。`,
        });
      }
      for (const hook of [...this.uploadedHooks]) hook(result);
    } catch (err) {
      if (controller.signal.aborted) {
        item.status = "ready";
        item.progress = 0;
        item.note = null;
      } else {
        this.handleFailure(item, err);
      }
    } finally {
      this.active.delete(item.id);
      this.emitItem(item.id);
        this.pump();
    }
  }

  private handleFailure(item: QueueItem, err: unknown): void {
    // 未登录 + 401 = 游客上传被关：停队、刷新站点信息，不再逐张重试
    if (this.handleGuestRevoked(err)) {
      item.status = "ready";
      item.progress = 0;
      item.note = "本站已关闭游客上传";
      return;
    }

    const apiError = err instanceof ApiError ? err : null;
    const permanent = isPermanentError(err);

    // 只有网络抖动与服务端临时不可用才等一会儿再试；尺寸/格式/配额这类
    // 明确规定导致的失败，重试多少次结果都一样，直接给出原因。
    const limit = apiError?.code === "NETWORK" ? MAX_NETWORK_RETRIES : MAX_RETRIES;
    if (!permanent && item.retries < limit) {
      item.retries += 1;
      const delay = backoffDelay(item.retries, apiError?.retryAfter);
      item.retryAt = Date.now() + delay;
      item.status = "ready";
      item.progress = 0;
      item.note = "正在重试…";
      item.error = friendlyUploadError(err);
      return;
    }

    item.status = "failed";
    item.retryAt = null;
    // 行内只显示脱敏后的原因；detail 里有请求路径/上游响应，不往界面上放
    item.error = friendlyUploadError(err);
    if (permanent && apiError) {
      item.note =
        apiError.code === "INVALID_IMAGE" ||
        apiError.code === "FILE_TOO_LARGE" ||
        apiError.code === "UNSUPPORTED_FORMAT"
          ? "请调整这张图片后重新选择"
          : "请先处理原因后再上传";
    } else if (item.retries > 0) {
      item.note = `已重试 ${item.retries} 次，可点击重试`;
    } else {
      item.note = "可点击重试";
    }

    // 连续多张网络失败通常不是「这张图有问题」，而是网络/服务端整体不可用。
    if (apiError?.code === "NETWORK") {
      this.networkFailures += 1;
      if (this.networkFailures === 3) {
        this.emit({
          kind: "notice",
          tone: "err",
          text: "网络连接异常，已停止重试。请检查网络后点击「重试全部失败项」。",
        });
      }
    } else {
      this.networkFailures = 0;
    }

    // 只把脱敏后的错误交给界面：原始 detail 里可能有上游地址与请求路径
    this.emit({ kind: "error", error: publicUploadError(err) });
  }

  /**
   * 未登录却撞上 401：站点把「允许游客上传」关掉了。
   *
   * 手上的站点信息是进页面时拉的，这会儿已经不作数了——重拉一次，让
   * `uploadAllowed()` 回到准的（下一次添加文件会在入队口直接被挡下），同时停掉
   * 队列：继续逐张试只会一张一张吃 401，本机白算哈希、服务端白收请求。
   * 返回 true 表示这次失败已经处理过，调用方不必再走常规的失败处置。
   */
  private handleGuestRevoked(err: unknown): boolean {
    // 登录用户的 401 是「会话过期」，归 api.ts 那个统一处理器管
    if (getState().user) return false;
    if (!(err instanceof ApiError) || err.status !== 401) return false;
    this.stop();
    if (this.guestClosed) return true;
    this.guestClosed = true;
    void loadSite();
    this.emit({
      kind: "notice",
      tone: "warn",
      text: "本站已关闭游客上传，队列已停止；登录后可以继续上传。",
    });
    return true;
  }

  private maybeFinish(): void {
    if (!this.running) return;
    if (this.active.size > 0) return;
    if (this.retryTimer !== undefined) return;
    const blockers = this.items.filter(
      (item) => item.status === "hashing" || item.status === "uploading" || (item.status === "ready" && item.file),
    );
    if (blockers.length > 0) return;

    this.running = false;
    this.paused = false;
    const stats = this.stats();
    this.emit({ kind: "state" });
    this.emit({ kind: "finished", stats });
    const completed = stats.success + stats.skipped;
    this.emit({
      kind: "notice",
      tone: stats.failed > 0 ? "warn" : "ok",
      text:
        stats.failed > 0
          ? `队列完成：完成 ${completed} 张，失败 ${stats.failed} 张。`
          : `队列完成：${completed} 张全部完成。`,
    });
  }

  private clearRetryTimer(): void {
    if (this.retryTimer !== undefined) {
      window.clearTimeout(this.retryTimer);
      this.retryTimer = undefined;
    }
  }
}

/** 全局单例：页面之间来回切也只有一个队列，几种入口都进这里。 */
let instance: UploadQueue | null = null;

export function getUploadQueue(): UploadQueue {
  instance ??= new UploadQueue();
  return instance;
}

export type { UploadQueue };
