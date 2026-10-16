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

Pixora 整体由三部分组成，先认清这三样东西，后面每一步都好理解：

| 角色 | 是什么 | 住在哪 |
|---|---|---|
| **后端** | 一个 Rust 进程（`pixora`），负责接口、直链、上传、后台任务 | 你的服务器 / 容器 |
| **前端** | 一堆静态文件（`main.js`、`index.html`…），用户看到的界面 | 跟着后端一起发（默认），或单独放静态托管 |
| **数据** | 数据库（账号、相册、标签）+ 图片文件 | 本地目录 + SQLite（默认），或云数据库 + 对象存储 |

默认形态里，前端被嵌在后端那个二进制里，所以**只要起一个进程，网站就能用**。
只有想把前端也放上 CDN 时，才需要把两者拆开——那是本文的[第四步](#第四步可选把前端放上-cdn)。

### 开始之前

**要准备的东西**

1. **一台机器**：云服务器（1 核 1G 就够）、家里的小主机或 NAS 都行。系统推荐 Ubuntu/Debian。
2. **一个域名**（可选，但强烈建议）：直链好分享，申请 TLS 证书也顺理成章。
   没有域名也能用，只是访问地址会带端口、浏览器会提示不安全。
3. **Docker**（用 Compose 部署的话）：

   ```bash
   # Ubuntu / Debian 官方一键脚本（国内网络装不上就换镜像源或用系统包管理器）
   curl -fsSL https://get.docker.com | sh
   sudo systemctl enable --now docker

   # 验证
   docker --version && docker compose version
   ```

   Windows / macOS 上装 Docker Desktop 即可，命令一样。

**名词速查**（后面会反复出现）

| 词 | 一句话解释 |
|---|---|
| 环境变量 | 写在 `.env` 文件或平台后台里的一串 `KEY=VALUE`，进程启动时读它 |
| 容器 / 镜像 | 镜像是一个打包好的程序，容器是它的一个运行实例 |
| 反向代理 | 站在最前面的那个服务，负责 https 证书和转发流量（这里是 Caddy） |
| 直链 | 图片的永久地址，形如 `https://你的域名/images/<哈希>.jpg` |
| 直链前缀 | 直链里 `images` 那一段，后台可以改，旧链不会失效 |
| 安装向导 | 第一次打开网站时走的五步引导，用来建管理员账号与数据库 |

**部署顺序建议**（照这个顺序走，出问题好定位）

```
先把后端跑起来 → 打开网站走安装向导 → 能上传能看图 → 再考虑把前端放上 CDN
```

**按你的情况挑入口**

| 你想怎么装 | 从哪看起 |
|---|---|
| 用 Docker（推荐） | [第一步：起后端](#第一步起后端docker-compose) |
| 不用 Docker，直接跑一个可执行文件 | [不用 Docker：单二进制](#不用-docker单二进制) |
| 自己改代码、自己编译 | [从源码构建](#从源码构建) |
| 只想看看接口 | [API 文档](docs/api-contract.md) |

### 第一步：起后端（Docker Compose）

下面三份编排是完整可复制的，**按你选的数据库挑一份**即可。三者的差别只有
「应用那份环境变量」和「数据库那个服务」，其余完全一样。

| 数据库 | 适合 | 内存 | 对应小节 |
|---|---|---|---|
| SQLite | 单机、个人、最省事 | 80MB 以内 | [SQLite 版](#sqlite-版完整-compose) |
| PostgreSQL | 想要正经数据库、单机多人用 | 200MB 左右 | [PostgreSQL 版](#postgresql-版完整-compose) |
| MySQL / MariaDB | 只有 MySQL 运维经验，或公司统一用 MySQL | 300MB 左右 | [MySQL-版](#mysql--mariadb-版完整-compose) |
| 云托管数据库 | 想把数据放云上（Neon / RDS / 云数据库…） | 只算应用那份 | [托管数据库版](#托管数据库版) |

准备目录与 `.env`（三份都要先做，把域名换成你自己的）：

```bash
mkdir -p pixora && cd pixora && mkdir -p data

cat > .env <<'EOF'
# 你的域名；Caddy 用它申请 https 证书。只有 IP、没有域名就先看「不用 Caddy」那节
PIXORA_DOMAIN=img.example.com
# 站点对外地址，和上一行保持一致（只有 IP 时写 http://IP:8080）
PIXORA_BASE_URL=https://img.example.com
EOF

# 加密主密钥与数据库密码，随机生成写进 .env
printf 'PIXORA_MASTER_KEY=%s\n' "$(openssl rand -hex 32)" >> .env
printf 'DB_PASSWORD=%s\n' "$(openssl rand -hex 16)" >> .env
cat .env      # 看一眼：四个值都在
```

> `PIXORA_MASTER_KEY` 用来加密存储后端凭据与 SMTP 密码。丢了它，那些凭据只能重填一遍，
> 所以生成完顺手存进密码管理器，并随数据目录一起备份。
>
> 仓库 `deploy/` 里那份编排把数据库密码这个变量叫 `POSTGRES_PASSWORD`，名字不同、作用一样；
> 两份编排别混着用，挑一份照抄。

#### SQLite 版（完整 compose）

最省事的形态：一个容器、一个数据目录，**整个站点只有一个进程**。

```yaml
# docker-compose.yml —— 与上一行同目录
services:
  pixora:
    # 换成你自己的用户名。`:latest` 只有发过版之后才有，之前用 `:edge`（主干最新构建）；
    # 也可以换成下面注释里的 build 两行，本地现编
    image: ghcr.io/lovefirefly-26710/pixora:latest
    # build:
    #   context: .
    #   dockerfile: Dockerfile
    restart: unless-stopped
    environment:
      # ---------- 数据库：SQLite，文件落在数据目录里 ----------
      PIXORA_DB_DRIVER: sqlite
      PIXORA_DB_DSN: /data/pixora.db
      # ---------- 数据与站点 ----------
      PIXORA_DATA_DIR: /data
      PIXORA_BIND: 0.0.0.0:8080
      # 站点对外地址：直链、邮件里的链接都按它拼。有域名就填域名
      PIXORA_BASE_URL: ${PIXORA_BASE_URL:?填站点对外地址，见 .env}
      # 主密钥（上一步生成的，从 .env 读）
      PIXORA_MASTER_KEY: ${PIXORA_MASTER_KEY:?先按上面的命令生成 .env}
      # 前面有反向代理（Caddy / Nginx / 平台负载均衡）时保持 1
      PIXORA_TRUST_PROXY: "1"
      PIXORA_WORKERS: "2"
      RUST_LOG: info
    volumes:
      - ./data:/data          # 数据库、图片、密钥都在这里，别丢
    ports:
      - "8080:8080"           # 没有反向代理时直接暴露；有 Caddy 时可以删掉这行
    deploy:
      resources:
        limits: { memory: 128M }
    healthcheck:
      test: ["CMD", "/pixora", "--health-probe"]
      interval: 30s
      timeout: 3s
      retries: 3
      start_period: 10s

  # 反向代理 + 自动 https 证书。没有域名、或者平台自带 https 时，整段删掉
  caddy:
    image: caddy:2-alpine
    restart: unless-stopped
    ports: ["80:80", "443:443"]
    environment:
      PIXORA_DOMAIN: ${PIXORA_DOMAIN:?填你的域名，例如 img.example.com}
      ACME_EMAIL: ${ACME_EMAIL:-}
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - ./data:/srv/data:ro
      - caddy_data:/data
      - caddy_config:/config
    depends_on: [pixora]
    deploy:
      resources:
        limits: { memory: 48M }

volumes:
  caddy_data:
  caddy_config:
```

用 Caddy 那份的话，同目录再放一个 `Caddyfile`（两行就够，证书由 Caddy 自动申请）：

```
{$PIXORA_DOMAIN} {
	reverse_proxy pixora:8080
}
```

仓库里那份 `deploy/Caddyfile` 更完整（上传体积上限、静态资源缓存、安全响应头），
可以直接下载来用。

#### PostgreSQL 版（完整 compose）

比 SQLite 多一个数据库容器，数据落在 `./pgdata`。

```yaml
# docker-compose.yml
services:
  pixora:
    image: ghcr.io/lovefirefly-26710/pixora:latest   # 换成自己的用户名；没发过版就写 :edge
    restart: unless-stopped
    environment:
      PIXORA_DB_DRIVER: postgres
      # 主机名 db 就是下面那个服务的名字，容器之间用服务名互相找
      PIXORA_DB_DSN: postgres://pixora:${DB_PASSWORD}@db:5432/pixora
      PIXORA_DATA_DIR: /data
      PIXORA_BIND: 0.0.0.0:8080
      PIXORA_BASE_URL: ${PIXORA_BASE_URL:?填站点对外地址，见 .env}
      PIXORA_MASTER_KEY: ${PIXORA_MASTER_KEY:?先按上面的命令生成 .env}
      PIXORA_TRUST_PROXY: "1"
      PIXORA_WORKERS: "2"
      RUST_LOG: info
    volumes:
      - ./data:/data
    expose: ["8080"]          # 只给 Caddy 用，不对外开端口
    depends_on:
      db:
        condition: service_healthy
    deploy:
      resources:
        limits: { memory: 128M }
    healthcheck:
      test: ["CMD", "/pixora", "--health-probe"]
      interval: 30s
      timeout: 3s
      retries: 3
      start_period: 10s

  db:
    image: postgres:16-alpine
    restart: unless-stopped
    # 内存小的机器上把 PostgreSQL 的默认值压下来，否则它会按机器内存的 1/4 去申请
    command: >
      postgres -c shared_buffers=64MB -c work_mem=4MB
               -c max_connections=50 -c effective_cache_size=128MB
    environment:
      POSTGRES_DB: pixora
      POSTGRES_USER: pixora
      POSTGRES_PASSWORD: ${DB_PASSWORD:?先按上面的命令生成 .env}
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
    ports: ["80:80", "443:443"]
    environment:
      PIXORA_DOMAIN: ${PIXORA_DOMAIN:?填你的域名}
      ACME_EMAIL: ${ACME_EMAIL:-}
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - ./data:/srv/data:ro
      - caddy_data:/data
      - caddy_config:/config
    depends_on: [pixora]
    deploy:
      resources:
        limits: { memory: 48M }

volumes:
  caddy_data:
  caddy_config:
```

#### MySQL / MariaDB 版（完整 compose）

把 `db` 换成 MySQL（MariaDB 也能用，把镜像换成 `mariadb:11`、环境变量改成
`MARIADB_DATABASE` / `MARIADB_USER` / `MARIADB_PASSWORD` / `MARIADB_ROOT_PASSWORD` 即可）。

```yaml
# docker-compose.yml
services:
  pixora:
    image: ghcr.io/lovefirefly-26710/pixora:latest   # 换成自己的用户名；没发过版就写 :edge
    restart: unless-stopped
    environment:
      PIXORA_DB_DRIVER: mysql
      # 云上 MySQL 常见要加 SSL：末尾补 ?ssl-mode=REQUIRED
      PIXORA_DB_DSN: mysql://pixora:${DB_PASSWORD}@db:3306/pixora
      PIXORA_DATA_DIR: /data
      PIXORA_BIND: 0.0.0.0:8080
      PIXORA_BASE_URL: ${PIXORA_BASE_URL:?填站点对外地址，见 .env}
      PIXORA_MASTER_KEY: ${PIXORA_MASTER_KEY:?先按上面的命令生成 .env}
      PIXORA_TRUST_PROXY: "1"
      PIXORA_WORKERS: "2"
      RUST_LOG: info
    volumes:
      - ./data:/data
    expose: ["8080"]
    depends_on:
      db:
        condition: service_healthy
    deploy:
      resources:
        limits: { memory: 128M }
    healthcheck:
      test: ["CMD", "/pixora", "--health-probe"]
      interval: 30s
      timeout: 3s
      retries: 3
      start_period: 10s

  db:
    image: mysql:8.4
    restart: unless-stopped
    # 同上：小机器上把 InnoDB 的缓冲池压到 64MB，默认值会吃掉几百兆
    command: >
      --innodb-buffer-pool-size=64M --innodb-log-buffer-size=8M
      --max-connections=50 --performance-schema=OFF
      --character-set-server=utf8mb4 --collation-server=utf8mb4_unicode_ci
    environment:
      MYSQL_DATABASE: pixora
      MYSQL_USER: pixora
      MYSQL_PASSWORD: ${DB_PASSWORD:?先按上面的命令生成 .env}
      MYSQL_ROOT_PASSWORD: ${DB_PASSWORD}
    volumes:
      - ./mysqldata:/var/lib/mysql
    healthcheck:
      test: ["CMD-SHELL", "mysqladmin ping -h 127.0.0.1 -u root -p$$MYSQL_ROOT_PASSWORD --silent"]
      interval: 10s
      timeout: 5s
      retries: 10
    deploy:
      resources:
        limits: { memory: 300M }

  caddy:
    image: caddy:2-alpine
    restart: unless-stopped
    ports: ["80:80", "443:443"]
    environment:
      PIXORA_DOMAIN: ${PIXORA_DOMAIN:?填你的域名}
      ACME_EMAIL: ${ACME_EMAIL:-}
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - ./data:/srv/data:ro
      - caddy_data:/data
      - caddy_config:/config
    depends_on: [pixora]
    deploy:
      resources:
        limits: { memory: 48M }

volumes:
  caddy_data:
  caddy_config:
```

MySQL 比 PostgreSQL 重一些（同配置下多留 100MB 内存）；字符集固定用 `utf8mb4`，
否则中文标签、emoji 相册名会插不进去。

#### 托管数据库版

不想自己维护数据库（或者后端要放在没有持久磁盘的平台上），把 `db` 服务整个删掉，
改成填连接串。这类库的迁移脚本会在启动时自动跑，你只要把连接串写对：

```yaml
services:
  pixora:
    image: ghcr.io/lovefirefly-26710/pixora:latest   # 换成自己的用户名；没发过版就写 :edge
    restart: unless-stopped
    environment:
      PIXORA_DB_DRIVER: postgres          # 云的 MySQL 就写 mysql
      PIXORA_DB_DSN: ${PIXORA_DB_DSN:?填连接串，见下方示例}
      PIXORA_DATA_DIR: /data
      PIXORA_BIND: 0.0.0.0:8080
      PIXORA_BASE_URL: ${PIXORA_BASE_URL:?填站点对外地址}
      PIXORA_MASTER_KEY: ${PIXORA_MASTER_KEY:?先按上面的命令生成 .env}
      PIXORA_TRUST_PROXY: "1"
      RUST_LOG: info
    volumes:
      - ./data:/data          # 本地图片与密钥还在磁盘上（图片也可以另配对象存储）
    ports: ["8080:8080"]
```

`.env` 里填连接串（**照抄厂商给的那一串，别自己拼**）：

```dotenv
# Neon / Vercel Postgres / Supabase
PIXORA_DB_DSN=postgres://user:pass@ep-xxx-pooler.region.aws.neon.tech/pixora?sslmode=require
# 阿里云 RDS / 腾讯云 CDB（PostgreSQL）
# PIXORA_DB_DSN=postgres://user:pass@pgm-xxx.pg.rds.aliyuncs.com:5432/pixora?sslmode=require
# 云厂商 MySQL / PlanetScale
# PIXORA_DB_DSN=mysql://user:pass@host:3306/pixora?ssl-mode=REQUIRED
```

#### 不用 Caddy：直接把端口开出去

没有域名、或在平台后面（平台自己管 https）时，把 `caddy` 那段删掉，给应用加 `ports`，
`PIXORA_BASE_URL` 填实际访问地址：

```dotenv
# 只有 IP 时
PIXORA_BASE_URL=http://203.0.113.10:8080
# 平台给了域名（如 Railway / Render / Koyeb）
PIXORA_BASE_URL=https://pixora-production.up.railway.app
```

**必须设对**：直链、邮件里的链接、OAuth 回跳都按它拼，写错了症状是「页面能开、图片全裂」。

#### 启动与验证

```bash
docker compose up -d            # 起
docker compose ps               # 三个服务都该是 running / healthy
docker compose logs -f pixora   # 跟日志，按 Ctrl+C 退出（不影响容器）
```

日志里会有一行「Pixora 已启动」，把关键事实都列出来了：

```
Pixora 已启动 version="0.1.0" listen=0.0.0.0:8080 data_dir=/data db="sqlite"
  storage_default=local master_key=环境变量 PIXORA_MASTER_KEY web=已嵌入二进制
```

排障先看这一行：`data_dir` 是不是你挂的那个、`db` 是不是你选的方言、
`master_key` 来源对不对。

再在服务器本机确认服务真的在：

```bash
curl -s http://127.0.0.1:8080/healthz          # 期望输出 ok
curl -s http://127.0.0.1:8080/api/v1/site | head -c 200
```

### 第二步：打开网站走安装向导

浏览器打开 `PIXORA_BASE_URL` 指向的地址，会跳到 `/#/install`，五步：

1. **环境检查**：数据目录能不能写、版本对不对，还会把你注入的数据库连接串显示成
   「已配置 / 未配置」；
2. **数据库**：这一步是**连接自检**，不是配置——数据库在容器启动时就由 `PIXORA_DB_DSN`
   定好了。选上你用的类型（SQLite / PostgreSQL / MySQL / MariaDB），点「测试连接」，
   看到成功就下一步。填的驱动与进程实际连的不一致时它会直说「改完环境变量并重启才切换」；
3. **管理员**：填你自己的邮箱和密码（这是最高权限账号，改不了角色、不能被处置）；
4. **站点与邮件**：站点名、对外地址（就是 `PIXORA_BASE_URL`）、SMTP。
   没配 SMTP 也能用，只是验证码要去看服务端日志；
5. **完成**：装完这个入口就锁上了。

装完就能用：登录 → 传图 → 复制直链。到这一步后端部分就结束了。

### 第三步（可选）：把图片与数据挪到云上

单机够用就跳过。想上云，两件事分别在后台点几下：

- **图片** → 后台「存储管理」加一个 S3 兼容后端（R2 / OSS / COS / MinIO），
  切过去之后新上传走远端，老图继续从本地读，不用搬。
- **数据库** → 改 `PIXORA_DB_DRIVER` + `PIXORA_DB_DSN` 指向托管库，重启容器。
  跨库搬家要自己导数据（`pg_dump` / `.dump` 之后再改连接串）。

细节见[图片存哪儿](#图片存哪儿)与[数据库](#数据库)。

### 第四步（可选）：把前端放上 CDN

这一步能拿到两个好处：静态资源走边缘节点、后端那台机器只扛接口。
代价是多一处要维护的配置。**先把后端跑通（第二步做完了）再回来做这一步。**

#### 先决定接法

前端和后端不同源时，浏览器有两条路可以走：

| | 边缘反代（推荐） | 跨域直连 |
|---|---|---|
| 长什么样 | 前端域名把 `/api/*` 和 `/images/*` 转发给后端 | 前端直接请求后端域名 |
| 前端要填 | 什么都不用填 | `PIXORA_API_BASE` = 后端地址 |
| 后端要填 | `PIXORA_BASE_URL` | `PIXORA_BASE_URL` + `PIXORA_CORS_ORIGINS` |
| 会话 Cookie | 第一方，`SameSite=Lax` | 跨站，自动切 `SameSite=None; Secure`，两边都得 https |
| 何时选 | 大多情况，尤其不想折腾 | 平台不支持反代外部源时 |

#### 前端构建时要填的变量

只有一个：`PIXORA_API_BASE`。

- **反代接法**：**留空**（同源，前端写相对路径就对了）。
- **跨域接法**：填后端地址，比如 `https://img-api.example.com`，不要带结尾斜杠。

它会被写进产物根目录的 `config.js`。各平台填的地方：

| 平台 | 变量名 | 填在哪儿 |
|---|---|---|
| Vercel | `PIXORA_API_BASE` | 项目 → Settings → Environment Variables（勾 Production，**改完要重新部署**） |
| Cloudflare Pages | 同上 | 项目 → Settings → Variables and Secrets → Production |
| EdgeOne Makers | 同上 | 项目 → 构建设置 → 环境变量 |
| GitHub Pages | 同上 | 仓库 Settings → Secrets and variables → Actions → **Variables** 页 |
| 本地 / 自建 CI | 同上 | 构建命令前加：`PIXORA_API_BASE=https://… npm run build` |

不想在平台后台改也行：部署完直接编辑服务器上的 `config.js` 文件，刷新页面即生效
（它是运行时读的，不用重新构建）。

#### Vercel

Vercel 只能放前端——它的函数是 JS 运行时，跑不了这个需要常驻进程的二进制。
**后端按第一步照旧放在你的服务器上**，这里是前端那部分的步骤。

1. 把仓库推到 GitHub，然后在 [vercel.com](https://vercel.com) 用 GitHub 账号登录；
2. **Add New → Project → Import**，选这个仓库；
3. 什么都不用填（仓库根目录的 `vercel.json` 已经写明安装命令、构建命令与输出目录），
   跨域接法的话在 **Environment Variables** 里加 `PIXORA_API_BASE=https://后端地址`；
4. **Deploy**，一两分钟后拿到 `https://<项目名>.vercel.app`；
5. **反代接法**再加一步：把 `vercel.json` 的 `rewrites` 换成下面这份（填你的后端地址），
   提交后 Vercel 会自动重新部署：

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

把两处 `img-api.example.com` 换成你的后端地址；`/images/` 换成你在后台设的直链前缀
（默认就是 `images`）。最后那条是 SPA 回落：让 `/images131231` 这类陌生路径也进前端。

命令行也行：

```bash
npx vercel login
npx vercel --prod          # 在仓库根目录执行，会读 vercel.json
```

跨域接法记得回后端放行前端域名：

```dotenv
PIXORA_CORS_ORIGINS=https://你的项目.vercel.app
```

#### Cloudflare

Cloudflare 这边有三块能力，按需取用：Pages 放前端、Worker 做反代、R2 存图片。

**A. Pages 放前端**

1. Cloudflare 控制台 → Workers & Pages → Create → Pages → Connect to Git，选仓库；
2. 构建配置填：

   | 字段 | 值 |
   |---|---|
   | Framework preset | None |
   | Build command | `npm --prefix web ci && npm --prefix web run build` |
   | Build output directory | `web/dist` |
   | 环境变量 | 跨域接法才填 `PIXORA_API_BASE`；反代接法留空 |

3. Save and Deploy，拿到 `https://<项目名>.pages.dev`；
4. 自定义域名在项目 → Custom domains 里加。

命令行等价（本地构建好之后再发，CI 里也这么用）：

```bash
npx wrangler pages deploy web/dist --project-name pixora
```

**别往产物里放 `404.html`**：Pages 的机制是「没有 404.html 时，没命中静态文件的路径
自动回 `index.html`（200）」，前端的路由正好接住；一旦存在 `404.html`，它就按真 404 处理。

**B. Worker 反代（反代接法才需要）**

`deploy/static/worker-proxy.js` 是现成的：`/api/*`、`/healthz` 与直链转发给后端，
其余交给静态产物。配一个 `wrangler.toml` 再 `npx wrangler deploy` 就行：

```toml
name = "pixora"
main = "worker-proxy.js"
compatibility_date = "2026-01-01"

[assets]
directory = "./web/dist"

[vars]
BACKEND = "https://img-api.example.com"
```

**C. R2 存图片**（后端在哪都能用）

R2 是 S3 兼容的，后台「存储管理」加一个 S3 后端就行：

| 字段 | 值 |
|---|---|
| 驱动 | S3 |
| Endpoint | `https://<账号ID>.r2.cloudflarestorage.com` |
| Region | `auto` |
| Bucket | 你建的桶名 |
| AccessKey / Secret | R2 的 API 令牌（对象读写权限） |

**想把后端也放 Cloudflare**：用 Containers 跑这个镜像。三条硬约束先记住——
容器只跑 x86_64、磁盘每次冷启动都是新的（所以图片必须进 R2、数据必须用托管库）、
镜像要推到 Cloudflare 自己的 registry（GHCR 不能直接拉，得先 `docker pull` 再
`npx wrangler containers push`）。细节看 Cloudflare 的 Containers 文档。

#### EdgeOne Makers

EdgeOne Pages 现在叫 EdgeOne Makers，账号与命令行都兼容。

1. 控制台从 Git 导入仓库，构建命令 `npm --prefix web ci && npm --prefix web run build`，
   输出目录 `web/dist`，Node 版本 20，跨域接法加环境变量 `PIXORA_API_BASE`；
2. 仓库根目录的 `edgeone.json` 已经写好构建配置、SPA 回落与产物缓存头，
   导入时会被自动读取；
3. 命令行（CI 里用 API Token）：

```bash
npx edgeone pages deploy ./web/dist -n pixora -t "$EDGEONE_API_TOKEN"
```

接口怎么通：EdgeOne 的 `rewrites` 是站内路径改写，指向外部域名的反代要看版本，
所以这里建议**跨域接法**——后端 `PIXORA_CORS_ORIGINS=https://你的EdgeOne域名`，
前端 `PIXORA_API_BASE` 填后端地址。绑定域名后 https 自动生效。

#### GitHub Pages

纯静态、免费，适合「前端就用仓库发」的场景。仓库里带了工作流，三步：

1. Settings → Pages → Source 选 **GitHub Actions**；
2. Settings → Secrets and variables → Actions → **Variables** 新增两个：
   `PIXORA_DEPLOY_FRONTEND=true` 和 `PIXORA_API_BASE=https://后端地址`；
3. Actions → 「前端静态站点」→ Run workflow，target 选 `github-pages`。

工作流会把 `index.html` 复制一份成 `404.html`——Pages 没有 SPA 回落，陌生路径会命中
这个 404 页面，但页面内容还是前端本体，路由照常接管。

**一条限制**：Pages 的项目站点挂在 `https://<用户名>.github.io/<仓库名>/` 子路径下，
而前端产物引用的是绝对路径（`/main.js`、`/config.js`），子路径会 404。
要么绑自定义域名，要么把仓库命名为 `<用户名>.github.io` 发到根路径。

#### 验证前端通没通

三条都过就算成了（任一条不过，看[排障](#排障)表）：

```bash
# 1. 后端自己的接口能通（把域名换成你的）
curl -sI https://img-api.example.com/api/v1/site | head -1      # 期望 200

# 2. 前端的接口通道能通
curl -sI https://你的前端域名/api/v1/site | head -1             # 反代接法才有这条，期望 200
```

3. 打开前端域名：广场能出图、登录后刷新不掉线、能传一张图。

### 后端还能放哪儿

前端放静态托管之后，后端只要满足「能长跑一个进程、能给一个地址」就行。常见去处：

| 去处 | 怎么放 | 注意 |
|---|---|---|
| 自己的云服务器 / 小主机 / NAS | 本文第一步的 compose，或下面的二进制 | 最稳，数据在自己手里 |
| Railway / Render / Fly.io / Koyeb / Zeabur | 用本仓库镜像，挂一个持久卷 | 没卷就别用 SQLite 与本地图片，改托管库 + R2 |
| Oracle 免费 ARM 实例 | 直接用多架构镜像（`linux/arm64`） | 一年免费额度足够 |
| Cloudflare Containers | 见上面 C 小节 | 必须配 R2 + 托管库，且只跑 x86_64 |
| 群晖 / 威联通 NAS | 容器管理界面里填 compose | 数据目录指到 NAS 的存储池 |

平台特有的那个端口变量（有的叫 `PORT`、有的要你在面板里填容器端口）直接对应
`PIXORA_BIND`，比如 `PIXORA_BIND=0.0.0.0:8080`。没有持久磁盘的平台一定要把
`PIXORA_DB_DRIVER` / `PIXORA_DB_DSN` 指向托管数据库，否则容器一重建数据就没了。

### 不用 Docker：单二进制

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

前面挂 Nginx 反代（Caddy 的配置见 `deploy/Caddyfile`，比这段短）：

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

Windows 上想开机自启，用 `sc.exe` 或 [NSSM](https://nssm.cc/) 注册成服务，
环境变量写进服务配置里；数据目录记得给足权限。

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

交叉编一个静态二进制（在 x86 机器上出 arm64 的 Linux 可执行文件）：

```bash
rustup target add aarch64-unknown-linux-musl
# 需要一份 musl 交叉工具链，或者用 cargo-zigbuild：
#   cargo install cargo-zigbuild && cargo zigbuild --release --target aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl -p pixora-server
```

CI 里怎么构建、出了哪些产物，见下面的 [CI 与发布](#ci-与发布)。

### 数据库

| 方案 | 环境变量 | 适用 | 备份方式 |
|---|---|---|---|
| SQLite（默认） | `PIXORA_DB_DRIVER=sqlite`、`PIXORA_DB_DSN=/data/pixora.db` | 单机、个人用 | 拷 `data/pixora.db`（连 `-wal`、`-shm` 一起） |
| 自建 PostgreSQL | `PIXORA_DB_DRIVER=postgres`、`postgres://用户:密码@主机:5432/库名` | 一台机器多人用 | `pg_dump` |
| 自建 MySQL | `PIXORA_DB_DRIVER=mysql`、`mysql://用户:密码@主机:3306/库名` | 公司统一用 MySQL | `mysqldump` |
| 云托管 PostgreSQL | 连接串照抄厂商的，**带 SSL 参数** | Neon / Vercel Postgres / Supabase / RDS / PolarDB | 厂商控制台 |
| 云托管 MySQL | `mysql://…?ssl-mode=REQUIRED` | 云厂商 MySQL / PlanetScale | 厂商控制台 |

三种方言各自的建表脚本在 `migrations/{sqlite,postgres,mysql}/`，启动时自动跑，
升级版本也是启动时自动迁移——换库、升级都不需要你手动执行 SQL。

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
| `PIXORA_BIND` | `0.0.0.0:8080` | 监听地址（平台要求特定端口时改这里） |
| `PIXORA_TRUST_PROXY` | `1` | 信任几层代理的转发头 |
| `PIXORA_CORS_ORIGINS` | 空 | 放行的前端来源（跨域接法才需要） |
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
docker compose pull && docker compose up -d     # 拉新镜像并重建容器
```

数据库迁移在启动时自动执行。备份分两块：

- **数据库**：见[数据库](#数据库)那栏的「备份方式」。
- **图片**：本地存储就备份数据目录（`data/blobs` 是物理文件所在，`data/users` 是给人看
  的硬链接镜像树，可选）；对象存储交给厂商的版本控制或生命周期规则。

还有一份单独存：`PIXORA_MASTER_KEY`（或数据目录里的 `master.key`）。少了它，
数据库里的存储凭据与 SMTP 密码全是废的。

### 排障

| 现象 | 多半是 |
|---|---|
| 直链地址是 `http://172.x.x.x/...` 或容器内网地址 | 没设 `PIXORA_BASE_URL` |
| 登录成功，一刷新又变未登录 | 站点是 https 但 Cookie 没带 `Secure`；或跨域接法却没配 `PIXORA_CORS_ORIGINS` |
| 前端页面上写着「服务暂时不可用」 | 后端没起来，或浏览器到后端被拦（跨域没配、域名写错） |
| 跨域请求报 `failed to fetch`，后端日志有「跨域预检被拒」 | 前端域名没写进 `PIXORA_CORS_ORIGINS`（要带 `https://`，端口也要写全） |
| 静态托管上页面能开、图片全裂 | 后端没设 `PIXORA_BASE_URL`，或跨域接法没填 `PIXORA_API_BASE` |
| 前端页面 404 / 白屏，控制台报 `/main.js` 404 | 站点挂在子路径下（比如 `/<仓库名>/`），产物引用的是绝对路径 |
| 注册页说「注册入口已暂停」 | 管理员关了注册；或磁盘占用触发「容量触顶自动关闭」，去「配额与容量」看 |
| 验证码收不到 | SMTP 没配好（服务端日志会写原因）；本地开发时验证码也会写进日志 |
| 上传报「域名收不到邮件」 | 收件域名查不到 MX 记录，注册类发信被拦下（`antispam.mx_check` 可关） |
| 容器起来又退出 | `docker compose logs pixora` 看最后几行；常见是 `PIXORA_MASTER_KEY` 没给 |
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
| [Docker 镜像](.github/workflows/docker.yml) | 推主干、打标签、手动 | 构建镜像推到 GHCR（`ghcr.io/<你的用户名>/pixora`），配了 Docker Hub 密钥就一起推 |
| [发行版二进制](.github/workflows/release.yml) | 推 `v*` 标签，或手动填版本号 | 出 Linux（musl 静态）与 Windows 可执行文件，附 SHA-256 校验和，挂到 Release |
| [前端静态站点](.github/workflows/frontend.yml) | 手动，或仓库变量开了开关 | 把前端产物发到 Vercel / Cloudflare Pages / EdgeOne Makers / GitHub Pages |

什么都不用配就能用：推一次主干，GHCR 上就有镜像；想要一版发行包，按下面任一方式。

**出镜像**（推主干时自动，也可以手动点一次）：

```bash
docker pull ghcr.io/<你的用户名>/pixora:edge      # 主干最新一次构建
docker pull ghcr.io/<你的用户名>/pixora:latest    # 最近一次发版
docker pull ghcr.io/<你的用户名>/pixora:1.2.3     # 指定版本
```

默认只构建 `linux/amd64`。要 ARM（甲骨文免费实例、树莓派、Apple 芯片的服务器）
就在 Actions → Docker 镜像 → Run workflow 里把 platforms 填成
`linux/amd64,linux/arm64`——arm64 那趟靠 QEMU 模拟来编，第一次要等很久，
之后有缓存。首次推成功后**记得把包的可见性改成公开**（GitHub 左边 Packages →
这个包 → Package settings → Change visibility → Public），否则别人 `docker pull`
会提示未授权。

**发一版**（二选一）：

```bash
# 方式一：打标签推上去（推荐，版本号留在 git 历史里）
git tag v0.1.0 && git push origin v0.1.0
```

方式二：Actions → 发行版二进制 → Run workflow，版本号填 `0.1.0`
（不用写 `v`，工作流会自己加，并创建 `v0.1.0` 这个标签）。两种方式出的产物一样：
Linux 与 Windows 的可执行文件、SHA-256 校验和，都挂在 Releases 页面上。

CI 里的冒烟是「起真服务、走真 HTTP」那一套，和本地 `scripts/smoke-new.sh` 是同一个脚本，
所以本地跑绿了，CI 基本也是绿的。

工作流里的 action 目前还是 v4，GitHub 会提示「Node.js 20 deprecated」——不影响运行，
等这些 action 出 v5 再一起升。

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
