/**
 * 品牌标记与主题图标。
 *
 * 萤火虫的四片薄翼用同一条路径镜像出来，中间一颗微金光点。
 * 不引用任何官方角色素材，避免把站点变成同人图站。
 */

import { h } from "../core/dom";

export const FIREFLY_MARK = `<svg viewBox="0 0 48 48" aria-hidden="true" focusable="false">
  <g fill="none" stroke="currentColor" stroke-width="1.35" stroke-linecap="round">
    <path d="M24 9 C17.5 13.5 15 19.5 16.6 26 C18.1 32.2 21.2 35.5 24 37.4 C26.8 35.5 29.9 32.2 31.4 26 C33 19.5 30.5 13.5 24 9Z" opacity=".82"/>
    <path d="M22.8 18.5 C14.5 13.2 8.9 15.1 7.2 20.6 C5.4 26.4 9.6 31.4 17.6 32.1"/>
    <path d="M25.2 18.5 C33.5 13.2 39.1 15.1 40.8 20.6 C42.6 26.4 38.4 31.4 30.4 32.1"/>
    <path d="M20.8 28.6 C14.2 27.8 10.5 30.9 10.8 36.2 C11.1 40.9 15.2 43.5 20.6 41.6"/>
    <path d="M27.2 28.6 C33.8 27.8 37.5 30.9 37.2 36.2 C36.9 40.9 32.8 43.5 27.4 41.6"/>
  </g>
  <circle class="brand-core" cx="24" cy="24.2" r="2.4"/>
</svg>`;

export const SUN_ICON = `<svg class="icon-sun" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" aria-hidden="true" focusable="false">
  <circle cx="12" cy="12" r="4"/>
  <path d="M12 2v2M12 20v2M4.93 4.93l1.42 1.42M17.65 17.65l1.42 1.42M2 12h2M20 12h2M4.93 19.07l1.42-1.42M17.65 6.35l1.42-1.42"/>
</svg>`;

export const MOON_ICON = `<svg class="icon-moon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" focusable="false">
  <path d="M20 14.5A8 8 0 0 1 9.5 4 8.5 8.5 0 1 0 20 14.5Z"/>
</svg>`;

/**
 * 品牌锁定组合：翼标 + 站名 + 一行很小的英文副标。
 *
 * 站名走 `[data-brand]`，state.applySiteIdentity 改了名字之后这个节点
 * 会跟着更新；默认的 Pixora 保留双色写法。
 */
export function brandLockup(name: string, sub = "FIREFLY ARCHIVE"): HTMLElement {
  const label =
    name === "Pixora"
      ? h("span.logo-name", { html: "Pi<em>x</em>ora" })
      : h("span.logo-name", null, h("span", { "data-brand": "1", text: name }));

  return h(
    "span.logo-lockup",
    null,
    h("span.logo-mark", { html: FIREFLY_MARK }),
    h("span.logo-text", null, label, h("span.logo-sub", { text: sub })),
  );
}
