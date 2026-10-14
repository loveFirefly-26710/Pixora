/** 轻提示。右下角堆叠，几秒后自动消失。 */

import { ApiError } from "./api";
import { h } from "./dom";

let host: HTMLElement | null = null;

function ensureHost(): HTMLElement {
  if (!host) {
    host = h("div.toast-host", { id: "toast-host" });
    document.body.appendChild(host);
  }
  return host;
}

export interface ToastOptions {
  /** 毫秒；0 表示不自动消失 */
  duration?: number;
  /** 需要额外强调的错误 */
  danger?: boolean;
  /** 允许点击复制的文本（例如直链） */
  copy?: string;
}

function push(content: Node | string, options: ToastOptions): void {
  const node = h("div.toast", { class: options.danger ? "err" : "ok" });
  node.append(content);
  const container = ensureHost();
  container.appendChild(node);
  const duration = options.duration ?? 2600;
  if (duration > 0) {
    window.setTimeout(() => {
      node.style.opacity = "0";
      node.style.transition = "opacity .2s";
      window.setTimeout(() => node.remove(), 220);
    }, duration);
  }
}

export function toast(message: string, options: ToastOptions = {}): void {
  push(document.createTextNode(message), options);
}

/** 带可复制片段的通知，例如「✓ 已复制直链 <code>…</code>」。 */
export function toastWithCode(prefix: string, code: string): void {
  const node = h("span", null, prefix, " ", h("code", { text: code }));
  push(node, { duration: 3200 });
}

export function toastError(err: unknown): void {
  const message = err instanceof ApiError ? err.message : err instanceof Error ? err.message : String(err);
  push(document.createTextNode(message), { danger: true, duration: 3600 });
}

export function toastOk(message: string): void {
  push(document.createTextNode(`✓ ${message}`), {});
}

/** 限流与队列满的错误要提示得更具体：告诉用户多久后重试。 */
export function toastRateLimited(err: ApiError): void {
  const seconds = err.retryAfter ?? 5;
  push(
    document.createTextNode(
      `${err.message || "请求过于频繁"}${seconds > 0 ? ` · ${seconds} 秒后可重试` : ""}`,
    ),
    { danger: true, duration: 4200 },
  );
}
