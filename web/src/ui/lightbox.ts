/**
 * 预览浮层。
 *
 * 一套交互覆盖 PC 与移动：
 *   - PC：`←` `→` 切换、滚轮缩放、`Esc` 关闭、点缩略图跳转
 *   - 移动：左右滑切换、双指缩放、下拉关闭（下拉超过阈值才关，避免误触）
 *
 * 单独抽成模块是因为广场、相册网格、相册列表三处都要用它，
 * 而它自己有一堆事件与手势状态，塞进页面里会让页面难读。
 */

import { h, copyText } from "../core/dom";
import { formatBytes, formatDateTime, formatDimensions, middleEllipsis } from "../core/format";
import { toastWithCode } from "../core/toast";
import type { ImageItem } from "../core/types";
import { directUrl } from "../core/state";

export interface LightboxOptions {
  /** 未登录：下载与复制直链换成「登录后可用」 */
  anonymous?: boolean;
  /** 关闭后的回调（相册页用它恢复滚动位置） */
  onClose?: () => void;
  /** 从浮层删除当前图片（相册页提供） */
  onDelete?: (item: ImageItem) => void;
}

export interface LightboxHandle {
  close: () => void;
}

const MIN_SCALE = 1;
const MAX_SCALE = 5;
/** 下拉关闭的距离阈值（像素） */
const PULL_THRESHOLD = 90;

/**
 * 打开浮层。`items` 是当前筛选结果，`current` 是点开的那一张。
 */
export function openLightbox(
  items: ImageItem[],
  current: ImageItem,
  options: LightboxOptions = {},
): LightboxHandle {
  let index = Math.max(
    0,
    items.findIndex((item) => item.id === current.id),
  );
  let scale = 1;
  let offsetX = 0;
  let offsetY = 0;
  let dragStart: { x: number; y: number } | null = null;
  let swipeStart: { x: number; y: number } | null = null;
  let pinchStart: { distance: number; scale: number } | null = null;
  let pullDistance = 0;

  const image = h("img", { alt: "", draggable: false });
  const stage = h("div.lb-stage", null, image);
  const title = h("b");
  const counter = h("span.cnt");
  const hint = h("span", {
    text: window.matchMedia("(hover: none)").matches
      ? "左右滑切换 · 双指缩放 · 下拉关闭"
      : "← → 切换 · 滚轮缩放 · Esc 关闭",
    style: { color: "#94a3b8", fontSize: "12px" },
  });
  const side = h("div.lb-side");
  const thumbs = h("div.lb-thumbs");

  const prevButton = h("button.lb-nav", {
    type: "button",
    text: "‹",
    "aria-label": "上一张",
    on: { click: () => step(-1) },
  });
  const nextButton = h("button.lb-nav", {
    type: "button",
    text: "›",
    "aria-label": "下一张",
    on: { click: () => step(1) },
  });

  const closeButton = h("button.lb-x", {
    type: "button",
    text: "×",
    "aria-label": "关闭",
    on: { click: () => close() },
  });

  const root = h(
    "div.lb",
    null,
    h("div.lb-top", null, title, counter, h("span.spacer"), hint, closeButton),
    h("div.lb-main", null, h("div", { style: { display: "flex", gap: "12px", flex: "1" } }, prevButton, stage, nextButton), side),
    thumbs,
  );

  function currentItem(): ImageItem {
    return items[index] ?? current;
  }

  function applyTransform(): void {
    image.style.transform = `translate(${offsetX}px, ${offsetY + pullDistance}px) scale(${scale})`;
    image.style.cursor = scale > 1 ? "grab" : "default";
    stage.style.opacity = pullDistance > 0 ? String(1 - pullDistance / 400) : "1";
  }

  function step(delta: number): void {
    if (items.length === 0) return;
    index = (index + delta + items.length) % items.length;
    scale = 1;
    offsetX = 0;
    offsetY = 0;
    pullDistance = 0;
    paint();
  }

  function paint(): void {
    const item = currentItem();
    title.textContent = item.title ?? item.original_name ?? item.short_hash;
    counter.textContent = `${index + 1} / ${items.length}`;

    // 优先用缩略图当骨架，再换成原图；这样切换时不会整片空白
    image.src = item.url;
    image.alt = item.title ?? item.short_hash;

    side.replaceChildren(
      h("div", { style: { fontWeight: "700", fontSize: "13.5px", marginBottom: "8px" }, text: title.textContent ?? "" }),
      h(
        "div",
        { style: { color: "var(--muted)", lineHeight: "1.9" } },
        `尺寸 ${formatDimensions(item.width, item.height)}`,
        h("br"),
        `大小 ${formatBytes(item.size)}`,
        h("br"),
        `格式 ${item.ext.toUpperCase()}`,
        h("br"),
        `相册 ${item.album_name ?? "未分类"}`,
        h("br"),
        `上传 ${formatDateTime(item.upload_time)}`,
      ),
      h("div", { style: { margin: "10px 0 4px", color: "var(--muted)" }, text: "SHA-256" }),
      h("div.mono", { style: { fontSize: "10px", color: "var(--faint)", wordBreak: "break-all" }, text: item.sha256 }),
      actions(item),
    );

    thumbs.replaceChildren(
      ...items.map((item, itemIndex) =>
        h("button.ph", {
          type: "button",
          class: itemIndex === index ? "cur" : "",
          title: item.title ?? item.short_hash,
          on: {
            click: () => {
              index = itemIndex;
              scale = 1;
              offsetX = 0;
              offsetY = 0;
              paint();
            },
          },
        }, thumbnailFor(item)),
      ),
    );

    // 当前缩略图滚进可视区
    const active = thumbs.children[index] as HTMLElement | undefined;
    active?.scrollIntoView({ block: "nearest", inline: "center" });

    applyTransform();
  }

  function thumbnailFor(item: ImageItem): HTMLElement {
    const img = h("img", {
      src: item.thumb_url ?? item.url,
      alt: "",
      loading: "lazy",
      decoding: "async",
    });
    return img;
  }

  function actions(item: ImageItem): HTMLElement {
    if (options.anonymous) {
      // 未登录：这两个按钮换成提示，而不是让用户点了才发现没权限
      return h(
        "div",
        { style: { marginTop: "12px", display: "flex", gap: "6px" } },
        h("button.btn.sm", { type: "button", text: "登录后可用", disabled: true, style: { flex: "1" } }),
        h("button.btn.sm", { type: "button", text: "登录后可用", disabled: true, style: { flex: "1" } }),
      );
    }
    const copy = h("button.btn.sm.pri", {
      type: "button",
      text: "复制直链",
      style: { flex: "1" },
      on: {
        click: async () => {
          const ok = await copyText(item.url);
          if (ok) toastWithCode("✓ 已复制直链", item.url);
        },
      },
    });
    const download = h("a.btn.sm", {
      href: `${item.url}${item.url.includes("?") ? "&" : "?"}download=1`,
      download: "",
      text: "下载",
      style: { flex: "1" },
    });
    const row = h("div", { style: { marginTop: "12px", display: "flex", gap: "6px" } }, copy, download);
    if (options.onDelete) {
      row.append(
        h("button.btn.sm.dgr", {
          type: "button",
          text: "删除",
          on: {
            click: () => {
              const target = currentItem();
              close();
              options.onDelete?.(target);
            },
          },
        }),
      );
    }
    return row;
  }

  function close(): void {
    root.remove();
    document.body.style.overflow = "";
    document.removeEventListener("keydown", onKey);
    options.onClose?.();
  }

  function onKey(event: KeyboardEvent): void {
    if (event.key === "Escape") {
      event.stopPropagation();
      close();
    } else if (event.key === "ArrowLeft") {
      step(-1);
    } else if (event.key === "ArrowRight") {
      step(1);
    }
  }

  // 滚轮缩放：以指针位置为锚点，缩放时手指下的那一点不动
  stage.addEventListener(
    "wheel",
    (event) => {
      const wheel = event as WheelEvent;
      wheel.preventDefault();
      const next = Math.min(MAX_SCALE, Math.max(MIN_SCALE, scale * (wheel.deltaY < 0 ? 1.12 : 0.89)));
      if (next === MIN_SCALE) {
        offsetX = 0;
        offsetY = 0;
      }
      scale = next;
      applyTransform();
    },
    { passive: false },
  );

  // 指针事件同时覆盖鼠标拖拽与单指滑动
  stage.addEventListener("pointerdown", (event) => {
    const pointer = event as PointerEvent;
    image.setPointerCapture?.(pointer.pointerId);
    if (scale > 1) {
      dragStart = { x: pointer.clientX - offsetX, y: pointer.clientY - offsetY };
    } else {
      swipeStart = { x: pointer.clientX, y: pointer.clientY };
    }
  });

  stage.addEventListener("pointermove", (event) => {
    const pointer = event as PointerEvent;
    if (dragStart) {
      offsetX = pointer.clientX - dragStart.x;
      offsetY = pointer.clientY - dragStart.y;
      applyTransform();
      return;
    }
    if (swipeStart) {
      // 竖向为主时按「下拉关闭」处理，否则留给左右滑切换
      const dx = pointer.clientX - swipeStart.x;
      const dy = pointer.clientY - swipeStart.y;
      if (Math.abs(dy) > Math.abs(dx) && dy > 0) {
        pullDistance = dy;
        applyTransform();
      }
    }
  });

  stage.addEventListener("pointerup", (event) => {
    const pointer = event as PointerEvent;
    if (swipeStart) {
      const dx = pointer.clientX - swipeStart.x;
      const dy = pointer.clientY - swipeStart.y;
      if (pullDistance > PULL_THRESHOLD) {
        close();
        return;
      }
      if (Math.abs(dx) > 60 && Math.abs(dx) > Math.abs(dy)) {
        step(dx < 0 ? 1 : -1);
      }
    }
    dragStart = null;
    swipeStart = null;
    pullDistance = 0;
    applyTransform();
  });

  // 双指缩放
  stage.addEventListener("touchstart", (event) => {
    if (event.touches.length === 2) {
      pinchStart = { distance: touchDistance(event.touches), scale };
    }
  });
  stage.addEventListener("touchmove", (event) => {
    if (event.touches.length === 2 && pinchStart) {
      event.preventDefault();
      const ratio = touchDistance(event.touches) / pinchStart.distance;
      scale = Math.min(MAX_SCALE, Math.max(MIN_SCALE, pinchStart.scale * ratio));
      applyTransform();
    }
  });
  stage.addEventListener(
    "touchend",
    () => {
      pinchStart = null;
    },
    { passive: true },
  );

  // 双击放大/还原，桌面上也好用
  stage.addEventListener("dblclick", () => {
    if (scale > 1) {
      scale = 1;
      offsetX = 0;
      offsetY = 0;
    } else {
      scale = 2.5;
    }
    applyTransform();
  });

  document.addEventListener("keydown", onKey);
  document.body.style.overflow = "hidden";
  document.body.appendChild(root);
  paint();

  // 预取相邻图片，切换时不会先看到空白
  for (const delta of [1, -1]) {
    const neighbour = items[(index + delta + items.length) % items.length];
    if (neighbour) {
      const prefetch = new Image();
      prefetch.src = neighbour.url;
    }
  }

  void directUrl; // 保持与 state 模块的引用关系，便于将来在此处补签名 URL
  void middleEllipsis;

  return { close };
}

function touchDistance(touches: TouchList): number {
  const [a, b] = [touches[0], touches[1]];
  return Math.hypot(a.clientX - b.clientX, a.clientY - b.clientY);
}
