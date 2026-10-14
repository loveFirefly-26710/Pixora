/**
 * 本地持久化（IndexedDB）。
 *
 * 目前只服务上传流程：队列快照与上传设置。
 * 队列里不能存 File——结构化克隆不了，存了也没法恢复成一个可上传的文件；
 * 所以快照只留元信息与结果，刷新后未完成的项由 UI 标成「需要重新选择文件」。
 * 这是浏览器能力边界，如实告诉用户，比假装能恢复要好。
 *
 * IndexedDB 打不开（隐私模式、第三方 Cookie 被禁、存储配额耗尽）时整体降级到内存：
 * 功能不中断，只是刷新后不再保留。降级静默发生在内部，调用方不需要写分支。
 */

const DB_NAME = "pixora-web";
const DB_VERSION = 1;
const STORE_QUEUE = "upload_queue";
const STORE_SETTINGS = "upload_settings";
const QUEUE_KEY = "current";
const SETTINGS_KEY = "upload";

/** localStorage 里的镜像键：IndexedDB 是异步的，首屏要立刻拿到并发数，就用一份同步镜像兜底。 */
const LS_SETTINGS_KEY = "pixora.upload.settings";
const LS_VIEW_KEY = "pixora.albums.view";

/** 队列快照的单条记录。字段名刻意与 upload/queue.ts 的 QueueItem 区分，避免误当作运行时对象用。 */
export interface PersistedQueueItem {
  id: string;
  name: string;
  size: number;
  mime: string;
  sha256: string | null;
  /** 落盘时的状态字符串；读回时收敛到合法值，旧数据不至于让页面挂掉 */
  status: string;
  error: string | null;
  note: string | null;
  retries: number;
  addedAt: number;
  /** 选择文件夹时从相对路径推出的标签 */
  folderTags: string[];
  /** 上传成功后拿到的直链，导出成功清单要用 */
  url: string | null;
  thumbUrl: string | null;
  dedup: boolean;
}

export interface StoredUploadSettings {
  albumId: number | null;
  tags: string;
  visibility: "public" | "private";
  concurrency: number;
}

/** IndexedDB 不可用时的替身：进程内数组/对象，语义与真存储一致。 */
const memory = {
  queue: [] as PersistedQueueItem[],
  settings: null as StoredUploadSettings | null,
};

let dbPromise: Promise<IDBDatabase | null> | null = null;

function openDatabase(): Promise<IDBDatabase | null> {
  if (dbPromise) return dbPromise;
  dbPromise = new Promise((resolve) => {
    if (typeof indexedDB === "undefined") {
      resolve(null);
      return;
    }
    let request: IDBOpenDBRequest;
    try {
      request = indexedDB.open(DB_NAME, DB_VERSION);
    } catch {
      resolve(null);
      return;
    }
    request.onupgradeneeded = () => {
      const db = request.result;
      // 两张表都用显式 key：队列只有一份「当前队列」，设置只有一份。
      if (!db.objectStoreNames.contains(STORE_QUEUE)) db.createObjectStore(STORE_QUEUE);
      if (!db.objectStoreNames.contains(STORE_SETTINGS)) db.createObjectStore(STORE_SETTINGS);
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => resolve(null);
    // blocked 说明有旧版本的连接没关；等不来就直接降级，不能让页面卡在这
    request.onblocked = () => resolve(null);
  });
  return dbPromise;
}

function runRequest<T>(
  storeName: string,
  mode: IDBTransactionMode,
  build: (store: IDBObjectStore) => IDBRequest,
): Promise<T | null> {
  return openDatabase().then((db) => {
    if (!db) return null;
    return new Promise<T | null>((resolve) => {
      try {
        const tx = db.transaction(storeName, mode);
        const request = build(tx.objectStore(storeName));
        request.onsuccess = () => resolve(request.result as T);
        request.onerror = () => resolve(null);
        tx.onabort = () => resolve(null);
      } catch {
        resolve(null);
      }
    });
  });
}

// ---------- 队列快照 ----------

/** 覆盖式写入：队列全量快照，不追加。调用方负责节流（见 queue.ts）。 */
export async function saveQueueSnapshot(items: PersistedQueueItem[]): Promise<void> {
  memory.queue = items;
  await runRequest(STORE_QUEUE, "readwrite", (store) => store.put(items, QUEUE_KEY));
}

export async function loadQueueSnapshot(): Promise<PersistedQueueItem[]> {
  const value = await runRequest<PersistedQueueItem[]>(STORE_QUEUE, "readonly", (store) =>
    store.get(QUEUE_KEY),
  );
  if (Array.isArray(value)) return value.filter((item) => item && typeof item === "object");
  return memory.queue;
}

export async function clearQueueSnapshot(): Promise<void> {
  memory.queue = [];
  await runRequest(STORE_QUEUE, "readwrite", (store) => store.delete(QUEUE_KEY));
}

// ---------- 上传设置 ----------

function normalizeSettings(input: unknown): StoredUploadSettings | null {
  if (!input || typeof input !== "object") return null;
  const raw = input as Partial<StoredUploadSettings>;
  const visibility = raw.visibility === "public" ? "public" : "private";
  const concurrency = [1, 2, 4].includes(Number(raw.concurrency)) ? Number(raw.concurrency) : 1;
  return {
    albumId: typeof raw.albumId === "number" ? raw.albumId : null,
    tags: typeof raw.tags === "string" ? raw.tags : "",
    visibility,
    concurrency,
  };
}

/** localStorage 镜像：同步可读，用于首屏把并发数/可见性回填到表单。 */
export function loadLocalUploadSettings(): StoredUploadSettings | null {
  try {
    const text = localStorage.getItem(LS_SETTINGS_KEY);
    if (!text) return null;
    return normalizeSettings(JSON.parse(text));
  } catch {
    return null;
  }
}

export function saveLocalUploadSettings(settings: StoredUploadSettings): void {
  try {
    localStorage.setItem(LS_SETTINGS_KEY, JSON.stringify(settings));
  } catch {
    /* 存储写满或被禁用：镜像丢了不影响主流程 */
  }
}

export async function loadUploadSettings(): Promise<StoredUploadSettings | null> {
  const value = await runRequest<unknown>(STORE_SETTINGS, "readonly", (store) =>
    store.get(SETTINGS_KEY),
  );
  return normalizeSettings(value) ?? memory.settings;
}

export async function saveUploadSettings(settings: StoredUploadSettings): Promise<void> {
  memory.settings = settings;
  saveLocalUploadSettings(settings);
  await runRequest(STORE_SETTINGS, "readwrite", (store) => store.put(settings, SETTINGS_KEY));
}

// ---------- 视图偏好 ----------

/** 网格/列表偏好存 localStorage：一个字符串，放 IndexedDB 是杀鸡用牛刀。 */
export function loadViewPreference(): "grid" | "list" {
  try {
    return localStorage.getItem(LS_VIEW_KEY) === "list" ? "list" : "grid";
  } catch {
    return "grid";
  }
}

export function saveViewPreference(view: "grid" | "list"): void {
  try {
    localStorage.setItem(LS_VIEW_KEY, view);
  } catch {
    /* 同上，偏好丢了大不了回到默认值 */
  }
}
