/**
 * 回收站（`#/recycle`）。
 *
 * 删掉的图片与相册都先到这里：图片仍然占着配额（它占着盘），物理文件按后台
 * 设置的保留天数留着，所以能恢复；相册连它里面的图片一起进来，恢复时也只
 * 带回那一批——用户自己在相册里单独删掉的图，不会被这条路径顺手捞回来。
 *
 * 到期之后由后台清理任务彻底删掉：这一页只是入口，不负责「定时」。
 *
 * 交互上有三条是照着「用完就想走」的场景定的：
 *   1. 恢复/彻底删除之后**就地重画**（不是让用户自己按 F5）；
 *   2. 网格与列表两种视图都能用，勾选框常驻，Shift 按范围连选；
 *   3. 「加载更多」是显式按钮：回收站里的东西通常一眼扫完，自动无限滚动
 *      反而会让人以为列表一直在变。
 */

import { ApiError, request, requestPage } from "../core/api";
import { h, mount } from "../core/dom";
import { formatBytes, formatDimensions, formatNumber, relativeTime } from "../core/format";
import { confirmDanger } from "../core/modal";
import type { PageContext } from "../core/router";
import { navigate } from "../core/router";
import { getState } from "../core/state";
import { loadViewPreference, saveViewPreference } from "../core/store";
import { toastError, toastOk } from "../core/toast";
import type { Album, ImageItem } from "../core/types";
import { openLightbox } from "../ui/lightbox";
import { applyMasonry } from "../ui/masonry";
import {
  cardHead,
  cardNode,
  emptyState,
  imageTile,
  pageHead,
  qtip,
  skeletonGrid,
} from "../ui/widgets";

const PER_PAGE = 24;

export async function render(ctx: PageContext): Promise<void> {
  const root = h("div.pad");
  ctx.outlet.append(root);

  const retention = getState().site?.recycle_retention_days ?? 0;

  // 重画整页内容，而不是再套一层 .pad。
  // 之前这里直接 `void render(ctx)`，每刷新一次就往 outlet 里再追加一棵
  // 同内容的 DOM 树——看起来像「没刷新」，其实是旧的还在上面
  const reload = (): void => {
    root.replaceChildren();
    void paint(root, retention, reload);
  };

  await paint(root, retention, reload);
}

async function paint(root: HTMLElement, retention: number, reload: () => void): Promise<void> {
  root.append(
    pageHead("回收站", "删除的图片与相册先留在这里，可以恢复或彻底删除。"),
    retention > 0
      ? qtip(
          "info",
          `保留 ${retention} 天`,
          `回收站里的内容仍然算你的已用额度（文件还占着盘），彻底删除或清空之后才会释放。超过 ${retention} 天由后台自动彻底删除。`,
        )
      : qtip(
          "warn",
          "回收站已关闭",
          "管理员把保留天数设成了 0，删除即彻底删除，无法恢复。",
        ),
  );
  if (retention === 0) return;

  const albumsHost = h("div");
  const imagesHost = h("div");
  root.append(h("div", { style: { marginTop: "14px" } }), albumsHost, imagesHost);

  await Promise.all([loadAlbums(albumsHost, reload), loadImages(imagesHost, reload)]);
}

/* ------------------------------------------------------------------ */
/* 相册                                                                */
/* ------------------------------------------------------------------ */

async function loadAlbums(host: HTMLElement, reload: () => void): Promise<void> {
  mount(host, skeletonGrid(2));
  try {
    const data = await request<{ albums: Album[] }>("/recycle/albums");
    if (data.albums.length === 0) {
      mount(host, h("div", { style: { marginBottom: "14px" } }));
      return;
    }
    mount(
      host,
      cardNode(
        cardHead(`回收站里的相册（${data.albums.length}）`),
        ...data.albums.map((album) => albumRow(album, reload)),
        h("div.tip", {
          text: "恢复会把相册连同「随它一起删掉」的图片一起带回来；彻底删除则一并清掉。",
        }),
      ),
    );
  } catch (err) {
    mount(host, qtip("err", "回收站里的相册没加载出来", messageOf(err)));
  }
}

function albumRow(album: Album, reload: () => void): HTMLElement {
  const restore = h("button.btn.sm", { type: "button", text: "恢复" });
  restore.addEventListener("click", async () => {
    restore.disabled = true;
    try {
      const result = await request<{ restored_images: number }>(
        `/recycle/albums/${album.id}/restore`,
        { method: "POST" },
      );
      toastOk(
        result.restored_images > 0
          ? `已恢复「${album.name}」，带回来 ${result.restored_images} 张图片`
          : `已恢复「${album.name}」`,
      );
      navigate("/albums");
    } catch (err) {
      toastError(err);
      restore.disabled = false;
    }
  });

  const purge = h("button.btn.sm.dgr", { type: "button", text: "彻底删除" });
  purge.addEventListener("click", async () => {
    const ok = await confirmDanger(
      `彻底删除「${album.name}」？`,
      `相册与其中的 ${album.image_count} 张图片会被永久删除，物理文件随后由后台回收，不可恢复。`,
      "彻底删除",
    );
    if (!ok) return;
    purge.disabled = true;
    try {
      await request(`/recycle/albums/${album.id}`, { method: "DELETE" });
      toastOk(`已彻底删除「${album.name}」`);
      reload();
    } catch (err) {
      toastError(err);
      purge.disabled = false;
    }
  });

  return h(
    "div.flexline",
    { style: { padding: "10px 0", borderBottom: "1px solid var(--border)" } },
    h(
      "div",
      { style: { flex: "1", minWidth: "160px" } },
      h("b", { text: album.name }),
      h("div.tip", {
        text: `${formatNumber(album.image_count)} 张 · ${formatBytes(album.bytes)} · ${
          album.updated_at ? relativeTime(album.updated_at) : ""
        }`,
      }),
    ),
    restore,
    purge,
  );
}

/* ------------------------------------------------------------------ */
/* 图片                                                                */
/* ------------------------------------------------------------------ */

async function loadImages(host: HTMLElement, reload: () => void): Promise<void> {
  const state = {
    view: loadViewPreference() as "grid" | "list",
    /** 已经取到的行（分页累加） */
    items: [] as ImageItem[],
    selected: new Set<number>(),
    /** Shift 连选的锚点：记住最后一次点过的下标 */
    anchor: null as number | null,
    page: 1,
    total: 0,
    /** 每页多少：服务端可能改，用它算「下一页」的边界 */
    perPage: PER_PAGE,
  };

  mount(host, skeletonGrid(4));
  try {
    const first = await requestPage<ImageItem>("/recycle/images", {
      query: { page: 1, per_page: PER_PAGE },
    });
    state.items = first.items;
    state.total = first.meta.total;
    state.perPage = first.meta.per_page || PER_PAGE;
  } catch (err) {
    mount(
      host,
      qtip("err", "回收站里的图片没加载出来", messageOf(err)),
      h(
        "div",
        { style: { marginTop: "10px" } },
        h("button.btn", {
          type: "button",
          text: "重试",
          on: { click: () => void loadImages(host, reload) },
        }),
      ),
    );
    return;
  }

  /** 取下一页并追加，然后就地重画。 */
  const loadMore = async (button: HTMLButtonElement): Promise<void> => {
    button.disabled = true;
    button.textContent = "加载中…";
    try {
      const next = await requestPage<ImageItem>("/recycle/images", {
        query: { page: state.page + 1, per_page: state.perPage },
      });
      state.page += 1;
      state.items = [...state.items, ...next.items];
      state.total = next.meta.total;
      render();
    } catch (err) {
      toastError(err);
      button.disabled = false;
      button.textContent = "加载更多";
    }
  };

  /** 恢复/彻底删除/清空之后的统一收尾：就地重画，不让用户自己刷新。 */
  const afterAction = async (): Promise<void> => {
    try {
      const fresh = await requestPage<ImageItem>("/recycle/images", {
        query: { page: 1, per_page: Math.max(state.perPage, state.items.length) },
      });
      state.items = fresh.items;
      state.total = fresh.meta.total;
      state.page = 1;
      state.perPage = fresh.meta.per_page || state.perPage;
      state.selected.clear();
      state.anchor = null;
    } catch (err) {
      toastError(err);
    }
    render();
  };

  const selectRange = (from: number, to: number): void => {
    const [start, end] = from <= to ? [from, to] : [to, from];
    for (let index = start; index <= end; index += 1) {
      const item = state.items[index];
      if (item) state.selected.add(item.id);
    }
  };

  /** 点击一行/一张时的选择逻辑：Shift 连选，Ctrl 离散，普通点击就是单选。 */
  const toggle = (item: ImageItem, event?: MouseEvent): void => {
    const index = state.items.findIndex((row) => row.id === item.id);
    const additive = Boolean(event && (event.ctrlKey || event.metaKey));

    if (event?.shiftKey && state.anchor !== null && index >= 0) {
      if (!additive) state.selected.clear();
      selectRange(state.anchor, index);
      render();
      return;
    }

    if (additive) {
      if (state.selected.has(item.id)) state.selected.delete(item.id);
      else state.selected.add(item.id);
    } else {
      state.selected.clear();
      state.selected.add(item.id);
    }
    state.anchor = index >= 0 ? index : null;
    render();
  };

  const restoreItems = async (ids: number[], button?: HTMLButtonElement): Promise<void> => {
    if (ids.length === 0) return;
    if (button) button.disabled = true;
    try {
      const payload = await request<{ restored: number }>("/recycle/images/restore", {
        method: "POST",
        json: { ids },
      });
      toastOk(`已恢复 ${payload.restored} 张`);
      await afterAction();
    } catch (err) {
      toastError(err);
      if (button) button.disabled = false;
    }
  };

  const purgeItems = async (ids: number[], button?: HTMLButtonElement): Promise<void> => {
    if (ids.length === 0) return;
    const ok = await confirmDanger(
      ids.length === 1 ? "彻底删除这一张？" : `彻底删除选中的 ${ids.length} 张？`,
      "会被永久删除，物理文件随后由后台回收，不可恢复；这一部分空间会从已用额度里释放。",
      ids.length === 1 ? "彻底删除" : `彻底删除 ${ids.length} 张`,
    );
    if (!ok) return;
    if (button) button.disabled = true;
    try {
      const payload = await request<{ purged: number }>("/recycle/images/purge", {
        method: "POST",
        json: { ids },
      });
      toastOk(`已彻底删除 ${payload.purged} 张`);
      await afterAction();
    } catch (err) {
      toastError(err);
      if (button) button.disabled = false;
    }
  };

  const emptyBin = async (button: HTMLButtonElement): Promise<void> => {
    const ok = await confirmDanger(
      "清空回收站？",
      `回收站里的全部图片会被永久删除（共 ${formatNumber(state.total)} 张），物理文件随后由后台回收，不可恢复。`,
      "清空回收站",
    );
    if (!ok) return;
    button.disabled = true;
    try {
      const payload = await request<{ purged: number }>("/recycle/images/empty", {
        method: "POST",
      });
      toastOk(`已清空回收站（${payload.purged} 张）`);
      await afterAction();
    } catch (err) {
      toastError(err);
      button.disabled = false;
    }
  };

  const setView = (view: "grid" | "list"): void => {
    state.view = view;
    saveViewPreference(view);
    render();
  };

  const render = (): void => {
    if (state.total === 0) {
      mount(host, emptyState("🗑", "回收站是空的。删除的图片会先到这里，保留期内都能恢复。"));
      return;
    }

    const countText = h("span", {
      text: state.selected.size > 0 ? `已选 ${state.selected.size} 张` : `共 ${state.total} 张`,
    });
    const restoreBtn = h("button.btn.sm", { type: "button", text: "恢复选中" });
    const purgeBtn = h("button.btn.sm.dgr", { type: "button", text: "彻底删除" });
    const emptyBtn = h("button.btn.sm.dgr.gho", { type: "button", text: "清空回收站" });
    const allBox = h("input", { type: "checkbox", "aria-label": "全选已加载的图片" });
    allBox.checked = state.items.length > 0 && state.items.every((item) => state.selected.has(item.id));
    allBox.indeterminate = !allBox.checked && state.selected.size > 0;
    allBox.addEventListener("change", () => {
      state.selected.clear();
      if (allBox.checked) for (const item of state.items) state.selected.add(item.id);
      state.anchor = null;
      render();
    });

    const gridChip = h("button.chip", {
      type: "button",
      class: state.view === "grid" ? "on" : "",
      text: "网格",
      on: { click: () => setView("grid") },
    });
    const listChip = h("button.chip", {
      type: "button",
      class: state.view === "list" ? "on" : "",
      text: "列表",
      on: { click: () => setView("list") },
    });

    const hasSelection = state.selected.size > 0;
    restoreBtn.disabled = !hasSelection;
    purgeBtn.disabled = !hasSelection;

    restoreBtn.addEventListener("click", () => void restoreItems([...state.selected], restoreBtn));
    purgeBtn.addEventListener("click", () => void purgeItems([...state.selected], purgeBtn));
    emptyBtn.addEventListener("click", () => void emptyBin(emptyBtn));

    const toolbar = h(
      "div.flexline",
      { style: { padding: "10px 0", flexWrap: "wrap", gap: "8px" } },
      h("label.flexline", { style: { gap: "6px", margin: "0", fontSize: "12.5px" } }, allBox, countText),
      h("span.spacer"),
      gridChip,
      listChip,
      restoreBtn,
      purgeBtn,
      emptyBtn,
    );

    const body =
      state.view === "grid"
        ? gridView(state, toggle, restoreItems, purgeItems)
        : listView(state, toggle, restoreItems, purgeItems);

    const more =
      state.items.length < state.total
        ? h(
            "div",
            { style: { display: "flex", justifyContent: "center", padding: "14px 0" } },
            h("button.btn.sm", {
              type: "button",
              text: `加载更多（还有 ${state.total - state.items.length} 张）`,
              on: {
                click: (event) => void loadMore(event.currentTarget as HTMLButtonElement),
              },
            }),
          )
        : h("div.tip", { text: "没有更多了" });

    mount(
      host,
      cardNode(
        cardHead("回收站里的图片"),
        h("div.tip", {
          style: { margin: "0 0 6px" },
          text: "Shift + 点击按范围连选，Ctrl / ⌘ + 点击离散多选。",
        }),
        toolbar,
        body,
        more,
      ),
    );

    if (state.view === "grid") {
      const grid = host.querySelector<HTMLElement>(".igrid");
      if (grid) applyMasonry(grid);
    }
  };

  render();
}

/** 网格视图：与广场同一套方格，勾选框常驻。 */
function gridView(
  state: {
    items: ImageItem[];
    selected: Set<number>;
  },
  toggle: (item: ImageItem, event?: MouseEvent) => void,
  restoreItems: (ids: number[], button?: HTMLButtonElement) => Promise<void>,
  purgeItems: (ids: number[], button?: HTMLButtonElement) => Promise<void>,
): HTMLElement {
  const grid = h("div.grid.igrid");
  for (const item of state.items) {
    grid.append(
      imageTile(item, {
        checkbox: true,
        selected: state.selected.has(item.id),
        title: item.title ?? item.original_name,
        showViews: false,
        // 回收站里的图仍然能看大图：确认「是不是这张」才能放心恢复
        onOpen: (clicked) => openLightbox(state.items, clicked),
        onSelect: (clicked, event) => toggle(clicked, event),
      }),
    );
  }
  // 每张下面补一行小按钮：网格视图里也要能单张恢复/删除
  const actions = h("div.row", { style: { marginTop: "12px", flexWrap: "wrap" } });
  const selectedIds = state.items.filter((item) => state.selected.has(item.id)).map((item) => item.id);
  if (selectedIds.length > 0) {
    actions.append(
      h("button.btn.sm", {
        type: "button",
        text: `恢复选中的 ${selectedIds.length} 张`,
        on: { click: (event) => void restoreItems(selectedIds, event.currentTarget as HTMLButtonElement) },
      }),
      h("button.btn.sm.dgr", {
        type: "button",
        text: `彻底删除选中的 ${selectedIds.length} 张`,
        on: { click: (event) => void purgeItems(selectedIds, event.currentTarget as HTMLButtonElement) },
      }),
    );
  }
  return h("div", null, grid, actions);
}

/** 列表视图：文件管理视角，带文件名与尺寸。 */
function listView(
  state: {
    items: ImageItem[];
    selected: Set<number>;
  },
  toggle: (item: ImageItem, event?: MouseEvent) => void,
  restoreItems: (ids: number[], button?: HTMLButtonElement) => Promise<void>,
  purgeItems: (ids: number[], button?: HTMLButtonElement) => Promise<void>,
): HTMLElement {
  const rows = state.items.map((item) => {
    const box = h("input", {
      type: "checkbox",
      "aria-label": `选择 ${item.title ?? item.original_name ?? item.short_hash}`,
    });
    box.checked = state.selected.has(item.id);
    // Shift 连选：这里和网格视图一样，锚点由调用方维护
    box.addEventListener("click", (event) => {
      event.stopPropagation();
      toggle(item, event);
    });

    const thumb = h("div.ph.mini", { class: `c${(item.id % 6) + 1}` });
    if (item.thumb_url || item.url) {
      thumb.append(
        h("img", { src: item.thumb_url ?? item.url, alt: "", loading: "lazy", decoding: "async" }),
      );
    }

    const row = h(
      "tr",
      null,
      h("td", { "data-label": "选择" }, box),
      h("td", { "data-label": "预览" }, thumb),
      h(
        "td",
        { "data-label": "文件名" },
        h("div", { text: item.title ?? item.original_name ?? item.short_hash }),
        item.album_name ? h("div.tip", { text: `原属：${item.album_name}` }) : null,
      ),
      h("td", { "data-label": "大小", class: "hide-md", text: formatBytes(item.size) }),
      h("td", { "data-label": "尺寸", class: "hide-md", text: formatDimensions(item.width, item.height) }),
      // 这一列原本写「删除时间」，但接口给的是上传时间——口径不能编
      h("td", { "data-label": "上传时间", text: relativeTime(item.upload_time) }),
      h(
        "td",
        { "data-label": "操作" },
        h(
          "div.act",
          null,
          h("button.btn.sm", {
            type: "button",
            text: "恢复",
            on: { click: (event) => void restoreItems([item.id], event.currentTarget as HTMLButtonElement) },
          }),
          h("button.btn.sm.dgr", {
            type: "button",
            text: "彻底删除",
            on: { click: (event) => void purgeItems([item.id], event.currentTarget as HTMLButtonElement) },
          }),
        ),
      ),
    );
    row.addEventListener("click", (event) => {
      // 点行本身也切换选择：整行的目标比那个 12px 的勾选框好点得多
      if ((event.target as HTMLElement).closest("button, input")) return;
      toggle(item, event);
    });
    return row;
  });

  const header = h(
    "tr",
    null,
    h("th", { style: { width: "36px" } }),
    h("th", { style: { width: "52px" }, text: "预览" }),
    h("th", { text: "文件名" }),
    h("th", { style: { width: "88px" }, class: "hide-md", text: "大小" }),
    h("th", { style: { width: "118px" }, class: "hide-md", text: "尺寸" }),
    h("th", { style: { width: "130px" }, text: "上传时间" }),
    h("th", { style: { width: "190px" }, text: "操作" }),
  );

  return h(
    "div.table-wrap",
    null,
    h("table.responsive", null, h("thead", null, header), h("tbody", null, ...rows)),
  );
}

function messageOf(err: unknown): string {
  return err instanceof ApiError ? err.message : String(err);
}
