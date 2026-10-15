/**
 * 通用组件。页面统一从这里取，保证 27 个界面的观感与交互一致。
 *
 * 约定：所有返回元素的函数都返回已绑好样式的 DOM，不返回 HTML 字符串
 * （除少数静态内容），这样调用方拿到就能直接改属性，不必再查选择器。
 */

import { copyText, h } from "../core/dom";
import {
  barTone,
  formatBytes,
  relativeTime,
  STATUS_LABEL,
  statusBadgeClass,
  ROLE_LABEL,
} from "../core/format";
import { toastWithCode } from "../core/toast";
import type { ImageItem } from "../core/types";

/**
 * 页面标题。
 *
 * UI 原型里每屏顶部那段「标题 + 说明」是**给评审者看的批注**，不在浏览器窗口（`.frame`）
 * 之内，所以应用界面里不该出现。这里改成只读屏可见：排版与原型一致，屏幕阅读器
 * 仍然能拿到页面标题，不为了视觉还原把可访问性丢掉。
 */
export function pageHead(title: string, description?: string): HTMLElement {
  return h(
    "h2.sr-only",
    null,
    h("span", { text: title }),
    description ? h("span", { text: `：${description}` }) : null,
  );
}

/** 带标题的卡片。`extra` 会右对齐放在标题旁。 */
export function card(title: string | null, ...content: Array<Node | string | null>): HTMLElement {
  const head = title
    ? h("h4", null, title)
    : null;
  return h(
    "div.card",
    null,
    head,
    ...(content.filter((item) => item !== null && item !== undefined) as Node[]),
  );
}

export function cardNode(head: Node | null, ...content: Array<Node | null>): HTMLElement {
  return h("div.card", null, head, ...content.filter(Boolean));
}

export function cardHead(title: string, extra?: Node | null): HTMLElement {
  return h(
    "div.flexline",
    { style: { marginBottom: "10px" } },
    h("h4", { text: title, style: { margin: "0" } }),
    extra,
  );
}

/** 表单字段。
 *
 * `<label>` 必须与控件关联（`for` + `id`，再补一个 `aria-label`），否则屏幕阅读器
 * 读出来的是一串「编辑框」——只有 placeholder 能被当成名字，而 placeholder 在
 * 输入后就消失了。这是浏览器验收时实测发现的问题：登录表单的字段没有可访问名。
 */
let fieldSeq = 0;

export function field(label: string, control: Node, tip?: string): HTMLElement {
  const labelEl = h("label.f", { text: label });
  if (control instanceof HTMLElement) {
    const id = control.id || `fld-${++fieldSeq}`;
    control.id = id;
    labelEl.setAttribute("for", id);
    const named =
      control instanceof HTMLInputElement ||
      control instanceof HTMLSelectElement ||
      control instanceof HTMLTextAreaElement;
    // 按钮组之类的容器不适合直接用 aria-label，会被读成一整个控件
    if (named && !control.getAttribute("aria-label")) {
      control.setAttribute("aria-label", label);
    }
  }
  return h(
    "div.fld",
    null,
    labelEl,
    control,
    tip ? h("div.tip", { text: tip }) : null,
  );
}

export function textInput(options: {
  value?: string;
  placeholder?: string;
  type?: string;
  readonly?: boolean;
  mono?: boolean;
  name?: string;
  min?: string;
  max?: string;
  step?: string;
}): HTMLInputElement {
  const input = h("input.f", {
    type: options.type ?? "text",
    placeholder: options.placeholder ?? "",
    class: options.mono ? "mono" : "",
    name: options.name,
    min: options.min,
    max: options.max,
    step: options.step,
  });
  if (options.value !== undefined) input.value = options.value;
  if (options.readonly) input.readOnly = true;
  return input;
}

export function select(options: Array<{ value: string; label: string }>, current?: string): HTMLSelectElement {
  const el = h("select.f");
  for (const item of options) {
    const option = h("option", { value: item.value, text: item.label });
    if (item.value === current) (option as HTMLOptionElement).selected = true;
    el.append(option);
  }
  return el;
}

export function textarea(options: {
  value?: string;
  placeholder?: string;
  rows?: number;
}): HTMLTextAreaElement {
  const el = h("textarea.f", {
    rows: options.rows ?? 4,
    placeholder: options.placeholder ?? "",
  });
  el.value = options.value ?? "";
  return el;
}

/** 开关行：左侧标题与说明，右侧按钮。 */
export function switchRow(
  title: string,
  description: string,
  checked: boolean,
  onChange: (next: boolean) => void,
): HTMLElement {
  const button = h("button.sw", {
    type: "button",
    class: checked ? "" : "off",
    role: "switch",
    "aria-checked": String(checked),
    "aria-label": title,
  });
  button.addEventListener("click", () => {
    const next = button.classList.contains("off");
    button.classList.toggle("off", !next);
    button.setAttribute("aria-checked", String(next));
    onChange(next);
  });
  return h(
    "div.swrow",
    null,
    h("div.txt", null, h("b", { text: title }), h("small", { text: description })),
    button,
  );
}

/** 只读开关行（原型里展示统计时用）。 */
export function switchStatic(title: string, description: string, on: boolean): HTMLElement {
  return h(
    "div.swrow",
    null,
    h("div.txt", null, h("b", { text: title }), h("small", { text: description })),
    h("span.sw", { class: on ? "" : "off", "aria-hidden": "true" }),
  );
}

/** 用量进度条。 */
export function progressBar(used: number, total: number): HTMLElement {
  const ratio = total > 0 ? Math.min(1, used / total) : 0;
  const tone = barTone(ratio);
  return h(
    "div.bar-l",
    null,
    h("i", { class: tone, style: { width: `${Math.round(ratio * 100)}%` } }),
  );
}

export function progressLine(label: string, used: number, total: number): HTMLElement {
  return h(
    "div",
    { style: { marginTop: "10px" } },
    h("div.flexline", { style: { justifyContent: "space-between", fontSize: "12.5px" } },
      h("span", { text: label }),
      h("span", { class: "mono", text: `${formatBytes(used)} / ${formatBytes(total)}` })),
    h("div", { style: { marginTop: "6px" } }, progressBar(used, total)),
  );
}

export function statCard(label: string, value: string, tone?: "ok" | "warn" | "err" | "muted"): HTMLElement {
  const color =
    tone === "ok"
      ? "var(--success)"
      : tone === "warn"
        ? "var(--warning)"
        : tone === "err"
          ? "var(--danger)"
          : tone === "muted"
            ? "var(--muted)"
            : undefined;
  return h(
    "div.card",
    null,
    h("div.k", { text: label }),
    h("div.v", { text: value, style: color ? { color } : undefined }),
  );
}

export function badge(text: string, tone: "ok" | "warn" | "err" | "info" | "mute" | "pp"): HTMLElement {
  return h("span.badge", { class: tone, text });
}

export function statusBadge(status: string, label?: string): HTMLElement {
  return badge(label ?? STATUS_LABEL[status] ?? status, statusBadgeClass(status) as "ok" | "warn" | "err" | "mute");
}

export function roleBadge(role: string): HTMLElement {
  return role === "admin"
    ? badge(ROLE_LABEL.admin, "pp")
    : badge(ROLE_LABEL.user, "info");
}

/** 提示条：`note` 是页尾的长提示，`qtip` 是卡片内的短提示。 */
export function qtip(
  tone: "info" | "ok" | "warn" | "err",
  title: string | null,
  text: string | Node,
): HTMLElement {
  const icon = tone === "ok" ? "✓" : tone === "err" ? "!" : "i";
  const content =
    typeof text === "string"
      ? h("div", null, title ? h("b", { text: title }) : null, document.createTextNode(text))
      : h("div", null, title ? h("b", { text: title }) : null, text);
  return h("div.qtip", { class: tone === "info" ? "info" : tone }, h("span.ic", { text: icon }), content);
}

export function note(tone: "default" | "w" | "d" | "g", ...content: Array<Node | string>): HTMLElement {
  return h(
    "div.note",
    { class: tone === "default" ? "" : tone },
    ...content.map((item) => (typeof item === "string" ? document.createTextNode(item) : item)),
  );
}

export function emptyState(icon: string, text: string, action?: Node): HTMLElement {
  return h(
    "div.empty",
    null,
    h("div.big", { text: icon }),
    h("div", { text }),
    action ? h("div", { style: { marginTop: "12px" } }, action) : null,
  );
}

export function skeletonGrid(count = 8): HTMLElement {
  return h(
    "div.grid.igrid",
    null,
    ...Array.from({ length: count }, () =>
      h("div.skel", { style: { height: "150px", borderRadius: "10px" } }),
    ),
  );
}

/** 分页条。总数不超过一页时不显示。 */
export function pagination(
  meta: { page: number; total_pages: number; total: number },
  onPage: (page: number) => void,
): HTMLElement | null {
  if (meta.total_pages <= 1) return null;
  const buttons: Node[] = [
    h("button.btn.sm", {
      type: "button",
      text: "上一页",
      disabled: meta.page <= 1,
      on: { click: () => onPage(meta.page - 1) },
    }),
  ];
  const window = 2;
  for (let page = 1; page <= meta.total_pages; page += 1) {
    if (page > 1 && page < meta.total_pages && Math.abs(page - meta.page) > window) {
      if (page === 2 || page === meta.total_pages - 1) {
        buttons.push(h("span", { text: "…", style: { color: "var(--faint)", padding: "0 4px" } }));
      }
      continue;
    }
    buttons.push(
      h("button.btn.sm", {
        type: "button",
        class: page === meta.page ? "pri" : "",
        text: String(page),
        on: { click: () => onPage(page) },
      }),
    );
  }
  buttons.push(
    h("button.btn.sm", {
      type: "button",
      text: "下一页",
      disabled: meta.page >= meta.total_pages,
      on: { click: () => onPage(meta.page + 1) },
    }),
  );
  return h(
    "div.flexline",
    { style: { justifyContent: "center", marginTop: "18px", marginBottom: "8px" } },
    ...buttons,
  );
}

/** 标签胶囊行。 */
export function tagChips(
  tags: Array<{ name: string; usage?: number }>,
  active: string | null,
  onPick: (tag: string | null) => void,
): HTMLElement {
  const row = h("div.flexline", { style: { marginBottom: "14px" } });
  row.append(
    h("button.chip", {
      type: "button",
      class: active === null ? "on" : "",
      text: "全部",
      on: { click: () => onPick(null) },
    }),
  );
  for (const tag of tags) {
    row.append(
      h("button.chip", {
        type: "button",
        class: active === tag.name ? "on" : "",
        text: tag.usage ? `${tag.name} ${tag.usage}` : tag.name,
        on: { click: () => onPick(tag.name) },
      }),
    );
  }
  return row;
}

/** 单个图片方格。左键打开预览，Shift/Ctrl 复选由容器统一处理。
 *
 * 卡片上的文字只有「标签 + 时间」，刻意不摆文件名与 UID：
 *   - 文件名是用户上传时的原始名字，常见于「IMG_20240312_身份证.jpg」这类，
 *     摆在广场上就是一次信息泄露；
 *   - UID 是账号的对外短标识，访客没有必要知道每一张图属于谁。
 * 需要看文件名的场景（自己的相册、管理后台）用列表视图，那里有。
 */
export function imageTile(
  item: ImageItem,
  options: {
    selected?: boolean;
    onOpen?: (item: ImageItem, event: MouseEvent) => void;
    onSelect?: (item: ImageItem, event: MouseEvent) => void;
    /** 副标题（默认是时间，回收站之类想换文案时传） */
    subtitle?: string;
    /** 方格上是否带标签 chip（相册页不显示，广场显示，跟原型一致） */
    showTags?: boolean;
    /** 左上角带勾选框（相册页用：勾选框常驻，不必先点「选择」进模式） */
    checkbox?: boolean;
  } = {},
): HTMLElement {
  const thumb = h("div.ph", { class: `c${(item.id % 6) + 1}` });
  // 有尺寸就写下真实比例：CSS 用它出高度，瀑布流用它算跨几行。
  // 不写的话只能按 4:3 兜底，一屏里就会高高低低地对不上
  if (item.width && item.height) {
    thumb.style.setProperty("--ratio", `${item.width} / ${item.height}`);
  }
  if (item.has_thumb && item.thumb_url) {
    const img = h("img", {
      src: item.thumb_url,
      alt: item.tags[0] ?? "图片",
      loading: "lazy",
      decoding: "async",
    });
    thumb.append(img);
  } else {
    // 没有缩略图时直接用原图，靠容器缩放，带宽影响有限
    const img = h("img", {
      src: item.url,
      alt: item.tags[0] ?? "图片",
      loading: "lazy",
      decoding: "async",
    });
    thumb.append(img);
  }

  const time = options.subtitle ?? relativeTime(item.upload_time);
  const tags = options.showTags === false ? [] : item.tags.slice(0, 3);
  const tile = h(
    "button.tile",
    {
      type: "button",
      class: options.selected ? "sel" : "",
      // 悬停提示也只给标签与时间：tooltip 同样是「展示」
      title: tags.length > 0 ? `${tags.join(" · ")} · ${time}` : time,
    },
    thumb,
    h(
      "div.meta",
      null,
      h(
        "div.s",
        null,
        ...tags.map((tag) => h("span.tag.b", { text: tag })),
        h("span", { text: time }),
      ),
    ),
  );
  tile.dataset.imageId = String(item.id);
  tile.addEventListener("click", (event) => {
    if (event.shiftKey || event.ctrlKey || event.metaKey) {
      event.preventDefault();
      options.onSelect?.(item, event);
      return;
    }
    options.onOpen?.(item, event);
  });

  if (!options.checkbox) return tile;

  // 勾选框放在 button 外面：交互元素不能嵌套（按钮里塞 input 是非法结构，
  // 点一下会同时触发两边的行为）。外层 .tile-cell 只负责定位。
  const pick = h("input", {
    type: "checkbox",
    class: "pick",
    checked: options.selected ?? false,
    "aria-label": `选择 ${item.tags[0] ?? "这张图片"}（${relativeTime(item.upload_time)}）`,
  });
  pick.addEventListener("click", (event) => {
    // 勾选框自己处理选中；Shift 点它同样按范围选
    event.stopPropagation();
    options.onSelect?.(item, event);
  });
  pick.addEventListener("change", (event) => event.stopPropagation());
  return h("div.tile-cell", { class: options.selected ? "on" : "" }, pick, tile);
}

/** 复制直链按钮：点了就提示，失败也提示。 */
export function copyButton(label: string, getUrl: () => string): HTMLElement {
  return h("button.btn.sm", {
    type: "button",
    text: label,
    on: {
      click: async (event) => {
        event.stopPropagation();
        const url = getUrl();
        const ok = await copyText(url);
        if (ok) toastWithCode("✓ 已复制直链", url);
      },
    },
  });
}

/** 键值展示行，右侧等宽字体。 */
export function kvRow(label: string, value: Node | string, mono = false): HTMLElement {
  return h(
    "div.flexline",
    { style: { padding: "6px 0", borderBottom: "1px solid #f3f4f6" } },
    h("span", { text: label, style: { color: "var(--muted)", flex: "0 0 132px" } }),
    typeof value === "string" ? h("span", { class: mono ? "mono" : "", text: value }) : value,
  );
}

/** 表格骨架：表头列名 + 行。窄屏走卡片形态（`.responsive`）。 */
export function responsiveTable(headers: string[], rows: Node[][]): HTMLElement {
  const thead = h("thead", null, h("tr", null, ...headers.map((title) => h("th", { text: title }))));
  const tbody = h(
    "tbody",
    null,
    ...rows.map((cells) =>
      h(
        "tr",
        null,
        ...cells.map((cell, index) =>
          h("td", { "data-label": headers[index] ?? "" }, cell),
        ),
      ),
    ),
  );
  return h("div.table-wrap", null, h("table.responsive", null, thead, tbody));
}

/** 危险操作区。 */
export function dangerZone(title: string, text: string, action: Node): HTMLElement {
  return h("div.dz", null, h("h4", { text: title }), h("p", { text }), action);
}

export function spinner(): HTMLElement {
  return h("span", { text: "⋯", style: { color: "var(--muted)" } });
}

/**
 * 图片排序的可选项。广场与相册共用这一份，两个页面的排序口径才一致。
 *
 * 默认是「随机」（`shuffle`）：图床的浏览场景里，总是同一批最新的图占着前排，
 * 后面的内容永远没人看得到。随机顺序让整个图库都有被翻到的机会。
 * 想按时间线看时，用户自己切到「最新」。
 */
export const IMAGE_SORT_OPTIONS: Array<{ value: string; label: string }> = [
  { value: "shuffle", label: "随机" },
  { value: "new", label: "上传时间 · 最新" },
  { value: "old", label: "上传时间 · 最久" },
  { value: "size", label: "文件大小 · 大到小" },
  { value: "smallest", label: "文件大小 · 小到大" },
  { value: "name", label: "文件名" },
  { value: "type", label: "文件类型" },
];

/** 排序下拉。`current` 传「本次生效的排序」，不在清单里就退回第一项。 */
export function sortSelect(
  current: string,
  onChange: (value: string) => void,
  extra: Array<{ value: string; label: string }> = [],
): HTMLSelectElement {
  const options = [...extra, ...IMAGE_SORT_OPTIONS];
  const element = select(options, current);
  if (!options.some((item) => item.value === element.value)) {
    element.value = options[0]?.value ?? "";
  }
  element.setAttribute("aria-label", "排序方式");
  element.addEventListener("change", () => onChange(element.value));
  return element;
}

/** 随机排序的种子。同一个种子翻页不会重复/漏图，换一次种子就重洗一次。 */
export function newShuffleSeed(): number {
  return Math.floor(Math.random() * 2_000_000_000) + 1;
}
