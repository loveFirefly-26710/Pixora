/**
 * 模态与抽屉。
 *
 * 窄屏走底部抽屉（可下拉关闭），宽屏走居中弹窗，形态由 CSS 断点决定，
 * 这里只负责结构与焦点管理：打开时锁滚动、`Esc` 关闭、关闭后焦点归位。
 */

import { h } from "./dom";

export interface ModalOptions {
  title: string;
  /** 标题右侧的额外元素（例如「全屏阅读」按钮） */
  headerExtra?: Node | null;
  /** 正文内容 */
  body: Node | Node[] | string;
  /** 底部按钮区；不传则不渲染底栏 */
  footer?: Node[] | null;
  /** 阅读类弹窗放宽到 720px */
  reading?: boolean;
  /** 表单字段多的弹窗再放宽一档（存储后端配置用） */
  wide?: boolean;
  /** 点击遮罩是否关闭，默认 true；条款通告这类强制阅读的传 false */
  dismissable?: boolean;
  onClose?: () => void;
}

export interface ModalHandle {
  root: HTMLElement;
  body: HTMLElement;
  footer: HTMLElement;
  close: () => void;
}

const openStack: ModalHandle[] = [];

export function openModal(options: ModalOptions): ModalHandle {
  const dismissable = options.dismissable ?? true;
  const previousFocus = document.activeElement as HTMLElement | null;

  const body = h("div.sheet-body");
  if (typeof options.body === "string") body.innerHTML = options.body;
  else if (Array.isArray(options.body)) body.append(...options.body);
  else body.append(options.body);

  const footer = h("div.sheet-foot");
  if (options.footer?.length) footer.append(...options.footer);
  else footer.style.display = "none";

  const head = h("div.sheet-head", null, h("b", { text: options.title }));
  if (options.headerExtra) {
    head.append(h("span.spacer"), options.headerExtra);
  }
  if (dismissable) {
    head.append(
      h("span", { class: options.headerExtra ? "" : "spacer" }),
      h("button.btn.sm.gho", { type: "button", text: "✕", "aria-label": "关闭", on: { click: () => close() } }),
    );
  }

  const sheetClass = options.wide ? "wide" : options.reading ? "reading" : "";
  const sheet = h("div.sheet", { class: sheetClass }, head, body, footer);
  const mask = h("div.mask");
  const root = h("div.modal", { role: "dialog", "aria-modal": "true" }, mask, sheet);

  function close(): void {
    root.remove();
    const index = openStack.indexOf(handle);
    if (index >= 0) openStack.splice(index, 1);
    if (openStack.length === 0) {
      document.body.style.overflow = "";
      document.removeEventListener("keydown", onKey);
    }
    options.onClose?.();
    previousFocus?.focus?.();
  }

  function onKey(event: KeyboardEvent): void {
    if (event.key === "Escape" && dismissable) close();
  }

  mask.addEventListener("click", () => {
    if (dismissable) close();
  });

  const handle: ModalHandle = { root, body, footer, close };
  openStack.push(handle);
  document.body.style.overflow = "hidden";
  document.addEventListener("keydown", onKey);
  document.body.appendChild(root);
  // 焦点移进弹窗，键盘用户不至于还停在背后的页面上
  const focusTarget = sheet.querySelector<HTMLElement>(
    "input, select, textarea, button:not([aria-label='关闭'])",
  );
  focusTarget?.focus();
  return handle;
}

export interface ConfirmOptions {
  title: string;
  message: string | Node;
  confirmText?: string;
  cancelText?: string;
  danger?: boolean;
  /** 危险操作要求用户输入指定文本才能确认 */
  requireText?: string;
}

/** 确认框。返回 true 表示用户确认。 */
export function confirmDialog(options: ConfirmOptions): Promise<boolean> {
  return new Promise((resolve) => {
    let settled = false;
    const finish = (value: boolean): void => {
      if (settled) return;
      settled = true;
      handle.close();
      resolve(value);
    };

    const message =
      typeof options.message === "string" ? h("div", { html: options.message }) : options.message;
    const nodes: Node[] = [message];

    let input: HTMLInputElement | null = null;
    if (options.requireText) {
      input = h("input.f", { placeholder: options.requireText, style: { marginTop: "12px" } });
      nodes.push(
        h(
          "div",
          { style: { marginTop: "10px" } },
          h("label.f", { text: `请输入「${options.requireText}」以确认` }),
          input,
        ),
      );
    }

    const confirmBtn = h("button.btn", {
      class: options.danger ? "dgr" : "pri",
      type: "button",
      text: options.confirmText ?? "确认",
      disabled: Boolean(options.requireText),
      on: { click: () => finish(true) },
    });

    if (input) {
      input.addEventListener("input", () => {
        confirmBtn.disabled = input?.value.trim() !== options.requireText;
      });
    }

    const handle = openModal({
      title: options.title,
      body: nodes,
      footer: [
        h("button.btn", { type: "button", text: options.cancelText ?? "取消", on: { click: () => finish(false) } }),
        confirmBtn,
      ],
      onClose: () => finish(false),
    });
  });
}

/** 输入一个值（新建相册这类场景）。 */
export function promptDialog(options: {
  title: string;
  label: string;
  value?: string;
  placeholder?: string;
  confirmText?: string;
  hint?: string;
}): Promise<string | null> {
  return new Promise((resolve) => {
    let settled = false;
    const finish = (value: string | null): void => {
      if (settled) return;
      settled = true;
      handle.close();
      resolve(value);
    };

    const input = h("input.f", {
      value: options.value ?? "",
      placeholder: options.placeholder ?? "",
    });

    const handle = openModal({
      title: options.title,
      body: [
        h("div.fld", null, h("label.f", { text: options.label }), input),
        options.hint ? h("div.tip", { text: options.hint }) : null,
      ].filter(Boolean) as Node[],
      footer: [
        h("button.btn", { type: "button", text: "取消", on: { click: () => finish(null) } }),
        h("button.btn.pri", {
          type: "button",
          text: options.confirmText ?? "确定",
          on: { click: () => finish(input.value.trim()) },
        }),
      ],
      onClose: () => finish(null),
    });

    input.addEventListener("keydown", (event) => {
      if ((event as KeyboardEvent).key === "Enter") finish(input.value.trim());
    });
    input.focus();
    input.select();
  });
}

/** 通用的危险操作确认（删除图片、删除账号）。 */
export function confirmDanger(title: string, message: string, confirmText = "删除"): Promise<boolean> {
  return confirmDialog({ title, message, confirmText, danger: true });
}
