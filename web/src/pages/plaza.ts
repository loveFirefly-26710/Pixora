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

import { request, requestPage } from "../core/api";
import { h } from "../core/dom";
import { relativeTime } from "../core/format";
import {
  autoPlayIfConfigured,
  currentTrack,
  musicState,
  subscribeMusic,
  toggleMusic,
} from "../core/music";
import { navigate, type PageContext } from "../core/router";
import { currentUser, getState } from "../core/state";
import { toastError } from "../core/toast";
import type { ImageItem, MusicTrack } from "../core/types";
import { createUploadPanel } from "../upload/panel";
import { FIREFLY_MARK } from "../ui/brand";
import { openLightbox } from "../ui/lightbox";
import { applyMasonry } from "../ui/masonry";
import {
  emptyState,
  imageTile,
  newShuffleSeed,
  qtip,
  skeletonTiles,
  sortSelect,
  spinner,
  thumbImage,
  viewHead,
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

const KNOWN_SORTS = [
  "shuffle",
  "new",
  "old",
  "name_desc",
  "name",
  "size",
  "smallest",
  "type",
  "hot",
];

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
      ? emptyState("✦", "没有匹配的图片，请更换关键词。")
      : emptyState(
          "✦",
          "广场还没有公开图片",
          h("div.tip", {
            style: { margin: "0" },
            text: loggedIn ? "上传一张，即可在广场展示。" : "登录后可上传自己的图片。",
          }),
        );
  node.classList.add("fill");
  return node;
}

interface PlazaHeroHandle {
  root: HTMLElement;
  setFeatured(item: ImageItem | null): void;
  setTotal(total: number): void;
  setAutoState(enabled: boolean): void;
  setAutoCountdown(remaining: number): void;
  setMusic(playing: boolean, track: MusicTrack | null): void;
  setFeaturedViews(item: ImageItem): void;
  setPanelOpen(open: boolean): void;
}

interface PlazaHeroOptions {
  onUpload: () => void;
  onOpen: (item: ImageItem) => void;
  onToggleAuto: () => void;
  onToggleMusic: () => void;
  autoSeconds: number;
  autoEnabled: boolean;
  musicEnabled: boolean;
}

/** 倒计时显示：一分钟以内写秒，超过一分钟写成 m:ss。 */
function autoCountdownText(seconds: number): string {
  if (seconds >= 60) {
    const minutes = Math.floor(seconds / 60);
    const rest = seconds % 60;
    return `${minutes}:${String(rest).padStart(2, "0")}`;
  }
  return `${seconds} 秒`;
}

/**
 * 广场首屏：一段品牌文案 + 一个圆形「随机一帧」。
 *
 * 图不是另找的素材：第一页回来之后直接拿第一张公开图当 hero。
 * 管理员启用背景音乐后，点这张圆片就是播放/暂停，圆片会像唱片一样转起来；
 * 没启用音乐时保持原行为——点开当前图片的预览。
 * 自动漫游按钮只在管理员设了间隔时出现，点一下暂停/继续。
 */
function buildPlazaHero(options: PlazaHeroOptions): PlazaHeroHandle {
  let featured: ImageItem | null = null;
  let playing = false;
  const count = h("span", { text: "0 幅公开影像" });
  const chipTitle = h("div.hero-chip-title", { text: "等待第一束光" });
  const chipSub = h("div.hero-chip-sub", { text: "上传后出现在这里" });
  const trackTitle = h("div.hero-chip-title", { text: "今夜有光" });
  const trackSub = h("div.hero-chip-sub", { text: "PUBLIC / ORBIT · 03:24" });
  const sourceChip = h(
    "div.hero-chip.one",
    null,
    h("div.hero-chip-label", { text: "来自广场 · 随机一帧" }),
    chipTitle,
    chipSub,
  );

  const core = h("span.hero-core", null, h("span.logo-mark", { html: FIREFLY_MARK }));
  const frameInner = h("div.hero-frame-inner", null, core);
  const frame = h(
    "button.hero-frame",
    { type: "button", "aria-label": "广场还没有公开图片" },
    frameInner,
  );
  const pulse = (): void => {
    frame.classList.remove("is-tapping");
    // 读取一次布局，确保连续点击也能重新播放动画
    void frame.offsetWidth;
    frame.classList.add("is-tapping");
  };
  frame.addEventListener("animationend", () => frame.classList.remove("is-tapping"));
  frame.addEventListener("click", () => {
    if (options.musicEnabled) {
      pulse();
      options.onToggleMusic();
      return;
    }
    if (featured) options.onOpen(featured);
  });

  const upload = h("button.btn.pri", { type: "button", text: "投递影像" });
  upload.addEventListener("click", options.onUpload);

  let autoButton: HTMLButtonElement | null = null;
  let autoLabel: HTMLElement | null = null;
  if (options.autoSeconds > 0) {
    autoLabel = h("span.auto-label");
    autoButton = h(
      "button.btn.gho.auto-roam",
      {
        type: "button",
        "aria-pressed": String(options.autoEnabled),
      },
      h("span.auto-dot", { "aria-hidden": "true" }),
      autoLabel,
    );
    autoButton.addEventListener("click", options.onToggleAuto);
  }

  const visual = h(
    "div.plaza-hero-visual",
    null,
    h("div.hero-halo", { "aria-hidden": "true" }),
    h("div.hero-ring.r1", { "aria-hidden": "true" }),
    h("div.hero-ring.r2", { "aria-hidden": "true" }),
    h("div.hero-ring.r3", { "aria-hidden": "true" }),
    h("div.hero-arc", { "aria-hidden": "true" }),
    frame,
    sourceChip,
    h(
      "div.hero-chip.two",
      null,
      h("div.hero-chip-label", { text: "Firefly signal 01" }),
      trackTitle,
      trackSub,
    ),
  );
  const hero = h(
    "section.plaza-hero",
    null,
    h(
      "div.plaza-hero-copy",
      null,
      h("div.view-eyebrow", { text: "Night archive · Firefly signal" }),
      h("h1.plaza-hero-title", null, "萤火", h("em", { text: "广场" })),
      h("p.plaza-hero-desc", {
        text: "在夜里，留下一点光。浏览公开影像，或把自己的那一帧投进这片夜空。",
      }),
      h("div.plaza-hero-actions", null, upload, autoButton),
      h(
        "div.plaza-hero-meta",
        null,
        count,
        h("i", { "aria-hidden": "true" }),
        h("span", { text: "每次刷新，都是新的轨迹" }),
      ),
    ),
    visual,
  );

  function setTotal(total: number): void {
    count.textContent = total > 0 ? `${total} 幅公开影像` : "广场还没有公开影像";
  }

  /** 小卡片那两行文案：标题 + 「标签 · N 次浏览」。 */
  function paintChip(item: ImageItem): void {
    chipTitle.textContent = item.title?.trim() || item.tags[0] || "来自广场";
    chipSub.textContent = `${item.tags.slice(0, 2).join(" / ") || "公开影像"} · ${item.views} 次浏览`;
  }

  function setFeatured(item: ImageItem | null): void {
    if (!item) {
      featured = null;
      frameInner.replaceChildren(core);
      frame.setAttribute(
        "aria-label",
        options.musicEnabled ? "播放或暂停背景音乐" : "广场还没有公开图片",
      );
      chipTitle.textContent = "等待第一束光";
      chipSub.textContent = "上传后出现在这里";
      return;
    }

    // 同一张图又来了（刷新后随机顺序没变）：只更新文案，不重建 <img>。
    // 旋转动画挂在 img 上，换一个元素就会从 0° 重新转
    if (featured && featured.id === item.id) {
      featured = item;
      paintChip(item);
      return;
    }

    // 音乐播放中不换画面：唱片正转着，突然换一张像跳针。
    // 等暂停、或用户自己点了别的图，画面再更新
    if (playing && featured) return;

    featured = item;
    const img = thumbImage({
      url: item.url,
      thumb_url: item.thumb_url,
      has_thumb: item.has_thumb,
      tags: item.tags,
      title: item.title,
    });
    img.loading = "eager";
    img.decoding = "async";
    img.alt = item.title ?? "广场图片";
    frameInner.replaceChildren(img);
    frame.setAttribute(
      "aria-label",
      options.musicEnabled
        ? `${playing ? "暂停" : "播放"}背景音乐（当前画面：${item.title ?? "广场图片"}）`
        : `打开 ${item.title ?? "这张图片"}`,
    );
    paintChip(item);
  }

  function setFeaturedViews(item: ImageItem): void {
    if (!featured || featured.id !== item.id) return;
    paintChip(item);
  }

  function setPanelOpen(open: boolean): void {
    sourceChip.classList.toggle("is-hidden", open);
    // 视觉上淡出之后，读屏也不该再念这张被侧栏让位的小卡片
    sourceChip.setAttribute("aria-hidden", String(open));
  }

  function setMusic(nextPlaying: boolean, nextTrack: MusicTrack | null): void {
    playing = nextPlaying;
    const active = options.musicEnabled && playing;
    frame.classList.toggle("is-playing", active);
    visual.classList.toggle("is-playing", active);
    if (!options.musicEnabled) return;
    frame.setAttribute(
      "aria-label",
      `${playing ? "暂停" : "播放"}背景音乐${featured?.title ? `（当前画面：${featured.title}）` : ""}`,
    );
    if (nextTrack) {
      trackTitle.textContent = nextTrack.title;
      trackSub.textContent = `${nextTrack.artist || "未知艺术家"} · ${playing ? "NOW PLAYING" : "PAUSED"}`;
      pulse();
    } else {
      trackTitle.textContent = "今夜有光";
      trackSub.textContent = "PUBLIC / ORBIT · 03:24";
    }
  }

  function setAutoState(enabled: boolean): void {
    if (!autoButton || !autoLabel) return;
    autoButton.classList.toggle("on", enabled);
    autoButton.setAttribute("aria-pressed", String(enabled));
    autoLabel.textContent = enabled ? "自动漫游" : "自动漫游已暂停";
    const action = enabled ? "暂停自动漫游" : "继续自动漫游";
    autoButton.setAttribute("aria-label", action);
    autoButton.title = action;
  }

  function setAutoCountdown(remaining: number): void {
    if (!autoLabel) return;
    autoLabel.textContent = `自动漫游 · ${autoCountdownText(Math.max(0, remaining))}`;
  }

  setAutoState(options.autoEnabled);
  if (options.autoEnabled) setAutoCountdown(options.autoSeconds);
  return {
    root: hero,
    setFeatured,
    setTotal,
    setAutoState,
    setAutoCountdown,
    setMusic,
    setFeaturedViews,
    setPanelOpen,
  };
}

export async function render(ctx: PageContext): Promise<void> {
  const query = readQuery(ctx);
  const user = currentUser();
  const site = getState().site;

  // 内容限宽居中：广场图片不铺满整屏，两侧留白（与其他页面同一套 .pad）
  const root = h("div.pad.plaza-pad");
  ctx.outlet.append(root);
  ctx = { ...ctx, outlet: root };

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

  const items: ImageItem[] = [];
  let jumpToUpload = () => navigate("/upload");

  // 搜索/标签结果页不摆 hero：结果应该直接出现在第一屏。
  // 普通广场用 hero 带出品牌气质，并把第一张公开图当作「随机一帧」。
  const isPlazaHome = !query.q && !query.tag;
  const configuredAuto = Math.max(0, site?.plaza_auto_refresh_seconds ?? 0);
  // 后端已经钳过一次；前端再兜一层，避免旧数据或手工请求把间隔压到 1 秒
  const autoSeconds =
    isPlazaHome && configuredAuto > 0 ? Math.min(3600, Math.max(5, configuredAuto)) : 0;
  let autoEnabled = autoSeconds > 0;
  let autoRemaining = autoSeconds;
  let autoTimer: number | null = null;
  let heroState: PlazaHeroHandle | null = null;
  const musicEnabled = Boolean(site?.music?.enabled);

  function paintTileViews(item: ImageItem): void {
    const tile = grid.querySelector<HTMLElement>(`.tile[data-image-id="${item.id}"]`);
    const sub = tile?.querySelector<HTMLElement>(".meta .s");
    if (!sub) return;
    let view = sub.querySelector<HTMLElement>(".v");
    if (!view) {
      view = h("span.v");
      sub.append(view);
    }
    view.textContent = `浏览 ${item.views}`;
  }

  async function recordView(item: ImageItem): Promise<void> {
    // 每次点击都记一次；属主自己看也计数（服务端只对公开图累加）。
    // 并发点击时响应可能乱序回来，浏览量只增不减，别让先到的旧数字盖掉新数字。
    try {
      const result = await request<{ views: number }>(`/images/${item.id}/view`, {
        method: "POST",
      });
      if (result.views > item.views) {
        item.views = result.views;
        paintTileViews(item);
        heroState?.setFeaturedViews(item);
      }
    } catch {
      // 计数失败不影响看图；网络恢复后下一次点击会继续累计
    }
  }

  function openPlazaItem(item: ImageItem): void {
    void recordView(item);
    openLightbox(items, item);
  }
  if (isPlazaHome) {
    heroState = buildPlazaHero({
      autoSeconds,
      autoEnabled,
      musicEnabled,
      onUpload: () => jumpToUpload(),
      onToggleAuto: () => toggleAuto(),
      onToggleMusic: () => void toggleMusic(),
      onOpen: openPlazaItem,
    });
    root.append(heroState.root);

    // 唱片只关心「是否在播放」和「哪一首」，不必跟着 timeupdate 每 250ms 重画
    let lastPlaying: boolean | null = null;
    let lastTrackId: string | null = null;
    let lastPanelOpen: boolean | null = null;
    const syncHeroMusic = (): void => {
      const music = musicState();
      const track = currentTrack();
      if (
        music.playing === lastPlaying &&
        (track?.id ?? null) === lastTrackId &&
        music.panelOpen === lastPanelOpen
      ) {
        return;
      }
      lastPlaying = music.playing;
      lastTrackId = track?.id ?? null;
      lastPanelOpen = music.panelOpen;
      heroState?.setMusic(music.playing, track);
      heroState?.setPanelOpen(music.panelOpen);
    };
    const unsubscribeMusic = subscribeMusic(() => {
      if (!root.isConnected) {
        unsubscribeMusic();
        return;
      }
      syncHeroMusic();
    });
    syncHeroMusic();
    void autoPlayIfConfigured();
  } else {
    root.append(
      viewHead(
        "Plaza / Gallery",
        "广场",
        query.q
          ? `搜索“${query.q}”的公开影像。`
          : "浏览公开影像。登录后可以上传和下载原图。",
      ),
    );
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
      compact: true,
      onUploaded: () => {
        // 传完把列表刷回第一页：按时间排序时新图在最前面；
        // 随机排序下也刷一次——重新洗牌后新图有机会出现在第一屏，
        // 总比让用户以为自己那张没传上去要好
        void loadFirstPage();
      },
    });
    panel.root.style.marginBottom = "18px";
    root.append(panel.root);
    jumpToUpload = () => {
      panel.root.scrollIntoView({
        behavior: window.matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth",
        block: "start",
      });
    };
    // 相册清单由面板自己拉（带一次重试与占位项）：
    // 页面再拉一次会和它抢同一个下拉，两边先后写值反而会闪
  }

  // 搜索只在顶栏那一个输入框里（页面里不再重复），标签筛选也交给它——
  // 后端的关键词匹配本来就同时命中标题与标签
  const tagRow = h("div.flexline.plaza-toolbar");
  root.append(tagRow, grid, footer);

  // 排序下拉。默认「随机」；「最热」按浏览量，后端有 views 统计，所以保留。
  // 换排序时换一个种子：不然「随机」切回来还是同一份顺序，看着像没生效
  const sortControl = sortSelect(query.sort, (value) => {
    update({ sort: value, seed: value === "shuffle" ? String(newShuffleSeed()) : null });
  }, [{ value: "hot", label: "最热（按浏览量）" }]);
  sortControl.classList.add("f");
  sortControl.style.maxWidth = "190px";
  const reshuffle = h("button.btn.sm.gho", { type: "button", text: "换一批" });
  reshuffle.addEventListener("click", () => {
    update({ sort: "shuffle", seed: String(newShuffleSeed()) });
  });
  tagRow.append(h("span.spacer"), reshuffle, sortControl);

  // 顶栏搜索框显示当前关键词（从别处带着 ?q= 进来时也对得上）
  const headerSearch = document.querySelector<HTMLInputElement>(".navbar .search input");
  if (headerSearch) headerSearch.value = query.q ?? "";

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
      grid.replaceChildren(...skeletonTiles(8));
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
      if (currentPage === 1) {
        heroState?.setTotal(page.meta.total);
        heroState?.setFeatured(page.items[0] ?? null);
      }

      if (reset) grid.replaceChildren();
      if (page.items.length === 0) {
        // 第一页就是空的：把骨架屏换成空状态。不要依赖 meta.page 一定是 1，
        // 否则空广场切排序时会留下几块灰色占位。
        if (items.length === 0) {
          grid.replaceChildren(emptyGridHint(query, Boolean(user)));
          currentPage = 1;
          totalPages = 1;
        }
      } else {
        for (const item of page.items) {
          grid.append(
            imageTile(item, {
              // 只给用户自己写的标题与时间，不暴露上传者 UID / 原始文件名
              title: item.title,
              subtitle: relativeTime(item.upload_time),
              // 广场每张图底部都带浏览量，0 也显示
              alwaysShowViews: true,
              // 预览浮层拿累加后的整份列表：翻到哪就能在浮层里前后翻到哪
              onOpen: openPlazaItem,
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

  function stopAutoTimer(): void {
    if (autoTimer !== null) {
      window.clearInterval(autoTimer);
      autoTimer = null;
    }
  }

  function startAutoTimer(): void {
    stopAutoTimer();
    if (!autoEnabled || autoSeconds <= 0) return;
    autoRemaining = autoSeconds;
    heroState?.setAutoCountdown(autoRemaining);
    autoTimer = window.setInterval(() => {
      // 离开广场后页面节点会被路由替换掉；等到下一次 tick 自己收掉，不用额外卸载钩子
      if (!root.isConnected) {
        stopAutoTimer();
        return;
      }
      if (!autoEnabled) {
        stopAutoTimer();
        return;
      }
      autoRemaining = Math.max(0, autoRemaining - 1);
      if (autoRemaining > 0) {
        heroState?.setAutoCountdown(autoRemaining);
        return;
      }
      // 到点先重置倒计时，再判断这一轮能不能换图。后台标签页、看图浮层、
      // 弹窗或正在加载时跳过，下一轮重新计满。
      autoRemaining = autoSeconds;
      heroState?.setAutoCountdown(autoRemaining);
      if (loading) return;
      if (document.visibilityState !== "visible") return;
      if (document.querySelector(".lb, .modal")) return;
      void quietRefresh();
    }, 1000);
  }

  function toggleAuto(): void {
    if (autoSeconds <= 0) return;
    autoEnabled = !autoEnabled;
    heroState?.setAutoState(autoEnabled);
    if (autoEnabled) startAutoTimer();
    else stopAutoTimer();
  }

  /**
   * 自动漫游的刷新：不先清空网格，等第一页回来再整体替换。
   * 这样不会每次换图都闪一次骨架屏；请求失败时也保留当前画面。
   */
  async function quietRefresh(): Promise<void> {
    if (loading) return;
    loading = true;
    try {
      // 随机排序换一个种子，非随机排序保持当前口径
      if (query.sort === "shuffle") query.seed = newShuffleSeed();
      const page = await requestPage<ImageItem>("/plaza", {
        query: {
          q: query.q ?? undefined,
          tag: query.tag ?? undefined,
          sort: query.sort,
          seed: query.seed,
          page: 1,
          per_page: PER_PAGE,
        },
      });
      currentPage = page.meta.page;
      totalPages = Math.max(1, page.meta.total_pages);
      items.length = 0;
      items.push(...page.items);
      heroState?.setTotal(page.meta.total);
      heroState?.setFeatured(page.items[0] ?? null);

      grid.replaceChildren();
      if (page.items.length === 0) {
        grid.replaceChildren(emptyGridHint(query, Boolean(user)));
        currentPage = 1;
        totalPages = 1;
      } else {
        for (const item of page.items) {
          grid.append(
            imageTile(item, {
              title: item.title,
              subtitle: relativeTime(item.upload_time),
              alwaysShowViews: true,
              onOpen: openPlazaItem,
            }),
          );
        }
        applyMasonry(grid);
      }
      failed = false;
    } catch {
      // 自动刷新失败不覆盖当前画面，也不弹错；等下一轮再试
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
  startAutoTimer();
}
