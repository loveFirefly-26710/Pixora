/**
 * 本地持久化（IndexedDB）。
 *
 * 只存上传设置（目标相册、标签、可见性、并发）与相册的视图偏好。
 * 队列条目刻意不存：文件本体浏览器存不下，而只留「已完成」的记录
 * 会在下次打开时堆在待上传预览里——列表就该从空的开始。
 *
 * IndexedDB 打不开（隐私模式、第三方 Cookie 被禁、存储配额耗尽）时整体降级到内存：
 * 功能不中断，只是刷新后不再保留。降级静默发生在内部，调用方不需要写分支。
 */

const DB_NAME = "pixora-web";
const DB_VERSION = 1;
const STORE_SETTINGS = "upload_settings";
const SETTINGS_KEY = "upload";

/** localStorage 里的镜像键：IndexedDB 是异步的，首屏要立刻拿到并发数，就用一份同步镜像兜底。 */
const LS_SETTINGS_KEY = "pixora.upload.settings";
const LS_VIEW_KEY = "pixora.albums.view";
const LS_PLAZA_COLUMNS = "pixora.plaza.columns";
const LS_PLAZA_AUTO = "pixora.plaza.auto";
const LS_ANNOUNCEMENT_ACK = "pixora.announcement.acked";
/** 本次浏览器会话里已经弹过的公告版本；存 sessionStorage，关标签页即清。 */
const SS_ANNOUNCEMENT_SHOWN = "pixora.announcement.shown";

export interface StoredUploadSettings {
  albumId: number | null;
  tags: string;
  visibility: "public" | "private";
  concurrency: number;
}

/** IndexedDB 不可用时的替身：进程内数组/对象，语义与真存储一致。 */
const memory = {
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
      // 设置只有一份，用显式 key
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

// ---------- 广场列数 ----------

/** 广场列数偏好。没设过时用管理员配置的默认值。 */
export function loadPlazaColumns(fallback: number): number {
  try {
    const raw = Number(localStorage.getItem(LS_PLAZA_COLUMNS));
    if (Number.isInteger(raw) && raw >= 2 && raw <= 6) return raw;
  } catch {
    /* 隐私模式下读不到就用默认值 */
  }
  return Math.min(6, Math.max(2, Math.round(fallback) || 3));
}

export function savePlazaColumns(columns: number): void {
  try {
    localStorage.setItem(LS_PLAZA_COLUMNS, String(columns));
  } catch {
    /* 偏好丢了大不了下一刷新回到后台默认值 */
  }
}

/** 自动漫游开关偏好；null 表示用户还没选过。 */
export function loadPlazaAuto(): boolean | null {
  try {
    const raw = localStorage.getItem(LS_PLAZA_AUTO);
    if (raw === "0") return false;
    if (raw === "1") return true;
  } catch {
    /* 读不到就按默认开启 */
  }
  return null;
}

export function savePlazaAuto(enabled: boolean): void {
  try {
    localStorage.setItem(LS_PLAZA_AUTO, enabled ? "1" : "0");
  } catch {
    /* 偏好丢了大不了下次回到默认开启 */
  }
}

// ---------- 首页公告 ----------

/** 用户确认过的公告版本。内容变了版本就变，会重新弹一次。 */
export function announcementAcknowledged(version: string): boolean {
  try {
    return localStorage.getItem(LS_ANNOUNCEMENT_ACK) === version;
  } catch {
    return false;
  }
}

export function acknowledgeAnnouncement(version: string): void {
  try {
    localStorage.setItem(LS_ANNOUNCEMENT_ACK, version);
  } catch {
    /* 存不进去也不影响看公告，只是下次还会弹 */
  }
}

/**
 * 本次会话是否已经弹过这个版本的公告。
 *
 * 用户会话期间只在第一次进首页时弹；关掉标签页（或换新会话）后，
 * 同一个版本会再提示一次，除非上次勾了「不再提示」。
 */
export function announcementShownThisSession(version: string): boolean {
  try {
    return sessionStorage.getItem(SS_ANNOUNCEMENT_SHOWN) === version;
  } catch {
    return false;
  }
}

export function markAnnouncementShown(version: string): void {
  try {
    sessionStorage.setItem(SS_ANNOUNCEMENT_SHOWN, version);
  } catch {
    /* sessionStorage 被禁用时退化为每次进入都提示，不影响阅读 */
  }
}
