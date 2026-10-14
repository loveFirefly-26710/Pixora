/**
 * 极简 DOM 工具。
 *
 * 不引框架：`h()` 拼元素、`mount()` 换页面、`on()` 做事件委托。
 * 页面之间靠 `data-act` 属性挂事件，避免每渲染一次就重新绑一堆监听器——
 * 列表页一页几十张图，逐一 addEventListener 会成为滚动时的负担。
 */

type Child = Node | string | number | null | undefined | false;

export interface Attrs {
  class?: string;
  id?: string;
  /** 事件一律用委托，这里只在确实需要时才用 */
  on?: Partial<Record<keyof HTMLElementEventMap, (event: Event) => void>>;
  /** 任意 data-* 与普通属性 */
  [key: string]: unknown;
}

/**
 * 建元素。`h("div.card", {…}, child, child)`
 *
 * 标签写 `"div.card.flat"` 这种紧凑形式：浏览器里手写 class 列表太啰嗦。
 * 重载只为把返回值收窄到具体元素类型（`input.value` 这类属性要用），
 * 不做运行期校验——写错标签名是开发者的问题，不是用户的问题。
 */
export function h(tag: "input" | `input.${string}`, attrs?: Attrs | null, ...children: Child[]): HTMLInputElement;
export function h(tag: "select" | `select.${string}`, attrs?: Attrs | null, ...children: Child[]): HTMLSelectElement;
export function h(tag: "textarea" | `textarea.${string}`, attrs?: Attrs | null, ...children: Child[]): HTMLTextAreaElement;
export function h(tag: "button" | `button.${string}`, attrs?: Attrs | null, ...children: Child[]): HTMLButtonElement;
export function h(tag: "a" | `a.${string}`, attrs?: Attrs | null, ...children: Child[]): HTMLAnchorElement;
export function h(tag: "form" | `form.${string}`, attrs?: Attrs | null, ...children: Child[]): HTMLFormElement;
export function h(tag: "img" | `img.${string}`, attrs?: Attrs | null, ...children: Child[]): HTMLImageElement;
export function h(tag: "label" | `label.${string}`, attrs?: Attrs | null, ...children: Child[]): HTMLLabelElement;
export function h(tag: string, attrs?: Attrs | null, ...children: Child[]): HTMLElement;
export function h(
  tag: string,
  attrs?: Attrs | null,
  ...children: Child[]
): HTMLElement {
  const [name, ...classes] = String(tag).split(".");
  const el = document.createElement(name || "div");
  if (classes.length) el.className = classes.join(" ");
  applyAttrs(el, attrs);
  append(el, children);
  return el;
}

/** 建 SVG 元素（图标用）。 */
export function svg(path: string, attrs?: Attrs): SVGElement {
  const el = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  el.setAttribute("viewBox", "0 0 24 24");
  el.setAttribute("fill", "none");
  el.setAttribute("stroke", "currentColor");
  el.setAttribute("stroke-width", "1.8");
  el.setAttribute("stroke-linecap", "round");
  el.setAttribute("stroke-linejoin", "round");
  el.setAttribute("aria-hidden", "true");
  const p = document.createElementNS("http://www.w3.org/2000/svg", "path");
  p.setAttribute("d", path);
  el.appendChild(p);
  if (attrs?.class) el.setAttribute("class", String(attrs.class));
  return el;
}

function applyAttrs(el: HTMLElement, attrs?: Attrs | null): void {
  if (!attrs) return;
  for (const [key, value] of Object.entries(attrs)) {
    if (value === undefined || value === null || value === false) continue;
    if (key === "class") {
      el.className = el.className ? `${el.className} ${String(value)}` : String(value);
    } else if (key === "on" && typeof value === "object") {
      for (const [event, handler] of Object.entries(value as Record<string, EventListener>)) {
        el.addEventListener(event, handler);
      }
    } else if (key === "style" && typeof value === "object") {
      Object.assign(el.style, value as Record<string, string>);
    } else if (key === "value" && el instanceof HTMLInputElement) {
      el.value = String(value);
    } else if (key === "checked" && el instanceof HTMLInputElement) {
      el.checked = Boolean(value);
    } else if (key === "html") {
      el.innerHTML = String(value);
    } else if (key === "text") {
      el.textContent = String(value);
    } else {
      el.setAttribute(key, String(value));
    }
  }
}

function append(parent: Node, children: Child[]): void {
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue;
    parent.appendChild(typeof child === "object" ? child : document.createTextNode(String(child)));
  }
}

/** 事件委托：在容器上监听，按 `[data-act]` 派发。 */
export function delegate(
  root: HTMLElement,
  handler: (action: string, target: HTMLElement, event: Event) => void,
): () => void {
  const listener = (event: Event) => {
    const target = (event.target as HTMLElement | null)?.closest<HTMLElement>("[data-act]");
    if (!target || !root.contains(target)) return;
    const action = target.dataset.act;
    if (action) handler(action, target, event);
  };
  root.addEventListener("click", listener);
  root.addEventListener("change", listener);
  root.addEventListener("submit", listener);
  return () => {
    root.removeEventListener("click", listener);
    root.removeEventListener("change", listener);
    root.removeEventListener("submit", listener);
  };
}

/** 清空并放入新内容。 */
export function mount(container: HTMLElement, ...content: Child[]): void {
  container.replaceChildren();
  append(container, content);
}

/** 取查询参数。 */
export function param(name: string): string | null {
  return new URLSearchParams(location.search).get(name);
}

/** 当前哈希路由里的查询部分（`#/albums?album_id=7` 的 `album_id=7`）。 */
export function hashQuery(hash: string): URLSearchParams {
  const index = hash.indexOf("?");
  return new URLSearchParams(index >= 0 ? hash.slice(index + 1) : "");
}

export function escapeHtml(input: string): string {
  return input
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

/** 防抖，搜索框用。 */
export function debounce<T extends (...args: never[]) => void>(fn: T, wait = 250): T {
  let timer: number | undefined;
  return ((...args: never[]) => {
    if (timer) window.clearTimeout(timer);
    timer = window.setTimeout(() => fn(...args), wait);
  }) as T;
}

/** 写入剪贴板。失败时退回到旧接口（http 环境下 navigator.clipboard 不可用）。 */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    /* 落到下面用兜底方案 */
  }
  try {
    const area = document.createElement("textarea");
    area.value = text;
    area.setAttribute("readonly", "");
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.appendChild(area);
    area.select();
    const ok = document.execCommand("copy");
    document.body.removeChild(area);
    return ok;
  } catch {
    return false;
  }
}
