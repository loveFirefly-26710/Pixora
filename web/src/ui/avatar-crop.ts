/**
 * 头像裁剪弹窗。
 *
 * 用户拖动图片、滚轮/滑杆缩放，圆形遮罩里的部分就是最终头像。输出 512×512
 * 的方形 JPEG：页面里用 CSS 裁圆，但用户选中的可见区域已经被摆正。
 */

import { h } from "../core/dom";
import { openModal } from "../core/modal";

const STAGE = 320;

interface CropState {
  bitmap: ImageBitmap | HTMLImageElement;
  width: number;
  height: number;
  scale: number;
  offsetX: number;
  offsetY: number;
}

export async function openAvatarCrop(file: File, outputSize = 512): Promise<Blob | null> {
  const state = await loadImage(file);
  const canvas = document.createElement("canvas");
  canvas.className = "avatar-crop-canvas";
  canvas.width = STAGE;
  canvas.height = STAGE;
  const context = canvas.getContext("2d");
  if (!context) return null;

  const draw = (): void => {
    context.clearRect(0, 0, STAGE, STAGE);
    const { bitmap, width, height, scale, offsetX, offsetY } = state;
    context.save();
    context.translate(STAGE / 2 + offsetX, STAGE / 2 + offsetY);
    context.scale(scale, scale);
    context.drawImage(bitmap as CanvasImageSource, -width / 2, -height / 2, width, height);
    context.restore();
  };

  const clamp = (): void => {
    const maxX = Math.max(0, (state.width * state.scale - STAGE) / 2);
    const maxY = Math.max(0, (state.height * state.scale - STAGE) / 2);
    state.offsetX = Math.min(maxX, Math.max(-maxX, state.offsetX));
    state.offsetY = Math.min(maxY, Math.max(-maxY, state.offsetY));
  };

  let dragging = false;
  let lastX = 0;
  let lastY = 0;
  const move = (event: PointerEvent): void => {
    if (!dragging) return;
    event.preventDefault();
    // 画布内部固定 320px，CSS 可能按屏幕缩过；把指针位移换算回画布坐标
    const factor = STAGE / Math.max(1, canvas.clientWidth);
    state.offsetX += (event.clientX - lastX) * factor;
    state.offsetY += (event.clientY - lastY) * factor;
    lastX = event.clientX;
    lastY = event.clientY;
    clamp();
    draw();
  };
  canvas.addEventListener("pointerdown", (event) => {
    dragging = true;
    lastX = event.clientX;
    lastY = event.clientY;
    canvas.setPointerCapture(event.pointerId);
  });
  canvas.addEventListener("pointermove", move);
  canvas.addEventListener("pointerup", () => {
    dragging = false;
  });
  canvas.addEventListener("pointercancel", () => {
    dragging = false;
  });
  canvas.addEventListener(
    "wheel",
    (event) => {
      event.preventDefault();
      const factor = event.deltaY < 0 ? 1.08 : 0.92;
      state.scale = Math.min(8, Math.max(state.scale * factor, baseScale(state)));
      clamp();
      draw();
    },
    { passive: false },
  );

  const zoom = h("input", {
    type: "range",
    min: "1",
    max: "4",
    step: "0.01",
    value: "1",
    "aria-label": "缩放头像",
  });
  zoom.addEventListener("input", () => {
    state.scale = baseScale(state) * Number(zoom.value);
    clamp();
    draw();
  });

  draw();

  return new Promise<Blob | null>((resolve) => {
    let settled = false;
    const finish = (blob: Blob | null): void => {
      if (settled) return;
      settled = true;
      handle.close();
      resolve(blob);
    };
    const handle = openModal({
      title: "调整头像",
      body: [
        h("div.avatar-crop-stage", null, h("div.avatar-crop-frame", null, canvas)),
        h("div.tip", { text: "拖动图片调整位置，滚轮或滑杆缩放；圆形遮罩内的部分会作为头像。" }),
        h("div.avatar-crop-zoom", null, h("span", { text: "缩放" }), zoom),
      ],
      footer: [
        h("button.btn", { type: "button", text: "取消", on: { click: () => finish(null) } }),
        h("button.btn.pri", {
          type: "button",
          text: "使用这张",
          on: {
            click: () => {
              const output = document.createElement("canvas");
              output.width = outputSize;
              output.height = outputSize;
              const out = output.getContext("2d");
              if (!out) {
                finish(null);
                return;
              }
              const ratio = outputSize / STAGE;
              out.fillStyle = "#000";
              out.fillRect(0, 0, outputSize, outputSize);
              out.save();
              out.translate(
                outputSize / 2 + state.offsetX * ratio,
                outputSize / 2 + state.offsetY * ratio,
              );
              out.scale(state.scale * ratio, state.scale * ratio);
              out.drawImage(
                state.bitmap as CanvasImageSource,
                -state.width / 2,
                -state.height / 2,
                state.width,
                state.height,
              );
              out.restore();
              output.toBlob((blob) => finish(blob), "image/jpeg", 0.9);
            },
          },
        }),
      ],
      onClose: () => {
        if (!settled) {
          settled = true;
          resolve(null);
        }
      },
    });
  });
}

function baseScale(state: CropState): number {
  return Math.max(STAGE / state.width, STAGE / state.height);
}

async function loadImage(file: File): Promise<CropState> {
  const bitmap: ImageBitmap | HTMLImageElement =
    typeof createImageBitmap === "function"
      ? await createImageBitmap(file)
      : await new Promise<HTMLImageElement>((resolve, reject) => {
          const image = new Image();
          const url = URL.createObjectURL(file);
          image.onload = () => resolve(image);
          image.onerror = () => reject(new Error("这张图读不出来，换一张试试"));
          image.src = url;
        });
  const width = bitmap instanceof HTMLImageElement ? bitmap.naturalWidth : bitmap.width;
  const height = bitmap instanceof HTMLImageElement ? bitmap.naturalHeight : bitmap.height;
  if (width <= 0 || height <= 0) throw new Error("这张图读不出来，换一张试试");
  const state: CropState = {
    bitmap,
    width,
    height,
    scale: baseScale({ bitmap, width, height, scale: 1, offsetX: 0, offsetY: 0 }),
    offsetX: 0,
    offsetY: 0,
  };
  return state;
}
