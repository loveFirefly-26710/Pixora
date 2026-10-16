# 静态前端部署样例

Pixora 的前端就是 `web/dist` 里那一堆静态文件，可以单独发到 Vercel、Cloudflare Pages、
EdgeOne Makers 这类平台上，后端（那个 Rust 进程）放在别处。这个目录里的文件是给那几个
平台用的配置，主 README 的[「前后端分开部署」](../../README.md#前后端分开部署)一节讲了
整体思路，这里只说每个文件怎么用。

| 文件 | 干什么 | 怎么用 |
|---|---|---|
| `config.js` | 运行时配置：告诉前端后端在哪 | 与产物一起放到站点根目录；只想改接口地址时改它即可，不用重新构建 |
| `_headers` | 缓存与安全响应头 | Cloudflare Pages / Netlify 会把同名文件当成响应头配置，放进产物根目录 |
| `vercel.json` | Vercel 的构建与路由配置 | 放到仓库根目录（Vercel 从 Git 导入时用），或随产物一起部署 |
| `wrangler.toml` | Cloudflare Pages 项目配置 | 用 `wrangler pages dev` / `deploy` 时读它 |
| `edgeone.json` | EdgeOne Makers 的项目配置 | 放到仓库根目录，字段与 Vercel 那套类似 |
| `worker-proxy.js` | Cloudflare Worker：把 `/api/*` 与直链反代到后端 | 让浏览器眼里只有一个源，免配跨域（文件头有部署命令） |

## 三条命令，各平台一条

```bash
# Cloudflare Pages（--project-name 不存在会自动创建）
npx wrangler pages deploy web/dist --project-name=pixora

# Vercel（产物已经构建好了，直接发目录）
npx vercel deploy web/dist --prod --yes

# EdgeOne Makers（CLI 版本 ≥ 1.2.30；-t 是 API Token，CI 里用得上）
npx edgeone pages deploy web/dist -n pixora -t "$EDGEONE_API_TOKEN"
```

## 两类接法

**边缘反代（推荐）**：让平台把 `/api/*` 和 `/{直链前缀}/*` 转发给后端。
浏览器只跟一个域名打交道，于是不用配跨域，会话也是第一方 Cookie。

- Vercel：在 `vercel.json` 的 `rewrites` 里加
  `{ "source": "/api/:path*", "destination": "https://img-api.example.com/api/:path*" }`
- Cloudflare：用 `worker-proxy.js`（比 Pages 的 rewrites 更灵活，重定向也会改写）
- EdgeOne Makers：`edgeone.json` 的 `rewrites` 里加同样的规则

**跨域直连**：前端直接调后端域名，后端要放行来源：

```dotenv
PIXORA_CORS_ORIGINS=https://pixora.vercel.app,https://pixora.pages.dev
```

配了之后 Cookie 会自动变成 `SameSite=None; Secure`，所以两边都得走 https。
前端的接口地址写在 `config.js`：

```js
window.__PIXORA__ = { api_base: "https://img-api.example.com" };
```

## 别忘了

- **产物里带上 `config.js`**：它默认是空对象（同源），跨域部署时没有它前端会去静态托管
  那边找接口。仓库里的 GitHub Actions（`.github/workflows/frontend.yml`）会自动写好。
- **`main.js` 这类文件名没有内容哈希**，所以要 `no-cache`（`_headers` 与 `vercel.json`
  里都配好了）。给它们长缓存会让用户一直拿到旧 bundle。
- **SPA 回落**：`/#/albums` 这种带哈希的地址本来就不需要服务端配合；不带哈希的深链
  （比如 `/images131231` 这种陌生路径，会被前端接住并跳去广场）需要服务端把未知路径
  回落到 `index.html`——三个平台的配置里都已经写好了。
