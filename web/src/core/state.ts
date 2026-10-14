/**
 * 应用状态。
 *
 * 只有一份：当前会话（用户、站点配置、CSRF）。页面按需订阅，
 * 状态变了就重渲染自己——不做虚拟 DOM，也不做细粒度依赖追踪，
 * 刷新整页在图片数几十的量级上是无感的。
 */

import { request, cacheSite, currentCsrfToken, setCsrfToken, type ApiError } from "./api";
import type { SiteInfo, UserProfile } from "./types";

type Listener = () => void;

interface AppState {
  site: SiteInfo | null;
  user: UserProfile | null;
  /** 管理员正在查看的用户（用户视图）；普通用户恒为 null */
  viewingUid: string | null;
  ready: boolean;
}

const state: AppState = {
  site: null,
  user: null,
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

export function addons(): { baseUrl: string; uploadPrefix: string } {
  return {
    baseUrl: state.site?.base_url ?? location.origin,
    uploadPrefix: state.site?.upload_prefix ?? "images",
  };
}

/** 拼直链。缩略图走 `?thumb=1`。 */
export function directUrl(hash: string, ext: string, thumb = false): string {
  const { baseUrl, uploadPrefix } = addons();
  const suffix = thumb ? "?thumb=1" : "";
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
      thumb_client: true,
      thumb_server_fallback: false,
      appeal_email: null,
      background: null,
      installed: true,
      terms: { required: false, version: null, articles: 0 },
    };
  }
  notify();
}

/** 拉当前用户。未登录返回 null（不是错误）。 */
export async function loadMe(): Promise<UserProfile | null> {
  try {
    const payload = await request<{ user: UserProfile; csrf_token?: string }>("/me");
    state.user = payload.user;
    if (payload.csrf_token) setCsrfToken(payload.csrf_token);
    notify();
    return state.user;
  } catch (err) {
    const apiError = err as ApiError;
    if (apiError.status === 401) {
      state.user = null;
      notify();
      return null;
    }
    throw err;
  }
}

export function setUser(user: UserProfile | null): void {
  state.user = user;
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
