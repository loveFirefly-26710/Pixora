/**
 * 全局背景音乐。
 *
 * 播放器只有一份，挂在 `document` 之外：切页面时顶栏会重建，但音频不能跟着
 * 停。界面（顶栏按钮、歌单侧栏、广场唱片）都订阅这里的状态。
 *
 * 歌单由服务端代理第三方 API 或扫描本地目录，浏览器只认站内的
 * `/music/playlist`；歌词同样走站内代理，避免把 API 地址散到前端。
 */

import { request } from "./api";
import { getState } from "./state";
import { toastError } from "./toast";
import type { MusicPlaylist, MusicTrack } from "./types";

export interface LyricLine {
  time: number;
  text: string;
}

export interface MusicSnapshot {
  enabled: boolean;
  loading: boolean;
  loaded: boolean;
  tracks: MusicTrack[];
  index: number;
  playing: boolean;
  currentTime: number;
  duration: number;
  /** 配置不完整 / 第三方接口失败时的说明 */
  message: string | null;
  panelOpen: boolean;
  tab: "playlist" | "lyrics";
  lyrics: string;
  lyricTrackId: string | null;
}

type Listener = () => void;

const state: MusicSnapshot = {
  enabled: false,
  loading: false,
  loaded: false,
  tracks: [],
  index: 0,
  playing: false,
  currentTime: 0,
  duration: 0,
  message: null,
  panelOpen: false,
  tab: "playlist",
  lyrics: "",
  lyricTrackId: null,
};

const listeners = new Set<Listener>();
const lyricCache = new Map<string, string>();
let audio: HTMLAudioElement | null = null;
let loadingPromise: Promise<void> | null = null;

function notify(): void {
  for (const listener of listeners) listener();
}

export function subscribeMusic(listener: Listener): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function musicState(): Readonly<MusicSnapshot> {
  return state;
}

export function currentTrack(): MusicTrack | null {
  return state.tracks[state.index] ?? null;
}

function configured(): boolean {
  return Boolean(getState().site?.music?.enabled);
}

function ensureAudio(): HTMLAudioElement {
  if (audio) return audio;
  audio = new Audio();
  audio.preload = "metadata";
  audio.addEventListener("play", () => {
    state.playing = true;
    notify();
  });
  audio.addEventListener("pause", () => {
    state.playing = false;
    notify();
  });
  audio.addEventListener("ended", () => {
    // 单曲循环会显得像卡住；有下一首就下一首，没有就从头再来
    if (state.tracks.length > 1) void next();
    else {
      audio!.currentTime = 0;
      void audio!.play().catch(() => {});
    }
  });
  audio.addEventListener("timeupdate", () => {
    state.currentTime = audio?.currentTime ?? 0;
    notify();
  });
  audio.addEventListener("loadedmetadata", () => {
    const duration = audio?.duration;
    if (duration && Number.isFinite(duration)) {
      state.duration = duration;
      notify();
    }
  });
  audio.addEventListener("error", () => {
    if (!audio?.src) return;
    state.playing = false;
    state.message = "这首音频加载失败，换一首试试";
    notify();
  });
  return audio;
}

/** 首次点击前把歌单拉好，播放动作才不会被网络请求打断。 */
export async function preloadMusic(): Promise<void> {
  state.enabled = configured();
  if (!state.enabled) {
    notify();
    return;
  }
  await loadPlaylist();
}

/** 管理员改完音乐设置后调用：停掉旧音频、清空缓存，再按新配置拉一次。 */
export async function reloadMusic(): Promise<void> {
  if (audio) {
    audio.pause();
    audio.removeAttribute("src");
    audio.load();
  }
  state.loaded = false;
  state.tracks = [];
  state.index = 0;
  state.playing = false;
  state.currentTime = 0;
  state.duration = 0;
  state.message = null;
  state.lyrics = "";
  state.lyricTrackId = null;
  lyricCache.clear();
  state.enabled = configured();
  notify();
  if (state.enabled) await loadPlaylist();
}

async function loadPlaylist(): Promise<void> {
  if (state.loaded) return;
  if (loadingPromise) return loadingPromise;
  state.loading = true;
  state.enabled = configured();
  notify();
  loadingPromise = (async () => {
    try {
      const data = await request<MusicPlaylist>("/music/playlist");
      state.tracks = data.tracks ?? [];
      state.message = data.message;
      state.loaded = true;
      if (state.tracks.length > 0) prepareTrack(0);
    } catch (err) {
      state.message = err instanceof Error ? err.message : "歌单加载失败";
      state.loaded = true;
    } finally {
      state.loading = false;
      loadingPromise = null;
      notify();
    }
  })();
  return loadingPromise;
}

function prepareTrack(index: number): void {
  const track = state.tracks[index];
  if (!track) return;
  const player = ensureAudio();
  if (player.src !== new URL(track.audio_url, location.href).href) {
    player.src = track.audio_url;
  }
  state.index = index;
  state.currentTime = 0;
  state.duration = track.duration_seconds || 0;
  state.lyrics = track.lyrics ?? "";
  state.lyricTrackId = null;
  state.message = null;
  void loadLyrics(track);
  notify();
}

async function loadLyrics(track: MusicTrack): Promise<void> {
  const cached = lyricCache.get(track.id);
  if (cached !== undefined) {
    if (state.tracks[state.index]?.id === track.id) {
      state.lyrics = cached;
      state.lyricTrackId = track.id;
      notify();
    }
    return;
  }
  if (track.lyrics) {
    lyricCache.set(track.id, track.lyrics);
    if (state.tracks[state.index]?.id === track.id) {
      state.lyrics = track.lyrics;
      state.lyricTrackId = track.id;
      notify();
    }
    return;
  }
  if (!track.lyric_url) {
    lyricCache.set(track.id, "");
    return;
  }
  try {
    const response = await fetch(track.lyric_url, { credentials: "same-origin" });
    const text = response.ok ? await response.text() : "";
    lyricCache.set(track.id, text);
    if (state.tracks[state.index]?.id === track.id) {
      state.lyrics = text;
      state.lyricTrackId = track.id;
      notify();
    }
  } catch {
    lyricCache.set(track.id, "");
  }
}

async function startPlayback(): Promise<void> {
  const player = ensureAudio();
  if (!player.src) {
    const first = state.tracks[state.index];
    if (!first) return;
    prepareTrack(state.index);
  }
  try {
    await player.play();
  } catch {
    toastError(new Error("浏览器没有开始播放，请再点一次播放按钮"));
  }
}

/** 顶部按钮与广场唱片都走它。 */
export async function toggleMusic(): Promise<void> {
  state.enabled = configured();
  if (!state.enabled) return;
  if (!state.loaded) await loadPlaylist();
  if (state.tracks.length === 0) {
    openPanel("playlist");
    return;
  }
  const player = ensureAudio();
  if (player.paused) await startPlayback();
  else player.pause();
}

export function selectTrack(index: number): void {
  if (index < 0 || index >= state.tracks.length) return;
  prepareTrack(index);
  void startPlayback();
}

export async function next(): Promise<void> {
  if (state.tracks.length === 0) return;
  if (!state.loaded) await loadPlaylist();
  if (state.tracks.length === 0) return;
  prepareTrack((state.index + 1) % state.tracks.length);
  void startPlayback();
}

export async function previous(): Promise<void> {
  if (state.tracks.length === 0) return;
  if (!state.loaded) await loadPlaylist();
  if (state.tracks.length === 0) return;
  prepareTrack((state.index - 1 + state.tracks.length) % state.tracks.length);
  void startPlayback();
}

export function seekMusic(seconds: number): void {
  const player = ensureAudio();
  if (!Number.isFinite(seconds)) return;
  try {
    player.currentTime = Math.max(0, Math.min(seconds, player.duration || seconds));
  } catch {
    /* 元数据还没到时忽略 */
  }
}

export function togglePanel(open?: boolean, tab?: "playlist" | "lyrics"): void {
  state.panelOpen = open ?? !state.panelOpen;
  if (tab) state.tab = tab;
  if (state.panelOpen) void loadPlaylist();
  notify();
}

/** 打开侧栏；歌单没加载时顺手拉一次。 */
export function openPanel(tab: "playlist" | "lyrics" = "playlist"): void {
  state.panelOpen = true;
  state.tab = tab;
  void loadPlaylist();
  notify();
}

export function closePanel(): void {
  state.panelOpen = false;
  notify();
}

export function setPanelTab(tab: "playlist" | "lyrics"): void {
  state.tab = tab;
  notify();
}

/** 解析 LRC：一行可有多个时间戳；`[ar:]` 这类元信息忽略。 */
export function parseLyrics(raw: string): LyricLine[] {
  const lines: LyricLine[] = [];
  const stamp = /\[(\d{1,3}):(\d{1,2})(?:[.:](\d{1,3}))?\]/g;
  for (const source of raw.split(/\r?\n/)) {
    const stamps: number[] = [];
    let match: RegExpExecArray | null;
    stamp.lastIndex = 0;
    while ((match = stamp.exec(source))) {
      const minutes = Number(match[1]);
      const seconds = Number(match[2]);
      const fraction = match[3] ? Number(match[3].padEnd(3, "0").slice(0, 3)) / 1000 : 0;
      stamps.push(minutes * 60 + seconds + fraction);
    }
    if (stamps.length === 0) continue;
    const text = source.replace(stamp, "").trim();
    if (!text) continue;
    for (const time of stamps) lines.push({ time, text });
  }
  lines.sort((a, b) => a.time - b.time);
  return lines;
}

export function formatClock(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "0:00";
  const total = Math.floor(seconds);
  const minutes = Math.floor(total / 60);
  return `${minutes}:${String(total % 60).padStart(2, "0")}`;
}
