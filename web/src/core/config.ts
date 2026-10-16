/**
 * 运行时配置：前端要不要把接口打到另一个域名上。
 *
 * 默认是空配置——前端与后端同一个源，所有请求走相对路径，这是自托管最省心
 * 的形态。把前端放到 Vercel / Cloudflare Pages / EdgeOne 这类静态托管上时，
 * 后端在别处，需要在 `config.js` 里写一行 `window.__PIXORA__ = { api_base: "..." }`。
 *
 * 三种注入方式，优先级从高到低：
 *   1. 部署平台上的 `config.js`（静态文件，改完刷新即生效，不用重新构建）；
 *   2. 构建时环境变量 `PIXORA_API_BASE`（构建脚本把它写进 `config.js`）；
 *   3. 都没有 —— 同源。
 *
 * 这个文件必须早于 `main.js` 加载：`config.js` 是普通脚本，`main.js` 是
 * ESM，浏览器按文档顺序执行，所以 `main.ts` 里读到的配置一定已经就位。
 */

/** `config.js` 里能写的东西。只认 `api_base`，多余字段忽略。 */
export interface RuntimeConfig {
  /**
   * 后端地址，例如 `https://img-api.example.com`。
   * 留空或省略即同源。
   */
  api_base?: string;
}

declare global {
  interface Window {
    __PIXORA__?: RuntimeConfig;
  }
}

/**
 * 规范化后端地址：去掉结尾的斜杠，只接受 http/https 的绝对地址。
 *
 * 写错的配置（比如填了 `example.com` 忘了协议、或者路径写成 `/api`）在这里
 * 被丢掉并留一条控制台警告。丢掉之后退回同源——多半表现为「接口 404」，
 * 比让 `fetch` 抛一个「Failed to construct URL」更容易查。
 */
export function normalizeApiBase(value: unknown): string {
  if (typeof value !== "string") return "";
  const trimmed = value.trim().replace(/\/+$/, "");
  if (!trimmed) return "";
  if (trimmed === "same-origin" || trimmed === "self") return "";
  if (!/^https?:\/\//i.test(trimmed)) {
    console.warn(
      `[pixora] config.js 里的 api_base 必须是 http(s) 开头的绝对地址，已忽略：${value}`,
    );
    return "";
  }
  return trimmed;
}

function readRuntime(): RuntimeConfig {
  const raw =
    typeof window === "undefined" ? undefined : (window as Window).__PIXORA__;
  return raw && typeof raw === "object" ? raw : {};
}

/** 后端地址；同源时为空串。 */
export const API_BASE: string = normalizeApiBase(readRuntime().api_base);

/** 是否跨域部署（前端与后端不同源）。 */
export const CROSS_ORIGIN: boolean = API_BASE.length > 0;

/** 跨域时要带 Cookie 就必须用 `include`；同源用 `same-origin` 更稳。 */
export const CREDENTIALS: RequestCredentials = CROSS_ORIGIN ? "include" : "same-origin";

/** 把站内路径拼到后端地址上：同源时原样返回。 */
export function apiUrl(path: string): string {
  if (!API_BASE) return path;
  return `${API_BASE}${path.startsWith("/") ? path : `/${path}`}`;
}

/**
 * 后端下发的「站内相对地址」转成可访问的绝对地址。
 *
 * `/site` 里的背景图与默认头像在「上传进库」这种形态下是站内路径
 * （`/api/v1/site/avatar?v=...`）。同源时它本来就能用；跨域部署时若原样塞进
 * `img.src`，浏览器会去静态托管那边找这一张，必然是 404。
 *
 * 已经在别处的绝对地址（外链头像、`data:`、`blob:`）原样返回。
 */
export function absUrl(value: string | null | undefined): string {
  const raw = (value ?? "").trim();
  if (!raw) return "";
  if (/^[a-z][a-z0-9+.-]*:/i.test(raw) || raw.startsWith("//")) return raw;
  return apiUrl(raw);
}

/** 后端地址的源，同源时为空串。用于少数需要自己拼绝对地址的地方。 */
export function apiOrigin(): string {
  if (!API_BASE) return "";
  const match = API_BASE.match(/^https?:\/\/[^/]+/i);
  return match ? match[0] : API_BASE;
}
