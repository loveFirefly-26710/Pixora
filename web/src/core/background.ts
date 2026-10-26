/**
 * 站点背景：单图、多图轮播，或调用本站随机图 API。
 *
 * 背景层是独立的 `position: fixed` 图层，不挂在 body 的 background 上：
 * 这样滚动时不会跟着长页面重绘，也不会被 body 的渐变盖住。切换用两层
 * <img> 做交叉淡入，换图不闪白。
 */

import { request } from "./api";
import type { SiteInfo } from "./types";

interface RandomImage {
  url?: string;
  thumb_url?: string;
}

let layer: HTMLDivElement | null = null;
let timer: number | null = null;
let runToken = 0;
let images: HTMLImageElement[] = [];
let front = 0;

export function applySiteBackground(site: SiteInfo): void {
  stop();
  const token = ++runToken;
  const root = document.documentElement;
  const urls = (site.backgrounds ?? []).filter(Boolean);
  if (urls.length === 0 && site.background) urls.push(site.background);
  const mode = site.background_mode ?? "single";
  // 随机图模式不需要预先上传背景：即使列表为空，也要让 /random 的图进来。
  if (urls.length === 0 && mode !== "random") {
    document.body.classList.remove("has-site-bg");
    return;
  }

  root.style.setProperty("--site-bg-opacity", String(site.background_opacity ?? 0.32));
  root.style.setProperty("--site-bg-blur", `${Math.max(0, site.background_blur ?? 0)}px`);
  document.body.classList.add("has-site-bg");

  layer = document.createElement("div");
  layer.className = "site-bg";
  layer.setAttribute("aria-hidden", "true");
  images = [document.createElement("img"), document.createElement("img")];
  for (const img of images) {
    img.alt = "";
    img.decoding = "async";
    layer.append(img);
  }
  document.body.prepend(layer);

  const interval = Math.max(3, site.background_interval ?? 12) * 1000;

  if (mode === "single") {
    display(urls[0]);
    return;
  }

  if (mode === "slideshow") {
    display(urls[0]);
    if (urls.length > 1) {
      timer = window.setInterval(() => {
        if (document.visibilityState !== "visible" || token !== runToken) return;
        const index = (Math.floor(Date.now() / interval) % urls.length + urls.length) % urls.length;
        display(urls[index]);
      }, interval);
    }
    return;
  }

  // random：先取一批本站随机图；接口失败时退回已上传的背景图，不至于白屏。
  let random: string[] = [];
  let cursor = 0;
  void (async () => {
    random = await fetchRandom(urls);
    if (token !== runToken) return;
    if (random.length === 0) random = urls;
    display(random[0]);
    if (random.length <= 1) return;
    timer = window.setInterval(() => {
      if (document.visibilityState !== "visible" || token !== runToken) return;
      cursor = (cursor + 1) % random.length;
      if (cursor === 0) {
        void fetchRandom(urls).then((next) => {
          if (token === runToken && next.length > 0) random = next;
        });
      }
      display(random[cursor]);
    }, interval);
  })();
}

export function stop(): void {
  runToken += 1;
  if (timer !== null) {
    window.clearInterval(timer);
    timer = null;
  }
  layer?.remove();
  layer = null;
  images = [];
  front = 0;
}

function display(url: string): void {
  if (!layer || !url) return;
  const nextIndex = images.length === 1 ? 0 : 1 - front;
  const next = images[nextIndex];
  const prev = images[front];
  if (!next) return;
  const show = () => {
    next.classList.add("on");
    if (prev && prev !== next) prev.classList.remove("on");
    front = nextIndex;
  };
  if (next.src === url && next.complete) {
    show();
    return;
  }
  next.onload = show;
  next.onerror = () => {
    next.classList.remove("on");
  };
  next.src = url;
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
