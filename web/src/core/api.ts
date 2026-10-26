/**
 * 后端接口客户端。
 *
 * 三件事：拼信封、带凭据、把错误码翻成人话。
 * 所有响应都长成 `{ data, error, meta? }`，这里拆掉信封只把 `data` 交给调用方，
 * 失败统一抛 `ApiError`（带 code / message / detail / status）。
 */

export type ApiErrorCode =
  | "BAD_REQUEST"
  | "UNAUTHORIZED"
  | "INVALID_CREDENTIALS"
  | "TOKEN_REVOKED"
  | "CSRF_FAILED"
  | "ACCOUNT_PENDING"
  | "ACCOUNT_SUSPENDED"
  | "ACCOUNT_BANNED"
  | "ACCOUNT_LOCKED"
  | "IP_BLOCKED"
  | "FORBIDDEN"
  | "NOT_FOUND"
  | "CONFLICT"
  | "QUOTA_EXCEEDED"
  | "FILE_TOO_LARGE"
  | "UNSUPPORTED_FORMAT"
  | "INVALID_IMAGE"
  | "CONTENT_REJECTED"
  | "RATE_LIMITED"
  | "QUEUE_FULL"
  | "MAINTENANCE"
  | "TERMS_REQUIRED"
  | "TOTP_REQUIRED"
  | "INTERNAL"
  | "STORAGE_UNAVAILABLE"
  | "NETWORK";

export interface ApiErrorBody {
  code: ApiErrorCode;
  message: string;
  detail?: string | null;
  [key: string]: unknown;
}

export interface ApiMeta {
  page: number;
  per_page: number;
  total: number;
  total_pages: number;
}

export interface Page<T> {
  items: T[];
  meta: ApiMeta;
}

export class ApiError extends Error {
  readonly code: ApiErrorCode;
  readonly detail?: string | null;
  readonly status: number;
  readonly body: ApiErrorBody;
  /** 带 Retry-After 的限流错误，客户端据此退避 */
  readonly retryAfter?: number;

  constructor(status: number, body: ApiErrorBody, retryAfter?: number) {
    super(body.message || body.code);
    this.name = "ApiError";
    this.code = body.code;
    this.detail = body.detail ?? null;
    this.status = status;
    this.body = body;
    this.retryAfter = retryAfter;
  }

  /** 是否需要把用户整页引导到「账号暂停」页 */
  get needsAccountNotice(): boolean {
    return (
      this.code === "ACCOUNT_SUSPENDED" ||
      this.code === "ACCOUNT_BANNED" ||
      this.code === "ACCOUNT_PENDING"
    );
  }

  /** 是否值得自动重试（退避后） */
  get isTransient(): boolean {
    return (
      this.code === "NETWORK" ||
      this.code === "RATE_LIMITED" ||
      this.code === "QUEUE_FULL" ||
      this.code === "STORAGE_UNAVAILABLE" ||
      this.status >= 500
    );
  }
}

const PREFIX = "/api/v1";

/** 同源部署，Cookie 只发给本站。 */
const CREDENTIALS: RequestCredentials = "same-origin";

/** 接口前缀；同源直接用相对路径。 */
const API_ROOT = PREFIX;

/** CSRF 令牌存在内存里，并用一个非 HttpOnly Cookie 备份，刷新后能还原。 */
let csrfToken: string | null = null;

function readCookie(name: string): string | null {
  const match = document.cookie.match(
    new RegExp("(?:^|; )" + name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + "=([^;]*)"),
  );
  return match ? decodeURIComponent(match[1]) : null;
}

export function setCsrfToken(token: string | null): void {
  csrfToken = token;
}

export function currentCsrfToken(): string | null {
    // Cookie 是权威来源：它由服务端在响应里设置，永远是最新的。
    // 内存里那份是「上一次响应体里看到的」——如果期间在别的标签页登录过，
    // 它会变成上一代的令牌，拿它去写就会一路 CSRF 校验失败。
    return readCookie("pixora_csrf") ?? csrfToken;
}

/** 会话续期：任何响应体里带了 csrf_token 就地更新 */
function absorbCsrf(data: unknown): void {
  if (data && typeof data === "object") {
    const token = (data as { csrf_token?: unknown }).csrf_token;
    if (typeof token === "string" && token.length > 0) csrfToken = token;
  }
}

/** 用户主动取消（暂停/取消队列）不应该触发回退重试。 */
function isCancelled(err: ApiError): boolean {
  return err.message.includes("取消") || err.message.includes("abort");
}

export interface RequestOptions {
  method?: string;
  /** JSON 体；与 form 二选一 */
  json?: unknown;
  /** multipart 体；与 json 二选一 */
  form?: FormData;
  query?: Record<string, string | number | boolean | undefined | null>;
  headers?: Record<string, string>;
  /** 需要鉴权的请求默认带上 Cookie；公开请求也一样，服务端会忽略 */
  anonymous?: boolean;
  signal?: AbortSignal;
  /** 上传进度回调（仅在有 body 时可用） */
  onProgress?: (loaded: number, total: number) => void;
}

function buildQuery(query: RequestOptions["query"]): string {
  if (!query) return "";
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) {
    if (value === undefined || value === null || value === "") continue;
    params.set(key, String(value));
  }
  const encoded = params.toString();
  return encoded ? `?${encoded}` : "";
}

/** 用 XHR 发请求，只为拿到上传进度。其余情况走 fetch。 */
function xhrSend(
  url: string,
  options: RequestOptions,
  onProgress: (loaded: number, total: number) => void,
): Promise<{ status: number; text: string; headers: Headers }> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open(options.method ?? "POST", url, true);
    xhr.withCredentials = true;
    // CSRF 请求头只能由 request() 统一注入一次。这里以前也设了一遍，
    // 结果是同一个头被设置两次——XHR 的 setRequestHeader 遇到重名会**拼接**
    // 而不是覆盖（「TOKEN, TOKEN」），服务端拿到一个长度翻倍的值，
    // 双提交比对必然失败。上传走的就是这条 XHR 路径，症状是并发上传时一片 403。
    for (const [key, value] of Object.entries(options.headers ?? {})) {
      xhr.setRequestHeader(key, value);
    }
    xhr.upload.onprogress = (event) => {
      if (event.lengthComputable) onProgress(event.loaded, event.total);
    };
    xhr.onload = () => {
      const headers = new Headers();
      for (const line of xhr.getAllResponseHeaders().trim().split(/[\r\n]+/)) {
        const index = line.indexOf(":");
        if (index > 0) headers.append(line.slice(0, index).trim(), line.slice(index + 1).trim());
      }
      resolve({ status: xhr.status, text: xhr.responseText, headers });
    };
    xhr.onerror = () => reject(networkError(url, xhr));
    xhr.ontimeout = () => reject(
      new ApiError(0, {
        code: "NETWORK",
        message: "请求超时",
        detail: `${options.method ?? "POST"} ${url}：服务端在超时时间内没有响应`,
      }),
    );
    if (options.signal) {
      options.signal.addEventListener("abort", () => xhr.abort());
      xhr.onabort = () => reject(new ApiError(0, { code: "NETWORK", message: "请求已取消" }));
    }
    xhr.send(options.form ?? null);
  });
}

/**
 * 把 XHR 的网络失败翻成能查的话。
 *
 * 原来只报一句「网络请求失败」，用户看到的就是截图里那 10 行一模一样的红字——
 * 既看不出是服务器停了、被拦截了，还是设备离线。这里把能拿到的线索都带上：
 * 请求方法与路径、XHR 的 readyState/status、浏览器的在线状态。
 *
 * 最要紧的一句提示是「连接被中断」：那通常意味着服务端进程不在了。图床的前端
 * 会一直自动重试，所以把原因写清楚比让用户反复点重试有用。
 */
function networkError(url: string, xhr: XMLHttpRequest): ApiError {
  const details: string[] = [`${xhr.status === 0 ? "未收到响应" : `HTTP ${xhr.status}`}`, `readyState=${xhr.readyState}`];
  let hint = "连接被中断：服务端可能已停止或正在重启";

  if (typeof navigator !== "undefined" && navigator.onLine === false) {
    hint = "设备当前离线";
  } else if (xhr.status === 0 && xhr.readyState === 0) {
    hint = "请求没有发出去：可能被浏览器扩展、代理或安全软件拦截，也可能服务端已停止";
  }

  return new ApiError(0, {
    code: "NETWORK",
    message: `网络请求失败（${hint}）`,
    detail: `${url} · ${details.join(" · ")}`,
  });
}

async function parseResponse(
  status: number,
  text: string,
  headers: Headers,
): Promise<unknown> {
  let payload: unknown = null;
  if (text) {
    try {
      payload = JSON.parse(text);
    } catch {
      throw new ApiError(status, {
        code: "INTERNAL",
        message: "服务端返回了无法解析的内容",
        detail: text.slice(0, 200),
      });
    }
  }

  const retryAfterHeader = headers.get("Retry-After");
  const retryAfter = retryAfterHeader ? Number(retryAfterHeader) : undefined;

  if (status >= 200 && status < 300) {
    const data = (payload as { data?: unknown } | null)?.data ?? null;
    absorbCsrf(data);
    return data;
  }

  const errorBody = (payload as { error?: ApiErrorBody } | null)?.error;
  if (errorBody && typeof errorBody.code === "string") {
    absorbCsrf(errorBody);
    throw new ApiError(status, errorBody, retryAfter);
  }
  throw new ApiError(
    status,
    {
      code: status === 401 ? "UNAUTHORIZED" : status === 403 ? "FORBIDDEN" : "INTERNAL",
      message: `请求失败（HTTP ${status}）`,
    },
    retryAfter,
  );
}

/**
 * 发一个请求，返回 `data`。
 *
 * CSRF 校验失败会自动重取令牌再试一次：令牌漂了通常是「在另一个标签页登录过」
 * 这类浏览器级原因，属于可自愈的情况，不该让用户对着「请刷新页面后重试」发呆。
 * 写操作在 CSRF 这一关卡住时什么都不会发生，重试是安全的。
 *
 * 401 单独处理后置收尾：会话有闲置超时，最长寿命也有限，超时之后再发请求就会
 * 拿到 401。这时把身份清掉并送回登录页——否则页面一直显示着旧数据，
 * 用户点什么都失败，却看不出原因。
 */
export async function request<T>(path: string, options: RequestOptions = {}): Promise<T> {
  try {
    return await sendRequest<T>(path, options);
  } catch (err) {
    if (err instanceof ApiError && err.status === 401) {
      handleSessionExpired(path);
    }
    if (!(err instanceof ApiError) || err.code !== "CSRF_FAILED") throw err;
    await refreshCsrfToken();
    return await sendRequest<T>(path, options);
  }
}

/**
 * 会话失效的统一处置。
 *
 * 登录/注册这类公开接口不参与：它们本来就允许在未登录状态下调用，
 * 401 在那里是「凭据不对」的另一种说法，跳走会打断正在填的表单。
 */
function handleSessionExpired(path: string): void {
  if (path.startsWith("/auth/") || path.startsWith("/install")) return;
  sessionExpiredHandler?.(path);
}

/**
 * 会话失效的处置钩子。
 *
 * 这里不直接 import state / router / toast：api 是被它们依赖的底层模块，
 * 反向 import 会绕成环。由 main.ts 在启动时注册一次，逻辑写在上层——
 * 那里能同时看到用户状态、路由与提示。
 */
let sessionExpiredHandler: ((path: string) => void) | null = null;

export function onSessionExpired(handler: (path: string) => void): void {
  sessionExpiredHandler = handler;
}

/** 取一次最新的 CSRF 令牌：Cookie 是权威来源，再拉一次 /me 把响应里的那份也吃进来。 */
async function refreshCsrfToken(): Promise<void> {
  const cookie = readCookie("pixora_csrf");
  if (cookie) setCsrfToken(cookie);
  try {
    const response = await fetch(`${PREFIX}/me`, { credentials: CREDENTIALS });
    const payload = await response.json().catch(() => null);
    absorbCsrf(payload?.data);
  } catch {
    // 拿不到就带着 Cookie 里那份重试，还是不行就把原始错误抛回去
  }
}

async function sendRequest<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const url = `${API_ROOT}${path}${buildQuery(options.query)}`;
  const headers: Record<string, string> = { ...(options.headers ?? {}) };

  const method = (options.method ?? (options.json || options.form ? "POST" : "GET")).toUpperCase();
  const isWrite = !["GET", "HEAD", "OPTIONS"].includes(method);

  let body: BodyInit | undefined;
  if (options.form) {
    // multipart 的 Content-Type 必须让浏览器自己带 boundary
    body = options.form;
  } else if (options.json !== undefined) {
    body = JSON.stringify(options.json);
    headers["Content-Type"] = "application/json";
  }

  if (isWrite && !headers["X-CSRF-Token"]) {
    const csrf = currentCsrfToken();
    if (csrf) headers["X-CSRF-Token"] = csrf;
  }

  if (options.form && options.onProgress) {
    try {
      const response = await xhrSend(url, { ...options, method, headers }, options.onProgress);
      return (await parseResponse(response.status, response.text, response.headers)) as T;
    } catch (err) {
      // XHR 在建连层就失败（status=0）时，用 fetch 再试一次同一份 FormData。
      //
      // 这一条是照着实测加的：应用里只有上传走 XHR（为了拿进度），其它请求走 fetch。
      // 浏览器扩展与部分安全软件会挂钩 XMLHttpRequest 但不挂钩 fetch，
      // 症状是「页面一切正常，一上传就 status=0，服务端连请求痕迹都没有」。
      // fetch 通得过就继续用，通不过再把原始错误抛出去——不掩盖真正的问题。
      const isTransportFailure =
        err instanceof ApiError && err.code === "NETWORK" && !isCancelled(err);
      if (!isTransportFailure) throw err;
      try {
        const response = await fetch(url, {
          method,
          headers,
          body,
          credentials: CREDENTIALS,
          signal: options.signal,
        });
        return (await parseResponse(response.status, await response.text(), response.headers)) as T;
      } catch {
        throw err;
      }
    }
  }

  let response: Response;
  try {
    response = await fetch(url, {
      method,
      headers,
      body,
      credentials: CREDENTIALS,
      signal: options.signal,
    });
  } catch (err) {
    if (err instanceof ApiError) throw err;
    throw new ApiError(0, {
      code: "NETWORK",
      message: err instanceof DOMException && err.name === "AbortError" ? "请求已取消" : "网络请求失败",
    });
  }

  return (await parseResponse(response.status, await response.text(), response.headers)) as T;
}

/** 分页请求的便捷封装：把 `meta` 一起带回来。 */
export async function requestPage<T>(
  path: string,
  options: RequestOptions = {},
): Promise<Page<T>> {
  const url = `${API_ROOT}${path}${buildQuery(options.query)}`;
  const response = await fetch(url, {
    method: "GET",
    headers: options.headers,
    credentials: CREDENTIALS,
    signal: options.signal,
  });
  const text = await response.text();
  let payload: { data?: T[]; meta?: ApiMeta; error?: ApiErrorBody } | null = null;
  if (text) {
    try {
      payload = JSON.parse(text);
    } catch {
      throw new ApiError(response.status, { code: "INTERNAL", message: "响应无法解析" });
    }
  }
  if (response.status >= 200 && response.status < 300) {
    absorbCsrf(payload?.data);
    return {
      items: (payload?.data ?? []) as T[],
      meta:
        payload?.meta ??
        ({ page: 1, per_page: 24, total: (payload?.data ?? []).length, total_pages: 1 } as ApiMeta),
    };
  }
  const errorBody = payload?.error;
  if (errorBody) throw new ApiError(response.status, errorBody);
  throw new ApiError(response.status, { code: "INTERNAL", message: "请求失败" });
}

/** 直链前缀等站点信息由 /site 提供，这里做一次内存缓存。 */
let siteCache: Record<string, unknown> | null = null;

export function cachedSite(): Record<string, unknown> | null {
  return siteCache;
}

export function cacheSite(site: Record<string, unknown>): void {
  siteCache = site;
}
