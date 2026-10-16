/**
 * Cloudflare Worker：让前端与后端看起来是同一个源。
 *
 * 这是「前端放边缘、后端放别处」的省事做法：浏览器始终只跟一个域名打交道，
 * 于是既不用配跨域（PIXORA_CORS_ORIGINS），也不用把会话 Cookie 换成
 * SameSite=None——它们本来就是第一方 Cookie。
 *
 * 部署（Workers + 静态资源，wrangler 3.90 起支持 assets 绑定）：
 *
 *   1. 把前端产物放进 ./web/dist（`cd web && npm run build`）；
 *   2. 改下面 BACKEND 常量，或在 wrangler.toml 里用 vars.BACKEND 给；
 *   3. npx wrangler deploy worker-proxy.js --assets ./web/dist \
 *        --compatibility-date 2026-01-01 --var BACKEND:https://img-api.example.com
 *      （用 wrangler.toml 写这几项也行：main / assets / vars.BACKEND）
 *
 * 代理哪几条路径：`/api/*`（接口）、`/images/*`（直链，后台改过一级目录的话
 * 也跟着改）、`/healthz`。其余全部交给静态产物，找不到的路径回落到
 * index.html（前端的 SPA 回落）。
 *
 * 注意两件事：
 *   - 后端要相信自己前面只有一层代理（PIXORA_TRUST_PROXY=1，默认值），
 *     这里的 X-Forwarded-For 才会被采信；
 *   - 直链走边缘转发等于让图片过一遍 Worker。图床的直链本就该给 CDN，
 *     如果后端已经挂了 CDN，把 PROXY_DIRECT 置为 false 让直链直接回后端域名。
 */

const BACKEND = "https://img-api.example.com";
const PROXY_DIRECT = true;
/** 直链的一级目录，与后台「直链前缀」设置保持一致（默认 images）。 */
const UPLOAD_PREFIX = "images";

export default {
  async fetch(request, env) {
    const backend = (env?.BACKEND || BACKEND).replace(/\/+$/, "");
    const url = new URL(request.url);
    const path = url.pathname;

    const proxied =
      path.startsWith("/api/") ||
      path === "/healthz" ||
      (PROXY_DIRECT && path.startsWith(`/${UPLOAD_PREFIX}/`));

    if (proxied) {
      const target = new URL(path + url.search, backend);
      const headers = new Headers(request.headers);
      // 只信客户端的真实地址：到了这个 Worker 手里，X-Forwarded-For 已经不可信
      const client = request.headers.get("cf-connecting-ip");
      if (client) headers.set("x-forwarded-for", client);
      headers.set("x-forwarded-proto", "https");
      headers.set("x-forwarded-host", url.host);

      const response = await fetch(new Request(target, {
        method: request.method,
        headers,
        body: ["GET", "HEAD"].includes(request.method) ? undefined : request.body,
        redirect: "manual",
      }));

      // 后端的 302（直链跳转、OAuth 起点）要原样交给浏览器；
      // 重定向目标里可能写着后端域名，这里改成边缘域名，省一次跨域跳转
      const out = new Response(response.body, response);
      const location = out.headers.get("location");
      if (location && location.startsWith(backend)) {
        out.headers.set("location", location.replace(backend, url.origin));
      }
      return out;
    }

    if (env?.ASSETS) {
      const response = await env.ASSETS.fetch(request);
      if (response.status === 404) {
        return env.ASSETS.fetch(new Request(new URL("/index.html", url), request));
      }
      return response;
    }

    return new Response("前端产物没有绑定到 Worker（缺少 assets 配置）", { status: 500 });
  },
};
