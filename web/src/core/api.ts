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
  | "RATE_LIMITED"
  | "QUEUE_FULL"
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
  return csrfToken ?? readCookie("pixora_csrf");
}

/** 会话续期：任何响应体里带了 csrf_token 就地更新 */
function absorbCsrf(data: unknown): void {
  if (data && typeof data === "object") {
    const token = (data as { csrf_token?: unknown }).csrf_token;
    if (typeof token === "string" && token.length > 0) csrfToken = token;
  }
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
    const csrf = currentCsrfToken();
    if (csrf) xhr.setRequestHeader("X-CSRF-Token", csrf);
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
    xhr.onerror = () => reject(new ApiError(0, { code: "NETWORK", message: "网络请求失败" }));
    xhr.ontimeout = () => reject(new ApiError(0, { code: "NETWORK", message: "请求超时" }));
    if (options.signal) {
      options.signal.addEventListener("abort", () => xhr.abort());
      xhr.onabort = () => reject(new ApiError(0, { code: "NETWORK", message: "请求已取消" }));
    }
    xhr.send(options.form ?? null);
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

/** 发一个请求，返回 `data`。 */
export async function request<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const url = `${PREFIX}${path}${buildQuery(options.query)}`;
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
    const response = await xhrSend(url, { ...options, method, headers }, options.onProgress);
    return (await parseResponse(response.status, response.text, response.headers)) as T;
  }

  let response: Response;
  try {
    response = await fetch(url, {
      method,
      headers,
      body,
      credentials: "same-origin",
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
  const url = `${PREFIX}${path}${buildQuery(options.query)}`;
  const response = await fetch(url, {
    method: "GET",
    headers: options.headers,
    credentials: "same-origin",
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

/** 表单式编码，给兼容端点的 `application/x-www-form-urlencoded` 用。 */
export function formEncode(data: Record<string, string | number | undefined>): string {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(data)) {
    if (value === undefined) continue;
    params.set(key, String(value));
  }
  return params.toString();
}

/** 直链前缀等站点信息由 /site 提供，这里做一次内存缓存。 */
let siteCache: Record<string, unknown> | null = null;

export function cachedSite(): Record<string, unknown> | null {
  return siteCache;
}

export function cacheSite(site: Record<string, unknown>): void {
  siteCache = site;
}
