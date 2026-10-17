/**
 * 服务调整 · 服务条款（用户端）
 *
 * 首次登录的强制通告由 main.ts 负责弹出；本页是「之后随时翻阅」的入口：
 * 当前版本、本次更新摘要、正文、历史版本与自己的同意记录。
 *
 * 阅读器三种形态（标准弹窗 / 全屏阅读 / 移动全屏）共用同一份 DOM 与同一份正文：
 * 切换形态只改容器尺寸（换一个 class），正文节点不重建、滚动位置不丢——
 * 读到一半点「全屏阅读」又跳回开头，是最典型的阅读体验事故。
 */

import { ApiError, request } from "../core/api";
import { h } from "../core/dom";
import { formatDate, formatDateTime } from "../core/format";
import type { PageContext } from "../core/router";
import { toastError, toastOk } from "../core/toast";
import type { TermsPayload, TermsVersion } from "../core/types";
import {
  badge,
  card,
  cardHead,
  cardNode,
  emptyState,
  pageHead,
  qtip,
} from "../ui/widgets";

export async function render(ctx: PageContext): Promise<void> {
  const pad = h("div.pad");
  ctx.outlet.append(pad);
  pad.append(
    pageHead(
      "服务调整 · 服务条款",
      "条款存在数据库里，管理员改完即时生效。这里可以随时翻阅当前版本、历史版本与自己的同意记录。",
    ),
  );

  const body = h("div");
  pad.append(body);

  async function load(): Promise<void> {
    body.replaceChildren(skeletonBlock(3));
    try {
      const payload = await request<TermsPayload>("/terms");
      body.replaceChildren(...renderPayload(payload, () => void load()));
    } catch (err) {
      body.replaceChildren(errorBlock(err, () => void load()));
    }
  }

  await load();
}

/** 从服务端数据拼页面主体。 */
function renderPayload(payload: TermsPayload, reload: () => void): Node[] {
  const current = payload.current;
  if (!current) {
    return [
      card(
        null,
        emptyState("📄", "管理员还没有发布服务条款。发布后这里会显示当前版本与正文。"),
      ),
    ];
  }

  const agreedCurrent = isAgreed(payload, current);
  const nodes: Node[] = [currentCard(payload, current, reload)];

  const items = summaryItems(current.summary);
  if (items.length > 0) {
    nodes.push(
      card(
        "本次更新摘要",
        h(
          "ul",
          { style: { margin: "0", paddingLeft: "20px", fontSize: "12.5px", lineHeight: "2", color: "var(--muted)" } },
          ...items.map((text) => h("li", { text })),
        ),
      ),
    );
  }

  const printScope = h("div.terms-print-scope", null, renderBody(current.body));
  nodes.push(
    cardNode(
      cardHead(
        "条款正文",
        h(
          "div.flexline",
          null,
          h("button.btn.sm", {
            type: "button",
            text: "阅读模式",
            title: "以弹窗阅读，可切换到全屏阅读",
            on: {
              click: () =>
                openReader({
                  title: current.title,
                  meta: versionMeta(current),
                  body: current.body,
                  version: current.version,
                  agreed: agreedCurrent,
                  agreedAt: payload.consent?.agreed_at ?? null,
                  agreedIp: payload.consent?.ip ?? null,
                  onAgree: agreedCurrent ? null : () => agree(current.id, reload),
                }),
            },
          }),
          h("button.btn.sm", {
            type: "button",
            text: "打印",
            title: "只打印正文，不带导航",
            on: { click: () => printTerms() },
          }),
        ),
      ),
      h(
        "div.terms-body-clip",
        { style: { maxHeight: "320px", overflow: "auto", paddingRight: "6px" } },
        printScope,
      ),
    ),
  );

  nodes.push(historyCard(payload, current));
  nodes.push(
    h("div.note", {
      text: "旧版本不删除，只归档。管理员发布新版本并勾选「要求重新同意」后，下次登录会再弹一次通告。",
    }),
  );
  return nodes;
}

/** 当前版本卡：版本信息 + 我的同意记录 + 未同意时的同意入口。 */
function currentCard(payload: TermsPayload, current: TermsVersion, reload: () => void): HTMLElement {
  const agreed = isAgreed(payload, current);
  const consent = payload.consent;
  return card(
    null,
    h(
      "div.flexline",
      { style: { justifyContent: "space-between", alignItems: "flex-start" } },
      h(
        "div",
        null,
        h("div", { style: { fontSize: "14px", fontWeight: "700" }, text: `当前生效版本 ${current.version}` }),
        h("div.tip", { style: { marginTop: "3px" }, text: versionMeta(current) }),
      ),
      agreed ? badge("你已同意", "ok") : badge("尚未同意", "warn"),
    ),
    h("div", {
      style: {
        marginTop: "12px",
        paddingTop: "12px",
        borderTop: "1px solid var(--border)",
        fontSize: "12.5px",
        color: "var(--muted)",
      },
      text: consent
        ? `你的同意记录：${formatDateTime(consent.agreed_at)} · IP ${consent.ip ?? "未记录"}（${consent.version}）`
        : "你还没有同意记录。管理员更新条款并要求重新同意时，下次登录会强制弹出通告。",
    }),
    !agreed
      ? h(
          "div",
          { style: { marginTop: "12px" } },
          qtip("warn", "需要重新同意", "当前生效版本你还没有同意，同意后才能继续使用上传等功能。"),
          h(
            "div",
            { style: { marginTop: "10px" } },
            h("button.btn.pri.sm", {
              type: "button",
              text: `同意 ${current.version}`,
              on: {
                click: async (event) => {
                  const button = event.currentTarget as HTMLButtonElement;
                  button.disabled = true;
                  try {
                    await agree(current.id, reload);
                  } catch {
                    // 失败原因已由 agree() 提示过，这里只负责恢复按钮
                    button.disabled = false;
                  }
                },
              },
            }),
          ),
        )
      : null,
  );
}

/** 历史版本表：版本 / 生效日期 / 摘要 / 我的同意 / 查看。 */
function historyCard(payload: TermsPayload, current: TermsVersion): HTMLElement {
  const rows = payload.history ?? [];
  if (rows.length === 0) {
    return card("历史版本", emptyState("🗂", "还没有更早的版本。"));
  }
  return card(
    "历史版本",
    h(
      "div.table-wrap",
      null,
      h(
        "table.responsive",
        null,
        h(
          "thead",
          null,
          h(
            "tr",
            null,
            h("th", { style: { width: "96px" }, text: "版本" }),
            h("th", { style: { width: "120px" }, text: "生效日期" }),
            h("th", { text: "更新摘要" }),
            h("th", { style: { width: "104px" }, text: "我的同意" }),
            h("th", { style: { width: "78px" } }),
          ),
        ),
        h(
          "tbody",
          null,
          ...rows.map((entry) =>
            h(
              "tr",
              null,
              h(
                "td",
                { "data-label": "版本" },
                entry.version === current.version
                  ? h("b", { text: entry.version })
                  : h("span", { text: entry.version }),
              ),
              h("td", { "data-label": "生效日期", text: formatDate(entry.effective_at) }),
              h("td", { "data-label": "更新摘要", text: entry.summary ?? "—" }),
              h(
                "td",
                { "data-label": "我的同意" },
                entry.agreed ? badge("已同意", "ok") : badge("未同意", "mute"),
              ),
              h(
                "td",
                { "data-label": "" },
                h("button.btn.sm", {
                  type: "button",
                  text: "查看",
                  on: {
                    click: () =>
                      openReader({
                        title: current.title,
                        meta: `版本 ${entry.version} · 生效日期 ${formatDate(entry.effective_at)}`,
                        // 契约的 history 项只给版本信息，不给正文；当前版本的那一条复用当前正文
                        body: entry.version === current.version ? current.body : "",
                        version: entry.version,
                        agreed: entry.agreed,
                        agreedAt: entry.version === current.version ? payload.consent?.agreed_at ?? null : null,
                        agreedIp: entry.version === current.version ? payload.consent?.ip ?? null : null,
                        onAgree: null,
                      }),
                  },
                }),
              ),
            ),
          ),
        ),
      ),
    ),
  );
}

/* ------------------------------------------------------------------ */
/* 阅读器                                                              */
/* ------------------------------------------------------------------ */

interface ReaderOptions {
  title: string;
  meta: string;
  /** 正文纯文本；空串表示接口没有提供该版本的正文 */
  body: string;
  version: string;
  agreed: boolean;
  agreedAt?: string | null;
  agreedIp?: string | null;
  /** 未同意时提供「同意并继续」，已同意或历史版本传 null */
  onAgree: (() => Promise<void>) | null;
}

function openReader(options: ReaderOptions): void {
  // 正文只建一次，后面切换形态不会再碰它
  const bodyBox = h(
    "div.tr-body",
    null,
    options.body
      ? renderBody(options.body)
      : qtip(
          "info",
          "历史版本的正文没有对外提供",
          "接口只返回当前版本的正文；这条记录保留的是版本信息与当时的更新摘要。",
        ),
  );

  const fullButton = h("button.btn.sm.tr-fullbtn", { type: "button", text: "⤢ 全屏阅读" });
  const makeCloseButton = (): HTMLButtonElement =>
    h("button.btn.sm.gho.tr-close", { type: "button", text: "✕", "aria-label": "关闭" });

  const sheet = h("div.tr-sheet");
  const reader = h("div.terms-reader", { role: "dialog", "aria-modal": "true" }, sheet);

  // 同意区：始终贴在底部。已同意、或历史版本这种给不出同意动作的情况，
  // 只显示状态，不给一个点了没反应的勾选框。
  const foot = h("div.tr-foot");
  if (options.agreed || !options.onAgree) {
    foot.append(
      h(
        "div.tr-state",
        null,
        options.agreed ? badge("你已同意", "ok") : badge("未同意该版本", "mute"),
        h("span.tip", {
          style: { margin: "0 0 0 8px" },
          text: options.agreed
            ? options.agreedAt
              ? `${formatDateTime(options.agreedAt)}${options.agreedIp ? ` · IP ${options.agreedIp}` : ""}`
              : "本条同意记录由系统保留"
            : "历史版本的同意状态只读，新的同意记录以当前生效版本为准。",
        }),
      ),
      h("div.tr-actions", null, makeCloseButton()),
    );
  } else {
    const checkbox = h("input", { type: "checkbox" });
    const agreeButton = h("button.btn.pri", { type: "button", text: "同意并继续", disabled: true });
    checkbox.addEventListener("change", () => {
      agreeButton.disabled = !checkbox.checked;
    });
    agreeButton.addEventListener("click", async () => {
      agreeButton.disabled = true;
      agreeButton.textContent = "提交中…";
      try {
        await options.onAgree?.();
        // 同意成功后关掉阅读器：页面主体已由 reload 刷新，留在旧视图里只会显示过期状态
        close();
      } catch {
        // agree() 内部已经 toast 过失败原因，这里只把按钮恢复可用
        agreeButton.disabled = false;
        agreeButton.textContent = "同意并继续";
      }
    });
    foot.append(
      h(
        "label.tr-agree",
        null,
        checkbox,
        h("span", { text: `我已阅读并同意《${options.title}》（${options.version}）` }),
      ),
      // 走到这个分支就一定有同意动作（没有的话早在上面按只读状态渲染了）
      h("div.tr-actions", null, makeCloseButton(), agreeButton),
    );
  }

  sheet.append(
    h(
      "div.tr-head",
      null,
      h(
        "div.tr-meta",
        null,
        h("b", { text: options.title }),
        h("small", { text: options.meta }),
      ),
      fullButton,
      makeCloseButton(),
    ),
    bodyBox,
    foot,
  );

  // 全屏切换：只加/去一个类，正文节点不重建。
  // 容器宽度变了，文字会重新折行，机械还原 scrollTop 没有意义——
  // 这里按「切换前正文顶部那一段」重新定位，让读者眼睛盯着的那一行不动，
  // 这才是「阅读位置保留」在会重排的正文里的可实现版本。
  fullButton.addEventListener("click", () => {
    const anchor = firstVisibleChild(bodyBox);
    const anchorBefore = anchor
      ? anchor.getBoundingClientRect().top - bodyBox.getBoundingClientRect().top
      : 0;
    const full = reader.classList.toggle("full");
    fullButton.textContent = full ? "⤡ 退出全屏" : "⤢ 全屏阅读";
    if (anchor) {
      // 重排后把同一段拉回原来的视线位置；越界时浏览器会自动夹住
      const anchorNow = anchor.getBoundingClientRect().top - bodyBox.getBoundingClientRect().top;
      bodyBox.scrollTop += anchorNow - anchorBefore;
    }
  });

  function close(): void {
    reader.remove();
    document.removeEventListener("keydown", onKey);
    document.body.style.overflow = "";
  }

  function onKey(event: KeyboardEvent): void {
    if (event.key === "Escape") close();
  }

  for (const button of Array.from(reader.querySelectorAll<HTMLElement>(".tr-close"))) {
    button.addEventListener("click", close);
  }
  // 点遮罩关闭；点正文不关，避免误触丢掉阅读位置
  reader.addEventListener("click", (event) => {
    if (event.target === reader) close();
  });

  document.addEventListener("keydown", onKey);
  document.body.style.overflow = "hidden";
  document.body.appendChild(reader);
}

/* ------------------------------------------------------------------ */
/* 工具                                                                */
/* ------------------------------------------------------------------ */

/** 正文可视区顶部那一条：切换形态时用它当阅读锚点。 */
function firstVisibleChild(box: HTMLElement): HTMLElement | null {
  const top = box.getBoundingClientRect().top;
  // 正文外面套着渲染容器，先逐层下钻到「按段落排布」的那一层，锚点才有意义
  let container: HTMLElement = box;
  while (container.children.length === 1 && container.firstElementChild) {
    container = container.firstElementChild as HTMLElement;
  }
  // 取顶部往下第一条完整落在可视区里的段落；都在上方（读到底部附近）时不做处理
  for (const child of Array.from(container.children)) {
    if (child.getBoundingClientRect().top >= top - 1) return child as HTMLElement;
  }
  return null;
}

/** 当前用户是否已同意这一版：接口用 required + consent.version 判断，别只看 consent 非空。 */
function isAgreed(payload: TermsPayload, version: TermsVersion): boolean {
  return payload.consent !== null && payload.consent.version === version.version;
}

function versionMeta(version: TermsVersion): string {
  const parts = [`生效日期 ${formatDate(version.effective_at)}`];
  if (version.published_at) parts.push(`更新于 ${formatDateTime(version.published_at)}`);
  parts.push(`共 ${version.articles} 条`);
  return parts.join(" · ");
}

/**
 * 条款正文是纯文本（管理员在后台手填）。
 * 按行拆开，把「一、」「第 3 条」这类小标题加粗，其余按段落渲染；
 * 不做 Markdown 解析——正文允许出现任何字符，解析器只会变成又一个出错点。
 */
function renderBody(text: string): HTMLElement {
  const box = h("div", { style: { fontSize: "12.5px", lineHeight: "1.95", color: "var(--muted)" } });
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line) {
      box.append(h("div", { style: { height: "8px" } }));
      continue;
    }
    if (/^(?:[一二三四五六七八九十]+、|第[一二三四五六七八九十百零\d]+条)/.test(line)) {
      box.append(
        h("div", { style: { fontWeight: "700", color: "var(--text)", marginTop: "4px" }, text: line }),
      );
    } else {
      box.append(h("div", { text: line }));
    }
  }
  return box;
}

/** 更新摘要可能是一行一条，也可能用分号堆在一起，两种都拆。 */
function summaryItems(summary: string | null | undefined): string[] {
  if (!summary) return [];
  return summary
    .split(/\r?\n|；|;/)
    .map((item) => item.trim())
    .filter((item) => item.length > 0);
}

async function agree(versionId: number, reload: () => void): Promise<void> {
  try {
    await request("/terms/consent", { method: "POST", json: { version_id: versionId } });
    toastOk("已记录你的同意");
    reload();
  } catch (err) {
    toastError(err);
    throw err;
  }
}

/**
 * 打印：只留正文。
 * body 上挂一个标记类，配合 components.css 里的 @media print 规则隐藏其余部分；
 * 打完立刻摘掉，不然会影响下一次打印别的页面。
 */
function printTerms(): void {
  document.body.classList.add("printing-terms");
  const cleanup = () => document.body.classList.remove("printing-terms");
  window.addEventListener("afterprint", cleanup, { once: true });
  window.print();
  // Safari 有时不触发 afterprint，兜底兜一下
  window.setTimeout(cleanup, 1500);
}

function skeletonBlock(count: number): HTMLElement {
  return h(
    "div",
    null,
    ...Array.from({ length: count }, () =>
      h("div.skel", { style: { height: "120px", marginBottom: "14px" } }),
    ),
  );
}

function errorBlock(err: unknown, retry: () => void): HTMLElement {
  const message =
    err instanceof ApiError
      ? err.detail
        ? `${err.message}（${err.detail}）`
        : err.message
      : err instanceof Error
        ? err.message
        : String(err);
  return h(
    "div.card",
    null,
    qtip("err", "条款加载失败", message),
    h(
      "div",
      { style: { marginTop: "10px" } },
      h("button.btn", { type: "button", text: "重试", on: { click: retry } }),
    ),
  );
}
