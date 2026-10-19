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
import { toast, toastError } from "./toast";
import type { MusicPlaylist, MusicTrack } from "./types";

export interface LyricLine {
  time: number;
  text: string;
}

export type PlaybackMode = "shuffle" | "list" | "once";

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
  mode: PlaybackMode;
  /** 0～1，叠加在系统音量上的倍率；默认由后台设置，初始 0.3 */
  volume: number;
  muted: boolean;
}

type Listener = () => void;

const MODE_KEY = "pixora-music-mode";
const VOLUME_KEY = "pixora-music-volume";
const MUTED_KEY = "pixora-music-muted";
/** 站点没配置时的兜底；正常由后台 `music.default_volume` 覆盖 */
const DEFAULT_VOLUME = 0.3;

function loadMode(): PlaybackMode {
  try {
    const saved = localStorage.getItem(MODE_KEY);
    if (saved === "list" || saved === "once" || saved === "shuffle") return saved;
  } catch {
    /* 隐私模式读不到就按默认 */
  }
  return "shuffle";
}

function savedVolume(): number | null {
  try {
    const saved = localStorage.getItem(VOLUME_KEY);
    if (saved !== null) {
      const value = Number(saved);
      if (Number.isFinite(value)) return Math.max(0, Math.min(1, value));
    }
  } catch {
    /* 隐私模式读不到就按默认 */
  }
  return null;
}

function loadVolume(): number {
  return savedVolume() ?? DEFAULT_VOLUME;
}

function loadMuted(): boolean {
  try {
    return localStorage.getItem(MUTED_KEY) === "1";
  } catch {
    return false;
  }
}

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
  mode: loadMode(),
  volume: loadVolume(),
  muted: loadMuted(),
};

/** 静音前最后的非零音量，点取消静音时恢复它。 */
let lastAudibleVolume = state.volume > 0 ? state.volume : DEFAULT_VOLUME;

/** 没有本地音量记忆时，采用管理员配置的默认值。 */
function syncDefaultVolume(): void {
  if (savedVolume() !== null) return;
  const configured = getState().site?.music?.default_volume;
  const value =
    typeof configured === "number" && Number.isFinite(configured)
      ? Math.max(0, Math.min(1, configured))
      : DEFAULT_VOLUME;
  if (state.volume === value) return;
  state.volume = value;
  if (value > 0) lastAudibleVolume = value;
  applyVolume();
  notify();
}

const listeners = new Set<Listener>();
const lyricCache = new Map<string, string>();
let audio: HTMLAudioElement | null = null;
let loadingPromise: Promise<void> | null = null;
let lastPlayErrorAt = 0;
/** 播放中按 ~10Hz 把 audio.currentTime 同步进状态：timeupdate 只有 4Hz，歌词和进度条会慢半拍。 */
let progressFrame = 0;
let lastProgressTick = 0;
/** 用户主动暂停过：之后回到首页不再擅自恢复播放。 */
let userPaused = false;
/** 自动播放被浏览器拦下后，等第一次交互再开始。 */
let autoplayGestureHandler: ((event: Event) => void) | null = null;
let autoplayToastShown = false;

function disarmAutoplayUnlock(): void {
  if (!autoplayGestureHandler) return;
  document.removeEventListener("pointerdown", autoplayGestureHandler, true);
  document.removeEventListener("touchstart", autoplayGestureHandler, true);
  document.removeEventListener("touchend", autoplayGestureHandler, true);
  document.removeEventListener("click", autoplayGestureHandler, true);
  document.removeEventListener("keydown", autoplayGestureHandler, true);
  document.removeEventListener("wheel", autoplayGestureHandler, true);
  document.removeEventListener("scroll", autoplayGestureHandler, true);
  document.removeEventListener("pointermove", autoplayGestureHandler, true);
  document.removeEventListener("touchmove", autoplayGestureHandler, true);
  autoplayGestureHandler = null;
}

function armAutoplayUnlock(): void {
  if (autoplayGestureHandler) return;
  const activationEvents = new Set([
    "pointerdown",
    "touchstart",
    "touchend",
    "click",
    "keydown",
  ]);
  /** 滚轮/滚动/移动只各尝试一次，避免鼠标一划就反复 play()。 */
  const attempted = new Set<string>();
  const handler = (event: Event): void => {
    const type = event.type;
    const activation = activationEvents.has(type);
    const target = event.target instanceof Element ? event.target : null;
    // 点到播放/暂停按钮、广场唱片或侧栏时，让它们按原本的意图处理，
    // 不要在 capture 阶段先播一遍又被按钮切成暂停。歌单按钮不在此列：
    // 第一次点开歌单时顺手把音乐带起来，符合「首次交互后自动播放」。
    if (activation && target?.closest(".music-play, .music-panel, .hero-frame")) {
      disarmAutoplayUnlock();
      return;
    }
    if (!getState().site?.music?.autoplay) {
      disarmAutoplayUnlock();
      return;
    }
    if (userPaused || state.playing || state.tracks.length === 0) return;
    if (!activation) {
      if (attempted.has(type)) return;
      attempted.add(type);
    }
    // 有的浏览器只认 click 的用户手势，不认 touchstart；一次触摸会依次
    // 发 pointerdown / touchstart / touchend / click，失败就留给下一个事件再试。
    // 滚轮、滚动和光标移动也试一次，但浏览器是否放行由它自己的策略决定。
    // 真正开始播放时由 play 事件拆掉监听。
    void startPlayback(true);
  };
  autoplayGestureHandler = handler;
  document.addEventListener("pointerdown", handler, true);
  document.addEventListener("touchstart", handler, true);
  document.addEventListener("touchend", handler, true);
  document.addEventListener("click", handler, true);
  document.addEventListener("keydown", handler, true);
  document.addEventListener("wheel", handler, { capture: true, passive: true });
  document.addEventListener("scroll", handler, { capture: true, passive: true });
  document.addEventListener("pointermove", handler, { capture: true, passive: true });
  document.addEventListener("touchmove", handler, { capture: true, passive: true });
}

// ---- 音乐律动：用 Web Audio 的频谱驱动广场圆片与光环 ----
let audioContext: AudioContext | null = null;
let mediaSource: MediaElementAudioSourceNode | null = null;
let analyser: AnalyserNode | null = null;
let gainNode: GainNode | null = null;
let rhythmFrame = 0;
let rhythmLevel = 0;
let rhythmBusy = false;
let rhythmGestureBound = false;

function prefersReducedMotion(): boolean {
  return (
    typeof window.matchMedia === "function" &&
    window.matchMedia("(prefers-reduced-motion: reduce)").matches
  );
}

function setBeat(value: number): void {
  const level = Math.max(0, Math.min(1, value));
  const root = document.documentElement;
  root.style.setProperty("--music-bass", level.toFixed(3));
  root.style.setProperty("--music-level", level.toFixed(3));
}

function stopRhythm(): void {
  if (rhythmFrame) {
    cancelAnimationFrame(rhythmFrame);
    rhythmFrame = 0;
  }
  rhythmLevel = 0;
  setBeat(0);
}

/** 播放时用 rAF 细粒度同步进度；暂停后停表，不让画面继续往前走。 */
function startProgressTicker(): void {
  if (progressFrame) return;
  lastProgressTick = 0;
  const tick = (now: number): void => {
    progressFrame = 0;
    if (!audio || !state.playing) return;
    if (now - lastProgressTick >= 100) {
      lastProgressTick = now;
      let changed = false;
      const time = audio.currentTime;
      if (Number.isFinite(time) && Math.abs(time - state.currentTime) >= 0.02) {
        state.currentTime = time;
        changed = true;
      }
      // 兜底同步时长：极少数情况下 loadedmetadata 会被跳过（切歌太快、媒体缓存），
      // 进度条的 max 会一直是 0，表现为「时间在走、滑块不动」。
      const duration = audio.duration;
      if (duration && Number.isFinite(duration) && Math.abs(duration - state.duration) > 0.01) {
        state.duration = duration;
        changed = true;
      }
      if (changed) notify();
    }
    progressFrame = requestAnimationFrame(tick);
  };
  progressFrame = requestAnimationFrame(tick);
}

function stopProgressTicker(): void {
  if (progressFrame) {
    cancelAnimationFrame(progressFrame);
    progressFrame = 0;
  }
  if (!audio) return;
  const time = audio.currentTime;
  if (Number.isFinite(time)) state.currentTime = time;
}

function startRhythm(): void {
  if (!analyser || rhythmFrame || prefersReducedMotion()) return;
  const bins = new Uint8Array(analyser.frequencyBinCount);
  let lastPaint = 0;
  const tick = (now: number): void => {
    if (!analyser || !state.playing) {
      rhythmFrame = 0;
      stopRhythm();
      return;
    }
    // 30fps 左右更新 CSS 变量就够了，别在 60fps 里反复改样式
    if (now - lastPaint >= 33) {
      lastPaint = now;
      analyser.getByteFrequencyData(bins);
      const count = Math.min(8, bins.length);
      let bass = 0;
      for (let index = 1; index < count; index += 1) bass += bins[index] ?? 0;
      bass /= Math.max(1, count - 1) * 255;
      const target = Math.min(1, bass * 1.8);
      // 起音快、回落慢，接近鼓点的视觉手感
      rhythmLevel += (target - rhythmLevel) * (target > rhythmLevel ? 0.55 : 0.12);
      setBeat(rhythmLevel);
    }
    rhythmFrame = requestAnimationFrame(tick);
  };
  rhythmFrame = requestAnimationFrame(tick);
}

async function setupRhythm(player: HTMLAudioElement): Promise<void> {
  // 这张图也负责音量增益：开了「减少动态效果」也要建，只是不启动动画。
  if (analyser || rhythmBusy) return;
  if (typeof window.AudioContext !== "function") return;
  rhythmBusy = true;
  try {
    if (!audioContext) audioContext = new window.AudioContext();
    if (audioContext.state !== "running") await audioContext.resume();
    if (audioContext.state !== "running" || analyser) return;
    mediaSource ??= audioContext.createMediaElementSource(player);
    gainNode ??= audioContext.createGain();
    analyser = audioContext.createAnalyser();
    analyser.fftSize = 64;
    analyser.smoothingTimeConstant = 0.8;
    gainNode.gain.value = effectiveVolume();
    // 接管后统一由 GainNode 控制音量，避免元素音量和增益叠两次。
    player.volume = 1;
    // 增益放在分析器前面：音量拧小，唱片和光环的律动也会跟着收敛。
    mediaSource.connect(gainNode);
    gainNode.connect(analyser);
    analyser.connect(audioContext.destination);
    if (state.playing) startRhythm();
  } catch {
    // 分析器不可用时保留普通音频播放，不影响听歌
  } finally {
    rhythmBusy = false;
  }
}

/** 自动播放允许但没有用户手势时 AudioContext 可能是 suspended；第一次点击再接管。 */
function bindRhythmGesture(): void {
  if (rhythmGestureBound) return;
  rhythmGestureBound = true;
  const wake = (): void => {
    if (!analyser && state.playing) void setupRhythm(ensureAudio());
  };
  document.addEventListener("pointerdown", wake, { capture: true });
  document.addEventListener("keydown", wake, { capture: true });
}

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

function effectiveVolume(): number {
  return state.muted ? 0 : state.volume;
}

/** 元素音量和 Web Audio 增益二选一，避免两边各乘一次。 */
function applyVolume(): void {
  if (audio) audio.volume = gainNode ? 1 : effectiveVolume();
  applyMuteFlag();
  if (gainNode) {
    const now = audioContext?.currentTime ?? 0;
    // 小步滑过去，拖滑块时不会「啪」一下
    gainNode.gain.setTargetAtTime(effectiveVolume(), now, 0.015);
  }
}

/** 用户静音或音量拖到 0 时，元素本身也静音；增益/音量另外照常保留。 */
function applyMuteFlag(): void {
  if (!audio) return;
  audio.muted = state.muted || state.volume === 0;
}

function persistVolume(): void {
  try {
    localStorage.setItem(VOLUME_KEY, state.volume.toFixed(2));
    localStorage.setItem(MUTED_KEY, state.muted ? "1" : "0");
  } catch {
    /* 存不下就只在本次生效 */
  }
}

/** 0～1。0.3 表示系统音量的 30%，网页读不到系统音量本身。 */
export function setMusicVolume(value: number): void {
  if (!Number.isFinite(value)) return;
  const volume = Math.max(0, Math.min(1, value));
  state.volume = volume;
  if (volume > 0) {
    state.muted = false;
    lastAudibleVolume = volume;
  }
  persistVolume();
  applyVolume();
  notify();
}

export function toggleMusicMute(): void {
  if (state.muted || state.volume === 0) {
    state.muted = false;
    state.volume = lastAudibleVolume > 0 ? lastAudibleVolume : DEFAULT_VOLUME;
  } else {
    state.muted = true;
  }
  persistVolume();
  applyVolume();
  notify();
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
  audio.volume = gainNode ? 1 : effectiveVolume();
  applyMuteFlag();
  bindRhythmGesture();
  audio.addEventListener("play", () => {
    disarmAutoplayUnlock();
    state.playing = true;
    notify();
    // 自动播放可能没有手势：先尝试接管，失败就等第一次点击
    void setupRhythm(audio!);
    if (analyser) startRhythm();
    startProgressTicker();
  });
  audio.addEventListener("pause", () => {
    state.playing = false;
    stopRhythm();
    // 暂停瞬间把实际时间落下来：否则进度条/歌词停在最后一次 timeupdate（最多慢 250ms）
    stopProgressTicker();
    notify();
  });
  audio.addEventListener("ended", () => {
    stopRhythm();
    stopProgressTicker();
    // 单曲循环会显得像卡住；有下一首就下一首，没有就从头再来
    if (state.tracks.length > 1) void next();
    else {
      audio!.currentTime = 0;
      state.currentTime = 0;
      void audio!.play().catch(() => {});
    }
  });
  audio.addEventListener("timeupdate", () => {
    state.currentTime = audio?.currentTime ?? 0;
    // 时长没对上时顺手补一次，避免播放器一直停在「0:00 / 0:00」
    const duration = audio?.duration;
    if (duration && Number.isFinite(duration) && Math.abs(duration - state.duration) > 0.01) {
      state.duration = duration;
    }
    notify();
  });
  audio.addEventListener("seeked", () => {
    state.currentTime = audio?.currentTime ?? state.currentTime;
    notify();
  });
  const syncDuration = (): void => {
    const duration = audio?.duration;
    if (duration && Number.isFinite(duration)) {
      state.duration = duration;
      notify();
    }
  };
  audio.addEventListener("loadedmetadata", syncDuration);
  audio.addEventListener("durationchange", syncDuration);
  audio.addEventListener("error", () => {
    if (!audio?.src) return;
    state.playing = false;
    stopRhythm();
    stopProgressTicker();
    state.message = "这首音频加载失败，换一首试试";
    notify();
  });
  return audio;
}

/**
 * 只同步后台配置的默认音量，不碰当前播放。
 *
 * 管理员保存设置后调用：歌单/API 没改时，音乐不该被打断；但「新访客默认音量」
 * 可能刚改过，本地还没存过音量的会话要跟着新值走。
 */
export function syncMusicDefaults(): void {
  syncDefaultVolume();
}

/** 首次点击前把歌单拉好，播放动作才不会被网络请求打断。 */
export async function preloadMusic(): Promise<void> {
  syncDefaultVolume();
  state.enabled = configured();
  if (!state.enabled) {
    notify();
    return;
  }
  await loadPlaylist();
}

/** 管理员改完音乐设置后调用：停掉旧音频、清空缓存，再按新配置拉一次。 */
export async function reloadMusic(): Promise<void> {
  disarmAutoplayUnlock();
  stopRhythm();
  stopProgressTicker();
  userPaused = false;
  if (audio) {
    audio.pause();
    audio.removeAttribute("src");
    audio.load();
  }
  applyMuteFlag();
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
  syncDefaultVolume();
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
    const controller = new AbortController();
    const timer = window.setTimeout(() => controller.abort(), 15000);
    try {
      const data = await request<MusicPlaylist>("/music/playlist", {
        signal: controller.signal,
      });
      state.tracks = data.tracks ?? [];
      state.message = data.message;
      state.loaded = true;
      if (state.tracks.length > 0) prepareTrack(initialIndex());
    } catch (err) {
      state.message = controller.signal.aborted
        ? "歌单读取超时（15 秒），请检查音乐 API 或备用源"
        : err instanceof Error
          ? err.message
          : "歌单加载失败";
      // 失败不写 loaded：下次点播放/打开歌单还能再试一次，
      // 不至于一次网络抖动就让整个会话停在空歌单。
      state.loaded = false;
    } finally {
      window.clearTimeout(timer);
      state.loading = false;
      loadingPromise = null;
      notify();
    }
  })();
  return loadingPromise;
}

function initialIndex(): number {
  if (state.mode === "shuffle" && state.tracks.length > 1) {
    return Math.floor(Math.random() * state.tracks.length);
  }
  return 0;
}

function prepareTrack(index: number): void {
  const track = state.tracks[index];
  if (!track) return;
  const player = ensureAudio();
  if (player.src !== new URL(track.audio_url, location.href).href) {
    player.src = track.audio_url;
  }
  // 重复点当前这首也要从头开始：只重置 state 而没动音频的话，
  // 进度和歌词会被下一次 timeupdate 拉回旧位置，看起来像「点了没反应」。
  try {
    player.currentTime = 0;
  } catch {
    /* 元数据还没到时忽略，下一首歌本来也会从 0 开始 */
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

async function startPlayback(silent = false): Promise<void> {
  const player = ensureAudio();
  if (!player.src) {
    const first = state.tracks[state.index];
    if (!first) return;
    prepareTrack(state.index);
  }
  try {
    await player.play();
  } catch {
    const now = Date.now();
    if (!silent && now - lastPlayErrorAt > 8000) {
      lastPlayErrorAt = now;
      toastError(new Error("播放没有开始，请再点一次"));
    }
  }
}

/** 顶部按钮与广场唱片都走它。 */
export async function toggleMusic(): Promise<void> {
  disarmAutoplayUnlock();
  state.enabled = configured();
  if (!state.enabled) return;
  if (!state.loaded) await loadPlaylist();
  if (state.tracks.length === 0) {
    openPanel("playlist");
    return;
  }
  const player = ensureAudio();
  if (player.paused) {
    userPaused = false;
    await startPlayback();
  } else {
    userPaused = true;
    player.pause();
  }
}

export function selectTrack(index: number): void {
  disarmAutoplayUnlock();
  if (index < 0 || index >= state.tracks.length) return;
  userPaused = false;
  prepareTrack(index);
  void startPlayback();
}

function randomIndex(): number {
  if (state.tracks.length <= 1) return 0;
  let next = state.index;
  while (next === state.index) next = Math.floor(Math.random() * state.tracks.length);
  return next;
}

export async function next(): Promise<void> {
  disarmAutoplayUnlock();
  userPaused = false;
  if (!state.loaded) await loadPlaylist();
  if (state.tracks.length === 0) return;
  if (state.mode === "shuffle") {
    prepareTrack(randomIndex());
    void startPlayback();
    return;
  }
  if (state.index + 1 < state.tracks.length) {
    prepareTrack(state.index + 1);
    void startPlayback();
    return;
  }
  if (state.mode === "list") {
    prepareTrack(0);
    void startPlayback();
    return;
  }
  // 顺序播放：到最后一首停下，保留在当前曲目
  const player = ensureAudio();
  player.pause();
  player.currentTime = 0;
  state.currentTime = 0;
  state.playing = false;
  notify();
}

export async function previous(): Promise<void> {
  disarmAutoplayUnlock();
  userPaused = false;
  if (!state.loaded) await loadPlaylist();
  if (state.tracks.length === 0) return;
  if (state.mode === "shuffle") {
    prepareTrack(randomIndex());
    void startPlayback();
    return;
  }
  if (state.index > 0) {
    prepareTrack(state.index - 1);
    void startPlayback();
    return;
  }
  if (state.mode === "list") {
    prepareTrack(state.tracks.length - 1);
    void startPlayback();
  }
}

export function seekMusic(seconds: number): void {
  const player = ensureAudio();
  if (!Number.isFinite(seconds)) return;
  const knownDuration =
    Number.isFinite(player.duration) && player.duration > 0
      ? player.duration
      : state.duration > 0
        ? state.duration
        : null;
  const target = Math.max(0, knownDuration ? Math.min(seconds, knownDuration) : seconds);
  try {
    player.currentTime = target;
    state.currentTime = target;
    notify();
  } catch {
    /* 元数据还没到时忽略 */
  }
}

/** 歌词行点击：跳到对应时间，暂停状态也直接开始播放。 */
export async function playFrom(seconds: number): Promise<void> {
  disarmAutoplayUnlock();
  userPaused = false;
  state.enabled = configured();
  if (!state.enabled) return;
  if (!state.loaded) await loadPlaylist();
  if (state.tracks.length === 0) return;
  const player = ensureAudio();
  if (!player.src) prepareTrack(state.index);
  if (player.readyState >= 1) {
    seekMusic(seconds);
  } else {
    player.addEventListener("loadedmetadata", () => seekMusic(seconds), { once: true });
  }
  if (player.paused) await startPlayback();
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

export function setMode(mode: PlaybackMode): void {
  state.mode = mode;
  try {
    localStorage.setItem(MODE_KEY, mode);
  } catch {
    /* 存不下就只在本次生效 */
  }
  notify();
}

/**
 * 首页自动播放：管理员开了才试。
 *
 * 浏览器允许时直接响；被自动播放策略拦下时，挂一次性的首次交互监听，
 * 用户第一次点击/触摸/按键后开始播放。用户自己按过暂停之后不再擅自恢复。
 */
export async function autoPlayIfConfigured(): Promise<void> {
  if (!configured() || !getState().site?.music?.autoplay) {
    disarmAutoplayUnlock();
    return;
  }
  if (userPaused) return;
  if (!state.loaded) await loadPlaylist();
  if (state.tracks.length === 0) return;
  if (state.playing) {
    disarmAutoplayUnlock();
    return;
  }
  const player = ensureAudio();
  if (!player.src) prepareTrack(initialIndex());
  try {
    await player.play();
    disarmAutoplayUnlock();
  } catch (err) {
    if (err instanceof DOMException && err.name === "NotAllowedError") {
      armAutoplayUnlock();
      if (!autoplayToastShown) {
        autoplayToastShown = true;
        toast("浏览器需要一次点击或触摸才能开始播放", { duration: 5000 });
      }
    }
  }
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
