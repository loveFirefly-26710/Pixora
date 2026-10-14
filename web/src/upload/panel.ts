/**
 * 可复用的上传面板。
 *
 * 上传页与广场页共用同一块面板，四种入口（拖拽 / 选文件 / 选文件夹 / 粘贴）效果一样，
 * 最终都进同一个队列（upload/queue.ts 的单例），所以两个页面来回切不会丢队列。
 *
 * 面板只负责「展示 + 收集设置」，不自己发请求：状态变化全部来自队列事件，
 * 这里只把事件翻成 DOM 更新，避免出现两套互相打架的进度来源。
 */

import { ApiError } from "../core/api";
import { copyText, h } from "../core/dom";
import { formatBytes, formatDuration, formatNumber, shortHash } from "../core/format";
import { getState } from "../core/state";
import { toastError, toastRateLimited, toastWithCode } from "../core/toast";
import type { UploadResult } from "../core/types";
import { badge, qtip, statCard } from "../ui/widgets";
import {
  CONCURRENCY_OPTIONS,
  getUploadQueue,
  type IncomingFile,
  type QueueItem,
  type QueueStats,
} from "./queue";

export interface UploadPanelOptions {
  /** 目标相册候选。广场页可以传空数组，面板会隐藏相册选择。 */
  albums?: Array<{ id: number; name: string }>;
  /** 预选的目标相册 */
  albumId?: number | null;
  /** 每张上传成功后的回调（广场页用来把新图插进列表） */
  onUploaded?: (result: UploadResult) => void;
  /** 一次队列跑完后的回调 */
  onFinished?: (stats: QueueStats) => void;
  /** 紧凑模式：广场页顶部用，隐藏「选择方式」四宫格 */
  compact?: boolean;
}

export interface UploadPanelHandle {
  root: HTMLElement;
  /** 相册列表异步到手后回填 */
  refreshAlbums(albums: Array<{ id: number; name: string }>, albumId?: number | null): void;
  destroy(): void;
}

/** 预览格最多渲染多少张：1000 张全铺出来会把首屏拖垮，剩下的靠汇总数字表达。 */
const MAX_PREVIEW = 64;
/** 明细表最多渲染多少行，理由同上。 */
const MAX_ROWS = 200;
const NOTICE_LIMIT = 3;

/** 状态徽标：与队列的七种状态一一对应，文案沿用原型。 */
function statusBadge(item: QueueItem): HTMLElement {
  switch (item.status) {
    case "hashing":
      return badge("计算哈希", "mute");
    case "ready":
      return item.retryAt !== null ? badge("退避重试中", "warn") : badge("等待中", "mute");
    case "uploading":
      return badge(`上传中 ${item.progress}%`, "warn");
    case "done":
      return badge("已完成", "ok");
    case "skipped":
      return badge("已跳过", "info");
    case "failed":
      return badge("失败", "err");
    case "needs-reselect":
      return badge("需重新选择", "warn");
  }
}

/** 上传文件的 accept：图片通配 + 站点白名单扩展名，HEIC/TIFF 这种没有 image/* 的也能选。 */
function acceptAttr(): string {
  const formats = getState().site?.formats ?? [];
  return ["image/*", ...formats.map((ext) => `.${ext}`)].join(",");
}

interface DirEntryLike {
  isFile: boolean;
  isDirectory: boolean;
  name: string;
  file?: (onSuccess: (file: File) => void, onError: (error: unknown) => void) => void;
  createReader?: () => {
    readEntries: (
      onSuccess: (entries: DirEntryLike[]) => void,
      onError: (error: unknown) => void,
    ) => void;
  };
}

/** 递归读取拖进来的目录。readEntries 一次最多回 100 条，必须循环读到空。 */
function walkEntry(entry: DirEntryLike, prefix: string, out: IncomingFile[]): Promise<void> {
  if (entry.isFile && entry.file) {
    return new Promise((resolve) => {
      entry.file!(
        (file) => {
          out.push({ file, path: `${prefix}${entry.name}` });
          resolve();
        },
        () => resolve(),
      );
    });
  }
  if (entry.isDirectory && entry.createReader) {
    const reader = entry.createReader();
    const dirPath = `${prefix}${entry.name}/`;
    return (async () => {
      for (;;) {
        const batch = await new Promise<DirEntryLike[]>((resolve) => {
          reader.readEntries(
            (entries) => resolve(entries),
            () => resolve([]),
          );
        });
        if (batch.length === 0) break;
        for (const child of batch) await walkEntry(child, dirPath, out);
      }
    })();
  }
  return Promise.resolve();
}

/** 把一次拖放/粘贴的数据翻成待入队文件；没有 entries API 时退回扁平文件列表。 */
export async function collectFiles(dataTransfer: DataTransfer): Promise<IncomingFile[]> {
  const out: IncomingFile[] = [];
  const entries: DirEntryLike[] = [];
  const items = dataTransfer.items;
  if (items && items.length > 0) {
    for (const item of Array.from(items)) {
      if (item.kind !== "file") continue;
      const getter = (item as DataTransferItem & { webkitGetAsEntry?: () => DirEntryLike | null })
        .webkitGetAsEntry;
      const entry = typeof getter === "function" ? getter.call(item) : null;
      if (entry) entries.push(entry as DirEntryLike);
    }
  }
  if (entries.length === 0) {
    for (const file of Array.from(dataTransfer.files)) out.push({ file });
    return out;
  }
  for (const entry of entries) await walkEntry(entry, "", out);
  return out;
}

export function createUploadPanel(options: UploadPanelOptions = {}): UploadPanelHandle {
  const queue = getUploadQueue();
  const compact = options.compact ?? false;

  let notices: Array<{ tone: "info" | "ok" | "warn" | "err"; text: string }> = [];
  let lastToast = { message: "", at: 0 };
  let destroyed = false;

  // ---------- 入口区 ----------

  const fileInput = h("input", { type: "file", accept: acceptAttr(), multiple: true });
  const folderInput = h("input", { type: "file", webkitdirectory: "", multiple: true });
  const albumInput = h("input", { type: "file", accept: "image/*", multiple: true });
  const cameraInput = h("input", { type: "file", accept: "image/*", capture: "environment" });
  const anyFileInput = h("input", { type: "file", accept: acceptAttr(), multiple: true });
  for (const input of [fileInput, folderInput, albumInput, cameraInput, anyFileInput]) {
    input.style.display = "none";
    input.addEventListener("change", () => {
      const files = Array.from(input.files ?? []);
      input.value = "";
      if (files.length === 0) return;
      queue.addFiles(
        files.map((file) => {
          const relative = (file as File & { webkitRelativePath?: string }).webkitRelativePath;
          return relative ? { file, path: relative } : { file };
        }),
      );
    });
  }

  const dropzone = h("div.dropzone");
  dropzone.append(
    h("div.ph", { style: { width: "46px", height: "46px", borderRadius: "10px", margin: "0 auto 8px" }, text: "＋" }),
    h("div", { style: { fontSize: "13.5px", fontWeight: "600" }, text: "拖拽到此处，或点击选择文件 / 文件夹" }),
    h(
      "div.tip",
      { style: { marginTop: "3px" } },
      "也可以直接按 ",
      h("span.kbd", { text: "Ctrl" }),
      " + ",
      h("span.kbd", { text: "V" }),
      " 粘贴剪贴板里的图片",
    ),
  );
  const dropButtons = h(
    "div.flexline",
    { style: { justifyContent: "center", marginTop: "12px" } },
    h("button.btn.sm.pri", { type: "button", text: "选择文件", on: { click: () => fileInput.click() } }),
    h("button.btn.sm", { type: "button", text: "选择文件夹", on: { click: () => folderInput.click() } }),
  );
  dropzone.append(dropButtons);
  dropzone.addEventListener("click", (event) => {
    if ((event.target as HTMLElement).closest("button")) return;
    fileInput.click();
  });

  let dragDepth = 0;
  dropzone.addEventListener("dragenter", (event) => {
    event.preventDefault();
    dragDepth += 1;
    dropzone.classList.add("hot");
  });
  dropzone.addEventListener("dragover", (event) => {
    event.preventDefault();
  });
  dropzone.addEventListener("dragleave", () => {
    dragDepth = Math.max(0, dragDepth - 1);
    if (dragDepth === 0) dropzone.classList.remove("hot");
  });
  dropzone.addEventListener("drop", (event) => {
    event.preventDefault();
    dragDepth = 0;
    dropzone.classList.remove("hot");
    const transfer = (event as DragEvent).dataTransfer;
    if (!transfer) return;
    void collectFiles(transfer).then((files) => {
      if (files.length > 0) queue.addFiles(files);
    });
  });

  const methodGrid = h(
    "div.meth",
    null,
    h("div.m-box", null, h("b", { text: "拖拽" }), h("small", { text: "文件或文件夹拖进虚线框，松开即入队" })),
    h("div.m-box", null, h("b", { text: "选择文件" }), h("small", { text: "系统文件选择器，支持多选与 Ctrl+A" })),
    h("div.m-box", null, h("b", { text: "选择文件夹" }), h("small", { text: "递归扫描，相对路径保留为标签" })),
    h("div.m-box", null, h("b", { text: "粘贴" }), h("small", { text: "页面任意位置 Ctrl+V，截图工具直接贴" })),
  );
  const desktopCard = h(
    "div.card.upload-desktop",
    null,
    h("h4", null, "选择方式 ", h("small", { text: "四种入口效果一样，进同一个队列" })),
    methodGrid,
    dropzone,
  );
  // 紧凑模式（广场页）：去掉说明性的四宫格，保留拖拽区本身——拖拽是 PC 上最快的入口
  if (compact) methodGrid.style.display = "none";

  const mobileEntry = (
    icon: string,
    title: string,
    hint: string,
    input: HTMLInputElement,
  ): HTMLButtonElement => {
    return h(
      "button.mentry",
      { type: "button", on: { click: () => input.click() } },
      h("div.ic", { text: icon }),
      h("b", { text: title }),
      h("small", { text: hint }),
    );
  };

  const mobileCard = h(
    "div.card.upload-mobile",
    null,
    h("h4", null, "添加入口 ", h("small", { text: "手机上拖拽不可用，换成三个入口" })),
    h(
      "div.mentry-row",
      null,
      mobileEntry("▤", "从相册", "可多选", albumInput),
      mobileEntry("◉", "拍照", "直接调用相机", cameraInput),
      mobileEntry("▧", "从文件", "其他 App 里的图", anyFileInput),
    ),
    qtip(
      "info",
      null,
      "系统剪贴板里的图片走「从相册」入口——iOS 与 Android 都会把刚截的图排在相册最前面。",
    ),
  );

  // ---------- 预览区 ----------

  const previewHead = h("h4");
  const previewGrid = h("div.grid.g8.qgrid");
  const previewMore = h("div.tip");
  const previewCard = h("div.card", null, previewHead, previewGrid, previewMore);

  // ---------- 设置区 ----------

  const albumSelect = h("select.f");
  const tagsInput = h("input.f", { placeholder: "风景,旅行", value: queue.getSettings().tags });
  const visibilitySelect = h(
    "select.f",
    null,
    h("option", { value: "private", text: "私有" }),
    h("option", { value: "public", text: "公开" }),
  );
  const concurrencySelect = h(
    "select.f",
    null,
    ...CONCURRENCY_OPTIONS.map((value) =>
      h("option", {
        value: String(value),
        text: value === 1 ? "1（逐张，推荐）" : String(value),
      }),
    ),
  );
  visibilitySelect.value = queue.getSettings().visibility;
  concurrencySelect.value = String(queue.getSettings().concurrency);

  const settingsTip = h("span.tip", {
    style: { margin: "0", marginRight: "auto" },
    text: "",
  });
  const clearBtn = h("button.btn", { type: "button", text: "清空" });
  const startBtn = h("button.btn.pri", { type: "button", text: "开始上传" });
  const inlineActions = h(
    "div.flexline.upload-actions-inline",
    { style: { justifyContent: "flex-end", marginTop: "10px" } },
    clearBtn,
    startBtn,
  );
  const settingsCard = h(
    "div.card",
    null,
    h("h4", null, "上传设置"),
    h(
      "div.row",
      { style: { marginBottom: "12px" } },
      h("div.fld", { style: { margin: "0" } }, h("label.f", { text: "目标相册" }), albumSelect),
      h("div.fld", { style: { margin: "0" } }, h("label.f", { text: "标签（全部共用）" }), tagsInput),
      h("div.fld", { style: { margin: "0" } }, h("label.f", { text: "可见性" }), visibilitySelect),
      h("div.fld", { style: { margin: "0" } }, h("label.f", { text: "并发数" }), concurrencySelect),
    ),
    h(
      "div.flexline",
      { style: { justifyContent: "flex-end" } },
      settingsTip,
      inlineActions,
    ),
  );

  const setAlbumOptions = (albums: Array<{ id: number; name: string }>, albumId?: number | null): void => {
    const desired = albumId != null ? albumId : queue.getSettings().albumId;
    albumSelect.replaceChildren(
      h("option", { value: "", text: "默认相册" }),
      ...albums.map((album) => h("option", { value: String(album.id), text: album.name })),
    );
    const wanted = desired !== null ? String(desired) : "";
    albumSelect.value = [...albumSelect.options].some((option) => option.value === wanted)
      ? wanted
      : "";
    // 只有页面明确指定了相册（URL 里的 album_id）才回写设置：
    // 否则每次进页面都会用「默认相册」把用户上次的选择冲掉
    if (albumId != null) {
      queue.setSettings({ albumId: albumSelect.value ? Number(albumSelect.value) : null });
    }
  };
  setAlbumOptions(options.albums ?? [], options.albumId);

  albumSelect.addEventListener("change", () => {
    queue.setSettings({ albumId: albumSelect.value ? Number(albumSelect.value) : null });
  });
  tagsInput.addEventListener("change", () => queue.setSettings({ tags: tagsInput.value }));
  visibilitySelect.addEventListener("change", () => {
    queue.setSettings({ visibility: visibilitySelect.value === "public" ? "public" : "private" });
  });
  concurrencySelect.addEventListener("change", () => {
    queue.setSettings({ concurrency: Number(concurrencySelect.value) || 1 });
  });

  clearBtn.addEventListener("click", () => {
    const pending = queue.snapshot().filter((item) => item.status !== "done" && item.status !== "skipped");
    if (pending.length === 0) {
      queue.clearAll();
      return;
    }
    queue.clearPending();
  });

  startBtn.addEventListener("click", () => {
    if (queue.isRunning() && queue.isPaused()) {
      queue.resume();
      return;
    }
    queue.start();
  });

  // ---------- 队列区 ----------

  const queueTitle = h("div", { style: { fontWeight: "700", fontSize: "13.5px" } });
  const queueSub = h("div.tip", { style: { marginTop: "2px" } });
  const pauseBtn = h("button.btn.sm", { type: "button", text: "暂停" });
  const stopBtn = h("button.btn.sm.dgr", { type: "button", text: "取消" });
  const barInner = h("i", { style: { width: "0%" } });
  const bar = h("div.bar-l", { style: { height: "8px" } }, barInner);
  const doneLabel = h("span");
  const etaLabel = h("span");
  const statHost = h("div.stat", { style: { marginTop: "14px" } });
  const tableHost = h("div", { style: { padding: "0", overflow: "hidden" } });
  const retryAllBtn = h("button.btn", { type: "button", text: "重试全部失败项" });
  // 一个按钮把上一轮失败清干净，省得一张张点叉
  const clearFailedBtn = h("button.btn", { type: "button", text: "清空失败项" });
  clearFailedBtn.addEventListener("click", () => {
    const removed = queue.clearFailed();
    if (removed > 0) {
      clearFailedBtn.textContent = `已清空 ${removed} 项`;
      window.setTimeout(() => {
        clearFailedBtn.textContent = "清空失败项";
      }, 1500);
    }
  });
  const exportBtn = h("button.btn", { type: "button", text: "导出成功清单" });
  const queueCard = h(
    "div.card",
    null,
    h(
      "div.flexline",
      { style: { justifyContent: "space-between", marginBottom: "10px" } },
      h("div", { style: { minWidth: "0" } }, queueTitle, queueSub),
      h("div.flexline", null, pauseBtn, stopBtn),
    ),
    bar,
    h(
      "div.flexline",
      { style: { justifyContent: "space-between", marginTop: "8px", fontSize: "12.5px", color: "var(--muted)" } },
      doneLabel,
      etaLabel,
    ),
    statHost,
    h("div", { style: { marginTop: "14px" } }, tableHost),
    h("div.flexline", { style: { marginTop: "14px" } }, retryAllBtn, clearFailedBtn, exportBtn),
  );

  const uploadBar = h("div.upload-bar");
  const barStartBtn = h("button.btn.pri", { type: "button", text: "开始上传" });
  const barPauseBtn = h("button.btn", { type: "button", text: "暂停" });
  uploadBar.append(barStartBtn, barPauseBtn);

  pauseBtn.addEventListener("click", () => (queue.isPaused() ? queue.resume() : queue.pause()));
  barPauseBtn.addEventListener("click", () => (queue.isPaused() ? queue.resume() : queue.pause()));
  stopBtn.addEventListener("click", () => queue.stop());
  barStartBtn.addEventListener("click", () => {
    if (queue.isRunning() && queue.isPaused()) queue.resume();
    else queue.start();
  });
  retryAllBtn.addEventListener("click", () => queue.retryFailed());
  exportBtn.addEventListener("click", async () => {
    const links = queue.successLinks();
    if (links.length === 0) {
      pushNotice("info", "还没有成功的上传，清单是空的。");
      return;
    }
    // 导出 = 复制直链列表（一行一条），用户可以粘进任何地方存起来
    const ok = await copyText(links.join("\n"));
    if (ok) toastWithCode(`✓ 已复制 ${links.length} 条直链`, links[0] ?? "");
    else toastError(new Error("复制失败，请检查浏览器剪贴板权限"));
  });

  const noticesHost = h("div");

  const root = h("div.upload-panel");
  root.append(
    noticesHost,
    desktopCard,
    mobileCard,
    previewCard,
    settingsCard,
    queueCard,
    uploadBar,
    fileInput,
    folderInput,
    albumInput,
    cameraInput,
    anyFileInput,
  );

  // ---------- 渲染 ----------

  const previewRefs = new Map<string, HTMLElement>();
  // 行表按 id 记住四个会被就地更新的格子，进度事件只改这几处，不重建整个表
  const rowRefs = new Map<string, { row: HTMLElement; status: HTMLElement; note: HTMLElement; op: HTMLElement }>();

  function renderNotices(): void {
    const children = notices.map((notice) =>
      qtip(notice.tone === "err" ? "err" : notice.tone, null, notice.text),
    );
    noticesHost.replaceChildren(
      ...children.map((node) => h("div", { style: { marginBottom: "10px" } }, node)),
    );
  }

  function pushNotice(tone: "info" | "ok" | "warn" | "err", text: string): void {
    notices = [...notices, { tone, text }].slice(-NOTICE_LIMIT);
    renderNotices();
  }

  function renderPreview(): void {
    const items = queue.snapshot();
    const stats = queue.stats();
    previewCard.style.display = items.length > 0 ? "" : "none";
    if (items.length === 0) return;

    previewHead.replaceChildren(
      document.createTextNode("待上传预览 "),
      h(
        "small",
        {
          text: `${stats.total} 张 · 合计 ${formatBytes(stats.bytesTotal)}${
            stats.success > 0 ? ` · 已完成 ${stats.success} 张` : ""
          }`,
        },
      ),
    );
    previewGrid.replaceChildren();
    previewRefs.clear();
    for (const item of items.slice(0, MAX_PREVIEW)) {
      previewGrid.append(previewItem(item));
    }
    previewMore.textContent =
      items.length > MAX_PREVIEW ? `只展示了前 ${MAX_PREVIEW} 张，其余 ${items.length - MAX_PREVIEW} 张在队列里继续处理。` : "";
  }

  function previewItem(item: QueueItem): HTMLElement {
    const thumb = h("div.ph");
    if (item.previewUrl) {
      thumb.append(h("img", { src: item.previewUrl, alt: item.name, loading: "lazy", decoding: "async" }));
    } else {
      thumb.append(h("span", { text: item.status === "done" || item.status === "skipped" ? "✓" : "—" }));
    }
    const status = statusBadge(item);
    pathClass(thumb, item.id);
    previewRefs.set(item.id, status);
    return h(
      "div.qitem",
      { title: `${item.name}（${formatBytes(item.size)}）` },
      h("button.rm", {
        type: "button",
        text: "×",
        "aria-label": `移除 ${item.name}`,
        on: { click: () => queue.removeItem(item.id) },
      }),
      thumb,
      h("div.nm", { text: item.name }),
      h("span.st", null, status),
    );
  }

  function pathClass(node: HTMLElement, seed: string): void {
    let hash = 0;
    for (let i = 0; i < seed.length; i += 1) hash = (hash * 31 + seed.charCodeAt(i)) % 997;
    node.classList.add(`c${(hash % 6) + 1}`);
  }

  function rowNote(item: QueueItem): { text: string; mono: boolean; danger: boolean } {
    if (item.status === "done" && item.result) {
      return { text: shortHash(item.result.sha256, "webp"), mono: true, danger: false };
    }
    if (item.status === "failed") {
      return { text: item.error ?? "上传失败", mono: false, danger: true };
    }
    return { text: item.note ?? "—", mono: false, danger: false };
  }

  function renderRows(): void {
    const items = queue.snapshot();
    rowRefs.clear();
    if (items.length === 0) {
      tableHost.replaceChildren();
      return;
    }
    const shown = items.slice(0, MAX_ROWS);
    const tbody = h("tbody");
    for (const item of shown) {
      const note = rowNote(item);
      const statusCell = h("td", { "data-label": "状态" }, statusBadge(item));
      const noteCell = h("td", {
        "data-label": "说明",
        class: note.mono ? "mono" : "",
        style: {
          fontSize: note.mono ? "11px" : "12.5px",
          color: note.danger ? "var(--danger)" : note.mono ? "var(--faint)" : "var(--muted)",
        },
        text: note.text,
      });
      const op = h("td", { "data-label": "操作" });
      if (item.status === "failed" || item.status === "needs-reselect") {
        op.append(
          h("button.btn.sm", {
            type: "button",
            text: "重试",
            on: { click: () => queue.retry(item.id) },
          }),
        );
      } else {
        op.append(
          h("button.btn.sm.gho", {
            type: "button",
            text: "移除",
            on: { click: () => queue.removeItem(item.id) },
          }),
        );
      }
      const row = h(
        "tr",
        { "data-id": item.id },
        h("td", { "data-label": "文件", text: item.name, style: { wordBreak: "break-all" } }),
        h("td", { "data-label": "大小", text: formatBytes(item.size) }),
        statusCell,
        noteCell,
        op,
      );
      rowRefs.set(item.id, { row, status: statusCell, note: noteCell, op });
      tbody.append(row);
    }
    const thead = h(
      "thead",
      null,
      h(
        "tr",
        null,
        h("th", { style: { width: "44%" }, text: "文件" }),
        h("th", { style: { width: "96px" }, text: "大小" }),
        h("th", { style: { width: "132px" }, text: "状态" }),
        h("th", { text: "说明" }),
        h("th", { style: { width: "86px" }, text: "操作" }),
      ),
    );
    const table = h("table.responsive", null, thead, tbody);
    const wrap = h("div.table-wrap", { style: { maxHeight: "260px", overflow: "auto" } }, table);
    tableHost.replaceChildren(wrap);
    if (items.length > MAX_ROWS) {
      tableHost.append(
        h("div.tip", {
          style: { padding: "8px 10px" },
          text: `明细只显示前 ${MAX_ROWS} 项，其余 ${items.length - MAX_ROWS} 项在后台继续处理。`,
        }),
      );
    }
  }

  function updateRow(item: QueueItem): void {
    const refs = rowRefs.get(item.id);
    if (!refs) return;
    refs.status.replaceChildren(statusBadge(item));
    const note = rowNote(item);
    refs.note.textContent = note.text;
    refs.note.className = note.mono ? "mono" : "";
    refs.note.style.color = note.danger ? "var(--danger)" : note.mono ? "var(--faint)" : "var(--muted)";
    refs.note.style.fontSize = note.mono ? "11px" : "12.5px";
    if (item.status === "failed" || item.status === "needs-reselect") {
      if (refs.op.dataset.mode !== "retry") {
        refs.op.dataset.mode = "retry";
        refs.op.replaceChildren(
          h("button.btn.sm", { type: "button", text: "重试", on: { click: () => queue.retry(item.id) } }),
        );
      }
    } else if (refs.op.dataset.mode !== "remove") {
      refs.op.dataset.mode = "remove";
      refs.op.replaceChildren(
        h("button.btn.sm.gho", { type: "button", text: "移除", on: { click: () => queue.removeItem(item.id) } }),
      );
    }
  }

  function updatePreviewItem(item: QueueItem): void {
    // 预览格里的徽标是队列建的那个节点，就地换内容；行表则用 rowRefs
    previewRefs.get(item.id)?.replaceChildren(statusBadge(item));
  }

  function updateProgress(): void {
    const stats = queue.stats();
    const items = queue.snapshot();
    queueCard.style.display = items.length > 0 ? "" : "none";
    uploadBar.classList.toggle("on", items.length > 0);

    barInner.style.width = `${stats.percent}%`;
    doneLabel.replaceChildren(
      document.createTextNode("已完成 "),
      h("b", { style: { color: "var(--text)" }, text: formatNumber(stats.success + stats.skipped + stats.failed) }),
      document.createTextNode(` / ${formatNumber(stats.total)}`),
    );
    const eta = queue.estimatedRemainingMs();
    etaLabel.textContent =
      queue.isRunning() && !queue.isPaused() && eta > 0 ? `预计剩余 ${formatDuration(eta / 1000)}` : "";

    const running = queue.isRunning();
    const paused = queue.isPaused();
    // 用 queuedCount 而不是 readyCount：刚选完文件的瞬间全在算哈希，
    // 这时按钮也该是可点的（点了就先置运行态，就绪后自动开传）
    const queued = queue.queuedCount();
    queueTitle.textContent = paused
      ? `已暂停（剩余 ${formatNumber(stats.waiting)} 张）`
      : running
        ? `正在上传 ${formatNumber(stats.total)} 张图片`
        : stats.total > 0
          ? `队列：${formatNumber(stats.total)} 张`
          : "";
    const settings = queue.getSettings();
    const albumName =
      [...albumSelect.options].find((option) => option.value === String(settings.albumId ?? ""))?.text ?? "默认相册";
    queueSub.textContent = `目标相册「${albumName}」${settings.tags ? ` · 标签「${settings.tags}」` : ""} · 可见性 ${
      settings.visibility === "public" ? "公开" : "私有"
    } · 并发 ${settings.concurrency}`;

    statHost.replaceChildren(
      statCard("上传成功", formatNumber(stats.success), "ok"),
      statCard("跳过（已存在）", formatNumber(stats.skipped)),
      statCard("等待中", formatNumber(stats.waiting), "muted"),
      statCard("失败", formatNumber(stats.failed), stats.failed > 0 ? "err" : "muted"),
    );

    pauseBtn.textContent = paused ? "继续" : "暂停";
    barPauseBtn.textContent = paused ? "继续" : "暂停";
    const label = `开始上传（${formatNumber(queued)} 张）`;
    // 正在跑且没暂停时按钮无事可做，置灰；其余情况要么开始要么继续
    const startDisabled = (running && !paused) || (!running && queued === 0 && queue.failedItems().length === 0);
    startBtn.textContent = running && paused ? "继续上传" : label;
    startBtn.disabled = startDisabled;
    barStartBtn.textContent = running && paused ? "继续上传" : label;
    barStartBtn.disabled = startDisabled;
    retryAllBtn.disabled = queue.failedItems().length === 0;
    clearFailedBtn.disabled = retryAllBtn.disabled;
    exportBtn.disabled = queue.successLinks().length === 0;
    clearBtn.disabled = items.length === 0;

    const hasReselect = stats.needsReselect > 0;
    settingsTip.textContent = hasReselect
      ? `${stats.needsReselect} 张需要重新选择文件；其余进度已存在本地，刷新不丢。`
      : "队列状态存在浏览器本地，刷新页面不会丢。";
  }

  function renderAll(): void {
    renderPreview();
    renderRows();
    updateProgress();
  }

  // ---------- 事件绑定 ----------

  const unsubscribe = queue.subscribe((event) => {
    if (destroyed) return;
    switch (event.kind) {
      case "items":
        renderAll();
        break;
      case "item": {
        const item = queue.snapshot().find((entry) => entry.id === event.id);
        if (!item) break;
        updateRow(item);
        updatePreviewItem(item);
        updateProgress();
        break;
      }
      case "state":
        updateProgress();
        if (albumSelect.value !== String(queue.getSettings().albumId ?? "")) {
          albumSelect.value = String(queue.getSettings().albumId ?? "");
        }
        tagsInput.value = queue.getSettings().tags;
        visibilitySelect.value = queue.getSettings().visibility;
        concurrencySelect.value = String(queue.getSettings().concurrency);
        break;
      case "notice":
        pushNotice(event.tone, event.text);
        break;
      case "error": {
        const err = event.error;
        const message = err instanceof ApiError ? err.message : err instanceof Error ? err.message : String(err);
        // 一次 1000 张的失败风暴不能变成 1000 个 toast：同样的消息 4 秒内只提示一次
        const now = Date.now();
        if (message !== lastToast.message || now - lastToast.at > 4000) {
          lastToast = { message, at: now };
          if (err instanceof ApiError && (err.code === "RATE_LIMITED" || err.code === "QUEUE_FULL")) {
            toastRateLimited(err);
          } else {
            toastError(err);
          }
        }
        break;
      }
      case "finished":
        options.onFinished?.(event.stats);
        break;
    }
  });

  const uploadUnsubscribe = options.onUploaded ? queue.onUploaded(options.onUploaded) : () => undefined;

  const onPaste = (event: ClipboardEvent): void => {
    if (destroyed || !root.isConnected) return;
    const target = event.target as HTMLElement | null;
    // 在输入框里粘贴文本不要触发上传
    if (target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA")) return;
    const files = Array.from(event.clipboardData?.files ?? []);
    if (files.length === 0) return;
    event.preventDefault();
    queue.addFiles(files.map((file) => ({ file })));
  };
  document.addEventListener("paste", onPaste);

  // 拖到面板外松手时不要让浏览器直接打开这个文件
  const preventDrop = (event: DragEvent): void => {
    if (!destroyed && root.isConnected) event.preventDefault();
  };
  document.addEventListener("dragover", preventDrop);
  document.addEventListener("drop", preventDrop);

  void queue.hydrate().then(() => {
    if (!destroyed) renderAll();
  });
  renderAll();

  return {
    root,
    refreshAlbums(albums, albumId) {
      setAlbumOptions(albums, albumId);
      updateProgress();
    },
    destroy() {
      destroyed = true;
      unsubscribe();
      uploadUnsubscribe();
      document.removeEventListener("paste", onPaste);
      document.removeEventListener("dragover", preventDrop);
      document.removeEventListener("drop", preventDrop);
      // 离开页面时立刻落盘：节流窗口里的状态不能因为一次导航就丢
      queue.flushSave();
      root.remove();
    },
  };
}
