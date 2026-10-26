/**
 * 预览浮层。
 *
 * 一套交互覆盖 PC 与移动：
 *   - PC：`←` `→` 切换、滚轮缩放（以指针为锚点）、`Esc` 关闭、点缩略图跳转
 *   - 移动：左右滑切换、双指缩放、下拉关闭（下拉超过阈值才关，避免误触）
 *
 * 单独抽成模块是因为广场、相册网格、相册列表三处都要用它，
 * 而它自己有一堆事件与手势状态，塞进页面里会让页面难读。
 */

import { h, copyText } from "../core/dom";
import { formatBytes, formatDateTime, formatDimensions } from "../core/format";
import { toastWithCode } from "../core/toast";
import type { ImageItem } from "../core/types";

export interface LightboxOptions {
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
 * 拖到边时至少留多少像素的图像还在舞台里。
 *
 * 只防「拖没了」：再往里拖就到头了，不会出现一张图被拖出视野、只剩黑底的情况。
 */
const KEEP_VISIBLE = 140;

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
  // 缩放百分比只在放大/缩小时显示（见 applyTransform）
  const zoomBadge = h("div.lb-zoom", { hidden: true });
  const stage = h("div.lb-stage", null, image, zoomBadge);
  const title = h("b");
  const counter = h("span.cnt");
  const hint = h("span.lb-hint", {
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

  /**
   * 夹住偏移：只要保证「还有一块图像留在舞台里」，别把它拖没了。
   *
   * 早先按「内容是否超出舞台」逐轴夹紧，结果是竖图放大后宽度仍小于舞台宽度，
   * 横向完全拖不动——用户看到的就是「只能上下拖」。现在两个方向一律可拖，
   * 边界只用来防止拖到什么都看不见。
   *
   * 用布局尺寸（`offsetWidth`，不受 transform 影响）而不是 `getBoundingClientRect`：
   * 后者已经把缩放算进去了，拿它算边界会算错。
   */
  function clampOffsets(): void {
    const box = stage.getBoundingClientRect();
    const maxX = Math.max(0, (image.offsetWidth * scale + box.width) / 2 - KEEP_VISIBLE);
    const maxY = Math.max(0, (image.offsetHeight * scale + box.height) / 2 - KEEP_VISIBLE);
    offsetX = Math.min(maxX, Math.max(-maxX, offsetX));
    offsetY = Math.min(maxY, Math.max(-maxY, offsetY));
  }

  function applyTransform(): void {
    clampOffsets();
    image.style.transform = `translate(${offsetX}px, ${offsetY + pullDistance}px) scale(${scale})`;
    image.style.cursor = scale > 1 ? "grab" : "default";
    stage.style.opacity = pullDistance > 0 ? String(1 - pullDistance / 400) : "1";
    zoomBadge.textContent = `${Math.round(scale * 100)}%`;
    zoomBadge.hidden = Math.abs(scale - 1) < 0.01;
  }

  /** 以某个视口坐标为锚点缩放：缩放前后，指针下的那一点停在原地。 */
  function zoomAt(nextScale: number, clientX: number, clientY: number): void {
    const next = Math.min(MAX_SCALE, Math.max(MIN_SCALE, nextScale));
    if (Math.abs(next - scale) < 0.0001) return;
    const box = stage.getBoundingClientRect();
    // 相对图片中心的坐标（transform-origin 默认是中心）
    const cx = clientX - box.left - box.width / 2;
    const cy = clientY - box.top - box.height / 2;
    const ratio = next / scale;
    offsetX = cx - (cx - offsetX) * ratio;
    offsetY = cy - (cy - offsetY) * ratio;
    scale = next;
    if (scale === MIN_SCALE) {
      offsetX = 0;
      offsetY = 0;
    }
    applyTransform();
  }

  function step(delta: number): void {
    if (items.length === 0) return;
    index = (index + delta + items.length) % items.length;
    scale = 1;
    offsetX = 0;
    offsetY = 0;
    pullDistance = 0;
    paint(delta);
  }

  function paint(slide = 0): void {
    const item = currentItem();
    title.textContent = item.title ?? item.original_name ?? item.short_hash;
    counter.textContent = `${index + 1} / ${items.length}`;

    const previous = image.getAttribute("src");
    image.src = item.url;
    image.alt = item.title ?? item.short_hash;
    // 切图时轻微横移 + 淡入；不碰缩放/拖动用的 inline transform 结果
    if (
      slide !== 0 &&
      previous &&
      !window.matchMedia("(prefers-reduced-motion: reduce)").matches &&
      typeof image.animate === "function"
    ) {
      image.animate(
        [
          { opacity: 0, transform: `translateX(${slide > 0 ? 28 : -28}px) scale(0.985)` },
          { opacity: 1, transform: "translateX(0) scale(1)" },
        ],
        { duration: 340, easing: "cubic-bezier(.22,1,.36,1)" },
      );
    }

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
        `相册 ${item.album_name ?? "默认相册"}`,
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
    // `draggable: false`：长按再拖时浏览器会给缩略图起一个原生拖拽幽灵，
    // 手指/光标明明在放大图上，飘起来的却是这张小图——浮层里所有图都不能拖
    return h("img", {
      src: item.thumb_url ?? item.url,
      alt: "",
      loading: "lazy",
      decoding: "async",
      draggable: false,
    });
  }

  /**
   * 复制直链与下载。
   *
   * 未登录也能用：直链本身按「有没有公开引用」判权限（私有图只对本人放行），
   * 不需要浏览器替他再挡一道——真正要节流的是服务端的下载频率限制。
   */
  function actions(item: ImageItem): HTMLElement {
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
    window.removeEventListener("hashchange", onHashChange);
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

  // 页面换了地方（点了导航、地址栏变了）就把浮层收掉：它是 `position: fixed`
  // 挂在 body 上的，不跟着路由走，留着就会盖住下一个页面
  function onHashChange(): void {
    close();
  }

  // 原图是异步加载的：加载完再校正一次，否则「按布局尺寸算的拖动范围」
  // 用的还是空图的 0×0
  image.addEventListener("load", () => applyTransform());

  // 滚轮缩放以指针为锚点：不锚的话放大后指针下那块内容会跑掉，
  // 接着拖的时候手感就是「拖的不是我看到的那一块」
  stage.addEventListener(
    "wheel",
    (event) => {
      const wheel = event as WheelEvent;
      wheel.preventDefault();
      zoomAt(scale * (wheel.deltaY < 0 ? 1.12 : 0.89), wheel.clientX, wheel.clientY);
    },
    { passive: false },
  );

  // 指针事件同时覆盖鼠标拖拽与单指滑动。
  //
  // 三条约束，缺一条就会出现「按住左键拖不动、松开以后图反而跟着光标走」：
  //   1. **捕获放在 stage 上**：放在 img 上时，指针划出图片边界后事件就断了；
  //   2. **只认左键，并且移动时必须还按着**：指针事件在没按键时也会持续上报，
  //      少了这个判断，一次拖拽之后光标随便动一下图都会跟着跑；
  //   3. **拖动期间关掉过渡动画**（见 `.dragging` 那条 CSS）：img 上有
  //      `transition: transform .12s`，每次都慢半拍追光标，手感就是「拖不动」。
  stage.addEventListener("pointerdown", (event) => {
    const pointer = event as PointerEvent;
    // 鼠标只认左键；触摸与触控笔没有「键」的概念，照常放行
    if (pointer.pointerType === "mouse" && pointer.button !== 0) return;
    if (scale > 1) {
      stage.setPointerCapture?.(pointer.pointerId);
      dragStart = { x: pointer.clientX - offsetX, y: pointer.clientY - offsetY };
      stage.classList.add("dragging");
    } else {
      swipeStart = { x: pointer.clientX, y: pointer.clientY };
    }
  });

  stage.addEventListener("pointermove", (event) => {
    const pointer = event as PointerEvent;
    if (dragStart) {
      // 松开之后不再跟手：有时 pointerup 被系统手势吃掉，靠这里兜底
      if (pointer.pointerType === "mouse" && (pointer.buttons & 1) === 0) {
        endDrag();
        return;
      }
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
    endDrag();
    swipeStart = null;
    pullDistance = 0;
    applyTransform();
  });

  /** 收尾一次拖拽：清掉状态与 `.dragging`，动画恢复。 */
  function endDrag(): void {
    dragStart = null;
    stage.classList.remove("dragging");
  }

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
      const [a, b] = [event.touches[0], event.touches[1]];
      zoomAt(pinchStart.scale * ratio, (a.clientX + b.clientX) / 2, (a.clientY + b.clientY) / 2);
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
  stage.addEventListener("dblclick", (event) => {
    const mouse = event as MouseEvent;
    if (scale > 1) {
      scale = 1;
      offsetX = 0;
      offsetY = 0;
      applyTransform();
      return;
    }
    zoomAt(2.5, mouse.clientX, mouse.clientY);
  });

  document.addEventListener("keydown", onKey);
  window.addEventListener("hashchange", onHashChange);
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

  return { close };
}

function touchDistance(touches: TouchList): number {
  const [a, b] = [touches[0], touches[1]];
  return Math.hypot(a.clientX - b.clientX, a.clientY - b.clientY);
}
