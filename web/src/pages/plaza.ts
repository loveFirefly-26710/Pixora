/**
 * 广场。
 *
 * 未登录：只有广场，右上角登录/注册入口；点图片能看预览浮层，但浮层里的
 * 下载与复制直链换成「登录后可用」——服务端对未登录用户也会把 `original_name` 置空，
 * 两边的口径是一致的。
 *
 * 已登录：上传区在广场上方，同屏可见，传完立刻能在下面看到自己的图（如果设为公开）。
 *
 * 列表是**一直往下滑一直加载**，没有翻页：画廊翻页很难受——页数一多就不知道
 * 自己在哪，点下一页还会跳回顶部。加载入口是底部一个哨兵元素，它进入视口就取下一页
 * （提前 600px 预取，滑到底时下一页往往已经在路上了）。
 */

import { requestPage } from "../core/api";
import { h } from "../core/dom";
import { relativeTime } from "../core/format";
import { navigate, type PageContext } from "../core/router";
import { currentUser, getState } from "../core/state";
import { toastError } from "../core/toast";
import type { ImageItem } from "../core/types";
import { createUploadPanel } from "../upload/panel";
import { openLightbox } from "../ui/lightbox";
import { applyMasonry } from "../ui/masonry";
import {
  emptyState,
  imageTile,
  newShuffleSeed,
  pageHead,
  qtip,
  skeletonGrid,
  sortSelect,
  spinner,
} from "../ui/widgets";

/** 一页取多少：与列表页一致。 */
const PER_PAGE = 24;

interface PlazaQuery {
  q: string | null;
  tag: string | null;
  sort: string;
  /** 随机排序的种子：翻页要带同一个，否则每页各洗一次牌会重复与漏图 */
  seed: number;
}

const KNOWN_SORTS = ["shuffle", "new", "old", "size", "smallest", "name", "type", "hot"];

function readQuery(ctx: PageContext): PlazaQuery {
  const sortParam = ctx.query.get("sort");
  const sort = sortParam && KNOWN_SORTS.includes(sortParam) ? sortParam : "shuffle";
  const seedParam = Number(ctx.query.get("seed"));
  return {
    q: ctx.query.get("q"),
    tag: ctx.query.get("tag"),
    sort,
    // 链接里带了种子就用它（翻页、后退时顺序不变），否则现取一个
    seed: Number.isInteger(seedParam) && seedParam > 0 ? seedParam : newShuffleSeed(),
  };
}

/** 广场的列数由后台配置（默认 3 列，比原来的 5 列大一圈）。 */
function gridColumnsStyle(): string {
  const columns = getState().site?.plaza_grid_columns ?? 3;
  const clamped = Math.min(6, Math.max(2, columns));
  return `--plaza-cols: ${clamped};`;
}

/**
 * 广场没有内容时的占位提示，占满网格区并上下居中（`.empty.fill`）。
 *
 * 「一张公开图都没有」和「搜索没命中」是两件事：前者要说清广场现在是空的、
 * 以及怎么让它有内容；后者只要提醒换个词。都落在网格区里，别留一排灰框。
 */
function emptyGridHint(query: PlazaQuery, loggedIn: boolean): HTMLElement {
  const node =
    query.q || query.tag
      ? emptyState("🖼", "没有匹配的图片，请更换关键词。")
      : emptyState(
          "🖼",
          "广场还没有公开图片",
          h("div.tip", {
            style: { margin: "0" },
            text: loggedIn ? "上传一张，即可在广场展示。" : "登录后可上传自己的图片。",
          }),
        );
  node.classList.add("fill");
  return node;
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
    root.append(
      qtip(
        "warn",
        "广场对未登录用户关闭",
        "管理员关闭了公开浏览。登录后可以查看你有权限的内容，或联系管理员开通。",
      ),
    );
    return;
  }

  // 网格只建一次，往下追加节点：重建会把已经排好的瀑布流和滚动位置一起丢掉
  const grid = h("div.grid.igrid", { style: gridColumnsStyle() });
  const footer = h("div", {
    style: {
      display: "flex",
      alignItems: "center",
      justifyContent: "center",
      gap: "8px",
      padding: "22px 0 8px",
      color: "var(--muted)",
      fontSize: "12.5px",
    },
  });

  let loadFirstPage: () => Promise<void> = async () => {};

  // 上传区：登录用户看得到；未登录的人只在站点开着「允许游客上传」时看到
  // （上传会落到访客账号里，见 core/guest.rs）
  const guestUpload = !user && (site?.guest_upload?.enabled ?? false);
  if (user || guestUpload) {
    const panel = createUploadPanel({
      guest: !user,
      onUploaded: () => {
        // 传完把列表刷回第一页：按时间排序时新图在最前面；
        // 随机排序下也刷一次——重新洗牌后新图有机会出现在第一屏，
        // 总比让用户以为自己那张没传上去要好
        void loadFirstPage();
      },
    });
    panel.root.style.marginBottom = "18px";
    root.append(panel.root);
    // 相册清单由面板自己拉（带一次重试与占位项）：
    // 页面再拉一次会和它抢同一个下拉，两边先后写值反而会闪
  }

  // 搜索只在顶栏那一个输入框里（页面里不再重复），标签筛选也交给它——
  // 后端的关键词匹配本来就同时命中标题与标签
  const tagRow = h("div.flexline", { style: { marginBottom: "14px" } });
  root.append(tagRow, grid, footer);

  // 排序下拉。默认「随机」，另给一个「最热」（按浏览量）。
  // 换排序时换一个种子：不然「随机」切回来还是同一份顺序，看着像没生效
  const sortControl = sortSelect(query.sort, (value) => {
    update({ sort: value, seed: value === "shuffle" ? String(newShuffleSeed()) : null });
  }, [{ value: "hot", label: "最热（按浏览量）" }]);
  sortControl.classList.add("f");
  sortControl.style.maxWidth = "190px";
  tagRow.append(h("span.spacer"), sortControl);

  // 顶栏搜索框显示当前关键词（从别处带着 ?q= 进来时也对得上）
  const headerSearch = document.querySelector<HTMLInputElement>(".navbar .search input");
  if (headerSearch) headerSearch.value = query.q ?? "";

  const items: ImageItem[] = [];
  let currentPage = 0;
  let totalPages = 1;
  let loading = false;
  let failed = false;

  function paintFooter(): void {
    if (loading) {
      footer.replaceChildren(spinner(), h("span", { text: "正在加载…" }));
      return;
    }
    if (failed) {
      footer.replaceChildren(
        h("span", { text: "加载失败" }),
        h("button.btn.sm", {
          type: "button",
          text: "重试",
          on: { click: () => void loadMore() },
        }),
      );
      return;
    }
    if (currentPage >= totalPages) {
      // 一张都没有（空广场、搜索无结果，或者第一页还没回来）时不摆「没有更多了」，
      // 列表空着就让它空着
      footer.replaceChildren(h("span", { text: items.length === 0 ? "" : "没有更多了" }));
      return;
    }
    // 自动加载为主，这个按钮是兜底：观察器不可用、或用户就想手动点一下时用得上
    footer.replaceChildren(
      h("button.btn.sm", {
        type: "button",
        text: "加载更多",
        on: { click: () => void loadMore() },
      }),
    );
  }

  /** 取下一页并追加。`reset` 用于排序变化后从头来。 */
  async function loadMore(reset = false): Promise<void> {
    if (loading) return;
    if (!reset && currentPage >= totalPages) return;
    loading = true;
    failed = false;
    if (reset) {
      currentPage = 0;
      totalPages = 1;
      items.length = 0;
      grid.replaceChildren(skeletonGrid(8));
      footer.replaceChildren();
    } else {
      paintFooter();
    }

    try {
      const page = await requestPage<ImageItem>("/plaza", {
        query: {
          q: query.q ?? undefined,
          tag: query.tag ?? undefined,
          sort: query.sort,
          seed: query.seed,
          page: currentPage + 1,
          per_page: PER_PAGE,
        },
      });
      currentPage = page.meta.page;
      totalPages = Math.max(1, page.meta.total_pages);

      if (reset) grid.replaceChildren();
      if (currentPage === 1 && page.items.length === 0) {
        grid.replaceChildren(emptyGridHint(query, Boolean(user)));
      } else {
        for (const item of page.items) {
          grid.append(
            imageTile(item, {
              // 只给时间：卡片上不再暴露上传者 UID（见 widgets.imageTile 的说明）
              subtitle: relativeTime(item.upload_time),
              // 预览浮层拿累加后的整份列表：翻到哪就能在浮层里前后翻到哪
              onOpen: (clicked) => openLightbox(items, clicked),
            }),
          );
        }
        items.push(...page.items);
        // 新加的这段算跨行数；已有节点的跨行数不会因为追加而改变
        applyMasonry(grid);
      }
    } catch (err) {
      failed = true;
      if (currentPage === 0) {
        grid.replaceChildren(
          emptyState(
            "⚠",
            "加载失败，请稍后重试。",
            h("button.btn", {
              type: "button",
              text: "重新加载",
              on: { click: () => void loadMore(true) },
            }),
          ),
        );
      }
      toastError(err);
    } finally {
      loading = false;
      paintFooter();
    }
  }

  loadFirstPage = () => loadMore(true);

  // 哨兵：进入视口就取下一页
  const sentinel = h("div", { style: { height: "1px" } });
  footer.after(sentinel);
  if (typeof IntersectionObserver === "function") {
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) void loadMore();
      },
      { rootMargin: "600px 0px" },
    );
    observer.observe(sentinel);
  }

  function update(
    patch: Partial<Record<"q" | "tag" | "sort" | "seed", string | null>>,
  ): void {
    const next: Record<string, string | null> = {
      q: query.q,
      tag: query.tag,
      sort: query.sort,
      seed: String(query.seed),
      ...patch,
    };
    const params = new URLSearchParams();
    for (const [key, value] of Object.entries(next)) {
      if (value === null || value === "") continue;
      // 默认值不进 URL，链接才干净
      if (key === "sort" && value === "shuffle") continue;
      params.set(key, String(value));
    }
    const encoded = params.toString();
    navigate(encoded ? `/?${encoded}` : "/");
  }

  await loadMore(true);
}
