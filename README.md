# Pixora

自托管的图片托管服务。浏览器算哈希、生成缩略图、排队上传，服务端只管存与发；
PostgreSQL 方案整栈常驻 145–225MB，SQLite 方案压到 80MB 以内，一台 1C1G 的小机器就能跑。
可以整栈放一台机器，也可以前端上 CDN、后端留在小机器上——[部署](#部署)一节把四条路都写全了。

![广场](docs/screenshots/plaza.webp)

设计文档在 [`docs/Pixora-技术方案.md`](docs/Pixora-技术方案.md)，接口契约在
[`docs/api-contract.md`](docs/api-contract.md)，界面原型在 [`docs/ui-preview.html`](docs/ui-preview.html)。

## 它解决什么问题

市面上的图床在单机 Docker 里跑起来，光运行时内存就吃掉几百兆。Pixora 的做法是把
能推到客户端的活全推过去：

| 常见做法 | Pixora 的做法 |
|---|---|
| 上传后再算哈希，发现重复已经浪费了带宽 | 浏览器先算 SHA-256，批量预检，重复的根本不发 |
| 服务端用 GD/ImageMagick 生成缩略图，CPU 是瓶颈 | 浏览器用 OffscreenCanvas 生成，随原图一起上传 |
| 一人一份文件，删一份容易误伤另一份 | 内容寻址 + 引用计数，物理文件全局唯一 |
| 一次选 1000 张，浏览器开 1000 条连接 | 客户端排队逐张发，服务端有界队列做背压 |
| 列表页为每张图查一次数据库 | 标签走关联表批量取，列表页没有 N+1 |

## 功能一览

- **上传**：拖拽 / 选文件夹 / 粘贴剪贴板；浏览器先算哈希去重，缩略图在本地生成；并发数与目标相册、标签可以整批设置。
- **广场**：未登录可浏览公开图片，标签筛选、排序、灯箱查看（可缩放拖动、显示缩放百分比）。
- **相册**：公开 / 私有；相册页支持网格与列表、拖拽排序、批量移动与标签。
- **直链**：`/{前缀}/{sha256}.{ext}`，前缀可在后台改且旧链仍有效；`?thumb=1` 走缩略图，`?download=1` 带原始文件名下载。
- **回收站**：删除先进回收站，保留期内可恢复，到期由后台任务彻底删除。
- **账号**：邮箱 + 密码、邮箱验证码、二次验证（TOTP，口令 + 恢复码）、会话与 API Token 管理、暂停/封禁申诉入口。
- **登录方式**：邮箱（必需）+ GitHub / Google / QQ / 微信 / 手机号验证码，用户可自行绑定与解绑，管理员能统一启停。
- **管理后台**：用户与配额、存储后端与容量、频率限制（含按用户覆盖）、IP 黑名单、内容审核、邮件模板、服务条款、系统日志。
- **存储后端**：本地目录、数据库 BLOB、S3 兼容（Cloudflare R2 / 阿里云 OSS / 腾讯云 COS / MinIO / B2）、WebDAV、GitHub 仓库、Telegram 频道。
- **部署形态**：单二进制（前端嵌在里面）、Docker 镜像（多架构）、前后端分开部署到静态托管 + API。

<img src="docs/screenshots/login.webp" alt="登录页" width="720">

登录方式可以切换（账密 / 手机号验证码 / 扫码），第三方入口只在配全凭据后才出现：

<table>
<tr>
<td width="50%"><img src="docs/screenshots/upload.webp" alt="上传页"><br><sub>上传：拖拽 / 选文件夹 / 粘贴，缩略图与哈希在浏览器里算</sub></td>
<td width="50%"><img src="docs/screenshots/albums.webp" alt="相册页"><br><sub>相册：公开或私有，网格或列表，批量操作</sub></td>
</tr>
<tr>
<td width="50%"><img src="docs/screenshots/viewer.webp" alt="图片查看器"><br><sub>查看器：尺寸、大小、SHA-256、所属相册，一键复制直链或下载</sub></td>
<td width="50%"><img src="docs/screenshots/recycle.webp" alt="回收站"><br><sub>回收站：删除先软删，保留期内可恢复</sub></td>
</tr>
<tr>
<td width="50%"><img src="docs/screenshots/admin-storage.webp" alt="存储管理"><br><sub>存储：切换后端、容量口径、直链前缀、缩略图重算</sub></td>
<td width="50%"><img src="docs/screenshots/admin-quota.webp" alt="配额与容量"><br><sub>配额：系统总额分配、触顶自动关闭注册</sub></td>
</tr>
<tr>
<td width="50%"><img src="docs/screenshots/admin-users.webp" alt="用户管理"><br><sub>用户：角色、状态、用量、批量配额、免登录切换</sub></td>
<td width="50%"><img src="docs/screenshots/admin-settings.webp" alt="系统设置"><br><sub>设置：站点、注册、会话、防盗链、内容审核</sub></td>
</tr>
</table>

手机端（同一套前端自适应，底部标签栏 + 全部入口）：

<img src="docs/screenshots/mobile-plaza.webp" alt="手机端广场" width="330">
<img src="docs/screenshots/mobile-account.webp" alt="手机端账号页" width="330">

## 部署

三条路，看你手上有什么：

| 场景 | 走哪条 | 前端在哪 | 数据在哪 |
|---|---|---|---|
| 一台机器、一个域名，最省心 | [Docker Compose](#docker-compose推荐) | 同一台机器（嵌在二进制里） | 本地目录 + SQLite |
| 已经有一台服务器，不想装 Docker | [单二进制 + systemd](#单二进制与-systemd) | 同上 | 本地目录 + 数据库 |
| 前端想上 CDN、数据想放云上 | [前后端分开部署](#前后端分开部署) | Vercel / Cloudflare / EdgeOne / GitHub Pages | 云数据库 + 对象存储 |

三条路的后端是同一个二进制，配置也同一套。第一次打开都会进安装向导
（环境检查 → 数据库 → 管理员 → 站点与邮件）：向导结束前没有任何账号能登录，
所以「先起服务、再慢慢配」是安全的。

### 环境要求

- Linux x86_64 / ARM64（生产）与 Windows 10+（开发）——支持的目标平台就是这两个，
  别的系统上 `cargo build` 未必编得过
- 磁盘：随便；内存：SQLite 方案 80MB 起，PostgreSQL 方案建议 200MB
- 出网：只有用到邮件、第三方登录、远端存储后端时才需要

### Docker Compose（推荐）

`deploy/` 里是一套完整编排：应用 + PostgreSQL + Caddy（自动申请证书、自动续期）。
下面这份与仓库里的 `deploy/docker-compose.yml` 一致，可以直接抄：

```yaml
services:
  pixora:
    # 本地构建（默认）。用 CI 推上来的镜像就删掉 build 两行，改成：
    #   image: ghcr.io/<你的 GitHub 用户名>/pixora:latest
    image: pixora:latest
    build:
      context: ..
      dockerfile: deploy/Dockerfile
      # 想让这份镜像里的前端指向别的后端时给这个参数：
      # args: { PIXORA_API_BASE: "https://img-api.example.com" }
    restart: unless-stopped
    environment:
      PIXORA_DB_DRIVER: ${PIXORA_DB_DRIVER:-postgres}
      PIXORA_DB_DSN: ${PIXORA_DB_DSN:-postgres://pixora:${POSTGRES_PASSWORD}@db:5432/pixora}
      PIXORA_DATA_DIR: /data
      # 主密钥：用于加密存储后端凭据与 SMTP 密码。生产环境必须显式注入，
      # 换掉它等于让已加密的凭据全部失效（要重新填一遍）。
      PIXORA_MASTER_KEY: ${PIXORA_MASTER_KEY:?必须设置 PIXORA_MASTER_KEY}
      # 反向代理后面必须显式设置，否则直链地址会变成容器内网地址
      PIXORA_BASE_URL: ${PIXORA_BASE_URL:-}
      # 只信最靠近服务端的一层代理，防止客户端伪造 X-Forwarded-For 绕过 IP 黑名单
      PIXORA_TRUST_PROXY: "1"
      # 想让反向代理直接发文件（零拷贝）时打开，并同步配置 Caddyfile 的
      # 内部路径与来源限制；默认关闭是因为 Caddy 没有 nginx internal 的等价物，
      # 直接把 CAS 目录暴露成公开路径会绕过私有图片的访问判断。
      # PIXORA_ACCEL_PREFIX: /data-files
      PIXORA_WORKERS: ${PIXORA_WORKERS:-4}
      # 前后端分开部署（前端在 Vercel / Cloudflare Pages / EdgeOne）时填前端来源，
      # 逗号分隔；留空即前后端同源，一个跨域头都不发。见 README 的部署一节
      PIXORA_CORS_ORIGINS: ${PIXORA_CORS_ORIGINS:-}
      RUST_LOG: ${RUST_LOG:-info}
    volumes:
      - ./data:/data
    expose:
      - "8080"
    depends_on:
      db:
        condition: service_healthy
    deploy:
      resources:
        limits: { memory: 128M }
        reservations: { memory: 64M }
    healthcheck:
      test: ["CMD", "/pixora", "--health-probe"]
      interval: 30s
      timeout: 3s
      retries: 3
      start_period: 10s

  db:
    image: postgres:16-alpine
    restart: unless-stopped
    command: >
      postgres -c shared_buffers=64MB -c work_mem=4MB
               -c max_connections=50 -c effective_cache_size=128MB
    environment:
      POSTGRES_DB: pixora
      POSTGRES_USER: pixora
      POSTGRES_PASSWORD: ${POSTGRES_PASSWORD:?必须设置 POSTGRES_PASSWORD}
    volumes:
      - ./pgdata:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U pixora -d pixora"]
      interval: 10s
      timeout: 3s
      retries: 5
    deploy:
      resources:
        limits: { memory: 200M }

  caddy:
    image: caddy:2-alpine
    restart: unless-stopped
    ports:
      - "80:80"
      - "443:443"
    environment:
      PIXORA_DOMAIN: ${PIXORA_DOMAIN:?必须设置域名，Caddy 用它申请证书}
      ACME_EMAIL: ${ACME_EMAIL:-}
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      # 只读挂载数据目录：Caddy 靠 X-Accel-Redirect 直接发文件
      - ./data:/srv/data:ro
      - caddy_data:/data
      - caddy_config:/config
    depends_on:
      - pixora
    deploy:
      resources:
        limits: { memory: 48M }

volumes:
  caddy_data:
  caddy_config:
```

配套的 `.env`（`cp .env.example .env` 之后至少改这三行）：

```dotenv
PIXORA_DOMAIN=img.example.com          # 你的域名，Caddy 用它申请证书
POSTGRES_PASSWORD=一串足够长的密码
PIXORA_MASTER_KEY=                     # openssl rand -hex 32 生成
```

`PIXORA_MASTER_KEY` 用来加密存储后端凭据与 SMTP 密码。**它一旦丢失，那些凭据就得重填一遍**，
所以生成完顺手放进密码管理器，并纳入备份。

```bash
docker compose up -d
docker compose logs -f pixora    # 看到「Pixora 已启动」就可以打开域名了
```

三个服务的内存上限加起来 376M，给系统留了余量，整栈压在 500MB 预算之内。
镜像可以自己构建（构建阶段要编一整套 musl 静态二进制，第一次会慢一些，之后有层缓存），
也可以直接用 CI 推上来的那份：

```bash
docker pull ghcr.io/lovefirefly-26710/pixora:latest   # 换成你自己的用户名
```

#### 只跑 SQLite 的最小编排

一台小机器、不想另起数据库进程时用这份（整栈 80MB 以内）。它把 Caddy 也省了，
直接开 8080 端口，前后端都由这一个容器发。

```yaml
# docker-compose.yml（放在一个空目录里，旁边建 ./data）
services:
  pixora:
    image: ghcr.io/lovefirefly-26710/pixora:latest
    restart: unless-stopped
    environment:
      PIXORA_DB_DRIVER: sqlite
      PIXORA_DB_DSN: /data/pixora.db
      PIXORA_DATA_DIR: /data
      PIXORA_BASE_URL: https://img.example.com   # 有反代就填域名，没有就填 http://机器IP:8080
      PIXORA_BIND: 0.0.0.0:8080
      PIXORA_MASTER_KEY: ${PIXORA_MASTER_KEY:?必须设置}
      RUST_LOG: info
    volumes:
      - ./data:/data
    ports:
      - "8080:8080"
    deploy:
      resources:
        limits: { memory: 96M }
```

```bash
mkdir -p data
printf 'PIXORA_MASTER_KEY=%s\n' "$(openssl rand -hex 32)" > .env
docker compose up -d
```

想换成托管数据库（Neon / Supabase / RDS / 云厂商 MySQL）时，只改这两行，不碰编排结构：

```dotenv
PIXORA_DB_DRIVER=postgres
PIXORA_DB_DSN=postgres://user:pass@ep-xxx-pooler.region.aws.neon.tech/pixora?sslmode=require
```

#### 不用 Caddy 的几种接法

- **直接开端口**：把 `caddy` 服务删掉，给 `pixora` 加 `ports: ["8080:8080"]`，
  `PIXORA_BASE_URL` 填 `http://你的IP:8080`。适合内网或先跑通再说。
  注意此时 `PIXORA_SECURE_COOKIES` 默认是关的（没有 https），别在公网上这么裸奔。
- **Nginx / 宝塔 / 其它反代**：见下面的 [Nginx 例子](#单二进制与-systemd)，
  照抄那段 `location /` 即可，注意 `client_max_body_size` 要略大于单文件上限。
- **Cloudflare Tunnel**（没有公网 IP 也能上 https）：

  ```bash
  # 在 Cloudflare Zero Trust 里建一个隧道，拿到 token 后：
  docker run -d --name cf-tunnel --network host --restart unless-stopped \
    cloudflare/cloudflared:latest tunnel --no-autoupdate run --token <你的隧道TOKEN>
  ```

  隧道的 Public Hostname 指到 `http://127.0.0.1:8080`，`PIXORA_BASE_URL` 填隧道绑的域名。

### 单二进制与 systemd

从 [Releases](../../releases) 下载对应平台的文件解压即用，没有运行时依赖。
Linux 那份是静态链接（musl），Alpine、群晖、老 CentOS 上都能跑。

```bash
# Linux
tar -xzf pixora-v0.1.0-linux-x86_64.tar.gz && cd pixora
PIXORA_BASE_URL=http://你的域名或IP:8080 ./pixora
```

```powershell
# Windows
Expand-Archive pixora-v0.1.0-windows-x86_64.zip .
cd pixora\pixora
$env:PIXORA_BASE_URL="http://127.0.0.1:8080"; .\pixora.exe
```

不带任何环境变量时它会在当前目录建一个 `./data`，用里面的 SQLite 库跑起来——
先试跑一条命令，再考虑要不要配数据库。数据目录、直链前缀这类东西界面上只读展示，
改配置只能改环境变量（理由见[技术方案里的 ADR-0003](docs/Pixora-技术方案.md)）。

做成开机自启（systemd）：

```ini
# /etc/systemd/system/pixora.service
[Unit]
Description=Pixora
After=network.target

[Service]
User=pixora
WorkingDirectory=/srv/pixora
Environment=PIXORA_DATA_DIR=/srv/pixora/data
Environment=PIXORA_DB_DRIVER=sqlite
Environment=PIXORA_DB_DSN=/srv/pixora/data/pixora.db
Environment=PIXORA_BASE_URL=https://img.example.com
Environment=PIXORA_MASTER_KEY=<openssl rand -hex 32 生成的那一串>
Environment=RUST_LOG=info
ExecStart=/srv/pixora/pixora
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload && sudo systemctl enable --now pixora
journalctl -u pixora -f          # 看日志
```

前面挂一个 Nginx 反代（Caddy 的配置见 `deploy/Caddyfile`，比这段更短）：

```nginx
server {
    listen 443 ssl http2;
    server_name img.example.com;
    client_max_body_size 64m;           # 略大于应用的单文件上限

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $remote_addr;   # 只信这一层
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_read_timeout 300s;
    }
}
```

反代后面 **必须** 设 `PIXORA_BASE_URL`，`PIXORA_TRUST_PROXY=1` 保持默认——
它表示「只信最靠近服务端的一层代理」，设成更大的值等于让客户端伪造
`X-Forwarded-For` 绕过 IP 黑名单。

Windows 上想开机自启，用 `sc.exe` 或 [NSSM](https://nssm.cc/) 把它注册成服务，
环境变量写进服务配置里即可；数据目录记得给足权限。

### 从源码构建

需要 Rust stable 与 Node 20+。**前端要先构建**：它的产物会被 `rust-embed` 嵌进二进制，
仓库里提交了一份（所以全新克隆可以直接 `cargo build`），改了前端就得重新构建再编 Rust。

```bash
# 1. 前端
cd web
npm ci                # 或 npm install
PIXORA_API_BASE=https://img-api.example.com npm run build   # 同源部署就不给这个变量
cd ..

# 2. 后端（默认带 embed-web，前端就在二进制里）
cargo build --release -p pixora-server
./target/release/pixora --health-probe && echo ok
```

产物是 `target/release/pixora`（Windows 上是 `pixora.exe`），单文件，数据目录和数据库
按环境变量来。开发时的热加载做法（`PIXORA_WEB_DIR` 读盘）见[开发](#开发)一节。

本地出镜像：

```bash
# 只出当前架构
docker build -f deploy/Dockerfile -t pixora:dev .

# 多架构（本地要有 buildx；先装 QEMU 才能编 arm64）
docker run --privileged --rm tonistiigi/binfmt --install arm64,amd64
docker buildx build --platform linux/amd64,linux/arm64 -f deploy/Dockerfile -t pixora:multi .
```

想交叉编一个静态二进制（在 x86 机器上出 arm64 的 Linux 可执行文件）也行：

```bash
rustup target add aarch64-unknown-linux-musl
# 需要一份 musl 交叉工具链（Debian/Ubuntu 上是 gcc-aarch64-linux-musl，
# 或者用 cargo-zigbuild：cargo install cargo-zigbuild && cargo zigbuild --target aarch64-unknown-linux-musl）
cargo build --release --target aarch64-unknown-linux-musl -p pixora-server
```

CI 里怎么构建、出了哪些产物，见下面的 [CI 与发布](#ci-与发布)。

### 前后端分开部署

前端其实只是一堆静态文件，后端才是那个 Rust 进程。把前端交给静态托管、后端留在任何能跑
容器或二进制的地方，有两个好处：静态资源走 CDN 边缘、后端只需要一台小机器。
只有一件事要摆平——**浏览器得知道接口在哪个域名上**。

#### 两种接法

| | 边缘反代（推荐） | 跨域直连 |
|---|---|---|
| 怎么做 | 平台把 `/api/*` 与 `/{直链前缀}/*` 转发给后端 | 前端直接调后端域名 |
| 后端要配 | `PIXORA_BASE_URL` | `PIXORA_BASE_URL` + `PIXORA_CORS_ORIGINS` |
| 前端要配 | 不动 | `config.js` 里写 `api_base` |
| Cookie | 第一方，`SameSite=Lax` 就行 | 跨站，自动切 `SameSite=None; Secure`（两边都得 https） |
| 适合 | 大多数情况，尤其想省心 | 平台不支持反代到外部源时 |

`config.js` 就在产物根目录，和 `index.html` 放一起：

```js
// 同源（边缘反代）时留空即可
window.__PIXORA__ = { api_base: "https://img-api.example.com" };
```

它是运行时读的，部署之后直接改这个文件、刷新页面就生效；构建时也可以用环境变量
`PIXORA_API_BASE` 写进去（见 `deploy/static/README.md`）。

部署完的自查清单，三条都过就算通：

1. 打开前端域名，广场能出图（说明直链地址对、后端可达）；
2. 登录后刷新一下，还是登录状态（说明 Cookie 域与 SameSite 都对）；
3. 传一张图，能在相册里看到（说明写接口与上传通道都通）。

#### Vercel

后端先起来（Compose 或二进制），记下它的地址，比如 `https://img-api.example.com`。

**发前端**，两种方式：

- 控制台：Add New → Project → 导入这个仓库。仓库根目录的 `vercel.json` 已经写好了
  安装与构建命令（`npm --prefix web ci` / `npm --prefix web run build`）和输出目录
  （`web/dist`），Dashboard 里不用再填。
- 命令行：`npx vercel login`，然后在仓库根目录 `npx vercel --prod`；
  只想发已经构建好的产物就 `npx vercel deploy web/dist --prod --yes`。

**让接口通**，把 `vercel.json` 换成这份（反代：浏览器眼里只有一个源，最省事）：

```json
{
  "$schema": "https://openapi.vercel.sh/vercel.json",
  "installCommand": "npm --prefix web ci",
  "buildCommand": "npm --prefix web run build",
  "outputDirectory": "web/dist",
  "headers": [
    {
      "source": "/(.*)",
      "headers": [
        { "key": "Cache-Control", "value": "no-cache" },
        { "key": "X-Content-Type-Options", "value": "nosniff" }
      ]
    }
  ],
  "rewrites": [
    { "source": "/api/:path*", "destination": "https://img-api.example.com/api/:path*" },
    { "source": "/images/:path*", "destination": "https://img-api.example.com/images/:path*" },
    { "source": "/((?!api/|images/).*)", "destination": "/index.html" }
  ]
}
```

把两处 `https://img-api.example.com` 换掉、`/images/` 换成你在后台设的直链前缀即可
（默认就是 `images`）。最后一条是 SPA 回落：让 `/images131231` 这种陌生路径也进前端。

跨域直连的话删掉前两条 rewrites，改成：后端 `PIXORA_CORS_ORIGINS=https://你的项目.vercel.app`，
前端 `config.js` 里写 `api_base`。

**注意**：Vercel 只能托管前端。它的 Serverless/Edge Function 是 JS 运行时，
跑不了这个需要常驻进程与文件系统的二进制；后端始终要放在别处。

#### Cloudflare

Cloudflare 这边可以只用它的静态托管，也可以把后端一起放上去。分开说。

**A. Pages 托管前端**

控制台：Workers & Pages → Create → Pages → Connect to Git → 选仓库，
构建命令 `npm --prefix web ci && npm --prefix web run build`，输出目录 `web/dist`，
要跨域就再加环境变量 `PIXORA_API_BASE`。命令行等价：

```bash
npx wrangler pages deploy web/dist --project-name pixora
```

- **SPA 回落不用配**：产物里没有 `404.html` 时，Pages 会把没命中静态文件的路径回
  `index.html`（200），前端的路由正好接住。⚠️ 也**别**往产物里放 `404.html`——
  一旦存在，Pages 就按真 404 处理，陌生路径会拿到 404 页面。
- 缓存头由产物里的 `_headers` 决定（`deploy/static/_headers`，工作流会自动带上）。
- 自定义域名在 Pages 项目里加，然后在后端 `PIXORA_CORS_ORIGINS` 里写上它（跨域模式）。

**B. Worker 反代（想走同源就加这一步）**

`deploy/static/worker-proxy.js` 是一个现成的 Worker：`/api/*`、`/healthz` 与直链转发给后端，
其余交给静态产物。配 `wrangler.toml`：

```toml
name = "pixora"
main = "worker-proxy.js"
compatibility_date = "2026-01-01"

[assets]
directory = "./web/dist"

[vars]
BACKEND = "https://img-api.example.com"
```

```bash
npx wrangler deploy
```

这样浏览器只跟 Worker 的域名打交道，会话是第一方 Cookie，不用配 CORS。

**C. 后端也放 Cloudflare（Containers）**

想把整栈都放在 Cloudflare 上，用 Containers 跑这个镜像 + R2 存图 + 托管 Postgres 存数据。
三条硬约束先知道：

- 容器只跑 **x86_64**，镜像得是 `linux/amd64`；
- 容器磁盘每次冷启动都是全新的，所以**图片必须放 R2、数据必须放托管数据库**，
  本地目录只能当缓存；
- 镜像要推到 Cloudflare 自己的 registry（GHCR 不直接支持拉取）：
  本地 `docker pull` 之后再 `npx wrangler containers push`，然后用一个 Worker 把请求转给容器。

这条路要自己拼，官方文档里的 Containers 一节讲得比这里细；配好后记得
`PIXORA_DB_DRIVER=postgres` + 托管库连接串，图片在后台存储页切到 R2。

**D. R2 存图**（不管后端在哪都适用）

R2 是 S3 兼容的，Pixora 的 S3 驱动直接能用：

| 后台要填的字段 | 值 |
|---|---|
| 驱动 | S3 |
| Endpoint | `https://<你的账号ID>.r2.cloudflarestorage.com` |
| Region | `auto` |
| Bucket | 你建的桶名 |
| AccessKey / Secret | R2 的 API 令牌（对象读写） |

切过去之后新上传走 R2，**已有图片继续从原来的后端读**，不用搬。

#### EdgeOne Makers

EdgeOne Pages 现在叫 EdgeOne Makers，账号、项目、命令行都还兼容。

控制台：从 Git 导入仓库，构建命令 `npm --prefix web ci && npm --prefix web run build`，
输出目录 `web/dist`，Node 版本 20。命令行（CI 里用得上，`-t` 是 API Token）：

```bash
npx edgeone pages deploy ./web/dist -n pixora -t "$EDGEONE_API_TOKEN"
```

仓库根的 `edgeone.json` 里已经写好三件事：构建与输出目录、SPA 回落
（`{"source":"/*","destination":"/index.html"}`，静态资源优先命中）、产物缓存头。

接口怎么通：EdgeOne 的 `rewrites` 是站内路径改写，指向外部域名的反代要看当前版本支持不支持；
稳妥做法是**跨域直连**——后端配 `PIXORA_CORS_ORIGINS=https://你的EdgeOne域名`，
前端 `config.js` 写 `api_base`。域名在 EdgeOne 控制台绑定，绑完 https 是自动的。

#### GitHub Pages

纯静态、免费，适合「仓库就是站点」的场景。仓库里已经带了工作流，三步：

1. Settings → Pages → Source 选 **GitHub Actions**；
2. Settings → Variables 加两个：`PIXORA_DEPLOY_FRONTEND=true`、`PIXORA_API_BASE=https://img-api.example.com`；
3. Actions → 「前端静态站点」→ Run workflow，target 选 `github-pages`。

工作流会把 `index.html` 复制成 `404.html` 一起发上去——Pages 没有 SPA 回落，
陌生路径会命中这个 404 页面，但页面内容还是前端本体，路由（哈希）照常接管。

**一条限制要先知道**：Pages 的项目站点挂在 `https://<用户名>.github.io/<仓库名>/` 这个
子路径下，而前端产物引用的是绝对路径（`/main.js`、`/config.js`）。子路径会 404。
要么绑一个自定义域名（站点就在根路径了），要么把仓库命名为 `<用户名>.github.io` 直接发到根。
后端照样要在别处，配法和上面一样。

#### 各平台对照

| 平台 | 发前端 | SPA 回落 | 能反代到后端 | 需要的密钥/变量 |
|---|---|---|---|---|
| Vercel | Git 集成 / `npx vercel --prod` | `vercel.json` 里的 rewrite | 能（rewrites 指向外部域名） | `VERCEL_TOKEN`（CI 用） |
| Cloudflare Pages | Git 集成 / `wrangler pages deploy` | 自动（别放 404.html） | 用 Worker（见 B） | `CLOUDFLARE_API_TOKEN`、`CLOUDFLARE_ACCOUNT_ID` |
| EdgeOne Makers | Git 集成 / `edgeone pages deploy` | `edgeone.json` 的 rewrites | 看版本支持，稳妥走跨域 | `EDGEONE_API_TOKEN` |
| GitHub Pages | 仓库里的工作流 | 靠 `404.html` | 不能 | 变量 `PIXORA_DEPLOY_FRONTEND`、`PIXORA_API_BASE` |

### 数据库

| 方案 | 怎么配 | 适用 |
|---|---|---|
| SQLite（默认） | `PIXORA_DB_DRIVER=sqlite`、`PIXORA_DB_DSN=/data/pixora.db` | 单机、个人用；备份就是拷一个文件 |
| 自建 PostgreSQL | `PIXORA_DB_DRIVER=postgres`、`PIXORA_DB_DSN=postgres://用户:密码@主机:5432/库名` | 已有一台数据库；数据与图片可以放在不同机器 |
| 云托管 PostgreSQL | 连接串照抄厂商给的，**带上 SSL 参数** | Neon / Vercel Postgres / Supabase / RDS / PolarDB |
| 云托管 MySQL | `mysql://用户:密码@主机:3306/库名?ssl-mode=REQUIRED` | 云厂商 MySQL、PlanetScale |

```dotenv
# 例：Neon（Vercel Postgres 同一套）
PIXORA_DB_DRIVER=postgres
PIXORA_DB_DSN=postgres://user:pass@ep-xxx-pooler.region.aws.neon.tech/pixora?sslmode=require
```

三种方言各自的建表脚本在 `migrations/{sqlite,postgres,mysql}/`，启动时自动跑，
升级版本也是启动时自动迁移。托管库在边缘网络里连不稳时，Cloudflare 侧可以加一层
Hyperdrive 做连接池，连接串换成 Hyperdrive 给的那个即可。

### 图片存哪儿

本地目录是默认值，够用；要让图片也上云，在后台「存储管理」里加一个后端再切过去：
凭据用主密钥加密后入库，切换时新上传的走新后端，**已有图片继续从原后端读**，
远端失败会自动回退本地并在上传结果里说明。

| 后端 | 填什么 | 备注 |
|---|---|---|
| 本地磁盘 | 数据目录（环境变量决定，界面上只读） | 唯一支持硬链接镜像树与容量探测的驱动 |
| S3 兼容 | Endpoint、Region、Bucket、AccessKey、Secret | **Cloudflare R2**：`https://<accountid>.r2.cloudflarestorage.com`、region `auto`；**阿里云 OSS / 腾讯云 COS / MinIO / Backblaze B2** 同一套 SigV4 |
| 数据库内 | 无 | 小图或不想碰文件系统的场景，字节进库 |
| WebDAV | URL、账号密码 | 坚果云这类 |
| GitHub 仓库 | Token、owner/repo、分支 | 用 Contents API 推文件；调 API 有频率限制 |
| Telegram | Bot Token、频道 ID | 图片存到频道里；存储键里带 message id，删得掉 |

### 环境变量

全部变量与默认值见 [`deploy/.env.example`](deploy/.env.example)，常用的这些：

| 变量 | 默认 | 说明 |
|---|---|---|
| `PIXORA_BASE_URL` | 空 | 站点对外地址。反代后面必须设，直链、邮件链接都按它拼 |
| `PIXORA_DATA_DIR` | `./data` | 数据目录（SQLite 文件、本地图片、主密钥文件都在这） |
| `PIXORA_DB_DRIVER` / `PIXORA_DB_DSN` | `sqlite` / 数据目录里的 `pixora.db` | 数据库 |
| `PIXORA_MASTER_KEY` | 首次启动自动生成到数据目录 | 加密凭据的主密钥，生产环境请显式注入 |
| `PIXORA_BIND` | `0.0.0.0:8080` | 监听地址 |
| `PIXORA_TRUST_PROXY` | `1` | 信任几层代理的转发头 |
| `PIXORA_CORS_ORIGINS` | 空 | 放行的前端来源（前后端分开部署时填） |
| `PIXORA_COOKIE_SAMESITE` | 配了 CORS 就 `none`，否则 `lax` | 会话 Cookie 的跨站策略 |
| `PIXORA_WORKERS` | CPU 核数 | 后台任务并发（邮件重试、Blob GC、对账） |
| `PIXORA_DB_MAX_CONNECTIONS` | SQLite 4 / 其它 10 | 连接池上限 |
| `PIXORA_ACCEL_PREFIX` | 空 | 让 Nginx 直接发文件的 `X-Accel-Redirect` 前缀 |
| `PIXORA_SECURE_COOKIES` | 按 `BASE_URL` 是否 https | 本地 http 调试时可以显式设 false |
| `RUST_LOG` | `info` | 日志级别 |

启动日志会把关键事实打一行摘要（监听地址、数据目录、数据库方言、存储后端、主密钥来源、
前端是嵌入还是读盘），排障先看它。

### 升级与备份

```bash
docker compose pull && docker compose up -d     # 镜像方式
```

数据库迁移在启动时自动执行，不需要手动跑脚本。备份分两块：

- **数据库**：PostgreSQL 用 `pg_dump`，SQLite 直接拷 `data/pixora.db`（连同 `-wal`、`-shm`）。
- **图片**：本地存储就备份数据目录（`data/blobs` 是物理文件所在，`data/users` 是给人看
  的硬链接镜像树，可选）；对象存储交给厂商的版本控制或生命周期规则。

还有一份备份建议单独存：`PIXORA_MASTER_KEY`（或数据目录里的 `master.key`）。少了它，
数据库里的存储凭据与 SMTP 密码全是废的。

### 排障

| 现象 | 多半是 |
|---|---|
| 直链地址是 `http://172.x.x.x/...` 或容器内网地址 | 没设 `PIXORA_BASE_URL` |
| 登录成功，一刷新又变未登录 | 站点是 https，但 Cookie 没带 `Secure`（`PIXORA_SECURE_COOKIES`），或前后端跨站却没配 `PIXORA_CORS_ORIGINS` |
| 前端页面上写着「服务暂时不可用」 | 后端没起来，或浏览器到后端被拦（跨域没配、域名写错） |
| 跨域请求报 `failed to fetch`，服务端日志里有「跨域预检被拒」 | 前端域名没写进 `PIXORA_CORS_ORIGINS`（要带 `https://`，端口也要写全） |
| 静态托管上页面能开、图片全裂 | 直链还是相对地址：后端没设 `PIXORA_BASE_URL`，或 `config.js` 里没写 `api_base` |
| 注册页说「注册入口已暂停」 | 管理员关了注册；或磁盘占用触发「容量触顶自动关闭」，去「配额与容量」看 |
| 验证码收不到 | SMTP 没配好（服务端日志会写原因）；本地开发时验证码也会写进日志 |
| 上传报「域名收不到邮件」 | 收件域名查不到 MX 记录，注册类发信被拦下（`antispam.mx_check` 可关） |
| `Cache-Control` 看起来不对 | 直链是 `immutable` 一年，带 `?thumb=1` 的是缩略图；私有图片是 `private, max-age=300` |

## 目录结构

```
crates/
├── pixora-crypto/    哈希、口令（Argon2id）、AES-256-GCM、令牌、TOTP、签名
├── pixora-repo/      仓储层：SQL 方言差异收敛在这里（sqlx::Any）
├── pixora-storage/   存储驱动微内核：local / database / github / webdav / telegram / s3
├── pixora-mail/      SMTP 与邮件模板（含内置文案，库里没有就用内置的）
├── pixora-core/      领域层：配置、鉴权、上传、配额、限流、条款、管理
├── pixora-queue/     进程内后台任务：邮件重试、Blob GC、对账、缩略图兜底
└── pixora-server/    单二进制入口：路由、中间件、直链、静态资源
web/                  前端（无框架 TypeScript + esbuild，产物 embed 进二进制）
  └── dist/           构建产物，故意提交进仓库：cargo build 依赖它存在
migrations/           三个方言各一套建表脚本
deploy/               Dockerfile、docker-compose、Caddyfile、.env.example
  └── static/         Vercel / Cloudflare / EdgeOne 的静态前端配置样例
docs/                 技术方案、接口契约、ADR、界面原型、截图
.github/workflows/    CI、Docker 多架构镜像、发行版二进制、前端静态站点
scripts/              冒烟测试（bash / PowerShell）与素材生成
```

## 关键设计

### 内容寻址存储与引用计数

物理文件按 `blobs/{sha256前2位}/{sha256}.{ext}` 存放，`(sha256, kind)` 唯一。
数据库里 `blobs` 记录物理实体，`images` 记录逻辑引用，`blobs.ref_count` 记录被引用次数。

两个用户传了同一张图：磁盘上只有一份，删除互不影响。删除图片只递减计数，
**物理文件由后台 GC 在计数归零后回收**——全库只有 `pixora-queue/src/gc.rs`
一个地方会删物理文件，这是刻意收窄的。

磁盘上还有一棵给人看的镜像树 `users/{uid}/{相册}/{哈希}.{ext}`，用硬链接指向
CAS 实体，`du` 不会重复计算。文件系统不支持硬链接时自动降级为「只维护数据库
虚拟目录树」，这个判断来自驱动声明的 `Capabilities`，不是 `if driver == "local"`。

### 直链

```
https://img.example.com/images/a3f9c1b8…e7.webp
```

只有一级目录加哈希文件名，不含 UID、相册名和原始文件名。一级目录可在后台改，
改完旧链依然有效——映射在数据库里，不在 URL 结构里。响应带
`Cache-Control: immutable`（内容寻址，同一路径内容永不变化）。

是否公开由 `images.visibility` 决定：私有图片只有属主与管理员拿得到，
其余一律 404（不告诉你「存在但没权限」）。

### 账号处置

暂停与封禁都让网页登录与 API 同时失效，区别在后续处理：暂停可申请解封，
封禁要管理员手动解封。**API Token 不删**——鉴权时按账号状态拦，解封即自动恢复。
管理员点确认后由注册发信邮箱自动发出通知邮件，正文里写明解封材料清单
（暂停给清单，封禁只写「请联系管理员」，不承诺解封）。

### 存储路径只走环境变量

容器里的挂载点是编排文件决定的，界面改了也写不进去；配置文件一旦可写，
就等于给了一个远程改服务器目录的入口。所以 `PIXORA_DATA_DIR` 注入，
界面上只读展示来源与当前值（见技术方案里的 ADR-0003）。

### 内存与容量

容量展示有三种模式：`auto` 探测文件系统（识别到 ossfs 这类网络挂载时自动降级，
因为 `statfs` 返回的是伪值）、`manual` 管理员手填总额、`hidden` 不展示。
已用量一律由库内文件实时汇总，不依赖文件系统。

配额是硬约束：所有用户配额合计 + 管理员账户配额 ≤ 系统总额 − 系统占用，
超额时拒绝保存并给出还差多少。磁盘占用触顶时可以自动暂停注册（阈值与恢复值在
「配额与容量」里配），恢复后下一次触顶才会再次暂停，管理员手动改过开关则以人工为准。

## 开发

```bash
# 后端（默认 SQLite，数据落在 ./data）
cargo run -p pixora-server

# 前端（另一个终端；改一行刷新即见，不必重编 Rust）
cd web && npm install && npm run build
PIXORA_WEB_DIR=./web/dist cargo run -p pixora-server
```

首次启动没有安装过时，访问 <http://127.0.0.1:8080/#/install> 走五步安装向导
（环境检查 → 数据库 → 管理员 → 站点与邮件 → 完成）。

没有配 SMTP 时，验证码会写进服务端日志，所以本地开发不需要真邮箱。

### Windows（cmd，不需要 Git Bash）

在 cmd 里逐条执行。第 2 步和第 3 步的顺序不能颠倒：前端产物要被 cargo 嵌进二进制。

```bat
cargo build --workspace
cargo test --workspace

cd web
npm install
npx tsc --noEmit
npm run build
cd ..

cargo build -p pixora-server

powershell -ExecutionPolicy Bypass -File scripts\smoke.ps1
```

起服务：

```bat
set PIXORA_DB_DRIVER=sqlite
set PIXORA_DB_DSN=./data/pixora.db
set PIXORA_DATA_DIR=./data
set PIXORA_BASE_URL=http://127.0.0.1:8080
cargo run -p pixora-server
```

Windows 上有三个坑，都踩过一遍：

1. **`.ps1` 要存成 UTF-8 with BOM。** Windows PowerShell 5.1 在没有 BOM 时按本地
   代码页读脚本，中文注释和字符串会变成乱码并直接报语法错误。
2. **不要把自己的函数命名成 `Curl`。** PowerShell 的命令解析顺序是「别名 > 函数」，
   内置别名 `curl` 指向 `Invoke-WebRequest`，同名函数会被它抢走，报
   「无法将 System.Object[] 转换为参数 Uri 所需的类型」。
3. **curl 的输出要按 UTF-8 解码。** 默认按本地代码页解，服务端返回的 UTF-8 JSON
   会变成乱码导致解析失败；`scripts\smoke.ps1` 里已经处理了这件事。
   同理，**带中文的请求体要走文件**（`--data-binary @文件`），别直接写在命令行上——
   交给原生 `curl.exe` 时会被按本地代码页转一次，服务端收到的是坏掉的 UTF-8。

## 测试

```bash
cargo test                     # 后端全部单元与集成测试（SQLite 临时库）
cd web && npx tsc --noEmit     # 前端类型检查
bash scripts/smoke.sh          # 端到端冒烟：起真服务走完整条主链路
bash scripts/smoke-new.sh      # 针对近期改动的冒烟（需要一台全新实例，端口 8099）
```

`scripts/smoke.sh` 会自己挑一个端口起服务、走完安装向导、上传一张真 PNG、
验证去重与引用计数、拉直链核对响应头与字节，最后清理临时目录。默认端口 18080，
要换就 `bash scripts/smoke.sh 19000`。

### 换方言跑同一套仓储用例

仓储层的集成测试默认在 SQLite 临时库上跑（真实建表 + 真实 SQL）。要在
PostgreSQL 或 MySQL 上验证同一份代码，把连接串交给 `PIXORA_TEST_DB_DSN`：

```bash
PIXORA_TEST_DB_DSN="postgres://pixora:pass@127.0.0.1/pixora" \
  cargo test -p pixora-repo --test dialects -- --nocapture

PIXORA_TEST_DB_DSN="mysql://root:pass@127.0.0.1/pixora" \
  cargo test -p pixora-repo --test dialects -- --nocapture
```

多个连接串用逗号分隔可以一次跑完。不设这个变量时用例自己跳过，
所以没有数据库的机器上 `cargo test` 照样是绿的。这套用例覆盖布尔往返、
唯一冲突识别、UPSERT、内容寻址复用、引用计数归零、批量预检的占位符改写、
标签筛选与时间戳字典序比较——方言差异最容易出问题的几处都在里面。

## CI 与发布

| 工作流 | 什么时候跑 | 干什么 |
|---|---|---|
| [CI](.github/workflows/ci.yml) | 推主干、开 PR、手动 | 前端类型检查与构建；后端全量测试 + 端到端冒烟；Windows 上 `cargo check` 证明能编过 |
| [Docker 镜像](.github/workflows/docker.yml) | 推主干、打标签、手动 | 构建 amd64/arm64 镜像推到 GHCR（`ghcr.io/<owner>/pixora`），配了 Docker Hub 密钥就一起推；手动跑时可以把架构改成只出 amd64，省掉 arm64 那趟 QEMU 模拟 |
| [发行版二进制](.github/workflows/release.yml) | 推 `v*` 标签 | 出 Linux（musl 静态）与 Windows 可执行文件，附 SHA-256 校验和，挂到 Release |
| [前端静态站点](.github/workflows/frontend.yml) | 手动，或仓库变量开了开关 | 把前端产物发到 Vercel / Cloudflare Pages / EdgeOne Makers / GitHub Pages |

发一版：

```bash
git tag v0.1.0 && git push origin v0.1.0
```

CI 里的冒烟是「起真服务、走真 HTTP」那一套，和本地 `scripts/smoke-new.sh` 是同一个脚本，
所以本地跑绿了，CI 基本也是绿的。

两个小提醒：GHCR 上的包跟随仓库的可见性，私有仓库拉镜像前先 `docker login ghcr.io`
（用户名 + 一个带 `read:packages` 的 PAT）；工作流里的 action 目前还是 v4，
GitHub 会提示「Node.js 20 deprecated」——不影响运行，等这些 action 出 v5 再一起升。

## 刻意不做的事

- **不用微服务**：500MB 内存预算下多进程是自相矛盾的。
- **不引入 Redis**：进程内队列够用，少一个组件少一份内存和一套运维。
- **不用 ORM**：sqlx 直接写 SQL 更可控，方言差异集中在仓储层。
- **服务端不做图像缩放**：缩放与哈希是浏览器的事。唯一例外是「缩略图兜底」
  这个可选开关，因为它服务的场景（API 上传）客户端本来就给不了缩略图。
- **不在界面开放存储路径**：见[技术方案里的 ADR-0003](docs/Pixora-技术方案.md)。

## 已知边界

- 前端的手势交互（拖拽、长按多选、双指缩放）需要真机或浏览器手工验证，
  自动化测试覆盖不到。
- 缩略图与哈希的 WASM 解码器（HEIC/TIFF/JXL）按需加载，本项目没有内置解码器：
  这些格式的缩略图由客户端的原生解码能力决定，解不动就退化成「不带缩略图」。
- 多实例部署需要额外的共享设施（限流计数与会话表），当前按单实例设计。
  前后端分开部署（静态托管 + 单个 API 实例）不受这条限制。
- Cloudflare Workers / Vercel Functions 这类纯 JS 边缘运行时跑不了这个二进制
  （它要常驻进程、TCP 连接和文件系统）；边缘那一层只适合放前端，或者像
  `deploy/static/worker-proxy.js` 那样做一层反代。要全在 Cloudflare 上，用
  Containers（x86_64）跑镜像 + R2 存图片 + 托管 Postgres 存数据。

## 许可

MIT。
