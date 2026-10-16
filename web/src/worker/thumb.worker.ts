/**
 * 缩略图 Worker：createImageBitmap + OffscreenCanvas → WebP。
 *
 * 生成失败不抛异常给队列，而是回「没有缩略图 + 原因」：
 * 缩略图是锦上添花，缺了照样能上传（列表页用原图缩放显示，服务端也可能开兜底队列），
 * 但用户需要知道为什么少了这一张——UI 会把原因展示在对应项上。
 */

declare const self: DedicatedWorkerGlobalScope;

/**
 * 最长边 1024px。
 *
 * 400px 那会儿是「够列表用」，结果是宽屏下列表一格 500–800 CSS px、
 * 2 倍屏再翻一倍——缩略图被放大两三倍去填，预览看着就是糊的（点开原图才清晰）。
 * 1024 覆盖得住网格里的实际显示尺寸，代价是每张多几十到一百多 KB，
 * 相对原图那几个 MB 还是零头。
 */
const MAX_EDGE = 1024;
const WEBP_QUALITY = 0.82;

export interface ThumbRequest {
  type: "thumb";
  id: string;
  file: File;
  maxEdge?: number;
}

export type ThumbResponse =
  | { type: "done"; id: string; blob: Blob | null; width: number; height: number; reason: string | null }
  | { type: "error"; id: string; message: string };

/** 缩略图目标尺寸：只缩不放，避免把小图糊成 1024px。纯函数，单测用。 */
export function thumbSize(
  width: number,
  height: number,
  maxEdge = MAX_EDGE,
): { width: number; height: number } {
  if (width <= 0 || height <= 0) return { width: 0, height: 0 };
  const scale = Math.min(1, maxEdge / Math.max(width, height));
  return { width: Math.max(1, Math.round(width * scale)), height: Math.max(1, Math.round(height * scale)) };
}

function extOf(name: string): string {
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot + 1).toUpperCase() : "该格式";
}

export async function makeThumbnail(
  file: File,
  maxEdge = MAX_EDGE,
): Promise<{ blob: Blob | null; width: number; height: number; reason: string | null }> {
  if (typeof OffscreenCanvas !== "function" || typeof createImageBitmap !== "function") {
    return { blob: null, width: 0, height: 0, reason: "当前浏览器不支持在 Worker 内生成缩略图" };
  }

  let bitmap: ImageBitmap;
  try {
    // from-image：按 EXIF 方向摆正，否则手机竖拍的照片缩略图会躺着
    bitmap = await createImageBitmap(file, { imageOrientation: "from-image" });
  } catch {
    return {
      blob: null,
      width: 0,
      height: 0,
      reason: `${extOf(file.name)} 浏览器无法解码（HEIC/TIFF 常见），本次不带缩略图`,
    };
  }

  try {
    const size = thumbSize(bitmap.width, bitmap.height, maxEdge);
    if (size.width === 0) {
      bitmap.close();
      return { blob: null, width: 0, height: 0, reason: "图片尺寸无法识别" };
    }
    const canvas = new OffscreenCanvas(size.width, size.height);
    const ctx = canvas.getContext("2d");
    if (!ctx) {
      bitmap.close();
      return { blob: null, width: 0, height: 0, reason: "画布不可用" };
    }
    ctx.drawImage(bitmap, 0, 0, size.width, size.height);
    bitmap.close();

    if (typeof canvas.convertToBlob === "function") {
      const blob = await canvas.convertToBlob({ type: "image/webp", quality: WEBP_QUALITY });
      if (blob.type === "image/webp") {
        return { blob, width: size.width, height: size.height, reason: null };
      }
      // 浏览器不支持 WebP 编码时会回落到 PNG。契约上 thumb 必须是 WebP，
      // 与其发一张名不副实的 PNG，不如不发，让服务端按「没有缩略图」处理。
      return { blob: null, width: 0, height: 0, reason: "浏览器不支持 WebP 编码，本次不带缩略图" };
    }
    return { blob: null, width: 0, height: 0, reason: "浏览器不支持 OffscreenCanvas 导出图片" };
  } catch (err) {
    try {
      bitmap.close();
    } catch {
      /* 已经关过或已释放 */
    }
    return {
      blob: null,
      width: 0,
      height: 0,
      reason: err instanceof Error ? err.message : "缩略图生成失败",
    };
  }
}

self.addEventListener("message", (event: MessageEvent<ThumbRequest>) => {
  const task = event.data;
  if (!task || task.type !== "thumb") return;
  void (async () => {
    try {
      const result = await makeThumbnail(task.file, task.maxEdge ?? MAX_EDGE);
      const response: ThumbResponse = { type: "done", id: task.id, ...result };
      self.postMessage(response);
    } catch (err) {
      const response: ThumbResponse = {
        type: "error",
        id: task.id,
        message: err instanceof Error ? err.message : "缩略图生成失败",
      };
      self.postMessage(response);
    }
  })();
});
