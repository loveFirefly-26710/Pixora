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

import { h, mount } from "../core/dom";
import { currentPath, navigate } from "../core/router";
import { getState, impersonator, isAdmin, setUser, subscribe } from "../core/state";
import { request } from "../core/api";
import { toast, toastError } from "../core/toast";

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
  { path: "/recycle", label: "回收站", icon: "🗑", match: ["/recycle"] },
];
const ADMIN_NAV: NavItem[] = [
  { path: "/admin/users", label: "用户管理", icon: "👥", match: ["/admin/users"] },
  { path: "/admin/storage", label: "存储管理", icon: "💾", match: ["/admin/storage"] },
  { path: "/admin/quota", label: "配额与容量", icon: "📊", match: ["/admin/quota"] },
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
  /** 页面内容上方的提示条槽位（目前放免登录切换的提示） */
  banner: HTMLElement;
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

  // 品牌文字跟着站点名走：`data-brand` 是给 state.applySiteIdentity 改文字用的。
  // 站名还是默认的 Pixora 时保留原来的双色写法——那是 Logo，不是文字
  const name = (getState().site?.name ?? "").trim() || "Pixora";
  const logo = h("a.logo", { href: "#/", "aria-label": "回到广场首页" });
  if (name === "Pixora") {
    logo.innerHTML = `Pi<em>x</em>ora`;
  } else {
    logo.append(h("span", { "data-brand": "1", text: name }));
  }
  navbar.append(logo);

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
  // 退出登录放在侧边栏最后：它是账号级动作，和上面那些「去哪一页」不同类
  sidebar.append(
    h("div.grp", { text: "账号" }),
    h(
      "button.logout",
      { type: "button", "data-act": "logout" },
      h("span", { text: "⏻" }),
      h("span", { text: "退出登录" }),
    ),
  );
  return sidebar;
}

/**
 * 退出登录。
 *
 * 服务端登出是幂等的（没有会话也返回成功），所以这里不纠结失败：
 * 本地状态一定要清干净，否则用户点了「退出」却还看着自己的页面。
 */
export async function logoutNow(): Promise<void> {
  try {
    await request("/auth/logout", { method: "POST" });
  } catch (err) {
    toastError(err);
  }
  setUser(null);
  navigate("/login", { replace: true });
  toast("已退出登录");
}

/** 侧边栏的退出按钮走事件委托，避免每页各绑一次。 */
document.addEventListener("click", (event) => {
  const target = event.target as HTMLElement | null;
  if (target?.closest('[data-act="logout"]')) void logoutNow();
});

/**
 * 把顶栏的真实高度写进 `--nav-h`。
 *
 * 粘性元素（侧边栏、相册页的工具栏与相册栏）都按它对齐。写死一个数字不行：
 * 顶栏在窄屏 49px、宽屏 59px，里面放头像还是「登录」按钮、有没有搜索框
 * 都会让它变——量出来的才是对的。
 */
function syncNavHeight(navbar: HTMLElement): void {
  const height = Math.round(navbar.getBoundingClientRect().height);
  if (height > 0) {
    document.documentElement.style.setProperty("--nav-h", `${height}px`);
  }
}

/** 建外壳并返回页面内容挂载点。 */
export function mountShell(root: HTMLElement): HTMLElement {
  const navbar = renderNavbar();
  const tabbar = renderTabbar();
  const banner = h("div.banner-slot");
  const outlet = h("main.app-main");
  const sidebar = renderSidebar();

  // 提示条放在 outlet 之外：路由每次切换都会清空 outlet，
  // 放在里面的话「你正在以别人身份登录」这条提示会被一起清掉
  const shell = h("div.shell", null, sidebar, h("div.shell-body", null, banner, outlet));
  root.replaceChildren(navbar, shell, tabbar);

  refs = { shell, navbar, sidebar, tabbar, outlet, banner };
  refreshBanner();
  syncNavHeight(navbar);
  window.addEventListener("resize", () => {
    if (refs) syncNavHeight(refs.navbar);
  });
  subscribe(() => refreshNav());
  return outlet;
}

/** 填提示条槽位。没有要提示的东西时槽位留空（CSS 里 `:empty` 不占位置）。 */
function refreshBanner(): void {
  if (!refs) return;
  const info = impersonator();
  mount(refs.banner, info ? impersonationBanner(info, () => void stopImpersonation()) : null);
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
  // 换页时顶栏内容会变（搜索框、按钮），高度跟着量一次
  syncNavHeight(navbar);

  const tabbar = renderTabbar();
  refs.tabbar.replaceWith(tabbar);
  refs.tabbar = tabbar;

  if (refs.sidebar) refs.sidebar.remove();
  refs.sidebar = renderSidebar();
  if (refs.sidebar) refs.shell.prepend(refs.sidebar);

  refreshBanner();
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

/**
 * 免登录切换用户的提示条。
 *
 * 与「用户视图」那条不同：这一条说明的是**当前登录身份就是别人**，
 * 所以一切都按对方账号走（相册、上传、配额），只是随时能一键切回管理员。
 * 这条提示是页面上唯一说明「你现在是谁」的东西，固定在内容顶部。
 */
export function impersonationBanner(
  info: { uid: string; email: string },
  onStop: () => void,
): HTMLElement {
  return h(
    "div",
    {
      style: {
        background: "var(--warning-soft, #fef3c7)",
        border: "1px solid #fcd34d",
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
      { style: { flex: "1", minWidth: "200px", fontSize: "13px", color: "#92400e" } },
      h("b", { text: `你正在以 u_${info.uid}（${info.email}）的身份登录` }),
      h("div", {
        style: { fontSize: "12px", marginTop: "2px" },
        text: "这是管理员免登录切换：当前操作都算在对方名下。刷新页面后仍可切回。",
      }),
    ),
    h("button.btn.sm", { type: "button", text: "切回管理员", on: { click: onStop } }),
  );
}

/** 切回管理员：服务端把会话换回去，前端刷新一次拿到管理员的身份。 */
export async function stopImpersonation(): Promise<void> {
  try {
    await request("/admin/impersonate/stop", { method: "POST" });
    toast("已切回管理员");
    // 身份换了，缓存里的用户信息一律作废：整页重载最省心也最不容易出错
    window.location.reload();
  } catch (err) {
    toastError(err);
  }
}

/** 页面自己决定要不要在顶部挂提示条。 */
export function topBanner(): HTMLElement | null {
  const info = impersonator();
  if (!info) return null;
  return impersonationBanner(info, () => void stopImpersonation());
}
