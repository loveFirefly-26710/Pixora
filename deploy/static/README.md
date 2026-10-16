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

## GitHub Pages 与 Cloudflare Pages 的回落差别

两者的 SPA 回落机制正好相反，混起来会踩坑：

- **Cloudflare Pages**：产物里没有 `404.html` 时，没命中静态文件的路径会**自动**回
  `index.html`（200）。所以什么都别做；一旦放了 `404.html`，它就按真 404 处理。
- **GitHub Pages**：没有自动回落，靠 `404.html`。做法是把它做成 `index.html` 的副本——
  陌生路径会命中这个 404 页面，但页面内容还是前端本体，哈希路由照常接管。
  仓库里的工作流只在发 Pages 时才做这一步。
- **EdgeOne Makers**：`edgeone.json` 里 `{"source":"/*","destination":"/index.html"}`
  这条 rewrite 就是回落，静态资源优先命中，不用额外文件。
- **Vercel**：`vercel.json` 的 `rewrites` 里那条排除 `/api/` 的规则负责回落。

另外 GitHub Pages 的项目站点挂在 `/<仓库名>/` 子路径下，而前端产物引用的是绝对路径
（`/main.js`、`/config.js`），子路径会 404——要么绑自定义域名，要么把仓库命名为
`<用户名>.github.io` 发到根路径。

## 别忘了

- **产物里带上 `config.js`**：它默认是空对象（同源），跨域部署时没有它前端会去静态托管
  那边找接口。仓库里的 GitHub Actions（`.github/workflows/frontend.yml`）会自动写好。
- **`main.js` 这类文件名没有内容哈希**，所以要 `no-cache`（`_headers`、`vercel.json`
  与 `edgeone.json` 里都配好了）。给它们长缓存会让用户一直拿到旧 bundle。
- **别忘了配域名**：绑定自定义域名之后，跨域模式要把它加进后端的 `PIXORA_CORS_ORIGINS`，
  否则浏览器拿不到会话。
