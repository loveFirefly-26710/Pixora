/**
 * 应用外壳：顶部导航 + 左侧边栏（≥1024px）+ 底部 tab（<1024px）。
 *
 * 顶栏只放四样东西：图标（点回广场）、搜索框、账号（登录按钮或头像）、API 文档。
 * 页面之间的导航走侧边栏；窄屏收进底部 tab——不再有 ☰ 抽屉，
 * 同一个入口两套形态只会让人猜「导航到底在哪」。
 *
 * 三种形态的 DOM 一次建好，之后只切 class 与高亮，不重建——
 * 重建会让移动端的滚动位置出问题。
 */

import { h } from "../core/dom";
import { currentPath, navigate } from "../core/router";
import { getState, isAdmin, subscribe } from "../core/state";

interface NavItem {
  path: string;
  label: string;
  icon: string;
  /** 匹配前缀，`/albums` 命中 `/albums/7` */
  match?: string[];
  adminOnly?: boolean;
}

const PLAZA: NavItem = { path: "/", label: "广场", icon: "◍", match: ["/", "/plaza"] };
const ALBUMS: NavItem = { path: "/albums", label: "相册", icon: "▤", match: ["/albums"] };
const UPLOAD: NavItem = { path: "/upload", label: "上传", icon: "＋", match: ["/upload"] };
const MINE: NavItem = { path: "/account", label: "我的", icon: "◇", match: ["/account"] };

/** 侧边栏的「用户中心」。广场不在这里：顶栏的品牌图标就是回广场的入口。 */
const USER_NAV: NavItem[] = [
  ALBUMS,
  UPLOAD,
  { path: "/account", label: "账号信息", icon: "◇", match: ["/account"] },
  { path: "/account/security", label: "安全", icon: "🔒", match: ["/account/security"] },
  { path: "/account/api", label: "API Token", icon: "🔑", match: ["/account/api"] },
  { path: "/account/terms", label: "服务调整 · 服务条款", icon: "📄", match: ["/account/terms"] },
];

const ADMIN_NAV: NavItem[] = [
  { path: "/admin/users", label: "用户管理", icon: "👥", match: ["/admin/users"] },
  { path: "/admin/storage", label: "存储管理", icon: "💾", match: ["/admin/storage"] },
  { path: "/admin/quota", label: "配额与容量", icon: "📊", match: ["/admin/quota"] },
  { path: "/admin/ratelimit", label: "频率限制", icon: "⏱", match: ["/admin/ratelimit"] },
  { path: "/admin/ip", label: "IP 黑名单", icon: "🛡", match: ["/admin/ip"] },
  { path: "/admin/settings", label: "系统设置", icon: "⚙", match: ["/admin/settings"] },
  { path: "/admin/terms", label: "服务条款", icon: "📄", match: ["/admin/terms"] },
];

const ABOUT_NAV: NavItem[] = [{ path: "/docs/api", label: "API 文档", icon: "📘", match: ["/docs"] }];

let refs: {
  shell: HTMLElement;
  navbar: HTMLElement;
  sidebar: HTMLElement | null;
  tabbar: HTMLElement;
  outlet: HTMLElement;
} | null = null;

/**
 * 当前路径命中的、最长的那条 match。
 *
 * 只有它对应的条目该高亮：`/account` 与 `/account/security` 是前缀关系，
 * 「谁 startsWith 谁」会让「账号信息」和「安全」同时亮着。
 * 取最长匹配之后，路径越具体，亮的条目就越具体。
 */
function bestMatch(): string | null {
  const path = currentPath();
  let best: string | null = null;
  const all = [...USER_NAV, ...ADMIN_NAV, ...ABOUT_NAV, PLAZA, ALBUMS, UPLOAD, MINE];
  for (const item of all) {
    for (const prefix of item.match ?? [item.path]) {
      if (path.startsWith(prefix) && (best === null || prefix.length > best.length)) best = prefix;
    }
  }
  return best;
}

function active(items: NavItem[]): boolean {
  const best = bestMatch();
  return best !== null && items.some((item) => (item.match ?? [item.path]).includes(best));
}

function navLink(item: NavItem): HTMLElement {
  const best = bestMatch();
  const isActive = best !== null && (item.match ?? [item.path]).includes(best);
  return h("a", {
    href: `#${item.path}`,
    class: isActive ? "on" : "",
    "data-nav": item.path,
  }, h("span", { text: item.icon }), h("span", { text: item.label }));
}

/** 顶栏。图标 + 搜索框 + 账号 + API 文档，别的一律不放。 */
export function renderNavbar(): HTMLElement {
  const user = getState().user;
  const navbar = h("div.navbar");

  navbar.append(
    h("a.logo", { href: "#/", html: `Pi<em>x</em>ora`, "aria-label": "回到广场首页" }),
  );

  // 搜索框固定在顶栏（不再只在广场与相册页出现）：在哪儿搜都落到当前板块的结果页
  const path = currentPath();
  const target = path.startsWith("/albums") ? "/albums" : "/";
  const input = h("input", { placeholder: "搜索标签、关键词…", "aria-label": "搜索" });
  // 回填当前查询串：刷新或前进后退后，搜索框和地址栏说同一件事
  const query = new URLSearchParams(location.hash.split("?")[1] ?? "").get("q");
  if (query) input.value = query;
  const form = h("form.search", { role: "search" }, "⌕", input);
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const keyword = input.value.trim();
    navigate(keyword ? `${target}?q=${encodeURIComponent(keyword)}` : target);
  });
  navbar.append(form);

  navbar.append(h("span.spacer"));

  if (user) {
    navbar.append(
      h("a.avatar", {
        href: "#/account",
        class: user.role === "admin" ? "admin" : "",
        text: (user.email[0] ?? "U").toUpperCase(),
        "aria-label": "账号",
      }),
    );
  } else {
    // 未登录只留一个入口，注册挪进登录页（右下角）。
    // 游客看到两个按钮会先纠结「我该点哪个」，而这两步的落点本来就是同一个表单。
    navbar.append(h("a.btn.pri", { href: "#/login", text: "登录" }));
  }
  navbar.append(h("a.btn.gho.sm", { href: "#/docs/api", text: "API 文档" }));

  return navbar;
}

export function renderTabbar(): HTMLElement {
  const tabbar = h("nav.tabbar", { "aria-label": "主导航" });
  const user = getState().user;
  const items = user ? [PLAZA, ALBUMS, UPLOAD, MINE] : [PLAZA, { ...ALBUMS }, MINE];
  for (const item of items) {
    const isActive = active([item]);
    tabbar.append(
      h(
        "a.it",
        { href: `#${item.path}`, class: isActive ? "on" : "", "aria-current": isActive ? "page" : null },
        h("span.ic", { text: item.icon }),
        h("span", { text: item.label }),
      ),
    );
  }
  return tabbar;
}

/**
 * 侧边栏。游客没有（广场页由顶栏与搜索框自足），登录后才有导航可讲。
 *
 * 广场页也不显示：那一页是「浏览」的入口，一进来就压一条导航栏反而挡图。
 * 顶栏的品牌图标与底部 tab 都能回广场，不缺这一个入口。
 */
function renderSidebar(): HTMLElement | null {
  const user = getState().user;
  if (!user) return null;
  const path = currentPath();
  if (path === "/" || path === "/plaza") return null;

  const sidebar = h("nav.sidebar", { "aria-label": "侧边导航" });
  const groups: Array<[string, NavItem[]]> = [
    ["用户中心", USER_NAV],
    ["管理后台", isAdmin() ? ADMIN_NAV : []],
    ["关于", ABOUT_NAV],
  ];
  for (const [title, items] of groups) {
    if (items.length === 0) continue;
    sidebar.append(h("div.grp", { text: title }));
    for (const item of items) sidebar.append(navLink(item));
  }
  return sidebar;
}

/** 建外壳并返回页面内容挂载点。 */
export function mountShell(root: HTMLElement): HTMLElement {
  const navbar = renderNavbar();
  const tabbar = renderTabbar();
  const outlet = h("main.app-main");
  const sidebar = renderSidebar();

  const shell = h("div.shell", null, sidebar, outlet);
  root.replaceChildren(navbar, shell, tabbar);

  refs = { shell, navbar, sidebar, tabbar, outlet };
  subscribe(() => refreshNav());
  return outlet;
}

export function outlet(): HTMLElement {
  if (!refs) throw new Error("外壳还没挂载");
  return refs.outlet;
}

/**
 * 重建导航，不动页面内容。每次路由变化都会走这里。
 *
 * 侧边栏该不该出现取决于「登录没登录 + 在哪个页面」，所以不能只建一次：
 * 进广场时摘掉、进相册时装回来，SHELL 就是那一个 flex 行，
 * 没有侧边栏时整行给内容，不会空出一列。
 */
export function refreshNav(): void {
  if (!refs) return;
  const navbar = renderNavbar();
  refs.navbar.replaceWith(navbar);
  refs.navbar = navbar;

  const tabbar = renderTabbar();
  refs.tabbar.replaceWith(tabbar);
  refs.tabbar = tabbar;

  if (refs.sidebar) refs.sidebar.remove();
  refs.sidebar = renderSidebar();
  if (refs.sidebar) refs.shell.prepend(refs.sidebar);
}

/** 判断当前是否在某个路径下（页面内部高亮用）。 */
export function under(prefix: string): boolean {
  return currentPath().startsWith(prefix);
}

/**
 * 管理员「用户视图」提示条。
 *
 * 进入用户视图后管理员自己的相册完全隐藏，界面上只有目标用户的空间——
 * 这条提示是唯一说明当前上下文的东西，所以固定在内容顶部。
 */
export function userViewBanner(
  uid: string,
  email: string,
  onSwitch: () => void,
  onExit: () => void,
): HTMLElement {
  return h(
    "div",
    {
      style: {
        background: "var(--purple-soft)",
        border: "1px solid #ddd6fe",
        borderRadius: "10px",
        padding: "10px 14px",
        marginBottom: "14px",
        display: "flex",
        gap: "10px",
        alignItems: "center",
        flexWrap: "wrap",
      },
    },
    h(
      "div",
      { style: { flex: "1", minWidth: "200px", fontSize: "13px", color: "#5b21b6" } },
      h("b", { text: `正在查看用户 u_${uid}（${email}）的空间` }),
      h("div", { style: { fontSize: "12px", marginTop: "2px" }, text: "管理员自身相册已隐藏 · 本次访问已记录审计日志" }),
    ),
    h("button.btn.sm", { type: "button", text: "切换用户", on: { click: onSwitch } }),
    h("button.btn.sm", { type: "button", text: "退出用户视图", on: { click: onExit } }),
  );
}

export { PLAZA, ALBUMS, UPLOAD, MINE, ADMIN_NAV, USER_NAV, ABOUT_NAV };
