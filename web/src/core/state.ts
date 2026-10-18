/**
 * 应用状态。
 *
 * 只有一份：当前会话（用户、站点配置、CSRF）。页面按需订阅，
 * 状态变了就重渲染自己——不做虚拟 DOM，也不做细粒度依赖追踪，
 * 刷新整页在图片数几十的量级上是无感的。
 */

import { request, cacheSite, currentCsrfToken, setCsrfToken, type ApiError } from "./api";
import { absUrl } from "./config";
import type { ImpersonatorInfo, SiteInfo, UserProfile } from "./types";

type Listener = () => void;

interface AppState {
  site: SiteInfo | null;
  user: UserProfile | null;
  /**
   * 管理员免登录切换用户时，这里是「从谁切过来的」。
   * 由服务端的 Cookie 决定，前端只负责显示提示条与切回按钮。
   */
  impersonator: ImpersonatorInfo | null;
  /** 管理员正在查看的用户（用户视图）；普通用户恒为 null */
  viewingUid: string | null;
  ready: boolean;
}

const state: AppState = {
  site: null,
  user: null,
  impersonator: null,
  viewingUid: null,
  ready: false,
};

const listeners = new Set<Listener>();

export function subscribe(listener: Listener): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function notify(): void {
  for (const listener of listeners) listener();
}

export function getState(): Readonly<AppState> {
  return state;
}

export function site(): SiteInfo | null {
  return state.site;
}

export function currentUser(): UserProfile | null {
  return state.user;
}

export function isLoggedIn(): boolean {
  return state.user !== null;
}

export function isAdmin(): boolean {
  return state.user?.role === "admin";
}

/**
 * 这个账号能不能被当前管理员操作。
 *
 * 安装时创建的管理员是这套部署的主钥匙：别的管理员不能改它、切进去用、
 * 处置或删掉，它自己操作自己不受限。服务端按同一条规则拒，界面这里提前
 * 把入口关掉，免得点了才吃一句报错。
 */
export function canOperateAccount(uid: string): boolean {
  const installUid = state.user?.install_admin_uid ?? null;
  if (!installUid || uid !== installUid) return true;
  return uid === state.user?.uid;
}

/**
 * 账号在界面上怎么称呼。
 *
 * 访客账号叫「访客」：它没有主人，邮箱又是 RFC 2606 的保留域，把
 * `visitor@pixora.invalid` 摆在列表和弹窗标题里既占宽度又读不懂。
 * 真实邮箱在详情页的基本信息里仍然看得到。
 */
export function accountLabel(user: { guest?: boolean; email: string }): string {
  return user.guest ? "访客" : user.email;
}

export function addons(): { baseUrl: string; uploadPrefix: string } {
  // 站点信息里没有地址时退回当前源；部署时必须配好 PIXORA_BASE_URL，
  // 否则反代后面直链会指到内网。
  return {
    baseUrl: state.site?.base_url || location.origin,
    uploadPrefix: state.site?.upload_prefix ?? "images",
  };
}

/** 拼直链。缩略图走 `?thumb=2`（版本号，见后端 `direct_url` 的说明）。 */
export function directUrl(hash: string, ext: string, thumb = false): string {
  const { baseUrl, uploadPrefix } = addons();
  const suffix = thumb ? "?thumb=2" : "";
  return `${baseUrl.replace(/\/$/, "")}/${uploadPrefix}/${hash}.${ext}${suffix}`;
}

/** 启动时拉一次站点信息，未登录也能看到站名与注册状态。 */
export async function loadSite(): Promise<void> {
  try {
    const info = await request<SiteInfo>("/site");
    state.site = info;
    cacheSite(info as unknown as Record<string, unknown>);
  } catch {
    // 站点信息拿不到也要能渲染：用一份保守的默认值
    state.site = state.site ?? {
      name: "Pixora",
      base_url: location.origin,
      upload_prefix: "images",
      registration_open: false,
      registration_closed_reason: "服务暂时不可用",
      plaza_public: true,
      formats: [],
      max_file_bytes: 20 * 1024 * 1024,
      max_batch_size: 1000,
      max_pixels: 80_000_000,
      max_width: 0,
      max_height: 0,
      queue_depth: 500,
      thumb_client: true,
      thumb_server_fallback: false,
      appeal_email: null,
      background: null,
      default_avatar: null,
      // 拿不到站点信息时一个第三方入口都不摆：按钮点下去大概率也是报错
      login_methods: { github: false, google: false, qq: false, wechat: false, phone: false },
      installed: true,
      plaza_grid_columns: 3,
      // 站点信息拿不到时不自动刷新：宁可少一个效果，也不替管理员做主
      plaza_auto_refresh_seconds: 0,
      music: { enabled: false, source: "api" },
      recycle_retention_days: 30,
      terms: { required: false, version: null, articles: 0 },
    };
  }
  applySiteIdentity();
  notify();
}

/**
 * 站点名与背景图落进 DOM。
 *
 * 站名改完要刷新才生效是不够的：它在浏览器标签页上，用户一眼就能看到。
 * 背景图同理——设置里存了却没有哪一行 CSS 用它，等于这个设置是摆设。
 */
export function applySiteIdentity(): void {
  const site = state.site;
  if (!site) return;

  const name = (site.name ?? "").trim() || "Pixora";
  document.title = name;

  // 品牌文字也跟着站名走：顶栏那个「Pixora」是硬编码的话，改了站名
  // 会出现「标签页是新名字、页头还是旧名字」的错位
  for (const node of Array.from(document.querySelectorAll<HTMLElement>("[data-brand]"))) {
    node.textContent = name;
  }

  // 背景图可能是后端上传的那张（站内相对路径），同源直接用
  const background = absUrl(site.background).trim();
  const root = document.documentElement;
  if (background) {
    root.style.setProperty("--site-bg-image", `url("${background}")`);
    document.body.classList.add("has-site-bg");
  } else {
    root.style.removeProperty("--site-bg-image");
    document.body.classList.remove("has-site-bg");
  }
}

export function impersonator(): ImpersonatorInfo | null {
  return state.impersonator;
}

export function setImpersonator(info: ImpersonatorInfo | null): void {
  state.impersonator = info;
  notify();
}

/** 拉当前用户。未登录返回 null（不是错误）。 */
export async function loadMe(): Promise<UserProfile | null> {
  try {
    const payload = await request<{
      user: UserProfile;
      csrf_token?: string;
      impersonator?: ImpersonatorInfo | null;
    }>("/me");
    state.user = payload.user;
    state.impersonator = payload.impersonator ?? null;
    if (payload.csrf_token) setCsrfToken(payload.csrf_token);
    notify();
    return state.user;
  } catch (err) {
    const apiError = err as ApiError;
    if (apiError.status === 401) {
      // 会话超时/被吊销都走这里：清掉身份，让路由守卫把用户送回登录页
      state.user = null;
      state.impersonator = null;
      notify();
      return null;
    }
    throw err;
  }
}

export function setUser(user: UserProfile | null): void {
  state.user = user;
  if (!user) state.impersonator = null;
  notify();
}

export function updateUser(patch: Partial<UserProfile>): void {
  if (!state.user) return;
  state.user = { ...state.user, ...patch };
  notify();
}

export function setViewingUid(uid: string | null): void {
  state.viewingUid = uid;
  notify();
}

/** 启动流程：站点 + 当前用户 + 条款状态。 */
export async function bootstrap(): Promise<void> {
  await loadSite();
  if (currentCsrfToken() === null) setCsrfToken(null);
  try {
    await loadMe();
  } catch {
    state.user = null;
  }
  state.ready = true;
  notify();
}

export function markReady(): void {
  state.ready = true;
  notify();
}
