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

/**
 * 主页面可见标题。广场用 hero 自带的 h1，上传与相册用这一块；
 * 后台页仍然以卡片分节，不强行加一层大标题。
 */
export function viewHead(
  eyebrow: string,
  title: string,
  description?: string,
  extra?: Node | null,
): HTMLElement {
  return h(
    "header.view-head",
    null,
    h(
      "div",
      null,
      h("div.view-eyebrow", { text: eyebrow }),
      h("h1.view-title", { text: title }),
      description ? h("p.view-desc", { text: description }) : null,
    ),
    extra ? h("div.view-head-extra", null, extra) : null,
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
    // 密码框的外面可能包着一层 .pw-wrap（显示/隐藏按钮），标签要绑到里面
    // 真正那个 input 上，否则 for 指向一个 div，屏幕阅读器读不出名字。
    const named =
      control instanceof HTMLInputElement ||
      control instanceof HTMLSelectElement ||
      control instanceof HTMLTextAreaElement
        ? control
        : control.querySelector<HTMLElement>("input, select, textarea");
    if (named) {
      const id = named.id || `fld-${++fieldSeq}`;
      named.id = id;
      labelEl.setAttribute("for", id);
      // 按钮组之类的容器不适合直接用 aria-label，会被读成一整个控件
      if (!named.getAttribute("aria-label")) {
        named.setAttribute("aria-label", label);
      }
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

/**
 * 密码框 + 显示/隐藏按钮。
 *
 * 返回的 `input` 是真正的输入框（取值、聚焦都继续用它），`node` 是要放进
 * 页面的那一层包装。包一层而不是往 input 里塞按钮，是因为 input 是空元素，
 * 里面放不了任何东西；按钮绝对定位在右侧，窄屏也不会把输入区挤没。
 */
export interface PasswordControl {
  input: HTMLInputElement;
  node: HTMLElement;
}

export function passwordControl(options: {
  value?: string;
  placeholder?: string;
  name?: string;
  mono?: boolean;
  readonly?: boolean;
} = {}): PasswordControl {
  const input = textInput({ ...options, type: "password" });
  const toggle = h("button.pw-toggle", {
    type: "button",
    text: "显示",
    "aria-label": "显示密码",
    "aria-pressed": "false",
    title: "显示密码",
  });
  toggle.addEventListener("click", () => {
    const reveal = input.type === "password";
    input.type = reveal ? "text" : "password";
    toggle.textContent = reveal ? "隐藏" : "显示";
    toggle.setAttribute("aria-label", reveal ? "隐藏密码" : "显示密码");
    toggle.setAttribute("aria-pressed", String(reveal));
    toggle.title = reveal ? "隐藏密码" : "显示密码";
    input.focus();
  });
  return { input, node: h("div.pw-wrap", null, input, toggle) };
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

/** 网格的骨架屏格子。直接作为外层网格的子节点，别再套一层 `.grid`。 */
export function skeletonTiles(count = 8): HTMLElement[] {
  return Array.from({ length: count }, () =>
    h("div.skel", { style: { height: "150px", borderRadius: "10px" } }),
  );
}

/** 自带网格容器的骨架屏。页面直接用它替换整块列表时使用。 */
export function skeletonGrid(count = 8): HTMLElement {
  return h("div.grid.igrid", null, ...skeletonTiles(count));
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

/** 网格里的图片节点。
 *
 * 一次读不出来就碎在那里，是列表页最难看的一种状态：一屏里混着几张灰框，
 * 用户不知道是自己网不好还是图没了。这里给两段退路——先用同一个地址重试一次
 * （服务端重启、连接被掐这类抖动多半第二次就成），仍旧不行再退回原图（原图是
 * 这张图最稳的一份实体，缩略图缺失或者读不出来时它一定在）。
 */
export function thumbImage(item: {
  url: string;
  thumb_url?: string | null;
  has_thumb?: boolean;
  tags?: string[];
  title?: string | null;
}): HTMLImageElement {
  const primary = item.has_thumb && item.thumb_url ? item.thumb_url : item.url;
  const img = h("img", {
    src: primary,
    alt: item.tags?.[0] ?? item.title ?? "图片",
    loading: "lazy",
    decoding: "async",
  });
  // 退路只走两次：重试一次、退回原图一次。两个都不行就认了，
  // 不留「失败又改地址再失败」的循环
  img.addEventListener("error", () => {
    const stage = img.dataset.fallback ?? "";
    if (stage === "") {
      img.dataset.fallback = "retry";
      img.src = `${primary}${primary.includes("?") ? "&" : "?"}retry=1`;
      return;
    }
    if (stage === "retry" && primary !== item.url) {
      img.dataset.fallback = "original";
      img.src = item.url;
    }
  });
  return img;
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
    /** 卡片标题：广场传用户自己写的标题，相册传文件名；不传就不占这一行 */
    title?: string | null;
    /** 是否显示浏览量；广场显示，相册/回收站不显示 */
    showViews?: boolean;
    /** 浏览量是 0 也显示（广场要求每张图底部都有统计） */
    alwaysShowViews?: boolean;
  } = {},
): HTMLElement {
  const thumb = h("div.ph", { class: `c${(item.id % 6) + 1}` });
  // 有尺寸就写下真实比例：CSS 用它出高度，瀑布流用它算跨几行。
  // 不写的话只能按 4:3 兜底，一屏里就会高高低低地对不上
  if (item.width && item.height) {
    thumb.style.setProperty("--ratio", `${item.width} / ${item.height}`);
  }
  if (item.has_thumb && item.thumb_url) {
    thumb.append(thumbImage({ url: item.url, thumb_url: item.thumb_url, has_thumb: true, tags: item.tags }));
  } else {
    // 没有缩略图时直接用原图，靠容器缩放，带宽影响有限
    thumb.append(thumbImage({ url: item.url, tags: item.tags }));
  }

  const time = options.subtitle ?? relativeTime(item.upload_time);
  const tags = options.showTags === false ? [] : item.tags.slice(0, 3);
  const title = (options.title ?? item.title ?? "").trim();
  const showViews =
    options.showViews !== false && (item.views > 0 || options.alwaysShowViews === true);
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
      title ? h("div.n", { text: title }) : null,
      h(
        "div.s",
        null,
        ...tags.map((tag) => h("span.tag.b", { text: tag })),
        h("span", { text: time }),
        showViews ? h("span.v", { text: `浏览 ${item.views}`, "aria-label": `${item.views} 次浏览` }) : null,
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
    { style: { padding: "6px 0", borderBottom: "1px solid var(--border)" } },
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
  { value: "new", label: "最新" },
  { value: "old", label: "最久" },
  { value: "name_desc", label: "文件名↓" },
  { value: "name", label: "文件名↑" },
  { value: "size", label: "文件大小↓" },
  { value: "smallest", label: "文件大小↑" },
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
