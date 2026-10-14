/**
 * 回收站（`#/recycle`）。
 *
 * 删掉的图片与相册都先到这里：图片不占配额（引用已经归零），但物理文件按
 * 后台设置的保留天数留着，所以能恢复；相册连它里面的图片一起进来，恢复时也只
 * 带回那一批——用户自己在相册里单独删掉的图，不会被这条路径顺手捞回来。
 *
 * 到期之后由后台清理任务彻底删掉：这一页只是入口，不负责「定时」。
 */

import { ApiError, request, requestPage } from "../core/api";
import { h, mount } from "../core/dom";
import { formatBytes, formatDimensions, formatNumber, relativeTime } from "../core/format";
import { confirmDanger } from "../core/modal";
import type { PageContext } from "../core/router";
import { navigate } from "../core/router";
import { getState } from "../core/state";
import { toastError, toastOk } from "../core/toast";
import type { Album, ImageItem } from "../core/types";
import { cardHead, cardNode, emptyState, pageHead, qtip, skeletonGrid } from "../ui/widgets";

const PER_PAGE = 24;

export async function render(ctx: PageContext): Promise<void> {
  const root = h("div.pad");
  ctx.outlet.append(root);

  const retention = getState().site?.recycle_retention_days ?? 0;
  root.append(
    pageHead("回收站", "删除的图片与相册先留在这里，可以恢复或彻底删除。"),
    retention > 0
      ? qtip(
          "info",
          `保留 ${retention} 天`,
          `回收站里的内容不占你的配额，但占服务器空间；超过 ${retention} 天由后台自动彻底删除。想立刻腾出空间就手动彻底删除或清空。`,
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

  const reload = (): void => void render(ctx);

  await Promise.all([loadAlbums(albumsHost, reload), loadImages(imagesHost, reload, 1, [])]);
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
    { style: { padding: "10px 0", borderBottom: "1px solid #f3f4f6" } },
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

async function loadImages(
  host: HTMLElement,
  reload: () => void,
  page: number,
  collected: ImageItem[],
): Promise<void> {
  if (page === 1) mount(host, skeletonGrid(4));
  try {
    const result = await requestPage<ImageItem>("/recycle/images", {
      query: { page, per_page: PER_PAGE },
    });
    const items = [...collected, ...result.items];
    const selected = new Set<number>();

    const render = (): void => {
      if (result.meta.total === 0) {
        mount(
          host,
          emptyState("🗑", "回收站是空的。删除的图片会先到这里，保留期内都能恢复。"),
        );
        return;
      }

      const rows = items.map((item) => rowFor(item, selected, render, reload));
      const header = h(
        "tr",
        null,
        h("th", { style: { width: "36px" } }, selectAllBox(items, selected, render)),
        h("th", { style: { width: "52px" }, text: "预览" }),
        h("th", { text: "文件名" }),
        h("th", { style: { width: "88px" }, class: "hide-md", text: "大小" }),
        h("th", { style: { width: "118px" }, class: "hide-md", text: "尺寸" }),
        h("th", { style: { width: "130px" }, text: "删除时间" }),
        h("th", { style: { width: "190px" }, text: "操作" }),
      );

      const countText = h("span", { text: `共 ${result.meta.total} 张` });
      const restoreBtn = h("button.btn.sm", { type: "button", text: "恢复选中" });
      const purgeBtn = h("button.btn.sm.dgr", { type: "button", text: "彻底删除" });
      const emptyBtn = h("button.btn.sm.dgr.gho", { type: "button", text: "清空回收站" });
      const syncBar = (): void => {
        const count = selected.size;
        countText.textContent = count > 0 ? `已选 ${count} 张` : `共 ${result.meta.total} 张`;
        restoreBtn.disabled = count === 0;
        purgeBtn.disabled = count === 0;
      };
      syncBar();

      restoreBtn.addEventListener("click", async () => {
        const ids = [...selected];
        if (ids.length === 0) return;
        restoreBtn.disabled = true;
        try {
          const payload = await request<{ restored: number }>("/recycle/images/restore", {
            method: "POST",
            json: { ids },
          });
          toastOk(`已恢复 ${payload.restored} 张`);
          reload();
        } catch (err) {
          toastError(err);
          restoreBtn.disabled = false;
        }
      });
      purgeBtn.addEventListener("click", async () => {
        const ids = [...selected];
        if (ids.length === 0) return;
        const ok = await confirmDanger(
          `彻底删除选中的 ${ids.length} 张？`,
          "这些图片会被永久删除，物理文件随后由后台回收，不可恢复。",
          `彻底删除 ${ids.length} 张`,
        );
        if (!ok) return;
        purgeBtn.disabled = true;
        try {
          const payload = await request<{ purged: number }>("/recycle/images/purge", {
            method: "POST",
            json: { ids },
          });
          toastOk(`已彻底删除 ${payload.purged} 张`);
          reload();
        } catch (err) {
          toastError(err);
          purgeBtn.disabled = false;
        }
      });
      emptyBtn.addEventListener("click", async () => {
        const ok = await confirmDanger(
          "清空回收站？",
          `回收站里的全部图片会被永久删除（共 ${result.meta.total} 张），物理文件随后由后台回收，不可恢复。`,
          "清空回收站",
        );
        if (!ok) return;
        emptyBtn.disabled = true;
        try {
          const payload = await request<{ purged: number }>("/recycle/images/empty", {
            method: "POST",
          });
          toastOk(`已清空回收站（${payload.purged} 张）`);
          reload();
        } catch (err) {
          toastError(err);
          emptyBtn.disabled = false;
        }
      });

      const toolbar = h(
        "div.flexline",
        { style: { padding: "10px 0", flexWrap: "wrap" } },
        countText,
        h("span.spacer"),
        restoreBtn,
        purgeBtn,
        emptyBtn,
      );

      const table = h(
        "div.table-wrap",
        null,
        h("table.responsive", null, h("thead", null, header), h("tbody", null, ...rows)),
      );

      const more =
        items.length < result.meta.total
          ? h(
              "div",
              { style: { display: "flex", justifyContent: "center", padding: "14px 0" } },
              h("button.btn.sm", {
                type: "button",
                text: `加载更多（还有 ${result.meta.total - items.length} 张）`,
                on: { click: () => void loadImages(host, reload, page + 1, items) },
              }),
            )
          : h("div.tip", { text: "没有更多了" });

      mount(host, cardNode(cardHead("回收站里的图片"), toolbar, table, more));
    };

    render();
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
          on: { click: () => void loadImages(host, reload, 1, []) },
        }),
      ),
    );
  }
}

function rowFor(
  item: ImageItem,
  selected: Set<number>,
  render: () => void,
  reload: () => void,
): HTMLElement {
  const box = h("input", { type: "checkbox", "aria-label": `选择 ${item.title ?? item.short_hash}` });
  box.checked = selected.has(item.id);
  box.addEventListener("change", () => {
    if (box.checked) selected.add(item.id);
    else selected.delete(item.id);
    render();
  });

  const thumb = h("div.ph.mini", { class: `c${(item.id % 6) + 1}` });
  if (item.thumb_url || item.url) {
    thumb.append(
      h("img", { src: item.thumb_url ?? item.url, alt: "", loading: "lazy", decoding: "async" }),
    );
  }

  const restore = h("button.btn.sm", { type: "button", text: "恢复" });
  restore.addEventListener("click", async () => {
    restore.disabled = true;
    try {
      await request("/recycle/images/restore", { method: "POST", json: { ids: [item.id] } });
      toastOk("已恢复 1 张");
      reload();
    } catch (err) {
      toastError(err);
      restore.disabled = false;
    }
  });

  const purge = h("button.btn.sm.dgr", { type: "button", text: "彻底删除" });
  purge.addEventListener("click", async () => {
    const ok = await confirmDanger(
      "彻底删除这一张？",
      "会被永久删除，物理文件随后由后台回收，不可恢复。",
      "彻底删除",
    );
    if (!ok) return;
    purge.disabled = true;
    try {
      await request("/recycle/images/purge", { method: "POST", json: { ids: [item.id] } });
      toastOk("已彻底删除 1 张");
      reload();
    } catch (err) {
      toastError(err);
      purge.disabled = false;
    }
  });

  return h(
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
    h("td", { "data-label": "删除时间", text: relativeTime(item.upload_time) }),
    h("td", { "data-label": "操作" }, h("div.act", null, restore, purge)),
  );
}

function selectAllBox(
  items: ImageItem[],
  selected: Set<number>,
  render: () => void,
): HTMLInputElement {
  const box = h("input", { type: "checkbox", "aria-label": "全选本页" });
  box.checked = items.length > 0 && items.every((item) => selected.has(item.id));
  box.addEventListener("change", () => {
    selected.clear();
    if (box.checked) for (const item of items) selected.add(item.id);
    render();
  });
  return box;
}

function messageOf(err: unknown): string {
  return err instanceof ApiError ? err.message : String(err);
}
