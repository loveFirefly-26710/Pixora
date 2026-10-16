/**
 * 我的相册。
 *
 * 两种视图（网格 / 列表）共用一套选择模型：
 *   左键单击打开预览浮层（不跳页），Shift 按范围连选，Ctrl 离散多选，
 *   移动端长按 500ms 进多选模式后逐张点选，Esc 关浮层或取消选择。
 *
 * 选择状态放在页面级对象里，DOM 只做增量同步（切 class / 改 checkbox）——
 * 一页几十个 tile，每选一张就重建整个网格会让滚动位置和浮层都抖。
 */

import { ApiError, request, requestPage } from "../core/api";
import { copyText, debounce, h, mount } from "../core/dom";
import {
  formatBytes,
  formatDateTime,
  formatDimensions,
  formatNumber,
  formatQuota,
  formatShortDateTime,
  shortHash,
} from "../core/format";
import { confirmDanger, openModal, promptDialog } from "../core/modal";
import { navigate, type PageContext } from "../core/router";
import { getState, setViewingUid } from "../core/state";
import { loadViewPreference, saveViewPreference } from "../core/store";
import { toastError, toastOk, toastWithCode } from "../core/toast";
import type { AdminUserRow, AlbumList, ImageItem } from "../core/types";
import { applyMasonry } from "../ui/masonry";
import { userViewBanner } from "../ui/shell";
import {
  emptyState,
  imageTile,
  newShuffleSeed,
  pageHead,
  progressBar,
  qtip,
  skeletonGrid,
  sortSelect,
  spinner,
  thumbImage,
} from "../ui/widgets";

type SortKey = string;

const PER_PAGE = 24;
/** 长按进入多选的门槛：低于 400ms 会被误触发，高于 600ms 用户会以为没反应。 */
const LONG_PRESS_MS = 500;
const NARROW = "(max-width: 767px)";

/** 列表里显示的标题：title → 原文件名 → 短哈希，三者都空是不可能的。 */
function titleOf(item: ImageItem): string {
  return item.title ?? item.original_name ?? shortHash(item.sha256, item.ext);
}

/** 下载走 `?download=1`，服务端会带 Content-Disposition；download 属性在跨域时会被浏览器忽略。 */
function downloadUrl(item: ImageItem): string {
  return `${item.url}${item.url.includes("?") ? "&" : "?"}download=1`;
}

function isNarrow(): boolean {
  return window.matchMedia(NARROW).matches;
}

export async function render(ctx: PageContext): Promise<void> {
  const parsedAlbumId = Number(ctx.params.albumId ?? "");
  const viewingUid = getState().viewingUid;

  const state = {
    albumId: Number.isFinite(parsedAlbumId) && parsedAlbumId > 0 ? parsedAlbumId : null,
    q: ctx.query.get("q") ?? "",
    // 默认随机：相册里也总有「最新的几张把后面全挡住」的问题
    sort: (ctx.query.get("sort") ?? "shuffle") as SortKey,
    // 随机排序的种子：同一次浏览内保持不变，翻页/换页不会重复或漏图
    seed: newShuffleSeed(),
    view: loadViewPreference(),
    page: 1,
    items: [] as ImageItem[],
    /** state.items[0] 在整个结果里的绝对下标：浮层翻页加载相邻页时用它换算 */
    offset: 0,
    total: 0,
    totalPages: 1,
    albums: null as AlbumList | null,
    selected: new Set<number>(),
    anchor: null as number | null,
    lightboxOpen: false,
    /**
     * 「整相册/整库全选」模式。
     *
     * 打开之后选中的不是当前页那 24 张，而是这个范围里的全部图片：
     * 批量操作走 `filter` 让服务端自己去匹配，不必把几千个 id 拉到浏览器再发回去。
     * 一旦手动勾选/取消某一张，就退回普通的多选（用户想挑具体的那些）。
     */
    allMatching: false,
  };

  /** 长按抬起时会补发一次 click，这个标记让 capture 处理器吞掉它，避免刚选中又被切掉。 */
  let suppressClick = false;

  const root = h("div.pad");
  ctx.outlet.append(root);

  // ---------- 骨架：先占位再拉数据，避免白屏 ----------

  const headHost = h("div");
  const sideHost = h("div.albumlist");
  // 工具栏容器：sticky 挂在它身上（挂工具栏自己不行——sticky 只能在父容器
  // 的高度范围内移动，而父容器就只有这一条工具栏那么高，等于没粘住）
  const toolbarHost = h("div.albums-toolbar");
  const listHost = h("div");
  // 页脚：状态文字/按钮居中，往下就是无限滚动的哨兵
  const pagerHost = h("div.albums-more", {
    style: {
      display: "flex",
      alignItems: "center",
      justifyContent: "center",
      gap: "8px",
      padding: "18px 0 6px",
      color: "var(--muted)",
      fontSize: "12.5px",
    },
  });
  const selbar = h("div.selbar");
  const body = h("div.albums-body", null, toolbarHost, listHost, pagerHost);
  root.append(headHost, h("div.albums-split", null, sideHost, body), selbar);

  // 无限滚动的哨兵：它进入视口就取下一页（提前 600px 预取，滑到底不用等）
  const sentinel = h("div", { style: { height: "1px" } });
  pagerHost.after(sentinel);
  if (typeof IntersectionObserver === "function") {
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) void loadMore();
      },
      { rootMargin: "600px 0px" },
    );
    observer.observe(sentinel);
  }
  mount(
    sideHost,
    ...Array.from({ length: 4 }, () => h("div.skel", { style: { height: "30px", marginBottom: "8px" } })),
  );
  mount(listHost, skeletonGrid(8));

  if (viewingUid) {
    headHost.append(
      buildViewBanner(viewingUid, () => {
        setViewingUid(null);
        navigate("/albums");
      }),
    );
  }
  headHost.append(
    pageHead(
      "我的相册",
      "每张图左上角有勾选框；左键单击图片打开预览，Shift 连选一段、Ctrl 点选多张；移动端长按即选中。",
    ),
  );



  // ---------- 常规工具栏（建一次，避免搜索框每次输入都被重建打断） ----------

  const searchInput = h("input.f", {
    placeholder: "在相册内搜索…",
    value: state.q,
    style: { width: "180px", padding: "4px 10px", fontSize: "12.5px" },
  });
  searchInput.addEventListener(
    "input",
    debounce(() => {
      state.q = searchInput.value.trim();
      void loadImages(1);
    }, 320),
  );

  const sortControl = sortSelect(state.sort, (value) => {
    state.sort = value;
    // 换排序时换一个种子：「随机」切回来才是一份新顺序
    if (value === "shuffle") state.seed = newShuffleSeed();
    void loadImages(1);
  });
  sortControl.style.width = "150px";
  sortControl.style.padding = "4px 10px";
  sortControl.style.fontSize = "12.5px";

  const gridChip = h("button.chip", {
    type: "button",
    text: "网格",
    on: { click: () => setView("grid") },
  });
  const listChip = h("button.chip", {
    type: "button",
    text: "列表",
    on: { click: () => setView("list") },
  });
  // 整范围全选：一次点完整个相册（或全部图片），批量操作交给服务端按条件匹配
  const allMatchingBtn = h("button.btn.sm", { type: "button", text: "全选" });
  allMatchingBtn.addEventListener("click", () => {
    if (state.allMatching) {
      cancelSelection();
      return;
    }
    enterAllMatching();
  });

  const normalToolbar = h(
    "div.toolbar",
    null,
    gridChip,
    listChip,
    h("span.spacer"),
    searchInput,
    sortControl,
    allMatchingBtn,
  );

  // ---------- 批量操作条：宽屏完整版，窄屏紧凑版 + 底部固定条 ----------

  const wideCount = h("span");
  const wideBatch = h(
    "div.toolbar.sel.sel-wide",
    null,
    wideCount,
    h("button.btn.sm", { type: "button", text: "打标签", on: { click: () => void batchTag() } }),
    h("button.btn.sm", { type: "button", text: "移动到相册", on: { click: () => void batchMove() } }),
    h("button.btn.sm", { type: "button", text: "复制直链", on: { click: () => void batchCopy() } }),
    h("button.btn.sm.dgr", { type: "button", text: "删除", on: { click: () => void batchDelete() } }),
    h("span.spacer"),
    h("button.btn.sm.gho", { type: "button", text: "全选已加载", on: { click: () => selectPage() } }),
    h("button.btn.sm.gho", { type: "button", text: "取消选择", on: { click: () => cancelSelection() } }),
  );

  const narrowCount = h("span", { style: { fontWeight: "600", color: "var(--primary)" } });
  const narrowBatch = h(
    "div.toolbar.sel.sel-narrow",
    null,
    narrowCount,
    h("span.spacer"),
    h("button.chip", { type: "button", text: "全选已加载", on: { click: () => selectPage() } }),
    h("button.chip", { type: "button", text: "取消", on: { click: () => cancelSelection() } }),
  );

  const selbarActions: Array<[string, string, () => void]> = [
    ["打标签", "", () => void batchTag()],
    ["移动", "", () => void batchMove()],
    ["复制直链", "", () => void batchCopy()],
    ["删除", "dgr", () => void batchDelete()],
  ];
  mount(
    selbar,
    ...selbarActions.map(([label, tone, action]) =>
      h("button.btn", { type: "button", class: tone, text: label, on: { click: action } }),
    ),
  );

  toolbarHost.append(normalToolbar, wideBatch, narrowBatch);

  // ---------- 键盘：Esc 关浮层或取消选择 ----------

  const onKeyDown = (event: KeyboardEvent): void => {
    if (!root.isConnected) {
      document.removeEventListener("keydown", onKeyDown);
      return;
    }
    if (event.key !== "Escape") return;
    if (document.querySelector(".lb")) return; // 浮层自己处理 Esc
    if (state.selected.size > 0) cancelSelection();
  };
  document.addEventListener("keydown", onKeyDown);

  // ---------- 数据加载 ----------

  async function loadAlbums(): Promise<void> {
    try {
      const data = await request<AlbumList>("/albums", {
        query: { uid: viewingUid ?? undefined },
      });
      state.albums = data;
      renderSidebar(data);
    } catch (err) {
      mount(
        sideHost,
        h("div.tip", {
          style: { padding: "8px" },
          text: err instanceof ApiError ? err.message : "相册列表加载失败",
        }),
        h("button.btn.sm", {
          type: "button",
          text: "重试",
          on: { click: () => void loadAlbums() },
        }),
      );
    }
  }

  async function loadImages(
    page: number,
    options: { append?: boolean; prepend?: boolean } = {},
  ): Promise<void> {
    if (!options.append && !options.prepend) {
      mount(listHost, skeletonGrid(8));
      pagerHost.replaceChildren();
    }
    try {
      const result = await requestPage<ImageItem>("/images", {
        query: {
          uid: viewingUid ?? undefined,
          album_id: state.albumId ?? undefined,
          q: state.q || undefined,
          sort: state.sort,
          seed: state.seed,
          page,
          per_page: PER_PAGE,
        },
      });
      state.total = result.meta.total;
      state.totalPages = result.meta.total_pages;
      const perPage = result.meta.per_page || PER_PAGE;
      // 删到当前页为空时回退一页，否则用户会停在一个空列表上（上一页还有图）
      if (!options.append && !options.prepend && result.items.length === 0 && page > 1 && result.meta.total > 0) {
        await loadImages(page - 1);
        return;
      }
      if (options.append) {
        state.items = [...state.items, ...result.items];
        // 追加也要推进「当前页」：不推进的话下一轮还会再取同一页，
        // 表现是同一批图被反复追加（列表越滑越长，件件重复）
        state.page = page;
      } else if (options.prepend) {
        state.items = [...result.items, ...state.items];
        state.offset = (page - 1) * perPage;
      } else {
        state.items = result.items;
        state.page = page;
        state.offset = (page - 1) * perPage;
      }
      renderList();
      renderPager(result.meta.page);
    } catch (err) {
      if (options.append) {
        // 追加失败只影响这一页：列表留着，页脚给「重试」
        loadMoreFailed = true;
        renderPager(state.page);
        toastError(err);
        return;
      }
      renderError(err);
    }
  }

  function renderError(err: unknown): void {
    const message = err instanceof ApiError ? err.message : "图片列表加载失败";
    mount(
      listHost,
      qtip("err", "加载失败", message),
      h(
        "div",
        { style: { marginTop: "12px" } },
        h("button.btn", { type: "button", text: "重试", on: { click: () => void loadImages(state.page) } }),
      ),
    );
    pagerHost.replaceChildren();
  }

  let loadingMore = false;
  let loadMoreFailed = false;

  /** 底部状态：正在加载 / 加载失败可重试 / 没有更多了 / 手动加载更多的兜底按钮。 */
  function renderPager(_page: number): void {
    if (loadingMore) {
      pagerHost.replaceChildren(spinner(), h("span", { text: "正在加载…" }));
      return;
    }
    if (loadMoreFailed) {
      pagerHost.replaceChildren(
        h("span", { text: "加载失败" }),
        h("button.btn.sm", { type: "button", text: "重试", on: { click: () => void loadMore() } }),
      );
      return;
    }
    if (state.page >= state.totalPages) {
      pagerHost.replaceChildren(h("span", { text: state.page === 0 ? "" : "没有更多了" }));
      return;
    }
    pagerHost.replaceChildren(
      h("button.btn.sm", { type: "button", text: "加载更多", on: { click: () => void loadMore() } }),
    );
  }

  /** 往下滑到底就取下一页：与广场同一套做法（提前 600px 预取）。 */
  async function loadMore(): Promise<void> {
    if (loadingMore || state.page >= state.totalPages) return;
    loadingMore = true;
    loadMoreFailed = false;
    renderPager(state.page);
    try {
      await loadImages(state.page + 1, { append: true });
    } finally {
      loadingMore = false;
      // 收尾一定要重画页脚：加载中那一帧是 loadImages 里画的，
      // 只把标志位改回来、不重画的话，页脚会一直停在「正在加载…」
      renderPager(state.page);
    }
  }

  // ---------- 相册侧栏 ----------

  function renderSidebar(data: AlbumList): void {
    const link = (id: number | null, name: string, count: number): HTMLElement =>
      h(
        "a",
        {
          href: id === null ? "#/albums" : `#/albums/${id}`,
          class: state.albumId === id ? "on" : "",
        },
        h("span", { text: "▤" }),
        h("span", { text: name, style: { overflow: "hidden", textOverflow: "ellipsis" } }),
        h("span.c", { text: formatNumber(count) }),
      );

    const usage = data.usage;
    mount(
      sideHost,
      h("div.par", { text: "我的相册" }),
      link(null, data.all.name || "全部图片", data.all.image_count),
      ...data.albums.map((album) => link(album.id, album.name, album.image_count)),
      h("button.new", {
        type: "button",
        text: "＋ 新建相册",
        on: { click: () => void createAlbum() },
      }),
      h("a.new", { href: "#/albums/manage", text: "管理相册" }),
      h("div.par.album-usage", { text: "用量" }),
      h(
        "div.album-usage",
        { style: { padding: "0 9px" } },
        progressBar(usage.used_bytes, usage.quota_bytes),
        h("div.tip", {
          style: { marginTop: "6px" },
          text: `已用 ${formatQuota(usage.used_bytes, usage.quota_bytes)}`,
        }),
      ),
    );
  }

  async function createAlbum(): Promise<void> {
    const name = await promptDialog({
      title: "新建相册",
      label: "相册名称",
      placeholder: "例如：旅行",
      hint: "slug 会按名称自动生成，重名时会提示冲突。",
    });
    if (!name) return;
    try {
      await request("/albums", { json: { name } });
      toastOk(`已创建相册「${name}」`);
      await loadAlbums();
    } catch (err) {
      toastError(err);
    }
  }

  // ---------- 视图与列表 ----------

  function setView(view: "grid" | "list"): void {
    if (state.view === view) return;
    state.view = view;
    saveViewPreference(view);
    applyViewChips();
    renderList();
  }

  function applyViewChips(): void {
    gridChip.classList.toggle("on", state.view === "grid");
    listChip.classList.toggle("on", state.view === "list");
  }
  applyViewChips();

  function renderList(): void {
    if (state.items.length === 0) {
      mount(
        listHost,
        emptyState(
          "▤",
          state.q ? `没有找到与「${state.q}」相关的图片。` : "这里还没有图片，去上传页选几张吧。",
          state.q
            ? undefined
            : h("button.btn.pri", {
                type: "button",
                text: "去上传",
                on: { click: () => navigate("/upload") },
              }),
        ),
      );
      syncSelectionUI();
      return;
    }
    if (state.view === "grid") renderGrid();
    else renderTable();
    syncSelectionUI();
  }

  function thumbFor(item: ImageItem): HTMLElement {
    const thumb = h("div.ph", { class: `c${(item.id % 6) + 1}` });
    // 有缩略图用缩略图；没有就用原图按容器缩放，带宽影响有限（契约 4.1）。
    // 读不出来时由 thumbImage 重试一次、再退回原图
    thumb.append(
      thumbImage({
        url: item.url,
        thumb_url: item.thumb_url,
        has_thumb: item.has_thumb,
        title: titleOf(item),
      }),
    );
    return thumb;
  }

  function renderGrid(): void {
    const grid = h("div.grid.igrid");
    state.items.forEach((item, index) => {
      grid.append(
        imageTile(item, {
          selected: state.selected.has(item.id),
          // 勾选框常驻：选图不用先点「选择」进模式
          checkbox: true,
          onOpen: () => openLightbox(state.offset + index),
          onSelect: (target, event) => handleTileSelect(target, event),
        }),
      );
    });
    mount(listHost, grid);
    applyMasonry(grid);
    attachLongPress(grid, ".tile[data-image-id]");
  }

  function renderTable(): void {
    selectAllRef = h("input", { type: "checkbox", "aria-label": "全选已加载的图片" });
    selectAllRef.addEventListener("change", () => {
      if (selectAllRef?.checked) selectPage();
      else clearPageSelection();
    });

    const headers = h(
      "tr",
      null,
      h("th", { style: { width: "36px" } }, selectAllRef),
      h("th", { style: { width: "52px" }, text: "预览" }),
      h("th", { text: "文件名" }),
      h("th", { text: "标签" }),
      h("th", { style: { width: "88px" }, text: "大小" }),
      h("th", { class: "hide-md", style: { width: "118px" }, text: "尺寸" }),
      h("th", { class: "hide-md", style: { width: "130px" }, text: "上传时间" }),
      h("th", { style: { width: "158px" }, text: "操作" }),
    );

    const tbody = h("tbody");
    state.items.forEach((item, index) => {
      const checkbox = h("input", {
        type: "checkbox",
        checked: state.selected.has(item.id),
        "aria-label": `选择 ${titleOf(item)}`,
      });
      checkbox.addEventListener("click", (event) => event.stopPropagation());
      checkbox.addEventListener("change", () => toggleSelect(item.id, { anchor: true }));

      const mini = thumbFor(item);
      mini.classList.add("mini");
      const previewCell = h("td", { "data-label": "预览" }, mini);
      previewCell.style.cursor = "pointer";
      previewCell.addEventListener("click", (event) => {
        event.stopPropagation();
        openLightbox(state.offset + index);
      });

      const row = h(
        "tr",
        { "data-image-id": String(item.id) },
        h("td", { "data-label": "" }, checkbox),
        previewCell,
        h(
          "td",
          { "data-label": "文件名" },
          h("b", { text: titleOf(item) }),
          h("div.mono", {
            style: { color: "var(--faint)", fontSize: "11px" },
            text: `${shortHash(item.sha256, item.ext)}`,
          }),
        ),
        h(
          "td",
          { "data-label": "标签" },
          ...item.tags.slice(0, 3).map((tag) => h("span.tag", { text: tag })),
        ),
        h("td", { "data-label": "大小", text: formatBytes(item.size) }),
        h("td", { "data-label": "尺寸", class: "hide-md", text: formatDimensions(item.width, item.height) }),
        h("td", { "data-label": "上传时间", class: "hide-md", text: formatShortDateTime(item.upload_time) }),
        h(
          "td",
          { "data-label": "操作" },
          h(
            "button.btn.sm",
            { type: "button", text: "复制", on: { click: (event) => void copyLink(item, event as MouseEvent) } },
          ),
          " ",
          h("button.btn.sm", {
            type: "button",
            text: "预览",
            on: { click: () => openLightbox(state.offset + index) },
          }),
          " ",
          h("button.btn.sm.dgr", {
            type: "button",
            text: "删除",
            on: { click: (event) => void deleteOne(item, event as MouseEvent) },
          }),
        ),
      );
      tbody.append(row);
    });

    const table = h("table.responsive", null, h("thead", null, headers), tbody);
    mount(listHost, h("div.table-wrap", null, table));

    // 行点击：Shift 按行范围连选，Ctrl 离散，普通点击单选（文件管理器的习惯）
    const onRowClick = (event: Event): void => {
      const target = event.target as HTMLElement;
      if (target.closest("input, button")) return;
      const row = target.closest<HTMLElement>("tr[data-image-id]");
      if (!row) return;
      const id = Number(row.dataset.imageId);
      const mouse = event as MouseEvent;
      if (mouse.shiftKey) {
        selectRange(id);
        return;
      }
      if (mouse.ctrlKey || mouse.metaKey) {
        toggleSelect(id, { anchor: true });
        return;
      }
      state.selected.clear();
      state.selected.add(id);
      state.anchor = id;
      syncSelectionUI();
    };
    table.addEventListener("click", onRowClick);
    attachLongPress(table, "tr[data-image-id]");
  }

  // ---------- 选择模型 ----------

  let selectAllRef: HTMLInputElement | null = null;

  function indexOfId(id: number): number {
    return state.items.findIndex((item) => item.id === id);
  }

  /** 已经加载出来的项（无限滚动下就是列表里现有的这些），用于全选与表头半选判定。 */
  function pageItems(): ImageItem[] {
    return state.items;
  }

  function toggleSelect(id: number, options: { anchor?: boolean } = {}): void {
    // 手动点选就意味着「我要的是这几个」，退出整范围模式
    state.allMatching = false;
    if (state.selected.has(id)) state.selected.delete(id);
    else state.selected.add(id);
    if (options.anchor) state.anchor = id;
    syncSelectionUI();
  }

  function cancelSelection(): void {
    state.selected.clear();
    state.anchor = null;
    state.allMatching = false;
    syncSelectionUI();
  }

  function selectPage(): void {
    state.allMatching = false;
    for (const item of pageItems()) state.selected.add(item.id);
    syncSelectionUI();
  }

  function clearPageSelection(): void {
    state.allMatching = false;
    for (const item of pageItems()) state.selected.delete(item.id);
    syncSelectionUI();
  }

  /**
   * Shift 范围选择。
   *
   * 网格：先按列数把下标折成行；同一行内取连续区间，跨行则整行整段选中——
   * 原型里的说法是「中间四行整段选中」，跨行时按整行取才符合「整段」。
   * 列表：行就是项，直接取连续区间。
   */
  function selectRange(targetId: number): void {
    state.allMatching = false;
    const anchorId = state.anchor;
    if (anchorId === null) {
      toggleSelect(targetId, { anchor: true });
      return;
    }
    const from = indexOfId(anchorId);
    const to = indexOfId(targetId);
    if (from < 0 || to < 0) return;
    const start = Math.min(from, to);
    const end = Math.max(from, to);

    if (state.view === "list") {
      for (let index = start; index <= end; index += 1) state.selected.add(state.items[index]!.id);
      syncSelectionUI();
      return;
    }

    const grid = listHost.querySelector<HTMLElement>(".igrid");
    const columns = grid ? countColumns(grid) : 1;
    const isSameRow = Math.floor(start / columns) === Math.floor(end / columns);
    const firstIndex = isSameRow ? start : Math.floor(start / columns) * columns;
    const lastIndex = isSameRow ? end : Math.floor(end / columns) * columns + columns - 1;
    for (let index = firstIndex; index <= Math.min(lastIndex, state.items.length - 1); index += 1) {
      state.selected.add(state.items[index]!.id);
    }
    syncSelectionUI();
  }

  function handleTileSelect(item: ImageItem, event: MouseEvent): void {
    if (event.shiftKey && state.anchor !== null) selectRange(item.id);
    else toggleSelect(item.id, { anchor: true });
  }

  /** 从 computed style 里数网格列数：布局随断点变，只有浏览器知道现在是几列。 */
  function countColumns(element: HTMLElement): number {
    const template = window.getComputedStyle(element).gridTemplateColumns;
    const tracks = template.split(" ").filter((track) => track && track !== "none");
    return tracks.length > 0 ? tracks.length : 1;
  }

  /** 触屏长按进入多选：pointerdown 起计时，移动超过 10px 或抬手即取消。 */
  function attachLongPress(container: HTMLElement, selector: string): void {
    let timer: number | undefined;
    let candidate: HTMLElement | null = null;
    let startX = 0;
    let startY = 0;
    const cancel = (): void => {
      if (timer !== undefined) {
        window.clearTimeout(timer);
        timer = undefined;
      }
      candidate = null;
    };
    // 长按抬起后浏览器会补发一次 click：吞掉它，别顺手把预览也打开
    container.addEventListener(
      "click",
      (event) => {
        if (!suppressClick) return;
        suppressClick = false;
        event.preventDefault();
        event.stopPropagation();
      },
      true,
    );
    container.addEventListener("pointerdown", (event) => {
      if (event.pointerType !== "touch") return;
      const node = (event.target as HTMLElement).closest<HTMLElement>(selector);
      if (!node) return;
      candidate = node;
      startX = event.clientX;
      startY = event.clientY;
      if (timer !== undefined) window.clearTimeout(timer);
      timer = window.setTimeout(() => {
        timer = undefined;
        const target = candidate;
        if (!target) return;
        // 长按即勾选/取消（不再有「进多选模式」这一步）
        suppressClick = true;
        const id = Number(target.dataset.imageId);
        if (Number.isFinite(id)) toggleSelect(id, { anchor: true });
        if (typeof navigator.vibrate === "function") navigator.vibrate(10);
      }, LONG_PRESS_MS);
    });
    container.addEventListener("pointermove", (event) => {
      if (timer === undefined) return;
      if (Math.abs(event.clientX - startX) > 10 || Math.abs(event.clientY - startY) > 10) cancel();
    });
    container.addEventListener("pointerup", cancel);
    container.addEventListener("pointercancel", cancel);
    container.addEventListener("pointerleave", cancel);
  }

  /** 选择变化后只改受影响的 DOM：切 class、同步勾选框与工具栏。 */
  function syncSelectionUI(): void {
    const count = selectedCount();
    const has = count > 0;

    for (const tile of listHost.querySelectorAll<HTMLElement>(".tile[data-image-id]")) {
      const id = Number(tile.dataset.imageId);
      const on = state.allMatching || state.selected.has(id);
      tile.classList.toggle("sel", on);
      // 勾选框也要跟着走：Shift 连选、全选本页这些路径没经过勾选框本身，
      // 只切 .sel 的话框还是空的，看起来像没选中
      const box = tile.parentElement?.querySelector<HTMLInputElement>("input.pick");
      if (box) box.checked = on;
    }
    for (const row of listHost.querySelectorAll<HTMLElement>("tr[data-image-id]")) {
      const id = Number(row.dataset.imageId);
      const on = state.allMatching || state.selected.has(id);
      row.classList.toggle("pick", on);
      const box = row.querySelector<HTMLInputElement>("input[type=checkbox]");
      if (box) box.checked = on;
    }
    if (selectAllRef) {
      const page = pageItems();
      const selectedOnPage = page.filter((item) => state.selected.has(item.id)).length;
      selectAllRef.checked = page.length > 0 && selectedOnPage === page.length;
      // 半选状态：表头勾选框支持全选 / 全不选 / 部分选
      selectAllRef.indeterminate = selectedOnPage > 0 && selectedOnPage < page.length;
    }

    normalToolbar.style.display = has ? "none" : "";
    wideBatch.classList.toggle("on", has);
    narrowBatch.classList.toggle("on", has);
    selbar.classList.toggle("on", has && isNarrow());
    const prefix = state.allMatching ? "已选全部 " : "已选 ";
    wideCount.replaceChildren(
      document.createTextNode(prefix),
      h("b", { text: String(count) }),
      document.createTextNode(" 张"),
    );
    narrowCount.textContent = `${prefix}${count} 张`;
    if (allMatchingBtn) {
      allMatchingBtn.textContent = state.allMatching ? "取消全选" : `全选 ${state.total} 张`;
    }
  }

  // ---------- 批量操作 ----------

  /** 当前算多少张：整范围模式下是总数，否则是勾选数。 */
  function selectedCount(): number {
    return state.allMatching ? state.total : state.selected.size;
  }

  /** 整范围模式对应的筛选条件：看的是哪个相册（全部图片则不限相册）。 */
  function scopeFilter(): Record<string, unknown> {
    return { album_id: state.albumId };
  }

  async function runBatch(
    action: "delete" | "move" | "tag" | "visibility",
    extra: Record<string, unknown>,
  ): Promise<number> {
    const body: Record<string, unknown> = { action, ...extra };
    if (state.allMatching) {
      // 交给服务端按范围匹配：几千张图不必先发 id 列表上来
      body.filter = scopeFilter();
    } else {
      body.ids = [...state.selected];
    }
    const result = await request<{ affected: number }>("/images/batch", {
      json: body,
      query: { uid: viewingUid ?? undefined },
    });
    return result.affected ?? selectedCount();
  }

  /** 进入整范围全选：先把当前页勾上（视觉上立刻有反馈），计数用总数。 */
  function enterAllMatching(): void {
    if (state.total === 0) return;
    state.allMatching = true;
    state.selected = new Set(state.items.map((item) => item.id));
    state.anchor = null;
    syncSelectionUI();
  }

  async function afterBatch(message: string): Promise<void> {
    toastOk(message);
    cancelSelection();
    // 累加出来的列表不能只刷「最后一页」：从第一页重新拉一遍
    await Promise.all([loadImages(1), loadAlbums()]);
  }

  async function batchTag(): Promise<void> {
    const count = selectedCount();
    if (count === 0) return;
    const input = await promptDialog({
      title: "批量打标签",
      label: `给${state.allMatching ? `全部 ${count} 张` : `选中的 ${count} 张`}图片追加标签（逗号分隔）`,
      placeholder: "风景,旅行",
      confirmText: "追加标签",
    });
    if (!input) return;
    const tags = input
      .split(/[,，\s]+/)
      .map((tag) => tag.trim())
      .filter(Boolean);
    if (tags.length === 0) return;
    try {
      const affected = await runBatch("tag", { tags });
      await afterBatch(`已为 ${affected} 张图片追加标签`);
    } catch (err) {
      toastError(err);
    }
  }

  async function batchMove(): Promise<void> {
    const count = selectedCount();
    if (count === 0) return;
    const albums = state.albums?.albums ?? [];
    // 没有「移出相册」这个选项：图片总得有归属，不指定就落在默认相册
    const selectEl = h(
      "select.f",
      null,
      ...albums.map((album) => h("option", { value: String(album.id), text: album.name })),
    );
    // 默认相册排在最前（服务端保证它存在），选它＝回到默认相册
    const fallback = albums.find((album) => album.slug === "default") ?? albums[0];
    const elsewhere = albums.find((album) => album.id !== state.albumId);
    selectEl.value = String(state.albumId === null ? (fallback?.id ?? "") : (elsewhere?.id ?? fallback?.id ?? ""));

    const handle = openModal({
      title: `移动 ${count} 张图片`,
      body: h("div.fld", null, h("label.f", { text: "目标相册" }), selectEl),
      footer: [
        h("button.btn", { type: "button", text: "取消", on: { click: () => handle.close() } }),
        h("button.btn.pri", {
          type: "button",
          text: "移动",
          on: {
            click: async () => {
              const albumId = selectEl.value ? Number(selectEl.value) : null;
              if (albumId === null) {
                toastError(new Error("请选择目标相册"));
                return;
              }
              handle.close();
              try {
                const affected = await runBatch("move", { album_id: albumId });
                await afterBatch(`已移动 ${affected} 张图片`);
              } catch (err) {
                toastError(err);
              }
            },
          },
        }),
      ],
    });
  }

  async function batchCopy(): Promise<void> {
    const urls = state.items.filter((item) => state.selected.has(item.id)).map((item) => item.url);
    if (urls.length === 0) return;
    const ok = await copyText(urls.join("\n"));
    if (ok) toastWithCode(`✓ 已复制 ${urls.length} 条直链`, urls[0] ?? "");
    else toastError(new Error("复制失败，请检查浏览器剪贴板权限"));
  }

  async function batchDelete(): Promise<void> {
    const count = selectedCount();
    if (count === 0) return;
    const scope = state.allMatching
      ? state.albumId === null
        ? "全部图片"
        : "这个相册里的全部图片"
      : `选中的 ${count} 张图片`;
    const retention = getState().site?.recycle_retention_days ?? 0;
    const ok = await confirmDanger(
      `把${scope}移到回收站？`,
      `${
        retention > 0
          ? `图片会先进回收站，${retention} 天内可以恢复（不占你的配额）。`
          : "回收站已关闭，这一步会直接彻底删除，无法恢复。"
      }${state.allMatching ? ` 一共 ${count} 张。` : ""}`,
      `移到回收站${count > 0 ? `（${count} 张）` : ""}`,
    );
    if (!ok) return;
    try {
      const affected = await runBatch("delete", {});
      await afterBatch(`已删除 ${affected} 张图片`);
    } catch (err) {
      toastError(err);
    }
  }

  async function copyLink(item: ImageItem, event: MouseEvent): Promise<void> {
    event.stopPropagation();
    const ok = await copyText(item.url);
    if (ok) toastWithCode("✓ 已复制直链", item.url);
  }

  async function deleteOne(item: ImageItem, event: MouseEvent): Promise<void> {
    event.stopPropagation();
    const ok = await confirmDanger(
      "删除这张图片？",
      `「${titleOf(item)}」删除后不可撤销。`,
      "删除",
    );
    if (!ok) return;
    try {
      await request(`/images/${item.id}`, { method: "DELETE", query: { uid: viewingUid ?? undefined } });
      toastOk("已删除 1 张图片");
      state.selected.delete(item.id);
      await Promise.all([loadImages(1), loadAlbums()]);
    } catch (err) {
      toastError(err);
    }
  }

  // ---------- 预览浮层 ----------

  /** 加载相邻页：浮层里 ← → 翻到已加载窗口之外时按需取下一页/上一页。 */
  async function ensureAbsolute(target: number): Promise<void> {
    if (target >= state.offset && target < state.offset + state.items.length) return;
    const page = Math.floor(target / PER_PAGE) + 1;
    if (page < 1 || page > state.totalPages) return;
    const firstLoadedPage = Math.floor(state.offset / PER_PAGE) + 1;
    await loadImages(page, {
      append: page > firstLoadedPage,
      prepend: page < firstLoadedPage,
    });
  }

  function openLightbox(index: number): void {
    if (state.lightboxOpen) return;
    const item = state.items[index];
    if (!item) return;
    state.lightboxOpen = true;

    let current = index;
    let scale = 1;
    let tx = 0;
    let ty = 0;
    let mode: "idle" | "pan" | "swipe" | "pinch" | "pull" = "idle";
    let touchStart: { x: number; y: number } | null = null;
    let dragOrigin = { x: 0, y: 0 };
    let pinchDistance = 0;
    let moveX = 0;
    let moveY = 0;

    const img = h("img", { alt: "", draggable: false });
    img.addEventListener("dragstart", (event) => event.preventDefault());

    const titleEl = h("b");
    const countEl = h("span.cnt");
    const closeBtn = h("button.lb-x", { type: "button", text: "×", "aria-label": "关闭预览" });
    const prevBtn = h("button.lb-nav", { type: "button", text: "‹", "aria-label": "上一张" });
    const nextBtn = h("button.lb-nav", { type: "button", text: "›", "aria-label": "下一张" });
    const stage = h("div.lb-stage", null, prevBtn, img, nextBtn);
    const side = h("div.lb-side");
    const actions = h("div.lb-actions");
    const thumbs = h("div.lb-thumbs");
    const hint = h("span.lb-hint", {
      style: { color: "#94a3b8", fontSize: "12px" },
      text: "← → 切换 · Esc 关闭 · 滚轮缩放",
    });

    const overlay = h(
      "div.lb",
      { role: "dialog", "aria-modal": "true" },
      h("div.lb-top", null, titleEl, countEl, h("span.spacer"), hint, closeBtn),
      h("div.lb-main", null, stage, side),
      actions,
      thumbs,
    );

    function apply(animate = false): void {
      img.style.transition = animate ? "transform .16s ease, opacity .16s ease" : "none";
      img.style.transform = `translate(${tx}px, ${ty}px) scale(${scale})`;
      img.style.opacity = "1";
      // 光标跟着缩放态走：1× 是「点开」，放大后是「拖动」
      img.classList.toggle("zoomed", scale > 1);
    }

    function resetZoom(): void {
      scale = 1;
      tx = 0;
      ty = 0;
      apply();
    }

    function setScale(next: number): void {
      scale = Math.min(6, Math.max(1, next));
      // 缩回原始比例时把平移一起归零，否则图会飘在角落
      if (scale === 1) {
        tx = 0;
        ty = 0;
      }
      apply();
    }

    function currentItem(): ImageItem | null {
      return state.items[current] ?? null;
    }

    function show(): void {
      const target = currentItem();
      if (!target) return;
      const absolute = state.offset + current;
      titleEl.textContent = titleOf(target);
      countEl.textContent = `${formatNumber(absolute + 1)} / ${formatNumber(state.total)}`;
      img.src = target.url;
      img.alt = titleOf(target);
      resetZoom();
      renderSide(target);
      renderActions(target);
      renderThumbs();
    }

    async function step(delta: number): Promise<void> {
      const target = state.offset + current + delta;
      if (target < 0 || target >= state.total) return;
      await ensureAbsolute(target);
      current = target - state.offset;
      show();
    }

    function copyDirectLink(item: ImageItem): void {
      void copyText(item.url).then((ok) => {
        if (ok) toastWithCode("✓ 已复制直链", item.url);
      });
    }

    function renderSide(item: ImageItem): void {
      const line = (label: string, value: string, mono = false): HTMLElement =>
        h(
          "div",
          { style: { display: "flex", justifyContent: "space-between", gap: "10px", padding: "2px 0" } },
          h("span", { text: label, style: { color: "var(--muted)" } }),
          h("span", { class: mono ? "mono" : "", text: value, style: { textAlign: "right" } }),
        );
      mount(
        side,
        h("div", { style: { fontWeight: "700", marginBottom: "6px" }, text: titleOf(item) }),
        line("尺寸", formatDimensions(item.width, item.height)),
        line("大小", formatBytes(item.size)),
        line("格式", item.ext.toUpperCase()),
        line("相册", item.album_name ?? "默认相册"),
        line("上传", formatDateTime(item.upload_time)),
        line("可见性", item.visibility === "public" ? "公开" : "私有"),
        h("div", { style: { marginTop: "10px", color: "var(--muted)" }, text: "SHA-256" }),
        h("div.mono", {
          style: { fontSize: "10px", color: "var(--faint)", wordBreak: "break-all" },
          text: item.sha256,
        }),
        // 宽屏的复制/下载放在信息卡里（窄屏走底部按钮条，见 renderActions）
        h(
          "div.flexline",
          { style: { marginTop: "12px", gap: "6px" } },
          h("button.btn.sm.pri", {
            type: "button",
            style: { flex: "1" },
            text: "复制直链",
            on: { click: () => copyDirectLink(item) },
          }),
          h("a.btn.sm", {
            href: downloadUrl(item),
            download: "",
            style: { flex: "1" },
            text: "下载",
          }),
        ),
      );
    }

    /** 窄屏底部三个按钮：信息卡默认收起，靠「信息」展开。 */
    function renderActions(item: ImageItem): void {
      mount(
        actions,
        h("button.btn.sm.pri", {
          type: "button",
          text: "复制直链",
          on: { click: () => copyDirectLink(item) },
        }),
        h("a.btn.sm", { href: downloadUrl(item), download: "", text: "下载" }),
        h("button.btn.sm", {
          type: "button",
          text: "信息",
          on: {
            click: () => {
              side.hidden = !side.hidden;
            },
          },
        }),
      );
    }

    function renderThumbs(): void {
      const from = Math.max(0, current - 20);
      const to = Math.min(state.items.length, current + 21);
      const nodes: Node[] = [];
      for (let index = from; index < to; index += 1) {
        const item = state.items[index];
        if (!item) continue;
        const node = h("button.ph", {
          type: "button",
          class: `${index === current ? "cur" : ""} c${(item.id % 6) + 1}`,
          "aria-label": titleOf(item),
          on: { click: () => void step(index - current) },
        });
        node.append(
          h("img", {
            src: item.has_thumb && item.thumb_url ? item.thumb_url : item.url,
            alt: "",
            loading: "lazy",
            decoding: "async",
          }),
        );
        nodes.push(node);
      }
      thumbs.replaceChildren(...nodes);
    }

    function close(): void {
      document.removeEventListener("keydown", onKey);
      overlay.remove();
      document.body.style.overflow = "";
      state.lightboxOpen = false;
    }

    function onKey(event: KeyboardEvent): void {
      if (event.key === "Escape") {
        close();
        return;
      }
      if (event.key === "ArrowLeft") void step(-1);
      if (event.key === "ArrowRight") void step(1);
    }

    // ---- 鼠标：滚轮缩放、缩放后拖拽平移、双击 1×/2× ----
    stage.addEventListener(
      "wheel",
      (event) => {
        event.preventDefault();
        setScale(scale * Math.exp(-event.deltaY * 0.0015));
      },
      { passive: false },
    );
    img.addEventListener("dblclick", () => {
      if (scale > 1) resetZoom();
      else setScale(2);
      apply(true);
    });
    stage.addEventListener("mousedown", (event) => {
      if (scale <= 1 || event.button !== 0) return;
      event.preventDefault();
      const originX = event.clientX - tx;
      const originY = event.clientY - ty;
      const onMove = (move: MouseEvent): void => {
        tx = move.clientX - originX;
        ty = move.clientY - originY;
        apply(false);
      };
      const onUp = (): void => {
        document.removeEventListener("mousemove", onMove);
        document.removeEventListener("mouseup", onUp);
      };
      document.addEventListener("mousemove", onMove);
      document.addEventListener("mouseup", onUp);
    });

    // ---- 触屏：左右滑切换、双指缩放、下拉关闭 ----
    const touchDistance = (touches: TouchList): number => {
      const [a, b] = [touches[0]!, touches[1]!];
      return Math.hypot(a.clientX - b.clientX, a.clientY - b.clientY);
    };
    stage.addEventListener(
      "touchstart",
      (event) => {
        if (event.touches.length === 2) {
          mode = "pinch";
          pinchDistance = touchDistance(event.touches);
          return;
        }
        const touch = event.touches[0];
        if (!touch) return;
        touchStart = { x: touch.clientX, y: touch.clientY };
        dragOrigin = { x: touch.clientX - tx, y: touch.clientY - ty };
        moveX = 0;
        moveY = 0;
        mode = scale > 1 ? "pan" : "idle";
      },
      { passive: true },
    );
    stage.addEventListener(
      "touchmove",
      (event) => {
        if (mode === "pinch" && event.touches.length === 2) {
          const distance = touchDistance(event.touches);
          if (pinchDistance > 0) setScale(scale * (distance / pinchDistance));
          pinchDistance = distance;
          event.preventDefault();
          return;
        }
        const touch = event.touches[0];
        if (!touch || !touchStart) return;
        moveX = touch.clientX - touchStart.x;
        moveY = touch.clientY - touchStart.y;

        if (mode === "pan") {
          tx = touch.clientX - dragOrigin.x;
          ty = touch.clientY - dragOrigin.y;
          apply(false);
        } else if (mode === "idle") {
          if (Math.abs(moveY) > Math.abs(moveX) && moveY > 0) mode = "pull";
          else if (Math.abs(moveX) > Math.abs(moveY)) mode = "swipe";
        }
        if (mode === "pull") {
          img.style.transition = "none";
          img.style.transform = `translate(0px, ${Math.max(0, moveY)}px) scale(1)`;
          img.style.opacity = String(Math.max(0.35, 1 - moveY / 320));
        } else if (mode === "swipe") {
          img.style.transition = "none";
          img.style.transform = `translate(${moveX * 0.35}px, 0) scale(1)`;
        }
        if (mode !== "idle") event.preventDefault();
      },
      { passive: false },
    );
    stage.addEventListener("touchend", () => {
      if (mode === "pinch") {
        pinchDistance = 0;
        mode = "idle";
        return;
      }
      if (mode === "pull") {
        if (moveY > 90) {
          close();
          return;
        }
        apply(true);
      } else if (mode === "swipe") {
        apply(true);
        if (moveX < -60) void step(1);
        else if (moveX > 60) void step(-1);
      }
      mode = "idle";
      touchStart = null;
      moveX = 0;
      moveY = 0;
    });
    stage.addEventListener("touchcancel", () => {
      mode = "idle";
      touchStart = null;
      apply(true);
    });

    prevBtn.addEventListener("click", () => void step(-1));
    nextBtn.addEventListener("click", () => void step(1));
    closeBtn.addEventListener("click", () => close());
    overlay.addEventListener("click", (event) => {
      if (event.target === overlay) close();
    });
    document.addEventListener("keydown", onKey);

    // 窄屏默认收起信息卡，靠底部「信息」按钮展开，避免挤占看图空间
    side.hidden = isNarrow();
    document.body.append(overlay);
    document.body.style.overflow = "hidden";
    show();
  }

  // ---------- 启动 ----------

  await Promise.all([loadAlbums(), loadImages(1)]);

  // 相册管理页的「选中全部图片」会带 select=all 过来：加载完直接进整范围模式
  if (ctx.query.get("select") === "all") {
    enterAllMatching();
  }
}

/** 用户视图横幅：先显示 uid，拿到邮箱后再补全文案（不阻塞页面渲染）。 */
function buildViewBanner(uid: string, onExit: () => void): HTMLElement {
  const banner = userViewBanner(uid, `u_${uid}`, () => {
    navigate("/admin/users");
  }, onExit);
  const label = banner.querySelector("b");
  if (!label) return banner;
  void requestPage<AdminUserRow>("/admin/users", { query: { query: uid, per_page: 1 } })
    .then((page) => {
      const row = page.items.find((entry) => entry.uid === uid) ?? page.items[0];
      if (row?.email) label.textContent = `正在查看用户 u_${uid}（${row.email}）的空间`;
    })
    .catch(() => {
      /* 邮箱拿不到就保留 uid */
    });
  return banner;
}
