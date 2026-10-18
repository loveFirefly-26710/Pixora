/**
 * 顶栏播放入口与全局歌单/歌词侧栏。
 *
 * 播放状态在 `core/music.ts`，这里只负责把状态画出来：
 *   - 顶栏一颗胶囊：唱片图标 + 当前曲目 + 播放/暂停小图标；
 *   - 右侧抽屉默认隐藏，用顶栏的队列按钮打开，可在「歌单 / 歌词」间切换。
 */

import { h, mount } from "../core/dom";
import {
  closePanel,
  currentTrack,
  formatClock,
  musicState,
  next,
  parseLyrics,
  previous,
  seekMusic,
  selectTrack,
  setMode,
  setPanelTab,
  subscribeMusic,
  toggleMusic,
  togglePanel,
  type PlaybackMode,
} from "../core/music";

const DISC_ICON = `<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">
  <circle cx="12" cy="12" r="9.2" fill="none" stroke="currentColor" stroke-width="1.4" opacity=".55"/>
  <circle cx="12" cy="12" r="6" fill="none" stroke="currentColor" stroke-width="1.1" opacity=".4"/>
  <circle cx="12" cy="12" r="2.1" fill="currentColor"/>
</svg>`;

const PLAY_ICON = `<svg class="ic-play" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
  <path d="M9 6.8v10.4c0 .7.8 1.1 1.4.7l7.4-5.2c.5-.4.5-1.1 0-1.4L10.4 6.1c-.6-.4-1.4 0-1.4.7z" fill="currentColor"/>
</svg>`;

const PAUSE_ICON = `<svg class="ic-pause" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
  <rect x="8" y="6" width="3.2" height="12" rx="1.1" fill="currentColor"/>
  <rect x="12.8" y="6" width="3.2" height="12" rx="1.1" fill="currentColor"/>
</svg>`;

const LIST_ICON = `<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">
  <path d="M5 7h11M5 12h11M5 17h7" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>
  <circle cx="18" cy="16.4" r="2.1" fill="none" stroke="currentColor" stroke-width="1.5"/>
  <path d="M20.1 16.4V9.2l-2.4.7" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>
</svg>`;

const PREV_ICON = `<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">
  <path d="M7 6v12M18 6.6v10.8c0 .7-.8 1.1-1.4.7l-7-5.4a.9.9 0 0 1 0-1.4l7-5.4c.6-.4 1.4 0 1.4.7z" fill="currentColor"/>
</svg>`;

const NEXT_ICON = `<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">
  <path d="M17 6v12M6 6.6v10.8c0 .7.8 1.1 1.4.7l7-5.4a.9.9 0 0 0 0-1.4l-7-5.4c-.6-.4-1.4 0-1.4.7z" fill="currentColor"/>
</svg>`;

const panelId = "pixora-music-panel";
let panelBody: HTMLElement | null = null;
let panelTabs: HTMLElement | null = null;
let panelTitle: HTMLElement | null = null;
let panelProgress: HTMLInputElement | null = null;
let panelTime: HTMLElement | null = null;
let panelMode: HTMLSelectElement | null = null;
let panelCover: HTMLElement | null = null;
let panelClose: HTMLButtonElement | null = null;
let panelScrim: HTMLElement | null = null;
let lastBodyKey = "";
/** 侧栏开关的焦点收尾：打开时记下来源，关闭时还回去。 */
let panelReturnFocus: HTMLElement | null = null;
let lastPanelOpen = false;

/** 顶栏胶囊。站点没启用音乐时调用方不会创建它。 */
export function musicControl(): HTMLElement {
  const root = h(
    "div.music-control",
    null,
    h(
      "button.music-play",
      {
        type: "button",
        "data-music-play": "",
        "aria-label": "播放背景音乐",
        title: "播放背景音乐",
      },
      h("span.music-disc", { html: DISC_ICON }),
      h("span.music-label", { text: "背景音乐" }),
      h("span.music-state", { html: `${PLAY_ICON}${PAUSE_ICON}` }),
    ),
    h(
      "button.music-queue",
      {
        type: "button",
        "data-music-queue": "",
        "aria-label": "打开歌单",
        "aria-controls": panelId,
        "aria-expanded": "false",
        title: "歌单与歌词",
      },
      h("span", { html: LIST_ICON }),
    ),
  );
  root.querySelector<HTMLButtonElement>("[data-music-play]")?.addEventListener("click", () => {
    void toggleMusic();
  });
  root.querySelector<HTMLButtonElement>("[data-music-queue]")?.addEventListener("click", () => {
    togglePanel();
  });
  syncControl(root);
  return root;
}

/** 外壳挂载时调一次，把侧栏放进 body；重复调用不会叠第二份。 */
export function mountMusicPanel(): void {
  if (document.getElementById(panelId)) return;
  const cover = h("div.music-cover", { html: DISC_ICON });
  const title = h("div.music-now", null, h("b", { text: "萤火电台" }), h("span", { text: "背景音乐" }));
  const close = h("button.music-close", {
    type: "button",
    "aria-label": "关闭歌单",
    title: "关闭歌单",
    text: "×",
  });
  const tabs = h(
    "div.music-tabs",
    null,
    h("button", { type: "button", "data-music-tab": "playlist", text: "歌单" }),
    h("button", { type: "button", "data-music-tab": "lyrics", text: "歌词" }),
  );
  const body = h("div.music-panel-body");
  const progress = h("input.music-progress", {
    type: "range",
    min: "0",
    max: "100",
    step: "0.1",
    value: "0",
    "aria-label": "播放进度",
  });
  const time = h("span.music-time", { text: "0:00 / 0:00" });
  const mode = h(
    "select.music-mode",
    { "aria-label": "播放模式", title: "播放模式" },
    h("option", { value: "shuffle", text: "随机播放" }),
    h("option", { value: "list", text: "列表循环" }),
    h("option", { value: "once", text: "顺序播放" }),
  );
  mode.value = musicState().mode;
  mode.addEventListener("change", () => setMode(mode.value as PlaybackMode));
  const controls = h(
    "div.music-panel-controls",
    null,
    h("button", { type: "button", "aria-label": "上一首", title: "上一首", on: { click: () => void previous() } }, h("span", { html: PREV_ICON })),
    h("button", { type: "button", "aria-label": "下一首", title: "下一首", on: { click: () => void next() } }, h("span", { html: NEXT_ICON })),
    mode,
    h("span.spacer"),
    time,
  );
  const panel = h(
    "aside.music-panel",
    { id: panelId, "aria-hidden": "true", "aria-label": "背景音乐" },
    h("div.music-panel-head", null, cover, title, h("span.spacer"), close),
    tabs,
    body,
    h("div.music-panel-foot", null, progress, controls),
  );
  // 手机上的侧栏是底部抽屉：加一层可点击的暗幕，点它等于关掉，
  // 同时把背景页面的滚动锁住（样式在 theme.css 的窄屏段）。
  const scrim = h("div.music-scrim", { "aria-hidden": "true" });
  scrim.addEventListener("click", () => closePanel());
  close.addEventListener("click", () => closePanel());
  for (const button of tabs.querySelectorAll<HTMLButtonElement>("[data-music-tab]")) {
    button.addEventListener("click", () => {
      setPanelTab(button.dataset.musicTab === "lyrics" ? "lyrics" : "playlist");
    });
  }
  progress.addEventListener("input", () => {
    const duration = musicState().duration;
    if (duration > 0) seekMusic((Number(progress.value) / 100) * duration);
  });
  document.body.append(scrim, panel);
  panelBody = body;
  panelTabs = tabs;
  panelTitle = title;
  panelProgress = progress;
  panelTime = time;
  panelMode = mode;
  panelCover = cover;
  panelClose = close;
  panelScrim = scrim;
  lastBodyKey = "";
  // Esc 关侧栏。看图浮层/弹窗自己用 Esc 时让它们先处理，
  // 免得一次按键把两层界面一起收掉。
  document.addEventListener("keydown", (event) => {
    if (event.key !== "Escape" || !musicState().panelOpen) return;
    if (document.querySelector(".modal, .lb")) return;
    event.preventDefault();
    closePanel();
  });
  renderPanel();
}

function syncControl(root: HTMLElement): void {
  const state = musicState();
  const track = currentTrack();
  root.classList.toggle("is-playing", state.playing);
  const button = root.querySelector<HTMLButtonElement>("[data-music-play]");
  const label = root.querySelector<HTMLElement>(".music-label");
  if (label) label.textContent = track?.title || "背景音乐";
  if (button) {
    const action = state.playing ? "暂停背景音乐" : "播放背景音乐";
    button.setAttribute("aria-label", action);
    button.setAttribute("title", track ? `${action} · ${track.title}` : action);
    button.setAttribute("aria-pressed", String(state.playing));
  }
  const queue = root.querySelector<HTMLButtonElement>("[data-music-queue]");
  if (queue) {
    const action = state.panelOpen ? "关闭歌单" : "打开歌单";
    queue.setAttribute("aria-label", action);
    queue.setAttribute("title", action);
    queue.setAttribute("aria-expanded", String(state.panelOpen));
  }
}

function renderControls(): void {
  for (const root of document.querySelectorAll<HTMLElement>(".music-control")) syncControl(root);
}

function focusPanel(): void {
  panelReturnFocus =
    document.activeElement instanceof HTMLElement ? document.activeElement : null;
  // 等抽屉开始滑入再聚焦，避免浏览器把整个面板滚进视口时抢走动画
  window.setTimeout(() => {
    if (musicState().panelOpen) panelClose?.focus({ preventScroll: true });
  }, 120);
}

function restorePanelFocus(): void {
  const target = panelReturnFocus;
  panelReturnFocus = null;
  target?.focus({ preventScroll: true });
}

function renderPanel(): void {
  const panel = document.getElementById(panelId);
  if (!panel) return;
  const state = musicState();
  const open = state.panelOpen;
  panel.classList.toggle("is-open", open);
  panel.classList.toggle("is-playing", state.playing);
  panel.setAttribute("aria-hidden", String(!open));
  // 关着的时候整块退出 Tab 顺序，避免键盘用户把焦点送进看不见的侧栏
  panel.toggleAttribute("inert", !open);
  document.documentElement.classList.toggle("music-panel-open", open);
  panelScrim?.classList.toggle("is-open", open);
  if (open !== lastPanelOpen) {
    lastPanelOpen = open;
    if (open) focusPanel();
    else restorePanelFocus();
  }
  if (panelTabs) {
    for (const button of panelTabs.querySelectorAll<HTMLButtonElement>("[data-music-tab]")) {
      const active = button.dataset.musicTab === state.tab;
      button.classList.toggle("on", active);
      button.setAttribute("aria-pressed", String(active));
    }
    const playlistTab = panelTabs.querySelector<HTMLElement>('[data-music-tab="playlist"]');
    if (playlistTab) {
      const count = state.tracks.length > 0 ? ` ${state.tracks.length}` : "";
      playlistTab.textContent = `歌单${count}`;
    }
  }
  const track = currentTrack();
  if (panelTitle) {
    const title = panelTitle.querySelector("b");
    const sub = panelTitle.querySelector("span");
    if (title) title.textContent = track?.title || "萤火电台";
    if (sub) sub.textContent = track?.artist || (state.loading ? "正在加载歌单…" : "背景音乐");
  }
  if (panelCover) {
    panelCover.innerHTML = track?.cover_url
      ? `<img src="${escapeAttr(track.cover_url)}" alt="">`
      : DISC_ICON;
  }
  const duration = state.duration || 0;
  if (panelProgress) {
    panelProgress.max = String(duration > 0 ? duration : 100);
    panelProgress.value = String(
      duration > 0 ? Math.min(state.currentTime, duration) : 0,
    );
    panelProgress.disabled = duration <= 0;
  }
  if (panelTime) {
    panelTime.textContent = `${formatClock(state.currentTime)} / ${formatClock(duration)}`;
  }
  if (panelMode) panelMode.value = state.mode;

  if (!panelBody) return;
  const key = [
    state.tab,
    state.loading ? "loading" : "ready",
    state.tracks.map((item) => item.id).join(","),
    state.index,
    state.lyricTrackId ?? "",
    state.message ?? "",
    state.lyrics.length,
  ].join("|");
  if (key !== lastBodyKey) {
    lastBodyKey = key;
    renderPanelBody(panelBody);
  }
  updateActiveRows(panelBody);
}

function renderPanelBody(body: HTMLElement): void {
  const state = musicState();
  if (state.loading) {
    mount(body, h("div.music-empty", { text: "正在读取歌单…" }));
    return;
  }
  if (state.tracks.length === 0) {
    mount(
      body,
      h(
        "div.music-empty",
        null,
        h("b", { text: state.message || "还没有可以播放的音乐" }),
        h("span", {
          text: state.message
            ? "管理员可在「系统设置 → 背景音乐」里调整。"
            : "管理员配置歌单或本地音乐目录后会显示在这里。",
        }),
      ),
    );
    return;
  }
  if (state.tab === "lyrics") {
    renderLyrics(body);
    return;
  }
  const list = h("div.music-list");
  state.tracks.forEach((track, index) => {
    const row = h(
      "button.music-row",
      {
        type: "button",
        class: index === state.index ? "on" : "",
        "data-track-index": String(index),
        on: { click: () => selectTrack(index) },
      },
      h(
        "span.music-row-cover",
        null,
        track.cover_url
          ? h("img", { src: track.cover_url, alt: "", loading: "lazy" })
          : h("span", { html: DISC_ICON }),
      ),
      h(
        "span.music-row-text",
        null,
        h("b", { text: track.title }),
        h("span", { text: track.artist || track.album || "未知艺术家" }),
      ),
      h("span.music-row-index", { text: String(index + 1).padStart(2, "0") }),
    );
    list.append(row);
  });
  mount(body, list);
}

function renderLyrics(body: HTMLElement): void {
  const state = musicState();
  if (state.lyrics.trim().length === 0) {
    mount(
      body,
      h("div.music-empty", null, h("b", { text: "这首歌还没有歌词" }), h("span", { text: "可以继续听歌，或换一首。" })),
    );
    return;
  }
  const lines = parseLyrics(state.lyrics);
  if (lines.length === 0) {
    mount(body, h("div.music-lyrics raw", { text: state.lyrics }));
    return;
  }
  mount(
    body,
    h(
      "div.music-lyrics",
      null,
      ...lines.map((line, index) =>
        h("div.music-lyric", { "data-lyric-index": String(index), text: line.text }),
      ),
    ),
  );
}

function updateActiveRows(body: HTMLElement): void {
  const state = musicState();
  for (const row of body.querySelectorAll<HTMLElement>(".music-row")) {
    row.classList.toggle("on", Number(row.dataset.trackIndex) === state.index);
  }
  if (state.tab !== "lyrics") return;
  const lines = parseLyrics(state.lyrics);
  if (lines.length === 0) return;
  let active = -1;
  for (let index = 0; index < lines.length; index += 1) {
    if (lines[index]!.time <= state.currentTime + 0.15) active = index;
    else break;
  }
  const nodes = body.querySelectorAll<HTMLElement>(".music-lyric");
  nodes.forEach((node, index) => node.classList.toggle("on", index === active));
  const activeNode = nodes[active];
  if (activeNode && state.panelOpen) {
    const target = activeNode.offsetTop - body.clientHeight / 2 + activeNode.clientHeight / 2;
    if (Math.abs(body.scrollTop - target) > 24) {
      body.scrollTo({ top: Math.max(0, target), behavior: "smooth" });
    }
  }
}

/** 面板里的曲目封面可能来自外链；插入 HTML 时转义属性。 */
function escapeAttr(value: string): string {
  return value.replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;");
}

subscribeMusic(() => {
  renderControls();
  renderPanel();
});
