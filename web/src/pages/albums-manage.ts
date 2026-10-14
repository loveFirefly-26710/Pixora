/**
 * 相册管理（`#/albums/manage`）。
 *
 * 管的是相册本身——名字、可见性、顺序、删除——而不是相册里的图片。
 * 图片的批量操作留在「我的相册」页：那里的勾选、预览、批量条是另一套交互，
 * 塞进同一页会两边都别扭。
 *
 * 删除的语义要写清楚：删相册不删图，里面的图片会回到「未分类」。
 * 这一点在确认框里说明白，并带上具体张数。
 */

import { ApiError, request, requestPage } from "../core/api";
import { h, mount } from "../core/dom";
import { formatBytes, formatDateTime, formatNumber } from "../core/format";
import { confirmDanger, promptDialog } from "../core/modal";
import { navigate, type PageContext } from "../core/router";
import { getState } from "../core/state";
import { toastError, toastOk } from "../core/toast";
import type { AdminUserRow, Album, AlbumList } from "../core/types";
import { userViewBanner } from "../ui/shell";
import { emptyState, pageHead, qtip, skeletonGrid } from "../ui/widgets";

export async function render(ctx: PageContext): Promise<void> {
  const root = h("div.pad");
  ctx.outlet.append(root);

  const viewingUid = getState().viewingUid;
  const readOnly = Boolean(viewingUid);

  root.append(
    h(
      "div.flexline",
      { style: { justifyContent: "space-between", alignItems: "flex-start" } },
      pageHead(
        "相册管理",
        readOnly
          ? "正在查看其他用户的相册：这一页只读。"
          : "改名、改可见性、调顺序、删相册。删除相册不会删图片——里面的图会回到「未分类」。",
      ),
      readOnly
        ? null
        : h("button.btn.pri", {
            type: "button",
            text: "新建相册",
            on: { click: () => void createAlbum(load) },
          }),
    ),
  );

  if (viewingUid) {
    root.append(buildViewBanner(viewingUid));
  }

  const listHost = h("div", { style: { marginTop: "12px" } });
  root.append(listHost);
  mount(listHost, skeletonGrid(2));

  async function load(): Promise<void> {
    try {
      const data = await request<AlbumList>("/albums", {
        query: { uid: viewingUid ?? undefined },
      });
      renderList(listHost, data, readOnly, load);
    } catch (err) {
      const message = err instanceof ApiError ? err.message : String(err);
      mount(listHost, qtip("err", "相册列表没加载出来", message));
    }
  }

  await load();
}

/** 用户视图横幅：先显示 uid，拿到邮箱后再补全文案（不阻塞页面渲染）。 */
function buildViewBanner(uid: string): HTMLElement {
  const banner = userViewBanner(uid, `u_${uid}`, () => navigate("/admin/users"), () =>
    navigate("/albums"),
  );
  const label = banner.querySelector("b");
  if (!label) return banner;
  void requestPage<AdminUserRow>("/admin/users", { query: { query: uid, per_page: 1 } })
    .then((page) => {
      const row = page.items.find((entry) => entry.uid === uid) ?? page.items[0];
      if (row?.email) label.textContent = `正在查看用户 u_${uid}（${row.email}）的空间`;
    })
    .catch(() => {});
  return banner;
}

type Reload = () => Promise<void>;

function renderList(host: HTMLElement, data: AlbumList, readOnly: boolean, reload: Reload): void {
  if (data.albums.length === 0) {
    mount(host, emptyState("▤", "还没有相册。"));
    return;
  }

  const sorted = [...data.albums].sort((a, b) => a.sort_order - b.sort_order || a.id - b.id);
  host.replaceChildren(
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
            h("th", { text: "相册" }),
            h("th", { style: { width: "92px" }, class: "hide-md", text: "图片" }),
            h("th", { style: { width: "92px" }, class: "hide-md", text: "占用" }),
            h("th", { style: { width: "110px" }, text: "可见性" }),
            h("th", { style: { width: "140px" }, class: "hide-md", text: "最后更新" }),
            h("th", { style: { width: readOnly ? "80px" : "300px" }, text: "操作" }),
          ),
        ),
        h(
          "tbody",
          null,
          ...sorted.map((album, index) =>
            albumRow(album, index, sorted, readOnly, reload),
          ),
        ),
      ),
    ),
  );
}

function albumRow(
  album: Album,
  index: number,
  sorted: Album[],
  readOnly: boolean,
  reload: Reload,
): HTMLElement {
  const isDefault = album.slug === "default";
  const actions = h("div.flexline", { style: { gap: "6px", flexWrap: "wrap" } });

  actions.append(
    h("button.btn.sm", {
      type: "button",
      text: "打开",
      on: { click: () => navigate(`/albums/${album.id}`) },
    }),
  );
  // 「选中全部图片」：跳到该相册、进去就是全选状态，接着就能批量删除/打标签/移动。
  // 默认相册同样适用——它只是不能删相册本身。
  if (album.image_count > 0) {
    actions.append(
      h("button.btn.sm", {
        type: "button",
        text: "选中全部图片",
        title: `进入相册并勾选全部 ${album.image_count} 张`,
        on: { click: () => navigate(`/albums/${album.id}?select=all`) },
      }),
    );
  }

  if (!readOnly) {
    const rename = h("button.btn.sm", { type: "button", text: "改名" });
    rename.addEventListener("click", () => void renameAlbum(album, reload));
    const visibility = h("button.btn.sm", {
      type: "button",
      text: album.visibility === "public" ? "改为私有" : "改为公开",
    });
    visibility.addEventListener("click", () => void toggleVisibility(album, reload));
    actions.append(rename, visibility);

    // 排序：与相邻一项交换。默认相册也允许挪：给它单开一套规则
    // （比如钉死在第一位）只会让人问「为什么这一行没有箭头」
    if (index > 0) {
      const up = h("button.btn.sm.gho", { type: "button", text: "↑", title: "上移" });
      up.addEventListener("click", () => void moveAlbum(sorted, index, index - 1, reload));
      actions.append(up);
    }
    if (index < sorted.length - 1) {
      const down = h("button.btn.sm.gho", { type: "button", text: "↓", title: "下移" });
      down.addEventListener("click", () => void moveAlbum(sorted, index, index + 1, reload));
      actions.append(down);
    }

    if (isDefault) {
      // 默认相册是「上传不指定相册」的落脚点：删掉之后上传没有地方落，
      // 而且界面上没有重建它的入口，所以不给删
      actions.append(
        h("span", { style: { fontSize: "11.5px", color: "var(--faint)" }, text: "默认相册不可删除" }),
      );
    } else {
      const remove = h("button.btn.sm.dgr", { type: "button", text: "删除" });
      remove.addEventListener("click", () => void deleteAlbum(album, reload));
      actions.append(remove);
    }
  }

  return h(
    "tr",
    null,
    h(
      "td",
      { "data-label": "相册" },
      h("b", { text: album.name }),
      isDefault ? h("span.tag", { title: "上传不指定相册时的落点", text: "默认" }) : null,
      album.description ? h("div.tip", { text: album.description }) : null,
      h("div.tip", { text: `/${album.slug}` }),
    ),
    h("td", { "data-label": "图片", class: "hide-md", text: formatNumber(album.image_count) }),
    h("td", { "data-label": "占用", class: "hide-md", text: formatBytes(album.bytes) }),
    h(
      "td",
      { "data-label": "可见性" },
      h("span.badge", {
        class: album.visibility === "public" ? "ok" : "mute",
        text: album.visibility === "public" ? "公开" : "私有",
      }),
    ),
    h("td", { "data-label": "最后更新", class: "hide-md", text: formatDateTime(album.updated_at) }),
    h("td", { "data-label": "操作" }, actions),
  );
}

async function createAlbum(reload: Reload): Promise<void> {
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
    await reload();
  } catch (err) {
    toastError(err);
  }
}

async function renameAlbum(album: Album, reload: Reload): Promise<void> {
  const name = await promptDialog({
    title: "重命名相册",
    label: "新的相册名称",
    value: album.name,
    confirmText: "保存",
  });
  if (!name || name === album.name) return;
  try {
    await request(`/albums/${album.id}`, { method: "PATCH", json: { name } });
    toastOk(`已改名为「${name}」`);
    await reload();
  } catch (err) {
    toastError(err);
  }
}

async function toggleVisibility(album: Album, reload: Reload): Promise<void> {
  const next = album.visibility === "public" ? "private" : "public";
  try {
    await request(`/albums/${album.id}`, { method: "PATCH", json: { visibility: next } });
    toastOk(next === "public" ? `「${album.name}」已改为公开` : `「${album.name}」已改为私有`);
    await reload();
  } catch (err) {
    toastError(err);
  }
}

/**
 * 与相邻相册交换位置。
 *
 * 不能只互换两条的 sort_order：新建的相册 sort_order 都是 0，
 * 换完还是一样，界面上看起来「点了没反应」（排序会退回按 id 比）。
 * 所以交换之后把整串重排成 10、20、30……，只提交真的变了的那些。
 */
async function moveAlbum(
  sorted: Album[],
  from: number,
  to: number,
  reload: Reload,
): Promise<void> {
  if (from === to) return;
  const order = [...sorted];
  const [moved] = order.splice(from, 1);
  if (!moved) return;
  order.splice(to, 0, moved);
  try {
    for (const [index, album] of order.entries()) {
      const next = (index + 1) * 10;
      if (album.sort_order !== next) {
        await request(`/albums/${album.id}`, { method: "PATCH", json: { sort_order: next } });
      }
    }
    await reload();
  } catch (err) {
    toastError(err);
  }
}

async function deleteAlbum(album: Album, reload: Reload): Promise<void> {
  const detail =
    album.image_count > 0
      ? `相册里的 ${album.image_count} 张图片会回到「未分类」，图片本身不会被删除。`
      : "相册是空的，删除没有任何影响。";
  const ok = await confirmDanger(
    "删除相册",
    `确定删除「${album.name}」？${detail}`,
    "删除相册",
  );
  if (!ok) return;
  try {
    const result = await request<{ detached_images: number }>(`/albums/${album.id}`, {
      method: "DELETE",
    });
    const detached = result.detached_images ?? 0;
    toastOk(
      detached > 0
        ? `已删除「${album.name}」，${detached} 张图片移到了未分类`
        : `已删除「${album.name}」`,
    );
    await reload();
  } catch (err) {
    toastError(err);
  }
}
