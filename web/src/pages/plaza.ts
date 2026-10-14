/**
 * 广场。
 *
 * 未登录：只有广场，右上角登录/注册入口；点图片能看预览浮层，但浮层里的
 * 下载与复制直链换成「登录后可用」——服务端对未登录用户也会把 `original_name` 置空，
 * 两边的口径是一致的。
 *
 * 已登录：上传区在广场上方，同屏可见，传完立刻能在下面看到自己的图（如果设为公开）。
 */

import { requestPage } from "../core/api";
import { h } from "../core/dom";
import { relativeTime } from "../core/format";
import { navigate, type PageContext } from "../core/router";
import { currentUser, getState } from "../core/state";
import { toastError } from "../core/toast";
import type { ImageItem } from "../core/types";
import { createUploadPanel, loadAlbumOptions } from "../upload/panel";
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

/**
 * 广场每行放几张图，由管理员在后台配置（默认 3 列）。
 *
 * 这里只把列数作为 CSS 变量交给样式表：真正的列数还要看窗口够不够宽，
 * 窄屏放不下时自动少放几列，这件事用 CSS 的 auto-fill 做掉，
 * 免得为了响应式在 JS 里监听窗口尺寸。
 */
function gridColumnsStyle(): string {
  const columns = getState().site?.plaza_grid_columns ?? 3;
  const clamped = Math.min(6, Math.max(2, columns));
  return `--plaza-cols: ${clamped};`;
}

export async function render(ctx: PageContext): Promise<void> {
  const query = readQuery(ctx);
  const user = currentUser();
  const site = getState().site;

  // 内容限宽居中：广场图片不铺满整屏，两侧留白（与其他页面同一套 .pad）
  const root = h("div.pad");
  ctx.outlet.append(root);
  ctx = { ...ctx, outlet: root };

  root.append(
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
      onUploaded: () => {
        if (query.sort === "new") void refresh();
      },
    });
    panel.root.style.marginBottom = "18px";
    ctx.outlet.append(panel.root);
    // 目标相册下拉要有真清单：拿不到就退回「默认相册」（面板自己的兜底）
    void loadAlbumOptions(getState().viewingUid)
      .then((albums) => panel.refreshAlbums(albums))
      .catch(() => {});
  }

  // 搜索只在顶栏那一个输入框里（页面里不再重复），标签筛选也交给它——
  // 后端的关键词匹配本来就同时命中标题与标签
  const tagRow = h("div.flexline", { style: { marginBottom: "14px" } });
  ctx.outlet.append(tagRow, gridHost, pagerHost);

  const sortChips = () => [
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
  ];
  tagRow.append(h("span.spacer"), ...sortChips());

  // 顶栏搜索框显示当前关键词（从别处带着 ?q= 进来时也对得上）
  const headerSearch = document.querySelector<HTMLInputElement>(".navbar .search input");
  if (headerSearch) headerSearch.value = query.q ?? "";

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
        const grid = h("div.grid.igrid", { style: gridColumnsStyle() });
        for (const item of page.items) {
          grid.append(
            imageTile(item, {
              subtitle: `u_${item.owner_uid} · ${relativeTime(item.upload_time)}`,
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
