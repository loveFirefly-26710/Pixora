/**
 * 首页公告的展示组件。
 *
 * 广场弹窗和后台「预览」共用同一份结构：运营在后台看到的样式就是用户看到的，
 * 不会出现「预览一个样、上线另一个样」。
 */

import { h } from "../core/dom";
import { openModal, type ModalHandle } from "../core/modal";
import { FIREFLY_MARK } from "./brand";

/** 公告正文卡片（不含模态壳）。 */
export function announcementBody(title: string, body: string): HTMLElement {
  return h(
    "div.announcement",
    null,
    h(
      "div.announcement-sky",
      { "aria-hidden": "true" },
      h("span.announcement-spark.s1"),
      h("span.announcement-spark.s2"),
      h("span.announcement-spark.s3"),
      h("span.announcement-ring"),
    ),
    h(
      "div.announcement-head",
      null,
      h("span.announcement-mark", { html: FIREFLY_MARK, "aria-hidden": "true" }),
      h(
        "div.announcement-heading",
        null,
        h("div.announcement-eyebrow", { text: "FIREFLY · NIGHT DISPATCH" }),
        h("h2.announcement-title", { text: title }),
        h("div.announcement-meta", { text: "流萤 · 致每一位夜航者" }),
      ),
    ),
    h("div.announcement-rule", { "aria-hidden": "true" }),
    h("div.announcement-body", { text: body }),
    h(
      "div.announcement-sign",
      null,
      h("span", { text: "——" }),
      h("b", { text: "流萤" }),
      h("span", { text: "· 夜航播报" }),
    ),
  );
}

/** 打开公告弹窗。带主题壳；footer 由调用方决定（确认 / 预览关闭）。 */
export function openAnnouncementModal(options: {
  title: string;
  body: string;
  footer: Node[];
  /** 预览允许关闭；线上公告默认必须点确认 */
  dismissable?: boolean;
}): ModalHandle {
  const handle = openModal({
    title: "站点公告",
    body: announcementBody(options.title.trim() || "站点公告", options.body),
    reading: true,
    dismissable: options.dismissable ?? false,
    footer: options.footer,
  });
  handle.root.classList.add("modal-announcement");
  return handle;
}
