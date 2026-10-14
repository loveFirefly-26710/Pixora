/**
 * 应用外壳：顶部导航、左侧边栏、底部 tab、抽屉。
 *
 * 三处导航形态由断点决定（PC 侧边栏 / 移动底部 tab / 后台抽屉），
 * 结构一次建好，之后只切 class 与高亮，不重建 DOM——
 * 重建会让移动端的滚动位置和抽屉动画都出问题。
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

const USER_NAV: NavItem[] = [
  PLAZA,
  ALBUMS,
  UPLOAD,
  { path: "/account", label: "账号信息", icon: "◇", match: ["/account"] },
  { path: "/account/security", label: "安全", icon: "🔒", match: ["/account/security"] },
  { path: "/account/api", label: "API Token", icon: "🔑", match: ["/account/api"] },
  { path: "/account/terms", label: "服务调整 · 服务条款", icon: "📄", match: ["/account/terms"] },
  { path: "/docs/api", label: "API 文档", icon: "📘", match: ["/docs"] },
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

let refs: {
  shell: HTMLElement;
  navbar: HTMLElement;
  tabbar: HTMLElement;
  drawer: HTMLElement;
  outlet: HTMLElement;
} | null = null;

function active(items: NavItem[]): boolean {
  const path = currentPath();
  return items.some((item) => (item.match ?? [item.path]).some((prefix) => path.startsWith(prefix)));
}

function navLink(item: NavItem): HTMLElement {
  const path = currentPath();
  const isActive = (item.match ?? [item.path]).some((prefix) => path.startsWith(prefix));
  return h("a", {
    href: `#${item.path}`,
    class: isActive ? "on" : "",
    "data-nav": item.path,
  }, h("span", { text: item.icon }), h("span", { text: item.label }));
}

export function renderNavbar(): HTMLElement {
  const state = getState();
  const site = state.site;
  const user = state.user;
  const navbar = h("div.navbar");

  navbar.append(
    h("a.logo", { href: "#/", html: `Pi<em>x</em>ora` }),
    // 导航收进抽屉后，这个按钮是所有页面唯一的入口，所以不分断点、始终显示
    h("button.btn.sm.gho", {
      type: "button",
      text: "☰",
      "aria-label": "打开导航菜单",
      on: { click: () => toggleDrawer(true) },
    }),
  );

  // 搜索框只在广场与相册页出现
  const path = currentPath();
  if (path === "/" || path.startsWith("/albums") || path === "/plaza") {
    const input = h("input", { placeholder: "搜索标签、关键词…", "aria-label": "搜索" });
    const form = h("form.search", { role: "search" }, "⌕", input);
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      const keyword = input.value.trim();
      const target = path.startsWith("/albums") ? "/albums" : "/";
      navigate(keyword ? `${target}?q=${encodeURIComponent(keyword)}` : target);
    });
    navbar.append(form);
  } else if (site) {
    navbar.append(h("span.hint", { text: site.name, style: { fontSize: "12px", color: "var(--muted)" } }));
  }

  navbar.append(h("span.spacer"));

  if (user) {
    if (isAdmin()) {
      navbar.append(
        h("a.btn.gho.sm", { href: "#/admin/users", text: "管理后台" }),
      );
    }
    navbar.append(
      h("a.btn.gho.sm", { href: "#/albums", text: "我的相册" }),
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

function renderDrawer(): HTMLElement {
  const drawer = h("div.drawer", { "aria-hidden": "true" });
  const mask = h("div.mask");
  mask.addEventListener("click", () => toggleDrawer(false));
  const sheet = h("div.sheet");
  const close = h("button.btn.sm.gho", {
    type: "button",
    text: "✕",
    "aria-label": "关闭菜单",
    on: { click: () => toggleDrawer(false) },
  });
  sheet.append(h("div.flexline", { style: { marginBottom: "8px" } }, h("b", { text: "导航" }), h("span.spacer"), close));

  const user = getState().user;
  const groups: Array<[string, NavItem[]]> = user
    ? [["用户中心", USER_NAV], ["管理后台", isAdmin() ? ADMIN_NAV : []], ["关于", [{ path: "/docs/api", label: "API 文档", icon: "📘" }]]]
    : [["浏览", [PLAZA]], ["关于", [{ path: "/docs/api", label: "API 文档", icon: "📘" }]]];

  for (const [title, items] of groups) {
    if (items.length === 0) continue;
    sheet.append(h("div.grp", { text: title }));
    for (const item of items) {
      const link = navLink(item);
      link.addEventListener("click", () => toggleDrawer(false));
      sheet.append(link);
    }
  }

  drawer.append(mask, sheet);
  return drawer;
}

export function toggleDrawer(open: boolean): void {
  if (!refs) return;
  refs.drawer.classList.toggle("on", open);
  refs.drawer.setAttribute("aria-hidden", String(!open));
  if (!open) {
    window.setTimeout(() => {
      if (refs) refs.drawer.classList.remove("on");
    }, 0);
  }
}

/** 建外壳并返回页面内容挂载点。 */
export function mountShell(root: HTMLElement): HTMLElement {
  const navbar = renderNavbar();
  const tabbar = renderTabbar();
  const drawer = renderDrawer();
  const outlet = h("main.app-main");

  // 按 UI 原型：界面里没有常驻侧边栏，导航全部走顶栏的菜单抽屉
  const shell = h("div.shell", null, outlet);
  root.replaceChildren(navbar, shell, tabbar, drawer);

  refs = { shell, navbar, tabbar, drawer, outlet };
  subscribe(() => refreshNav());
  return outlet;
}

export function outlet(): HTMLElement {
  if (!refs) throw new Error("外壳还没挂载");
  return refs.outlet;
}

/** 只重建三处导航，不动页面内容。 */
export function refreshNav(): void {
  if (!refs) return;
  const navbar = renderNavbar();
  refs.navbar.replaceWith(navbar);
  refs.navbar = navbar;

  const tabbar = renderTabbar();
  refs.tabbar.replaceWith(tabbar);
  refs.tabbar = tabbar;

  refs.drawer.replaceWith(renderDrawer());
  refs.drawer = refs.shell.parentElement?.querySelector(".drawer") ?? refs.drawer;
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

export { PLAZA, ALBUMS, UPLOAD, MINE, ADMIN_NAV, USER_NAV };
