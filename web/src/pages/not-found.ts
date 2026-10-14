/**
 * 404。
 *
 * 不做自动跳转：用户可能是点了一个失效的链接，直接把人甩回首页会让人以为
 * 是自己点错了。给一个明确的说明与两个出口。
 */

import { h } from "../core/dom";
import { navigate, type PageContext } from "../core/router";
import { emptyState } from "../ui/widgets";

export async function render(ctx: PageContext): Promise<void> {
  ctx.outlet.append(
    h(
      "div.pad",
      null,
      emptyState(
        "🧭",
        `页面不存在：${ctx.path}`,
        h(
          "div.flexline",
          { style: { justifyContent: "center" } },
          h("button.btn.pri", {
            type: "button",
            text: "回到广场",
            on: { click: () => navigate("/") },
          }),
          h("button.btn", {
            type: "button",
            text: "我的相册",
            on: { click: () => navigate("/albums") },
          }),
        ),
      ),
    ),
  );
}
