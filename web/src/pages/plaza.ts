/**
 * 广场。
 *
 * 未登录：只有广场，右上角登录/注册入口；点图片能看预览浮层，但浮层里的
 * 下载与复制直链换成「登录后可用」——服务端对未登录用户也会把 `original_name` 置空，
 * 两边的口径是一致的。
 *
 * 已登录：上传区在广场上方，同屏可见，传完立刻能在下面看到自己的图（如果设为公开）。
 */

import { request, requestPage } from "../core/api";
import { debounce, h } from "../core/dom";
import { relativeTime } from "../core/format";
import { navigate, type PageContext } from "../core/router";
import { currentUser, getState } from "../core/state";
import { toastError } from "../core/toast";
import type { ImageItem, TagRow } from "../core/types";
import { createUploadPanel } from "../upload/panel";
import { openLightbox } from "../ui/lightbox";
import {
  emptyState,
  imageTile,
  pageHead,
  pagination,
  qtip,
  skeletonGrid,
} from "../ui/widgets";

interface PlazaQuery {
  q: string | null;
  tag: string | null;
  sort: string;
  page: number;
}

function readQuery(ctx: PageContext): PlazaQuery {
  return {
    q: ctx.query.get("q"),
    tag: ctx.query.get("tag"),
    sort: ctx.query.get("sort") === "hot" ? "hot" : "new",
    page: Number(ctx.query.get("page") ?? "1") || 1,
  };
}

export async function render(ctx: PageContext): Promise<void> {
  const query = readQuery(ctx);
  const user = currentUser();
  const site = getState().site;

  ctx.outlet.append(
    pageHead(
      "广场",
      user
        ? "上传区在上方，广场内容在下方，同屏可见。"
        : "浏览公开图片。登录后可以上传和下载原图。",
    ),
  );

  if (!user && site && !site.plaza_public) {
    ctx.outlet.append(
      qtip(
        "warn",
        "广场对未登录用户关闭",
        "管理员关闭了公开浏览。登录后可以查看你有权限的内容，或联系管理员开通。",
      ),
    );
    return;
  }

  const gridHost = h("div");
  const pagerHost = h("div");
  let loading = false;
  let refresh: () => Promise<void> = async () => {};

  // 已登录时把上传面板放在最上面：传完刷新列表就能看到结果
  if (user) {
    const panel = createUploadPanel({
      compact: true,
      onUploaded: () => {
        if (query.sort === "new") void refresh();
      },
    });
    panel.root.style.marginBottom = "18px";
    ctx.outlet.append(panel.root);
  }

  const search = h("input", {
    placeholder: "搜索标签、关键词…",
    value: query.q ?? "",
    "aria-label": "搜索广场",
  });
  search.addEventListener(
    "input",
    debounce(() => update({ q: search.value.trim() || null, page: null }), 350),
  );

  const tagRow = h("div.flexline", { style: { marginBottom: "14px" } });
  ctx.outlet.append(
    h(
      "div.flexline",
      { style: { marginBottom: "14px" } },
      search,
      h("span.spacer"),
      h("button.chip", {
        type: "button",
        class: query.sort === "new" ? "on" : "",
        text: "最新",
        on: { click: () => update({ sort: "new", page: null }) },
      }),
      h("button.chip", {
        type: "button",
        class: query.sort === "hot" ? "on" : "",
        text: "最热",
        on: { click: () => update({ sort: "hot", page: null }) },
      }),
    ),
    tagRow,
    gridHost,
    pagerHost,
  );

  tagRow.append(skeletonGrid(1));
  void request<TagRow[]>("/plaza/tags", { query: { limit: 24 } })
    .then((tags) => {
      tagRow.replaceChildren(
        h("button.chip", {
          type: "button",
          class: query.tag === null ? "on" : "",
          text: "全部",
          on: { click: () => update({ tag: null, page: null }) },
        }),
        ...tags.map((tag) =>
          h("button.chip", {
            type: "button",
            class: query.tag === tag.name ? "on" : "",
            text: `${tag.name} ${tag.usage}`,
            on: { click: () => update({ tag: tag.name, page: null }) },
          }),
        ),
      );
    })
    .catch(() => tagRow.replaceChildren());

  async function load(): Promise<void> {
    if (loading) return;
    loading = true;
    gridHost.replaceChildren(skeletonGrid(8));
    try {
      const page = await requestPage<ImageItem>("/plaza", {
        query: {
          q: query.q ?? undefined,
          tag: query.tag ?? undefined,
          sort: query.sort,
          page: query.page,
          per_page: 24,
        },
      });

      if (page.items.length === 0) {
        gridHost.replaceChildren(
          emptyState(
            "🖼",
            query.q || query.tag
              ? "没有匹配的图片，换个关键词试试。"
              : "广场上还没有公开图片。",
            user && !query.q && !query.tag
              ? h("button.btn.pri", {
                  type: "button",
                  text: "去上传",
                  on: { click: () => navigate("/upload") },
                })
              : undefined,
          ),
        );
      } else {
        const grid = h("div.grid.igrid");
        for (const item of page.items) {
          grid.append(
            imageTile(item, {
              subtitle: `${item.title ?? item.short_hash} · u_${item.owner_uid} · ${relativeTime(item.upload_time)}`,
              onOpen: (clicked) =>
                openLightbox(page.items, clicked, { anonymous: !user }),
            }),
          );
        }
        gridHost.replaceChildren(grid);
      }

      const pager = pagination(page.meta, (next) =>
        update({ page: next > 1 ? String(next) : null }),
      );
      pagerHost.replaceChildren(pager ?? h("span"));
    } catch (err) {
      gridHost.replaceChildren(
        emptyState(
          "⚠",
          "加载失败，请稍后重试。",
          h("button.btn", {
            type: "button",
            text: "重新加载",
            on: { click: () => void load() },
          }),
        ),
      );
      toastError(err);
    } finally {
      loading = false;
    }
  }

  refresh = load;

  function update(patch: Partial<Record<"q" | "tag" | "sort" | "page", string | number | null>>): void {
    const next: Record<string, string | number | null> = {
      q: query.q,
      tag: query.tag,
      sort: query.sort,
      page: query.page,
      ...patch,
    };
    const params = new URLSearchParams();
    for (const [key, value] of Object.entries(next)) {
      if (value === null || value === "") continue;
      // 默认值不进 URL，链接才干净
      if (key === "page" && value === 1) continue;
      if (key === "sort" && value === "new") continue;
      params.set(key, String(value));
    }
    const encoded = params.toString();
    navigate(encoded ? `/?${encoded}` : "/");
  }

  await load();
}
