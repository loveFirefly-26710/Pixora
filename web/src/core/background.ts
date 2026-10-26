/**
 * 站点背景：单图、多图轮播，或调用本站随机图 API。
 *
 * 背景层是独立的 `position: fixed` 图层，不挂在 body 的 background 上：
 * 这样滚动时不会跟着长页面重绘，也不会被 body 的渐变盖住。切换用两层
 * <img> 做交叉淡入，换图不闪白。
 *
 * 首张图没就绪前，main.ts 不会挂载外壳（等 `backgroundReady`）：无痕模式或
 * 首次访问时 /site 和壁纸都比 HTML 慢，先画一套没有背景的界面、过一会再把
 * 壁纸贴上来，看起来就像页面坏了一下又自己好了。
 */

import { request } from "./api";
import type { SiteInfo } from "./types";

interface RandomImage {
  url?: string;
  thumb_url?: string;
}

type BackgroundMode = "single" | "slideshow" | "random";

interface BackgroundRun {
  signature: string;
  layer: HTMLDivElement;
  images: HTMLImageElement[];
  front: number;
  timer: number | null;
}

let current: BackgroundRun | null = null;
let readyPromise: Promise<void> = Promise.resolve();
let readyResolve: (() => void) | null = null;

/** 首张壁纸已经进入背景层。没有配置背景时立即兑现。 */
export function backgroundReady(): Promise<void> {
  return readyPromise;
}

function prepareReady(): void {
  settleReady();
  readyPromise = new Promise<void>((resolve) => {
    readyResolve = resolve;
  });
}

function settleReady(): void {
  const resolve = readyResolve;
  readyResolve = null;
  resolve?.();
}

export function applySiteBackground(site: SiteInfo): void {
  const root = document.documentElement;
  const urls = (site.backgrounds ?? []).filter(Boolean);
  if (urls.length === 0 && site.background) urls.push(site.background);
  const mode: BackgroundMode = site.background_mode ?? "single";
  // 随机图模式不需要预先上传背景：即使列表为空，也要让 /random 的图进来。
  if (urls.length === 0 && mode !== "random") {
    stop();
    document.body.classList.remove("has-site-bg");
    return;
  }

  const opacity = site.background_opacity ?? 0.32;
  const blur = Math.max(0, site.background_blur ?? 0);
  const intervalSeconds = Math.max(3, site.background_interval ?? 12);
  root.style.setProperty("--site-bg-opacity", String(opacity));
  root.style.setProperty("--site-bg-blur", `${blur}px`);

  // 登出、上传额度刷新都会重拉 /site。配置没变就别拆掉正在显示的背景层：
  // 拆了再建会让壁纸消失一瞬，还会平白重新下载同一个文件。
  const signature = `${mode}|${intervalSeconds}|${urls.join("\n")}`;
  if (current && current.signature === signature) return;

  stop();
  prepareReady();
  document.body.classList.add("has-site-bg");

  const layer = document.createElement("div");
  layer.className = "site-bg initial";
  layer.setAttribute("aria-hidden", "true");
  const images = [document.createElement("img"), document.createElement("img")];
  for (const img of images) {
    img.alt = "";
    img.decoding = "async";
    layer.append(img);
  }
  document.body.prepend(layer);

  const run: BackgroundRun = { signature, layer, images, front: 0, timer: null };
  current = run;

  void start(run, urls, mode, intervalSeconds * 1000);
}

async function start(
  run: BackgroundRun,
  urls: string[],
  mode: BackgroundMode,
  interval: number,
): Promise<void> {
  if (mode === "single" || mode === "slideshow") {
    // 列表里可能混着已经失效的地址（换过存储、删过图都会留下旧 URL）。
    // 首图失败必须立刻试下一张，不能等一个轮播周期——用户看到的就是
    // “先纯色背景，过十秒才终于出现壁纸”。
    const initial = await showFirstAvailable(run, urls);
    markInitial(run);
    if (mode === "slideshow" && urls.length > 1) {
      let cursor = initial ?? 0;
      run.timer = window.setInterval(() => {
        if (document.visibilityState !== "visible" || current !== run) return;
        cursor = (cursor + 1) % urls.length;
        void display(run, urls[cursor]);
      }, interval);
    }
    return;
  }

  // random：先取一批本站随机图；接口失败时退回已上传的背景图，不至于白屏。
  // 有上传图就先挂上，别让首屏陪着 /random 一起等。
  let random: string[] = [];
  let shown = false;
  let cursor = 0;
  if (urls.length > 0) {
    const initial = await showFirstAvailable(run, urls);
    shown = initial !== null;
    if (initial !== null) cursor = initial;
    markInitial(run);
  }
  random = await fetchRandom(urls);
  if (current !== run) return;
  if (random.length === 0) random = urls;
  if (!shown) {
    const initial = await showFirstAvailable(run, random);
    if (initial !== null) cursor = initial;
    markInitial(run);
  } else if (random.length > 0) {
    // 首图已经显示，随机图加载好再交叉淡入；不要把外壳重新扣住
    void showFirstAvailable(run, random).then((index) => {
      if (current === run && index !== null) cursor = index;
    });
  }
  if (random.length <= 1) return;

  run.timer = window.setInterval(() => {
    if (document.visibilityState !== "visible" || current !== run) return;
    cursor = (cursor + 1) % random.length;
    if (cursor === 0) {
      void fetchRandom(urls).then((next) => {
        if (current === run && next.length > 0) random = next;
      });
    }
    void display(run, random[cursor]);
  }, interval);
}

/**
 * 依次尝试候选地址，返回第一张成功显示的下标；全失败返回 null。
 *
 * 首屏每次尝试只给 5 秒：坏的资源通常立刻 404，真遇到挂起的地址也不能
 * 让用户对着纯色背景等满一个轮播周期。轮播阶段的换图仍走 15 秒超时。
 */
async function showFirstAvailable(run: BackgroundRun, candidates: string[]): Promise<number | null> {
  const limit = Math.min(candidates.length, 4);
  for (let index = 0; index < limit; index += 1) {
    if (current !== run) return null;
    if (await display(run, candidates[index], 5000)) return index;
  }
  return null;
}

/** 首图已经挂上：放行外壳，并结束“不做淡入”的首屏状态。 */
function markInitial(run: BackgroundRun): void {
  if (current !== run) return;
  settleReady();
  // 1.15s 淡入是给轮播换图用的；首图若等它淡完，用户看到的仍然是空背景。
  // 首帧画完后恢复正常过渡；隐藏标签页里 rAF 可能不跑，用定时器兜底。
  window.requestAnimationFrame(() => {
    window.requestAnimationFrame(() => {
      if (current === run) run.layer.classList.remove("initial");
    });
  });
  window.setTimeout(() => {
    if (current === run) run.layer.classList.remove("initial");
  }, 250);
}

export function stop(): void {
  const run = current;
  current = null;
  if (run && run.timer !== null) window.clearInterval(run.timer);
  run?.layer.remove();
  // 旧的一轮等待既不能永远悬着，也不能把下一轮的首图当成已就绪
  settleReady();
}

function display(run: BackgroundRun, url: string, timeoutMs = 15_000): Promise<boolean> {
  if (current !== run || !url) return Promise.resolve(false);
  const nextIndex = 1 - run.front;
  const next = run.images[nextIndex];
  const prev = run.images[run.front];
  if (!next) return Promise.resolve(false);

  const show = (): void => {
    next.classList.add("on");
    if (prev && prev !== next) prev.classList.remove("on");
    run.front = nextIndex;
  };

  if (next.dataset.bgUrl === url && next.complete && next.naturalWidth > 0) {
    show();
    return Promise.resolve(true);
  }

  return new Promise<boolean>((resolve) => {
    let settled = false;
    let timeout = 0;
    const finish = (ok: boolean): void => {
      if (settled) return;
      settled = true;
      window.clearTimeout(timeout);
      next.onload = null;
      next.onerror = null;
      if (!ok) {
        next.classList.remove("on");
        resolve(false);
        return;
      }
      show();
      resolve(true);
    };
    // 坏图或卡死的图不能把首屏永久扣住；main.ts 那边还有更短的兜底
    timeout = window.setTimeout(() => finish(false), timeoutMs);
    next.dataset.bgUrl = url;
    next.onload = () => {
      // decode 完成前就打开 opacity，部分浏览器会先画一帧空图
      if (typeof next.decode === "function") {
        void next.decode().then(
          () => finish(true),
          () => finish(true),
        );
      } else {
        finish(true);
      }
    };
    next.onerror = () => finish(false);
    next.src = url;
  });
}

async function fetchRandom(fallback: string[]): Promise<string[]> {
  try {
    const list = await request<RandomImage[]>("/random", {
      query: {
        format: "json",
        count: 16,
        public_only: 1,
      },
    });
    const urls = (Array.isArray(list) ? list : [list])
      .map((item) => item.url || item.thumb_url || "")
      .filter(Boolean);
    return urls.length > 0 ? urls : fallback;
  } catch {
    return fallback;
  }
}
