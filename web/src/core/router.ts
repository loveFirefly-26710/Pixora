/**
 * 哈希路由。
 *
 * 用 hash 而不是 history API：单二进制部署时服务端只需把未知路径回落到 index.html
 * 即可，不需要为每条前端路由配 rewrite，也就少一处出错的地方。
 *
 * 路由表在这里集中声明，权限守卫（需登录 / 需管理员）也在这里判——
 * 页面自己不写权限判断，避免漏一处就露出管理入口。
 */

import { getState, isAdmin, isLoggedIn } from "./state";

export interface PageContext {
  /** 路径参数，如 `/admin/users/:id` 的 `{ id: "4120" }` */
  params: Record<string, string>;
  /** `#/albums?album_id=7` 里的查询参数 */
  query: URLSearchParams;
  /** 页面内容挂载点 */
  outlet: HTMLElement;
  /** 完整路径（不含查询部分） */
  path: string;
}

export type PageHandler = (ctx: PageContext) => Promise<void> | void;

interface Route {
  pattern: string;
  segments: string[];
  handler: PageHandler;
  auth: boolean;
  admin: boolean;
}

const routes: Route[] = [];
let outlet: HTMLElement | null = null;
let notFound: PageHandler | null = null;
let onNavigate: (() => void) | null = null;

export function route(
  pattern: string,
  handler: PageHandler,
  options: { auth?: boolean; admin?: boolean } = {},
): void {
  routes.push({
    pattern,
    segments: pattern.split("/").filter(Boolean),
    handler,
    auth: options.auth ?? false,
    admin: options.admin ?? false,
  });
}

export function setNotFound(handler: PageHandler): void {
  notFound = handler;
}

export function setOutlet(element: HTMLElement): void {
  outlet = element;
}

/** 每次导航后的回调（用高亮导航项、滚到顶部）。 */
export function setNavigateHook(hook: () => void): void {
  onNavigate = hook;
}

export function currentHash(): string {
  return location.hash.replace(/^#/, "") || "/";
}

export function currentPath(): string {
  return currentHash().split("?")[0] || "/";
}

export function navigate(to: string, options: { replace?: boolean } = {}): void {
  const target = to.startsWith("#") ? to : `#${to}`;
  if (options.replace) {
    history.replaceState(null, "", target);
    void dispatch();
    return;
  }
  location.hash = target;
}

/** 就地替换查询参数，不刷新页面。 */
export function updateQuery(patch: Record<string, string | null>): void {
  const hash = currentHash();
  const path = hash.split("?")[0];
  const params = new URLSearchParams(hash.includes("?") ? hash.split("?")[1] : "");
  for (const [key, value] of Object.entries(patch)) {
    if (value === null || value === "") params.delete(key);
    else params.set(key, value);
  }
  const encoded = params.toString();
  navigate(encoded ? `${path}?${encoded}` : path, { replace: true });
}

function match(segments: string[]): { route: Route; params: Record<string, string> } | null {
  for (const candidate of routes) {
    if (candidate.segments.length !== segments.length) continue;
    const params: Record<string, string> = {};
    let ok = true;
    for (let index = 0; index < segments.length; index += 1) {
      const pattern = candidate.segments[index];
      const actual = segments[index];
      if (pattern.startsWith(":")) {
        params[pattern.slice(1)] = decodeURIComponent(actual);
      } else if (pattern !== actual) {
        ok = false;
        break;
      }
    }
    if (ok) return { route: candidate, params };
  }
  return null;
}

async function dispatch(): Promise<void> {
  if (!outlet) return;
  const hash = currentHash();
  const [path, queryString] = hash.split("?");
  const query = new URLSearchParams(queryString ?? "");
  const segments = (path ?? "/").split("/").filter(Boolean);

  const matched = match(segments);
  outlet.replaceChildren();
  onNavigate?.();

  if (!matched) {
    if (notFound) {
      await notFound({ params: {}, query, outlet, path: path ?? "/" });
      return;
    }
    outlet.textContent = "页面不存在";
    return;
  }

  const { route, params } = matched;

  // 权限守卫：先看未登录，再看管理员。跳转时把来路带上，登录后能回原页。
  if (route.auth && !isLoggedIn()) {
    navigate(`/login?next=${encodeURIComponent(hash)}`, { replace: true });
    return;
  }
  if (route.admin && !isAdmin()) {
    outlet.append(renderForbidden());
    return;
  }
  // 管理员直接设过密码的账号：改掉之前哪儿也去不了，服务端同样只放行
  // /me 与改密码。放在这里而不是各页面自己判——登录成功后那一次跳转
  // 也走 dispatch，所以「登录完先落地再被 403 弹回来」这种事不会发生。
  // 不限于需要登录的路由：带标记的人落在广场也是不能用的状态
  if (path !== "/password-required" && getState().user?.must_change_password) {
    navigate("/password-required", { replace: true });
    return;
  }
  // 会话与站点信息没到位时不渲染页面，避免闪一下空架子
  if (!getState().ready && route.auth) {
    outlet.append(renderLoading());
    return;
  }

  await route.handler({ params, query, outlet, path: path ?? "/" });
}

function renderForbidden(): HTMLElement {
  const box = document.createElement("div");
  box.className = "empty";
  box.innerHTML =
    '<div class="big">🔒</div><div>这个页面需要管理员权限。</div>' +
    '<div class="tip" style="margin-top:8px">如果你想访问自己的空间，去「我的相册」。</div>';
  return box;
}

function renderLoading(): HTMLElement {
  const box = document.createElement("div");
  box.className = "pad";
  box.innerHTML =
    '<div class="grid igrid" style="margin-top:8px">' +
    Array.from({ length: 8 }, () => '<div class="skel" style="height:150px"></div>').join("") +
    "</div>";
  return box;
}

export function startRouter(): void {
  window.addEventListener("hashchange", () => void dispatch());
  void dispatch();
}

/** 让页面在数据变化后重新渲染自己。 */
export function refresh(): void {
  void dispatch();
}
