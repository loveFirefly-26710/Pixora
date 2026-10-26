# Pixora 部署方案

当前版本 0.1.8。本文按部署方式分三条主线：纯 Docker、Linux 单二进制、Windows 单二进制。前端嵌在后端二进制里，前后端同源。每条线都从准备工作写到验收，命令可以直接复制。

第一次部署建议先走 Docker + SQLite（§4.2），跑通后再换数据库。遇到报错先看 §10 的对照表。

## 0. 怎么用这份文档

### 0.1 从零到上线的顺序

| 步骤 | 做什么 | 完成标志 | 详见 |
|---|---|---|---|
| 1 | 准备服务器与系统 | 能 SSH 登录，磁盘有空间 | §2.1 |
| 2 | 解析域名 | `dig +short 你的域名` 返回服务器 IP | §2.1 |
| 3 | 放行端口 | 22、80、443 可从外网访问 | §2.1 |
| 4 | 生成主密钥 | 得到 64 位十六进制字符串 | §2.2 |
| 5 | 装 Docker 或下载二进制 | `docker compose version` 可用，或 `pixora` 能执行 | §4.0 / §5 / §6 |
| 6 | 写配置 | 域名、站点地址、主密钥、数据库密码都换成自己的 | §4.1 |
| 7 | 启动 | `docker compose ps` 显示 `pixora` 为 healthy | §4.2 |
| 8 | 走安装向导 | 五步完成，能登录后台 | §7 |
| 9 | 验收 | 能上传、复制直链并打开，重启后数据还在 | §7 |
| 10 | 做一次备份 | 备份文件能列出内容 | §4.7 |
| 11 | 安全验收 | 响应头齐、`/install/*` 返回 404、管理员开启 TOTP、申诉邮箱已填 | §12 |

### 0.2 文档约定

| 占位 | 换成什么 |
|---|---|
| `img.example.com` | 你的域名 |
| `https://img.example.com` | 站点对外地址，和域名一致 |
| `<主密钥>` | `openssl rand -hex 32` 的输出 |
| `<数据库密码>` | 自己设的强密码，建议用十六进制 |
| `ghcr.io/lovefirefly-26710/pixora` | 镜像地址；fork 过仓库就换成自己的用户名 |

命令默认在 Linux/macOS/WSL 的 shell 里执行，标了 `PS>` 的是 Windows PowerShell。Docker 命令按“当前用户已加入 docker 组”来写；没有加入就前面加 `sudo`。

## 1. 先认三样东西

| 角色 | 是什么 | 住在哪 |
|---|---|---|
| 后端 | 一个 Rust 进程（可执行文件 `pixora`），负责接口、直链、上传、后台任务 | 服务器或容器 |
| 前端 | 静态文件（`index.html`、`main.js` 等），用户看到的界面 | 嵌在后端二进制或镜像里，不单独托管 |
| 数据 | 数据库（账号、相册、标签、设置）与图片文件 | 数据目录；数据库可以换成自建或托管 |

前端被 `rust-embed` 嵌进二进制，启动一个进程网站就能用。前端与接口始终同源，不需要跨域配置。

### 名词速查

| 词 | 一句话解释 |
|---|---|
| 环境变量 | `KEY=VALUE` 形式的启动配置，这个项目的部署参数都走它 |
| 容器 / 镜像 | 镜像是打包好的程序，容器是它的运行实例 |
| 反向代理 | 站在最前面处理 https 证书与转发的服务，本文的编排里是 Caddy |
| 直链 | 图片的永久地址，形如 `https://你的域名/images/<哈希>.jpg` |
| 直链前缀 | 直链里 `images` 那一段，后台可以改，旧链不会失效 |
| 安装向导 | 第一次打开网站时的五步引导：环境检查、数据库自检、管理员、站点与邮件、完成 |
| 主密钥 | 加密存储后端凭据与 SMTP 密码的钥匙，丢了只能重新填凭据 |

### 部署顺序

先让后端跑起来，走完安装向导，确认能登录、能上传、能打开直链。前端已经嵌在同一个进程里，不需要额外部署。

## 2. 开工前准备好这四样

### 2.1 机器、域名与端口

云服务器 1 核 1G 就能跑，推荐 Ubuntu 22.04/24.04 或 Debian 12。Windows 也支持原生部署（§6），不需要 WSL。NAS、小主机同样可以。

域名要做 A（IPv4）或 AAAA（IPv6）记录，指向服务器公网 IP。用编排里自带的 Caddy 时，域名直接解析到服务器，放行 80 和 443；用 1Panel 或 Nginx 时，公开域名解析到那一层，应用只监听本机端口（默认 `127.0.0.1:2345`）。云厂商的安全组和系统防火墙都要放行对应端口。国内云主机用域名对外提供 80/443 服务，通常要先完成备案。

登录服务器先确认三件事：

```bash
uname -m                       # x86_64 或 aarch64
dig +short img.example.com     # 应返回你的服务器 IP
sudo ufw status                # 或 firewall-cmd --list-all
```

没有 `dig` 就用 `getent hosts img.example.com`。域名还没解析好之前，Caddy 申请证书或反向代理回源都会失败，先把这一步做完。

### 2.2 主密钥

```bash
openssl rand -hex 32
```

输出 64 位十六进制，就是 `PIXORA_MASTER_KEY`。存进密码管理器，并随数据目录一起备份。

它加密数据库里的敏感配置：S3/R2 的 AccessKey、WebDAV 密码、SMTP 密码、GitHub Token。换了它，这些值全部解不开，只能进后台重填。图片和账号密码不受影响，口令是单向哈希。

不设这个变量也能启动：首次运行会在数据目录里生成 `master.key`。单机自托管够用，但多实例、迁移容器、备份恢复时容易丢，生产环境建议显式注入。

### 2.3 站点对外地址

写用户浏览器里看到的地址：

```dotenv
PIXORA_BASE_URL=https://img.example.com
# Pixora 不提供代理服务；服务器访问 Google/GitHub 所需的出网能力
# 由部署环境自己解决。Linux 设 HTTPS_PROXY/ALL_PROXY，Windows 会自动读取系统代理；
# 需要覆盖系统设置时用下面的 PIXORA_HTTP_PROXY。
# PIXORA_HTTP_PROXY=http://10.0.0.2:7890
```

只有 IP、没有域名就写 `http://203.0.113.10:8080`。直链、邮件里的链接、OAuth 回跳都按它拼。忘了设置时，反向代理后面会推出来容器内网地址（`http://172.x.x.x/...`），现象是“页面能开，图片全裂”。

安装向导第 4 步会把这个地址写进数据库。之后**以数据库里的值为准**，环境变量只作为向导的预填值。换域名时要去后台“系统设置”改，不能只改 compose。

### 2.4 数据目录

一个可读写的目录，用来放 SQLite 文件、图片、主密钥。建议：

- Docker：`./data` 挂到容器里的 `/data`；
- Linux：`/srv/pixora/data`，属主是运行服务的用户；
- Windows：`C:\pixora\data` 这类普通目录，不要放 `C:\Program Files` 下。

这个目录就是全部身家，备份就是备份它（§4.7）。

## 3. 选一条路线

| 你的情况 | 走哪条 | 看哪节 |
|---|---|---|
| 有 Docker，想最快跑起来 | 纯 Docker 单机 | §4 |
| 有 Linux 服务器，不想装 Docker | Linux 单二进制 + systemd | §5 |
| 只有 Windows 机器或小主机 | Windows 单二进制 + 服务 | §6 |
| 数据库已经单独部署了 | Docker 或二进制都行，把连接指向它 | §4.5 / §8 |

数据库怎么选：

| 数据库 | 适合 | 验证情况（2026-10） |
|---|---|---|
| **SQLite**（默认） | 单机、个人、最省事 | 299 项冒烟全过；备份就是拷文件 |
| PostgreSQL | 想用正经数据库、单机多人 | 上次实测 298 项全过（PostgreSQL 17.6）；本次审计未复跑 |
| MySQL | 团队统一用 MySQL | 上次实测 298 项全过（MySQL 8.4.0）；本次审计未复跑 |

三种都实测过，切换只改 `PIXORA_DB_DRIVER` 与 `PIXORA_DB_DSN`，细节见 §8。审计后新增的 SVG、限流和安全头检查先在 SQLite 上跑通；发布前如果准备上 PostgreSQL/MySQL，建议用 §8 的命令再跑一遍这套冒烟。

几条限制先说清楚：

- **SQLite 只能一个实例用**。不要让两个容器或两个进程指向同一个 `pixora.db`，SQLite 的写锁会把它们互相挡住。要横向扩展就换 PostgreSQL/MySQL。
- 发行包只有 `linux-x86_64` 与 `windows-x86_64`。ARM Linux 需要从源码构建（README 的“从源码构建”）。
- 容器镜像默认只出 `linux/amd64`。要 ARM 镜像，手动跑“发行版与镜像”工作流，`platforms` 填 `linux/amd64,linux/arm64`。
- MariaDB 与 MySQL 共用同一套迁移和驱动，但这次没有单独跑 MariaDB，见 §8。

## 4. 方案 A：纯 Docker

一个文件跑完整站：应用、数据库（SQLite 版没有单独的数据库服务）、自动 https 的反向代理都在一份 compose 里。Caddy 的配置内嵌在文件末尾，不需要 `.env`，也不需要单独的 Caddyfile。

### 4.0 装 Docker、拿镜像

Ubuntu / Debian 用 Docker 官方安装脚本：

```bash
curl -fsSL https://get.docker.com | sudo sh
sudo systemctl enable --now docker
sudo usermod -aG docker "$USER"
newgrp docker            # 当前终端生效；也可以退出重新登录
docker run --rm hello-world
docker compose version   # 需要 2.23 以上
```

`docker` 组的成员权限等同于 root，只把可信账号加进去。不想用脚本就按 Docker 官方文档加 apt 源，效果一样。

从 GHCR 拉镜像：

```bash
docker pull ghcr.io/lovefirefly-26710/pixora:0.1.4
```

报 `denied` 或 `unauthorized` 时，说明包是私有的，或者当前 Docker 没登录。用一个带 `read:packages` 权限的 GitHub PAT：

```bash
echo "<PAT>" | docker login ghcr.io -u <GitHub用户名> --password-stdin
```

包想公开给别人拉，去 GitHub → Packages → 这个包 → Package settings → Change visibility → Public。如果 CI 推镜像时报 `write_package`，是旧仓库留下的同名包在挡路：删掉旧包，或者给当前仓库授权，详见 §4.8。

镜像标签：

| 标签 | 是什么 |
|---|---|
| `latest` | 最近一次正式发版，生产可用 |
| `0.1.4` | 固定版本，生产推荐 |
| `edge` | 主干最新代码，包含尚未发版的修复 |
| `master` / `sha-<短哈希>` | 调试与追溯用 |

生产环境建议把 compose 里的 `:latest` 换成具体版本号，升级时再改，出问题好回滚。

部署前先确认要用的标签真的存在（`docker pull ...:0.1.4`）。GHCR 推送失败时 `latest` 会停在上一个版本；这时要么先修好权限，要么临时用 `:edge`，不要拿 0.1.0 跑 PostgreSQL/MySQL。

建目录、下载编排文件：

```bash
sudo mkdir -p /opt/pixora
sudo chown "$USER":"$USER" /opt/pixora
cd /opt/pixora

# SQLite 版
curl -fsSL https://raw.githubusercontent.com/loveFirefly-26710/Pixora/master/deploy/docker-compose.yml -o docker-compose.yml

# PostgreSQL 版用这个：
# curl -fsSL https://raw.githubusercontent.com/loveFirefly-26710/Pixora/master/deploy/docker-compose.postgres.yml -o docker-compose.yml

# MySQL 版用这个：
# curl -fsSL https://raw.githubusercontent.com/loveFirefly-26710/Pixora/master/deploy/docker-compose.mysql.yml -o docker-compose.yml
```

仓库还没推上去或网络拉不下来时，直接把 §4.2 / §4.3 / §4.4 里对应的整段 YAML 存成 `docker-compose.yml`。改完先用 Compose 自己渲染一遍：

```bash
docker compose config >/dev/null && echo "编排语法 OK"
```

应用镜像基于 `scratch`，容器里没有 shell、curl、psql。排障用 `/pixora --health-probe`，或者在宿主机上 curl 域名。

### 4.1 要改的就这几处

| 占位符 | 换成 | 在哪 |
|---|---|---|
| `img.example.com` | 你的域名 | `PIXORA_DOMAIN`，Caddy 用它申请证书 |
| `https://img.example.com` | 站点对外地址，和域名一致 | `PIXORA_BASE_URL` |
| `REPLACE_WITH_openssl_rand_hex_32` | `openssl rand -hex 32` 的输出 | `PIXORA_MASTER_KEY` |
| `REPLACE_WITH_A_STRONG_PASSWORD` | 一个强密码 | PostgreSQL/MySQL 版才有，出现 2～3 处，改成同一个值 |

主密钥的占位符故意不是合法的十六进制：忘了改，应用启动会直接报错，不会拿占位文字当密钥用。

SQLite 版 compose 里 `PIXORA_TRUST_PROXY` 默认是 `1`，因为前面有 Caddy。直接暴露公网、前面没有反向代理时改成 `0`，否则客户端可以伪造 `X-Forwarded-For` 绕过 IP 黑名单和限流。HTTPS 下 `PIXORA_SECURE_COOKIES` 不填也会按 `PIXORA_BASE_URL` 自动开；如果站点从 HTTPS 改成 HTTP 调试，记得显式设成 `false`，并清掉浏览器里的旧 Cookie。

本地音乐库要在容器里读：在 compose 的 `volumes` 里加 `- ./music:/music:ro`，再给应用加 `PIXORA_MUSIC_DIR: /music`。这样后台「本地音乐目录」填什么都不作数，实际扫描的是容器内的 `/music`；换宿主机目录时改挂载并重建即可。

数据库密码建议用 `openssl rand -hex 16` 生成。密码里的 `$` 会被 Compose 当变量插值，`@`、`:` 会把 DSN 拆错；用十六进制可以完全避开这些坑。非要用特殊字符，就把 DSN 里的字符做百分号编码，并把 `$` 写成 `$$`。

安装向导第 4 步的“基础 URL”会覆盖 `PIXORA_BASE_URL`。向导现在会预填环境变量里的地址；如果手动改了，之后以数据库里的值为准。

### 4.2 A1：SQLite 版（推荐）

应用 + Caddy 两个容器，数据就是一个 SQLite 文件。

```yaml
# Pixora 单机部署 —— 一个文件跑完整站（应用 + 自动 https 的反向代理）。
#
# 用法三步：
#   1. 把这份文件存成 docker-compose.yml
#   2. 改下面标着「必改」的三处：域名、站点地址、主密钥
#   3. docker compose up -d
#
# 数据落在同目录的 ./data（Docker 会自动建），备份就是备份这个目录。
# 没有域名：删掉 caddy 服务与文件末尾的 configs，给 pixora 加 ports: ["8080:8080"]，
# PIXORA_BASE_URL 写 http://服务器IP:8080。
#
# 需要 Docker Compose 2.23 以上（用 configs.content 内嵌了 Caddy 配置）：
#   docker compose version
#
# 镜像默认用 :latest（最近一次发版）。生产环境建议固定成具体版本号（如 :0.1.4），
# 升级时再改，出问题好回滚；主干上尚未发版的修复在 :edge 里。
#
# 安装向导第 4 步的「基础 URL」会写进数据库，之后以数据库里的值为准。
# 向导会预填这里的环境变量值；换域名时记得两边一起改。
#
# 想从源码构建镜像，见 README 的「从源码构建」一节。

services:
  pixora:
    # fork 过仓库就换成自己的用户名。生产建议把 :latest 固定成具体版本号
    image: ghcr.io/lovefirefly-26710/pixora:latest
    restart: unless-stopped
    environment:
      PIXORA_DB_DRIVER: sqlite
      PIXORA_DB_DSN: /data/pixora.db
      PIXORA_DATA_DIR: /data
      PIXORA_BIND: 0.0.0.0:8080
      # 必改：站点对外地址。直链、邮件里的链接都按它拼，写错的表现是「页面能开、图全裂」
      PIXORA_BASE_URL: https://img.example.com
      # 必改：openssl rand -hex 32 的输出。留着这串占位符会启动失败（故意的）
      PIXORA_MASTER_KEY: REPLACE_WITH_openssl_rand_hex_32
      # 前面有反向代理时保持 1：只信最靠近服务端的一层转发头
      PIXORA_TRUST_PROXY: "1"
      PIXORA_WORKERS: "2"
      RUST_LOG: info
    volumes:
      - ./data:/data
    expose:
      - "8080"
    deploy:
      resources:
        limits: { memory: 128M }
    healthcheck:
      test: ["CMD", "/pixora", "--health-probe"]
      interval: 30s
      timeout: 3s
      retries: 3
      start_period: 10s
    # 默认的 json-file 日志不封顶，长时间跑会把磁盘写满
    logging:
      driver: json-file
      options:
        max-size: "10m"
        max-file: "3"

  caddy:
    image: caddy:2-alpine
    restart: unless-stopped
    ports: ["80:80", "443:443"]
    environment:
      # 必改：你的域名，Caddy 用它申请证书
      PIXORA_DOMAIN: img.example.com
    configs:
      - source: caddyfile
        target: /etc/caddy/Caddyfile
    depends_on:
      - pixora
    deploy:
      resources:
        limits: { memory: 48M }
    logging:
      driver: json-file
      options:
        max-size: "10m"
        max-file: "3"

# Caddy 的配置就写在这里，不用另外放一个 Caddyfile
configs:
  caddyfile:
    content: |
      # Compose 对 content 里的变量符号也会插值，这里写两遍做转义；
      # Caddy 实际读到的是单个变量引用，再从容器环境取域名。
      {$$PIXORA_DOMAIN} {
          encode zstd gzip
          # 上传体积上限，与应用侧的上限保持一致
          request_body {
              max_size 1GB
          }
          reverse_proxy pixora:8080
          header {
              Strict-Transport-Security "max-age=31536000; includeSubDomains"
              X-Frame-Options "SAMEORIGIN"
              X-Content-Type-Options "nosniff"
              Referrer-Policy "strict-origin-when-cross-origin"
              -Server
          }
      }
```

启动与验收：

```bash
cd /opt/pixora
docker compose pull
docker compose config >/dev/null && echo "编排语法 OK"
docker compose up -d
docker compose ps
docker compose logs --tail=100 pixora
docker compose exec pixora /pixora --health-probe && echo "应用健康"
curl -s https://img.example.com/healthz
```

预期结果：

- `docker compose ps` 里 `pixora` 是 `Up (healthy)`，`caddy` 是 `Up`；
- 日志里有 `INFO Pixora 已启动 version="0.1.4" ... db="sqlite"`；
- `/healthz` 返回 `{"db":"ok","status":"ok","uptime_seconds":...}`。

`--health-probe` 只确认进程能响应；数据库是否通，看 `/healthz` 里的 `db` 字段。Caddy 第一次申请证书要几秒到几十秒，失败就看 `docker compose logs caddy`。

证书就绪后打开 `https://img.example.com`，会跳转安装向导（§7）。

### 4.3 A2：PostgreSQL 版

```yaml
# Pixora 单机部署 —— PostgreSQL 版（一个文件跑完整站）。
#
# 用法三步：存成 docker-compose.yml → 改三处「必改」与数据库密码 → docker compose up -d
# 数据库密码在文件里出现 2 处（DSN 与 POSTGRES_PASSWORD），改成同一个值。
#
# 需要 Docker Compose 2.23 以上（configs.content 内嵌了 Caddy 配置）。
# 镜像默认用 :latest；生产建议固定成具体版本号（如 :0.1.4）。
# 安装向导第 4 步的「基础 URL」会写进数据库，之后以数据库里的值为准。

services:
  pixora:
    image: ghcr.io/lovefirefly-26710/pixora:latest
    restart: unless-stopped
    environment:
      PIXORA_DB_DRIVER: postgres
      # 必改：密码两处一起改（这一行与下面 db 服务的 POSTGRES_PASSWORD）
      PIXORA_DB_DSN: postgres://pixora:REPLACE_WITH_A_STRONG_PASSWORD@db:5432/pixora
      PIXORA_DATA_DIR: /data
      PIXORA_BIND: 0.0.0.0:8080
      PIXORA_BASE_URL: https://img.example.com
      PIXORA_MASTER_KEY: REPLACE_WITH_openssl_rand_hex_32
      PIXORA_TRUST_PROXY: "1"
      PIXORA_WORKERS: "2"
      RUST_LOG: info
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
    healthcheck:
      test: ["CMD", "/pixora", "--health-probe"]
      interval: 30s
      timeout: 3s
      retries: 3
      start_period: 10s
    logging:
      driver: json-file
      options:
        max-size: "10m"
        max-file: "3"

  db:
    image: postgres:16-alpine
    restart: unless-stopped
    command: >
      postgres -c shared_buffers=64MB -c work_mem=4MB
               -c max_connections=50 -c effective_cache_size=128MB
    environment:
      POSTGRES_DB: pixora
      POSTGRES_USER: pixora
      # 必改：与上面 DSN 里的密码保持一致
      POSTGRES_PASSWORD: REPLACE_WITH_A_STRONG_PASSWORD
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
    logging:
      driver: json-file
      options:
        max-size: "10m"
        max-file: "3"

  caddy:
    image: caddy:2-alpine
    restart: unless-stopped
    ports: ["80:80", "443:443"]
    environment:
      PIXORA_DOMAIN: img.example.com
    configs:
      - source: caddyfile
        target: /etc/caddy/Caddyfile
    depends_on:
      - pixora
    deploy:
      resources:
        limits: { memory: 48M }
    logging:
      driver: json-file
      options:
        max-size: "10m"
        max-file: "3"

configs:
  caddyfile:
    content: |
      # Compose 对 content 里的变量符号也会插值，这里写两遍做转义；
      # Caddy 实际读到的是单个变量引用，再从容器环境取域名。
      {$$PIXORA_DOMAIN} {
          encode zstd gzip
          request_body {
              max_size 1GB
          }
          reverse_proxy pixora:8080
          header {
              Strict-Transport-Security "max-age=31536000; includeSubDomains"
              X-Frame-Options "SAMEORIGIN"
              X-Content-Type-Options "nosniff"
              Referrer-Policy "strict-origin-when-cross-origin"
              -Server
          }
      }
```

启动命令同 §4.2。第一次启动会等数据库健康检查通过，再跑迁移，通常多等十几秒。日志里应是 `db="postgres"`，`/healthz` 的 `db` 为 `ok`；迁移没跑完就多等一会儿，失败看 `docker compose logs db` 与 `docker compose logs pixora`。备份与恢复用 `pg_dump` / `pg_restore`，见 §4.7。

### 4.4 A3：MySQL 版

```yaml
# Pixora 单机部署 —— MySQL 8.0.13+ / MariaDB 版（一个文件跑完整站）。
#
# 用法三步：存成 docker-compose.yml → 改三处「必改」与数据库密码 → docker compose up -d
# 数据库密码在文件里出现 3 处（DSN、MYSQL_PASSWORD、MYSQL_ROOT_PASSWORD），
# 三处改成同一个值。
#
# 需要 Docker Compose 2.23 以上（configs.content 内嵌了 Caddy 配置）。
# 镜像默认用 :latest；生产建议固定成具体版本号（如 :0.1.4）。
# 安装向导第 4 步的「基础 URL」会写进数据库，之后以数据库里的值为准。

services:
  pixora:
    image: ghcr.io/lovefirefly-26710/pixora:latest
    restart: unless-stopped
    environment:
      PIXORA_DB_DRIVER: mysql
      # 必改：密码三处一起改（这一行、MYSQL_PASSWORD、MYSQL_ROOT_PASSWORD）
      # 云上 MySQL 通常还要加 SSL 参数：末尾补 ?ssl-mode=REQUIRED
      PIXORA_DB_DSN: mysql://pixora:REPLACE_WITH_A_STRONG_PASSWORD@db:3306/pixora
      PIXORA_DATA_DIR: /data
      PIXORA_BIND: 0.0.0.0:8080
      PIXORA_BASE_URL: https://img.example.com
      PIXORA_MASTER_KEY: REPLACE_WITH_openssl_rand_hex_32
      PIXORA_TRUST_PROXY: "1"
      PIXORA_WORKERS: "2"
      RUST_LOG: info
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
    healthcheck:
      test: ["CMD", "/pixora", "--health-probe"]
      interval: 30s
      timeout: 3s
      retries: 3
      start_period: 10s
    logging:
      driver: json-file
      options:
        max-size: "10m"
        max-file: "3"

  db:
    image: mysql:8.4
    restart: unless-stopped
    # 小机器上把 InnoDB 的缓冲池压到 64MB，默认值会吃掉几百兆
    command: >
      --innodb-buffer-pool-size=64M --innodb-log-buffer-size=8M
      --max-connections=50 --performance-schema=OFF
      --character-set-server=utf8mb4 --collation-server=utf8mb4_unicode_ci
    environment:
      MYSQL_DATABASE: pixora
      MYSQL_USER: pixora
      # 必改：与上面 DSN 里的密码保持一致
      MYSQL_PASSWORD: REPLACE_WITH_A_STRONG_PASSWORD
      MYSQL_ROOT_PASSWORD: REPLACE_WITH_A_STRONG_PASSWORD
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
    logging:
      driver: json-file
      options:
        max-size: "10m"
        max-file: "3"

  caddy:
    image: caddy:2-alpine
    restart: unless-stopped
    ports: ["80:80", "443:443"]
    environment:
      PIXORA_DOMAIN: img.example.com
    configs:
      - source: caddyfile
        target: /etc/caddy/Caddyfile
    depends_on:
      - pixora
    deploy:
      resources:
        limits: { memory: 48M }
    logging:
      driver: json-file
      options:
        max-size: "10m"
        max-file: "3"

configs:
  caddyfile:
    content: |
      # Compose 对 content 里的变量符号也会插值，这里写两遍做转义；
      # Caddy 实际读到的是单个变量引用，再从容器环境取域名。
      {$$PIXORA_DOMAIN} {
          encode zstd gzip
          request_body {
              max_size 1GB
          }
          reverse_proxy pixora:8080
          header {
              Strict-Transport-Security "max-age=31536000; includeSubDomains"
              X-Frame-Options "SAMEORIGIN"
              X-Content-Type-Options "nosniff"
              Referrer-Policy "strict-origin-when-cross-origin"
              -Server
          }
      }
```

字符集固定 `utf8mb4`，否则中文标签、emoji 相册名插不进去。MySQL 比 PostgreSQL 重一些，同配置下多留 100MB 内存。启动后确认日志里的 `db="mysql"` 和 `/healthz` 的 `db` 为 `ok`；第一次初始化通常要二三十秒。

MariaDB 把镜像换成 `mariadb:11`，环境变量换成 `MARIADB_DATABASE` / `MARIADB_USER` / `MARIADB_PASSWORD` / `MARIADB_ROOT_PASSWORD`。迁移共用一套 MySQL 的实现，但 MariaDB 这次没有单独验证，见 §8。

### 4.5 A4：数据库已经单独部署了

数据库不在这个编排里：可能是同一台机器上用面板装的，可能在局域网另一台机器，也可能是云托管。做法一样——从 §4.2 的 SQLite 版出发，把连接指过去。

先建专用库和账号，不要和别的应用共用一个库。

PostgreSQL（在数据库所在机器执行）：

```bash
sudo -u postgres psql <<'SQL'
CREATE USER pixora WITH PASSWORD '换成强密码';
CREATE DATABASE pixora OWNER pixora;
SQL
```

MySQL / MariaDB：

```sql
CREATE DATABASE pixora CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
CREATE USER 'pixora'@'%' IDENTIFIED BY '换成强密码';
GRANT ALL PRIVILEGES ON pixora.* TO 'pixora'@'%';
FLUSH PRIVILEGES;
```

然后改 compose 的三处：

1. `PIXORA_DB_DRIVER` 改成 `postgres` 或 `mysql`；
2. `PIXORA_DB_DSN` 换成下面的连接串；
3. 给 `pixora` 服务加 `extra_hosts`（只有连宿主机时才需要）。

```yaml
  pixora:
    environment:
      PIXORA_DB_DRIVER: postgres
      PIXORA_DB_DSN: postgres://pixora:换成强密码@host.docker.internal:5432/pixora
    extra_hosts:
      - "host.docker.internal:host-gateway"
```

地址怎么写：

| 数据库在哪 | DSN 里的主机 | 要不要 `extra_hosts` |
|---|---|---|
| 同一台机器（宿主机） | `host.docker.internal` | 要 |
| 局域网另一台机器 | 那台的 IP，如 `192.168.1.20` | 不要 |
| 另一个 compose 或面板里的容器 | 开了端口用 `host.docker.internal`；或把两边接到同一个 Docker 网络，用服务名 | 前者要 |
| 云托管 | 厂商给的主机名 | 不要 |

如果库是 1Panel 里的容器（例如 `1Panel-postgresql-owhZ`），最稳的是让 pixora 直接加入它所在的网络，DSN 用容器名：

```yaml
services:
  pixora:
    networks:
      - 1panel-network

networks:
  1panel-network:
    external: true
```

只在文件末尾声明 `networks` 不够，`pixora` 服务本身也要挂上；否则 Compose 会把它放进自己的 `default` 网络，容器里解析不到数据库名，日志会是 `failed to lookup address information`。验证：

```bash
docker network inspect 1panel-network \
  --format '{{range .Containers}}{{.Name}}{{"\n"}}{{end}}'
```

两个容器都出现在输出里，DSN 才写得通。连宿主机上直接安装的数据库才需要 `extra_hosts` 和 `host.docker.internal`。

PostgreSQL 侧要放开：

1. `postgresql.conf` 的 `listen_addresses` 改成 `'*'` 或具体网卡地址；
2. `pg_hba.conf` 加一行：

   ```
   host    all    all    172.16.0.0/12    scram-sha-256
   ```

3. 重启数据库：`sudo systemctl restart postgresql`；
4. 防火墙放行 5432，来源收在容器网段：

   ```bash
   sudo ufw allow from 172.16.0.0/12 to any port 5432 proto tcp
   ```

MySQL 侧要放开：

1. `mysqld.cnf` 的 `bind-address` 改成 `0.0.0.0` 或具体网卡地址；
2. 账号来源尽量收紧（`'pixora'@'172.%'`），不要长期用 `%`；
3. 重启：`sudo systemctl restart mysql`；
4. 防火墙放行 3306。

面板（1Panel、宝塔这类）里装的库，数据库详情页通常有“允许访问的 IP”，把 `172.16.0.0/12` 加进去，等于上面的 `pg_hba.conf` 与防火墙两步。跨机器建议走 SSL，连接串末尾加 `?sslmode=require`（PostgreSQL）或 `?ssl-mode=REQUIRED`（MySQL）。

云托管只是把 DSN 换成厂商给的那串，不需要 `extra_hosts`，SSL 参数别删：

```dotenv
# Neon / Supabase
postgres://user:pass@ep-xxx-pooler.region.aws.neon.tech/pixora?sslmode=require
# 阿里云 RDS / 腾讯云 CDB（PostgreSQL）
postgres://user:pass@pgm-xxx.pg.rds.aliyuncs.com:5432/pixora?sslmode=require
# 云厂商 MySQL / PlanetScale
mysql://user:pass@host:3306/pixora?ssl-mode=REQUIRED
```

启动后先看日志最后几行，确认迁移执行完、`Pixora 已启动`，再跑一次 `/healthz`。连不上时按 §4.8 的“连不上单独部署的库”逐项对。

### 4.6 不用 Caddy：已有反向代理

两种情况会走到这里：只有 IP、没有域名；或者前面已经有一层 Nginx / 1Panel。做法是删掉 `caddy` 服务与文件末尾的 `configs:` 整段，给 `pixora` 加端口映射。

```yaml
    # 直接对外：
    ports: ["8080:8080"]
    # 前面是本机 Nginx/1Panel，只让本机访问：
    # ports: ["127.0.0.1:2345:2345"]
```

`PIXORA_BASE_URL` 写用户浏览器里实际打开的地址。只有 IP 就写 `http://203.0.113.10:8080`；有域名就写 `https://你的域名`。**不要把内部端口写进基础 URL**，比如 `https://img.example.com:999` 是错的，浏览器不会去连那个内部端口。

前面有 Nginx 或平台负载均衡时，`PIXORA_TRUST_PROXY` 保持 `1`；直接暴露到公网、前面没有代理时设 `0`，否则客户端可以伪造转发头。

#### 1Panel / Nginx 反向代理

前面已经有一层 Nginx 或 1Panel 时，pixora 的 compose 只保留本机端口映射，不要内置 Caddy：

```yaml
    ports:
      - "127.0.0.1:2345:2345"
```

反向代理要把所有路径转到应用，不能只代理 `/api`：直链 `/images/...`、前端路由与上传接口都在这一个进程里。

```nginx
location / {
    proxy_pass http://127.0.0.1:2345;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_set_header X-Forwarded-For $remote_addr;
}
```

公开域名直接解析到这台反向代理；`PIXORA_BASE_URL` 写浏览器里实际打开的地址（如 `https://img.example.com`），不要带内部端口。只有这一层代理时 `PIXORA_TRUST_PROXY` 保持 `1`；直接暴露到公网、前面没有代理时设 `0`，否则客户端可以伪造转发头。

上线前按顺序验证：

```bash
# 1) 绕过反向代理，直接问应用
curl -I -H 'Host: img.example.com' 'http://127.0.0.1:2345/healthz'

# 2) 走公开入口
curl -I 'https://img.example.com/healthz'
```

第 1 步通、第 2 步不通，先看反向代理的站点匹配、证书与上游地址；两步都不通，先看 Pixora 容器日志。图片预览失败时，确认直链里没有内部端口，再单独测 `/images/<哈希>.<扩展名>?thumb=2`。

### 4.7 升级、备份、恢复

升级：

```bash
cd /opt/pixora
docker compose pull
docker compose up -d
docker compose logs --tail=50 pixora
```

数据库迁移在启动时自动执行，只向前不回头。升级前先做备份。生产环境建议在 compose 里固定版本号，而不是一直用 `latest`。

拉新镜像不需要删除 GHCR 上的包，也不需要先在服务器上删旧镜像。`docker compose pull` 会拿到 compose 里写的标签，`docker compose up -d` 发现镜像 ID 变了就会重建容器；旧的本地镜像留着可以回滚，想清理再 `docker image prune`。如果 compose 固定的是 `:0.1.4`，要把标签改成新版本；用 `:latest` 的话，只有正式 Release 发布后它才会移动，平时推主干只更新 `:edge` / `:master` / `:sha-<短哈希>`。

备份 SQLite 版（数据目录 + 图片 + 主密钥）：

```bash
cd /opt/pixora
docker compose stop pixora
sudo tar -czf "pixora-data-$(date +%F).tar.gz" data
tar -tzf "pixora-data-$(date +%F).tar.gz" | head      # 确认压缩包能读
docker compose start pixora
```

恢复 SQLite 版：

```bash
cd /opt/pixora
docker compose stop pixora
sudo rm -rf data
sudo tar -xzf pixora-data-2026-10-17.tar.gz
docker compose start pixora
docker compose logs --tail=50 pixora
```

`rm -rf data` 不可逆，先确认备份文件在、能列出内容，再执行。

备份 PostgreSQL：

```bash
cd /opt/pixora
docker compose exec -T db pg_dump -U pixora -d pixora -Fc > "pixora-$(date +%F).dump"
```

恢复 PostgreSQL（应用先停）：

```bash
cd /opt/pixora
docker compose stop pixora
docker compose exec -T db pg_restore -U pixora -d pixora --clean --if-exists < pixora-2026-10-17.dump
docker compose start pixora
```

备份 MySQL：

```bash
cd /opt/pixora
docker compose exec -T db mysqldump -u pixora -p'<数据库密码>' \
  --single-transaction --no-tablespaces pixora > "pixora-$(date +%F).sql"
```

恢复 MySQL：

```bash
cd /opt/pixora
docker compose stop pixora
docker compose exec -T db mysql -u pixora -p'<数据库密码>' pixora < pixora-2026-10-17.sql
docker compose start pixora
```

数据库在别的机器或面板里时，把 `docker compose exec db ...` 换成在那台机器上直接跑 `pg_dump` / `pg_restore` / `mysqldump` / `mysql`，主机和账号用 §4.5 里那套。

回滚镜像：把 `image` 改回旧版本号，`docker compose up -d`。如果新版本已经迁移过数据库，旧版本未必认，要连备份一起恢复。

不要用 `docker compose down -v` 清数据；`-v` 会删命名卷。本文的编排用绑定目录，但养成习惯没坏处。

### 4.8 Docker 这条路的常见坑

| 现象 | 原因与处理 |
|---|---|
| 启动报“主密钥格式不对” | compose 里的 `PIXORA_MASTER_KEY` 还是占位符，换成 `openssl rand -hex 32` 的输出 |
| Caddy 起不来，日志里域名是空的，或提示 `{}` | Caddyfile 里的域名变量被 Compose 插值吃掉了。本文的编排写的是 `{$$PIXORA_DOMAIN}`，复制时不要改成单个 `$`；数据库密码里的 `$` 也要写成 `$$` 或用十六进制密码 |
| `additional properties 'content' not allowed` | Docker Compose 低于 2.23，不认内嵌的 Caddy 配置。升级 compose；或删掉 `configs:` 整段，给 caddy 加 `command: caddy reverse-proxy --from https://你的域名 --to http://pixora:8080`（代价是没有那几行安全响应头） |
| `pull access denied` / `unauthorized` | GHCR 包是私有的，先 `docker login ghcr.io`，PAT 要有 `read:packages` |
| CI 推镜像报 `denied: permission_denied: write_package` | 同名 GHCR 包是旧仓库留下的，当前仓库的令牌没有写权限。去 Packages 删掉旧包，或在包设置里给当前仓库授权，然后重跑失败的任务 |
| `exec format error` | 镜像架构和机器不一致。默认镜像只有 amd64；ARM 机器要手动构建 arm64 镜像，或改用二进制 |
| 容器起来又退出 | `docker compose logs pixora` 看最后几行；最常见是主密钥没换、数据库连不上、数据目录写不进去 |
| `80/443 端口被占用` | 宿主机上另有 Nginx/Apache；改 Caddy 的端口，或先停掉别的服务 |
| 证书申请失败 | 域名没解析到这台机器、80/443 没放行（云安全组也要看），或者同一个域名被别的服务占用 |
| 浏览器打开是 502 | Caddy 起来了但应用没起来，`docker compose ps` 看 `pixora` 是否 healthy |
| 连不上单独部署的库 | 按 §4.5 逐项对：`host.docker.internal` 有没有配 `extra_hosts`；PG 的 `pg_hba.conf` / `listen_addresses`；MySQL 的 `bind-address` 与账号来源；防火墙 5432/3306 |
| 图片裂开、地址是 `172.x.x.x` | `PIXORA_BASE_URL` 没填对，或安装向导里写错了基础 URL |
| 上传报 413 | 内嵌配置里的 `request_body` 上限被改小了；默认 1GB，与应用侧的 1GB 硬上限一致 |
| 日志把磁盘写满 | 本文编排已经给每个服务加了 10MB × 3 的日志轮转；自己改过的话检查 `logging` 段 |
| `database is locked`（SQLite） | 有两个进程或容器在写同一个 `pixora.db`。SQLite 只能单实例 |
| `./data` 权限报错 | 镜像以 root 运行，通常不用改；只有 rootless Docker 或用户命名空间才需要按映射的 UID 授权 |
| 推了主干但 `:latest` 没变 | 版本号没改，这一版已经发过 Release；工作流只更新 `:edge` / `:master` / `:sha-<短哈希>`。要发新版，先 `sh scripts/bump-version.sh` 再推 |
| 想“删掉镜像重新构建” | 不需要删 GHCR 包。删包会连权限与可见性一起重置；正常升级只需推新标签，服务器上 `docker compose pull` 即可 |

## 5. 方案 B：Linux 单二进制

发行版提供静态链接（musl）的 Linux 可执行文件，不依赖 glibc，Alpine、群晖、老 CentOS 都能跑。发行包只有 x86_64；ARM Linux 要从源码构建。

### 5.1 下载与校验

在 [Releases](https://github.com/loveFirefly-26710/Pixora/releases) 下载对应版本。以 0.1.4 为例：

```bash
VERSION=0.1.4
curl -LO "https://github.com/loveFirefly-26710/Pixora/releases/download/v${VERSION}/pixora-v${VERSION}-linux-x86_64.tar.gz"
curl -LO "https://github.com/loveFirefly-26710/Pixora/releases/download/v${VERSION}/SHA256SUMS.txt"
sha256sum -c SHA256SUMS.txt --ignore-missing   # 期望：...: OK
tar -xzf "pixora-v${VERSION}-linux-x86_64.tar.gz"
ls pixora/                                      # pixora  README.md  .env.example  deploy/
```

### 5.2 目录与用户

```bash
id pixora >/dev/null 2>&1 || \
  sudo useradd --system --home /srv/pixora --shell /usr/sbin/nologin pixora
sudo mkdir -p /srv/pixora/data
sudo install -m 755 pixora/pixora /srv/pixora/pixora
sudo chown -R pixora:pixora /srv/pixora
sudo chmod 750 /srv/pixora/data
```

### 5.3 环境变量文件

配置集中到 `/etc/pixora.env`，systemd 单元里不塞密钥：

```bash
sudo tee /etc/pixora.env >/dev/null <<'EOF'
PIXORA_BIND=127.0.0.1:8080
PIXORA_DATA_DIR=/srv/pixora/data
PIXORA_DB_DRIVER=sqlite
PIXORA_DB_DSN=/srv/pixora/data/pixora.db
PIXORA_BASE_URL=https://img.example.com
# Pixora 不提供代理服务；服务器访问 Google/GitHub 所需的出网能力
# 由部署环境自己解决。Linux 设 HTTPS_PROXY/ALL_PROXY，Windows 会自动读取系统代理；
# 需要覆盖系统设置时用下面的 PIXORA_HTTP_PROXY。
# PIXORA_HTTP_PROXY=http://10.0.0.2:7890
PIXORA_MASTER_KEY=<主密钥>
# 前面有 Nginx/Caddy 时填 1；直接暴露到公网填 0
PIXORA_TRUST_PROXY=1
# HTTPS 站点保持 true；只在本地 http 调试时改 false
PIXORA_SECURE_COOKIES=true
PIXORA_COOKIE_SAMESITE=lax
PIXORA_WORKERS=2
RUST_LOG=info
EOF
sudo chown root:root /etc/pixora.env
sudo chmod 600 /etc/pixora.env
```

`PIXORA_TRUST_PROXY` 的具体层数要和实际链路一致：只挂一层 Nginx 就填 1，直连公网就填 0。填大了比填小了更危险——客户端可以自己往 `X-Forwarded-For` 里塞地址，把 IP 黑名单和限流绕过去。

用 PostgreSQL/MySQL 时改这两行：

```dotenv
PIXORA_DB_DRIVER=postgres
PIXORA_DB_DSN=postgres://pixora:换成密码@127.0.0.1:5432/pixora
# 或
PIXORA_DB_DRIVER=mysql
PIXORA_DB_DSN=mysql://pixora:换成密码@127.0.0.1:3306/pixora
```

值里不要带引号；含空格的路径才需要引号。监听 `127.0.0.1` 是为了只让本机反向代理访问；想直接对外就写 `0.0.0.0:8080`，并自己开防火墙。

### 5.4 systemd 单元

`/etc/systemd/system/pixora.service`：

```ini
[Unit]
Description=Pixora
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=pixora
Group=pixora
WorkingDirectory=/srv/pixora
EnvironmentFile=/etc/pixora.env
ExecStart=/srv/pixora/pixora
Restart=always
RestartSec=3
TimeoutStopSec=30
NoNewPrivileges=true
PrivateTmp=true
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now pixora
systemctl status pixora --no-pager
sudo journalctl -u pixora -f
```

### 5.5 反向代理

Nginx（`/etc/nginx/sites-available/pixora`，链接到 `sites-enabled`）：

```nginx
server {
    listen 443 ssl http2;
    server_name img.example.com;

    # 略大于应用的单文件上限（默认 20MiB），与应用侧的 64MiB 请求上限对齐
    client_max_body_size 64m;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        # 只信最靠近服务端的一层；客户端伪造的 X-Forwarded-For 会被覆盖
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_read_timeout 300s;
    }

    # 可选：让 Nginx 直接发本地图片，应用只回 X-Accel-Redirect
    # 先在 /etc/pixora.env 里加 PIXORA_ACCEL_PREFIX=/data-files
    location /data-files/ {
        internal;
        alias /;
    }
}
```

证书用 certbot：`sudo certbot --nginx -d img.example.com`。

Caddy 更简单，`/etc/caddy/Caddyfile`：

```
img.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

`alias /` 配合 `internal` 只在反向代理内部跳转时生效，外部直接访问 `/data-files/...` 会被拒绝。不用这个功能就整段删掉，`PIXORA_ACCEL_PREFIX` 也留空。

### 5.6 验证

```bash
curl -s http://127.0.0.1:8080/healthz        # db 应为 ok
curl -s https://img.example.com/api/v1/site | head -c 200
sudo journalctl -u pixora -n 50 --no-pager
```

浏览器打开 `https://img.example.com`，走安装向导（§7）。

### 5.7 升级与回滚

```bash
sudo systemctl stop pixora
sudo cp /srv/pixora/pixora /srv/pixora/pixora.bak-$(date +%F)
sudo install -m 755 pixora/pixora /srv/pixora/pixora
sudo chown pixora:pixora /srv/pixora/pixora
sudo systemctl start pixora
systemctl status pixora --no-pager
```

数据库迁移在启动时自动执行、只向前不回头。回滚二进制之前先备份数据库；新版本迁过的库，旧版本未必认。

### 5.8 Linux 上容易踩的坑

| 现象 | 原因与处理 |
|---|---|
| `Permission denied` 写数据目录 | `/srv/pixora/data` 的属主不是 `pixora`，`chown -R pixora:pixora /srv/pixora` |
| 启动后立刻退出，日志说地址被占用 | 8080 被别的进程占了；`ss -lntp | grep 8080`，换端口并同步改 `PIXORA_BASE_URL` |
| Nginx 502 | 应用没起来，或 `PIXORA_BIND` 写成了容器/别的地址；`curl http://127.0.0.1:8080/healthz` |
| CentOS/RHEL 上 Nginx 502 且应用正常 | SELinux 不允许 Nginx 连后端：`sudo setsebool -P httpd_can_network_connect 1` |
| 环境变量里有空格导致启动失败 | `/etc/pixora.env` 的值不要加引号；含空格才加，且注意 systemd 的解析规则 |
| 登录成功，刷新掉登录 | 站点是 https 但 `PIXORA_BASE_URL` 写成 http，或反向代理没传 `X-Forwarded-Proto` |

## 6. 方案 C：Windows 单二进制

发行包里是 `pixora\pixora.exe` 加上 `README.md`、`.env.example`、`deploy\`。没有运行时依赖，不需要装 Rust、Node 或 VC++ 运行库。

### 6.1 下载与解压

在 [Releases](https://github.com/loveFirefly-26710/Pixora/releases) 下载 `pixora-v0.1.4-windows-x86_64.zip` 与 `SHA256SUMS.txt`：

```powershell
PS> Expand-Archive .\pixora-v0.1.4-windows-x86_64.zip C:\pixora -Force
PS> Get-FileHash C:\pixora\pixora\pixora.exe -Algorithm SHA256   # 与 SHA256SUMS.txt 对照
PS> cd C:\pixora\pixora
```

目录结构：

```
C:\pixora\pixora\
├── pixora.exe
├── README.md
├── .env.example
└── deploy\           # Caddyfile 与 compose 样例，纯二进制部署用不上
```

### 6.2 先跑起来看看

最小启动，数据落到 `C:\pixora\data`：

```powershell
PS> $env:PIXORA_BIND = "127.0.0.1:8080"
PS> $env:PIXORA_BASE_URL = "http://127.0.0.1:8080"
PS> $env:PIXORA_DATA_DIR = "C:\pixora\data"
PS> .\pixora.exe
```

预期输出（截取）：

```
INFO Pixora 已启动 version="0.1.4" listen=127.0.0.1:8080 data_dir=C:\pixora\data db="sqlite"
     storage_default=local master_key=首次启动自动生成，已写入 C:\pixora\data\master.key web=已嵌入二进制
WARN 主密钥是自动生成的（…）：单机自托管够用，生产环境建议改用 PIXORA_MASTER_KEY 注入并纳入备份
```

另开一个窗口验证：

```powershell
PS> Invoke-RestMethod http://127.0.0.1:8080/healthz
PS> (Invoke-RestMethod http://127.0.0.1:8080/api/v1/site).data.base_url
```

健康探针可以单独跑（容器编排与监控脚本用它）：

```powershell
PS> .\pixora.exe --health-probe; echo $LASTEXITCODE
0
```

### 6.3 首次启动做了什么

在数据目录（默认 `./data`，可用 `PIXORA_DATA_DIR` 改）里：

| 文件 | 说明 |
|---|---|
| `pixora.db` | SQLite 数据库（含 `-wal` / `-shm`），账号、相册、标签、设置都在里面 |
| `master.key` | 只在没有注入 `PIXORA_MASTER_KEY` 时生成；有它才能解开库里的凭据 |
| `blobs/` | 图片与缩略图的物理文件（内容寻址，第一次上传后出现） |
| `users/` | 给人看的硬链接镜像树（可选，方便按账号找图） |

### 6.4 做成开机自启

Windows 上把普通程序注册成服务，最省事的是 [NSSM](https://nssm.cc/)：

1. 下载 NSSM，解压得到 `nssm.exe`，放到 `C:\pixora\nssm.exe`；
2. 以管理员身份打开 PowerShell，建日志目录、注册服务并设置环境变量：

```powershell
PS> New-Item -ItemType Directory -Force C:\pixora\logs | Out-Null
PS> C:\pixora\nssm.exe install Pixora "C:\pixora\pixora\pixora.exe"
PS> C:\pixora\nssm.exe set Pixora AppDirectory "C:\pixora\pixora"
PS> C:\pixora\nssm.exe set Pixora AppEnvironmentExtra `
      PIXORA_BIND=0.0.0.0:8080 `
      PIXORA_BASE_URL=http://192.168.1.10:8080 `
      PIXORA_DATA_DIR=C:\pixora\data `
      PIXORA_TRUST_PROXY=0 `
      PIXORA_MASTER_KEY=你的64位十六进制密钥
PS> C:\pixora\nssm.exe set Pixora AppStdout "C:\pixora\logs\pixora.log"
PS> C:\pixora\nssm.exe set Pixora AppStderr "C:\pixora\logs\pixora.log"
PS> C:\pixora\nssm.exe set Pixora AppRotateFiles 1
PS> C:\pixora\nssm.exe set Pixora AppRotateOnline 1
PS> C:\pixora\nssm.exe set Pixora AppRotateBytes 10485760
PS> C:\pixora\nssm.exe set Pixora AppExit Default Restart
PS> Start-Service Pixora
```

`sc.exe` 也能注册服务，但它要求 exe 自己实现服务协议，这个二进制不是服务程序，直接用会报“服务没有及时响应启动或控制请求”。要么用 NSSM，要么用任务计划程序（`Register-ScheduledTask`，触发器选“计算机启动时”，勾选“不管用户是否登录都要运行”）。

查看与停止：

```powershell
PS> Get-Service Pixora
PS> C:\pixora\nssm.exe restart Pixora
PS> C:\pixora\nssm.exe stop Pixora
```

### 6.5 防火墙与内网访问

只在本机用就绑 `127.0.0.1`，不用动防火墙。要让同一局域网的其他设备访问：

```powershell
PS> $env:PIXORA_BIND = "0.0.0.0:8080"
PS> $env:PIXORA_BASE_URL = "http://192.168.1.10:8080"    # 换成本机内网 IP
PS> New-NetFirewallRule -DisplayName "Pixora 8080" -Direction Inbound -Protocol TCP -LocalPort 8080 -Profile Private -Action Allow
```

然后用 `http://192.168.1.10:8080` 打开。直链与登录都按 `PIXORA_BASE_URL` 拼，改成 IP 后要重启服务让它生效。

想对外提供 https，就在 IIS 或 Caddy for Windows 前面挂一层反向代理，`PIXORA_BASE_URL` 写那个 https 地址，并保留 `PIXORA_TRUST_PROXY=1`。

### 6.6 升级与备份

升级：停服务 → 备份 `data\` → 用新版 `pixora.exe` 覆盖 → 启动。数据库迁移在启动时自动执行。

```powershell
PS> C:\pixora\nssm.exe stop Pixora
PS> Compress-Archive -Path C:\pixora\data -DestinationPath C:\pixora\backup-data-$(Get-Date -Format yyyyMMdd).zip
PS> Copy-Item C:\pixora\new\pixora.exe C:\pixora\pixora\pixora.exe -Force
PS> C:\pixora\nssm.exe start Pixora
```

备份三样：`data\`、`master.key`（在 data 里）、`PIXORA_MASTER_KEY`（如果走环境变量注入）。不要在服务运行时复制 `pixora.db`，先停服务。

### 6.7 Windows 上容易踩的坑

| 现象 | 原因与处理 |
|---|---|
| 启动报 `unable to open database file`，数据目录里也没有 `master.key` | 数据目录不可写（放在 `C:\Program Files` 下，或安全软件拦了写文件）。换到 `C:\pixora\data` 这类普通目录，并给当前用户写权限 |
| 从 Git Bash 里启动，各种诡异 | Git Bash 会把 `TEMP` 设成 `/tmp` 这类 MSYS 路径，原生程序解析不了。用 PowerShell 或 cmd 启动 |
| 服务启动后立刻停止 | NSSM 的 `AppDirectory` 没设，或环境变量没通过 `AppEnvironmentExtra` 传进去；看 `C:\pixora\logs\pixora.log` |
| 浏览器提示证书不安全 | 直接 http 访问是正常的；要 https 就前面挂反向代理并配域名 |
| 端口被占用 | 8080 常被别的开发工具占用，换 `PIXORA_BIND=0.0.0.0:8081` 并同步改 `PIXORA_BASE_URL` |
| 文件被杀毒软件锁住 | 把 `C:\pixora` 加入白名单，尤其是 `data\` 和日志目录 |

在源码目录里折腾时，别把 exe 和 data 放在 cargo 的 `target\` 下面——那是构建系统的工作区，权限和清理规则都不适合放运行数据。

## 7. 安装向导：所有路线都要走一遍

地址是 `https://你的域名/#/install`，未安装时打开首页也会自动跳过去。**页面第一步要求安装口令**：首次启动时服务端日志会打印 `install_token`，同时写入数据目录的 `install.token`（也可以用 `PIXORA_INSTALL_TOKEN` 指定）。口令只用于这次安装，装完接口整体下线；没有口令的写请求返回 403，避免公网实例在运维走完向导前被人抢先建管理员。

五步：

1. **环境检查**：数据目录能不能写、数据库驱动与连接串来源、主密钥来源（环境变量 / 密钥文件 / 自动生成）。页面上会显示“已检测到 N 张表”。
2. **数据库**：这是**连接自检**，不是配置。数据库在进程启动时就由 `PIXORA_DB_DRIVER` / `PIXORA_DB_DSN` 定好了。页面上填的主机、端口、用户名、密码不会切换数据库，服务端只探测当前进程已经连着的那个库；选你实际用的类型，点“测试连接”。如果选的类型和进程实际连的不一致，它会提示“改完环境变量并重启才会切换”。想换库，去改容器环境变量并重建，不是在这个页面改 IP。
3. **管理员**：填邮箱与密码。这个账号是最高权限：改不了角色、不能被处置、不能被删除。密码有强度要求。
4. **站点与邮件**：站点名、基础 URL、直链前缀、SMTP。基础 URL 会预填 `PIXORA_BASE_URL`，保持它等于对外地址；保存后以数据库里的值为准。SMTP 可以整段留空，之后在后台再配；没配 SMTP 时，验证码会写进服务端日志。
5. **完成**：标记已安装，安装接口从此整体下线（再访问返回 404）。这是有意的，否则任何人都能重跑向导重设管理员。

装完立即做六项验收：

| 检查 | 怎么做 | 通过标准 |
|---|---|---|
| 登录 | 用第 3 步的管理员账号登录 | 能进后台，刷新后仍是登录状态 |
| 上传 | 上传一张图片 | 返回直链，复制到新标签页能打开 |
| 直链域名 | 看直链开头 | 是 `https://你的域名/...`，不是内网地址 |
| 安装入口关闭 | `curl -I https://你的域名/api/v1/install/status` | 返回 404，不能再跑向导 |
| 安全响应头 | `curl -I https://你的域名/healthz` | 有 `x-content-type-options`、`x-frame-options`、`referrer-policy`、`permissions-policy`、`content-security-policy` |
| 重启不丢 | `docker compose restart pixora`（二进制则 `systemctl restart pixora`） | 账号、图片、设置都还在，`/healthz` 的 `db` 为 ok |

登录后到「账号 → 安全」开启 TOTP，保存恢复码；到「系统设置 → 邮件」填好 SMTP 与申诉邮箱；到「服务条款」发布当前版本。这三步不做，出问题时只能进服务器改库或看日志。

到这一步就结束了。前端已经嵌在同一个进程里，不需要再单独部署。

## 8. 数据库支持现状

三种方言都跑过端到端冒烟；SQLite 当前 299 项全过，PostgreSQL 与 MySQL 上次实测 298 项：

| 数据库 | 验证版本 | 迁移 | 冒烟 | 服务端 5xx |
|---|---|---|---|---|
| SQLite | 内置 | 9/9 | 299/299 | 0 |
| PostgreSQL | 17.6 | 9/9，26 张表 | 298/298（审计后未复跑） | 0 |
| MySQL | 8.4.0 | 9/9 | 298/298（审计后未复跑） | 0 |

SQLite 是默认，也是依赖最少的一条；PostgreSQL / MySQL 按 §4.3 / §4.4 填连接串，数据库已经单独部署的接法见 §4.5。审计后新增的 SVG 净化、API 限流和安全响应头检查已在 SQLite 上覆盖；发布前如果要用 PostgreSQL/MySQL，建议按本节末尾的命令再跑一遍。

MariaDB 共用 MySQL 的驱动与迁移，这次没有单独跑，不能算“已验证”。用 MariaDB 时建议先在自己的环境跑一遍 §8 末尾的方言用例和冒烟。

### 这次修掉的方言差异

之前 CI 只跑 SQLite，另外两条方言的迁移与查询一直没被走过（建表就失败）。这次装了便携版 PostgreSQL 17.6 与 MySQL 8.4.0 逐条跑，修了这些：

| 层 | 问题 | 处理 |
|---|---|---|
| PG 迁移 | 部分唯一索引的 `WHERE is_default` 要布尔表达式 | 改成 `= 1` |
| PG 迁移 | `must_change_password` 等 int4 列与代码的 i64 不一致 | 统一成 `BIGINT` |
| PG 迁移 | `user_identities` 的时间列是 `TIMESTAMPTZ` | 改回文本 |
| PG 查询 | `SUM(bigint)` 返回 `NUMERIC`，Any 驱动解不成 i64 | `Dialect::sum_i64()` 按方言包 `CAST` |
| PG 查询 | 两参数 `MAX(a, b)` 只有 SQLite 认 | 改用 `GREATEST` |
| PG 查询 | `ON CONFLICT ... count = count + ?` 列名歧义 | 限定成 `rate_counters.count` |
| PG 查询 | 布尔值直接写入 `BIGINT` 列 | 转成 `i64` |
| MySQL 迁移 | 生成列的基列上不能挂级联外键（1215） | 改 `RESTRICT`，删账号时显式清策略行 |
| MySQL 迁移 | `TEXT` 列不能进索引（1170） | 时间列统一 `VARCHAR(32)` |
| MySQL 迁移 | `BOOLEAN` / `TINYINT` 在 Any 里解不出来 | 改 `BIGINT` |
| MySQL 查询 | `TEXT` 被 Any 报成 BLOB，按 `String` 解失败 | 文本读取加“按字节读再转 UTF-8”兜底 |
| MySQL 查询 | `LAST_INSERT_ID()` 是连接级的 | 插入与取 id 在同一条连接上完成 |
| MySQL 查询 | `key` 是保留字 | `storage_objects` 的语句加反引号 |
| MySQL 查询 | 不认 `CAST(... AS BIGINT)` | 按方言给 `SIGNED` |
| MySQL 查询 | `ESCAPE '\'` 会把字符串的结束引号转义掉 | SQL 改写层对 MySQL 去掉这个子句 |
| MySQL 查询 | 唯一冲突只给 SQLSTATE 23000 | 增加按 `Duplicate entry` 消息识别 |

### 自己怎么验

仓储层的方言用例，一个连接串跑一整套：

```bash
PIXORA_TEST_DB_DSN="postgres://user:pass@127.0.0.1/pixora" \
  cargo test -p pixora-repo --test dialects
PIXORA_TEST_DB_DSN="mysql://user:pass@127.0.0.1/pixora" \
  cargo test -p pixora-repo --test dialects
# 逗号分隔可以一次跑完
```

端到端冒烟（先按 §4/§5/§6 起一台服务，再跑 `scripts/smoke-new.sh`）：默认读 `target/smoke/pixora.db`；跑在别的方言上时，给一条“从 stdin 读 SQL、输出制表符分隔结果”的客户端命令：

```bash
SMOKE_DB_CLIENT='psql postgres://user:pass@127.0.0.1/pixora -tA' \
  bash scripts/smoke-new.sh

SMOKE_DB_CLIENT='mysql --batch --skip-column-names --default-character-set=utf8mb4 \
  -h127.0.0.1 -uroot pixora' bash scripts/smoke-new.sh
```

MySQL 那条记得带 `--default-character-set=utf8mb4`：客户端默认按系统代码页输出，中文邮件正文会被解成乱码。

### 换库与迁移

三种方言的表结构一致，数据本身不通用，也没有一键换库工具。换库按“先在测试库演练 → 用数据库自带工具按表搬数据 → 改环境变量 → 重启”走，图片文件不用动；建表在启动时自动执行。同一个库升级版本也是启动时自动迁移。最省事的做法还是继续用原来的库。

## 9. 本次实测记录

环境：Windows 11（10.0.22631）、x86_64；便携版 PostgreSQL 17.6（EDB 二进制包）、MySQL 8.4.0（Community zip），分别跑在 127.0.0.1 的 55432 / 33306 端口，独立数据目录；没有 Docker 守护进程，也没有 WSL。所有服务实例都放在仓库外的 `E:\Develop\db\` 下，不碰仓库里的 `data/`。

下表是实际跑过的内容。发行包那两行用的是 v0.1.0（本次数据库修复前的最后一版）；0.1.4 的二进制与镜像由推送后的工作流构建，发布后建议按 §5.1 再校验一次 SHA256。

| 场景 | 怎么验的 | 结果 |
|---|---|---|
| 发行包完整性（v0.1.0） | 下载 v0.1.0 的 linux/win 两个包与 `SHA256SUMS.txt`，`sha256sum -c` | 两个包都 OK |
| Windows 二进制（v0.1.0） | 解压 zip → 全新数据目录启动 → `/healthz`、`/api/v1/site`、`--health-probe` | 自动生成主密钥、healthz 返回 `db: ok`、探针退出码 0 |
| SQLite 端到端 | 起服务 → 安装向导 → CI 同款冒烟 `scripts/smoke-new.sh` | 299 项全过（2026-10-18 复跑） |
| 单元与集成测试 | `cargo test --workspace` | 全部通过 |
| PostgreSQL 17.6 | 建库 → 启动 → 迁移 → 冒烟（`SMOKE_DB_CLIENT` 指向 psql） | 迁移 9 条/26 张表、冒烟 298/298、服务端 0 个 5xx |
| MySQL 8.4.0 | 建库 → 启动 → 迁移 → 冒烟（客户端带 utf8mb4） | 迁移 9 条、冒烟 298/298、服务端 0 个 5xx |
| 方言用例 | `PIXORA_TEST_DB_DSN=pg,mysql cargo test -p pixora-repo --test dialects` | 两种方言一起通过 |
| 三份 compose | 用 Docker Compose v5.6.0 CLI 执行 `config` 渲染，检查服务清单、`depends_on`、健康检查、端口与卷、`configs` 引用、占位符 | 三份都通过；**容器实跑未做**（本机没有 Docker 守护进程） |
| Caddy 配置 | 从 compose 渲染出 Caddyfile，用 Caddy 2.11.7 `validate` | 三份都是 `Valid configuration` |

没能在这台机器上验的，以及原因：

- **Docker 容器实跑**：没有 Docker Desktop，也没有 WSL2/Hyper-V。编排用真实 Compose CLI 渲染通过，Caddyfile 也用 Caddy 校验通过；但“按 compose 起三个容器、Caddy 真的申请证书”没有在这台机器上做。镜像构建步骤在 CI 里跑过，上次卡在 GHCR 的写权限（§4.8），是否已推到 Registry 要看 Actions 结果。
- **Linux 二进制执行**：没有 WSL 或 Linux 环境，无法直接跑 musl 产物。Linux 的准备步骤按发行包里的二进制位置写好，但没有在 Linux 上实际启动过。

0.1.0 的镜像和二进制早于这次的 PostgreSQL/MySQL 修复；0.1.1 起包含。用非 SQLite 数据库时不要拉 0.1.0。

## 10. 排障

| 现象 | 多半是 |
|---|---|
| 直链地址是 `http://172.x.x.x/...` 或容器内网地址 | 没设 `PIXORA_BASE_URL`，或后台基础 URL 写错了 |
| 直链里带了内部端口（如 `:999`） | 基础 URL 写成了回源地址；公开地址不能带内部端口 |
| 页面能开，图片和直链连接被关闭 | 公开域名没有 DNS 记录、反向代理回源失败，或基础 URL 指向了不存在的域名 |
| 登录成功，一刷新又变未登录 | 站点是 https 但 Cookie 没带 Secure，或基础 URL 与实际访问地址不一致 |
| 前端页面写着“服务暂时不可用” | 后端没起来，或反向代理没有把请求转到应用 |
| 图片直链 404 | 直链前缀不对、文件已经被删，或 Nginx 只代理了 `/api` 没代理 `/images` |
| 上传后缩略图不显示、原图正常 | 缩略图这份实体缺失；先测原图与 `?thumb=2` 两个地址的状态码 |
| 注册页说“注册入口已暂停” | 管理员关了注册；或磁盘占用触发了“容量触顶自动关闭”，去“配额与容量”看 |
| 验证码收不到 | SMTP 没配好（服务端日志会写原因）；开发时验证码也写进日志 |
| 上传报“域名收不到邮件” | 收件域名查不到 MX 记录，注册类发信被拦（`antispam.mx_check` 可关） |
| 容器起来又退出 | `docker compose logs pixora` 看最后几行；常见是主密钥还是占位符，或数据库连不上 |
| 容器日志报 `failed to lookup address information` | pixora 没接到数据库所在的 Docker 网络；把两边放到同一个 external network |
| Caddy 日志里域名是空的 / 启动即退出 | 编排里的 `{$$PIXORA_DOMAIN}` 被改成了单个 `$`，或者数据库密码里的 `$` 被 Compose 插值了 |
| 证书申请失败 | 域名没解析、80/443 没放行、云安全组没开，或域名被别的服务占用 |
| `pull denied` / `unauthorized` | GHCR 包私有或未登录；PAT 要 `read:packages`；CI 推不上去看 §4.8 |
| `exec format error` | 镜像架构与机器不符；ARM 要手动构建 arm64 |
| Windows 下报 `unable to open database file` | 数据目录不可写（放到普通目录，别放 `Program Files` / `target\`）；也别从 Git Bash 启动 |
| SQLite 报 `database is locked` | 有两个进程/容器在写同一个库；SQLite 只能单实例 |
| 磁盘满了 | 看 `docker system df` 与数据目录；编排已加日志轮转，图片要自己清理或上对象存储 |
| `Cache-Control` 看起来不对 | 直链是 `immutable` 一年，带 `?thumb=2` 的是缩略图；私有图片是 `private, max-age=300` |

## 11. 附录

### 11.1 环境变量全表

| 变量 | 默认 | 说明 |
|---|---|---|
| `PIXORA_BIND` | `0.0.0.0:8080` | 监听地址；平台要求特定端口时改这里 |
| `PIXORA_DB_DRIVER` | `sqlite` | `sqlite` / `postgres` / `mysql` |
| `PIXORA_DB_DSN` | 数据目录里的 `pixora.db` | 连接串；托管库照抄厂商给的 |
| `PIXORA_DATA_DIR` | `./data` | 数据目录（SQLite 文件、图片、主密钥） |
| `PIXORA_MASTER_KEY` | 空 | 加密凭据的主密钥；不给就首次启动自动生成到数据目录 |
| `PIXORA_MASTER_KEY_FILE` | 数据目录里的 `master.key` | 密钥文件的路径 |
| `PIXORA_BASE_URL` | 空（按请求推断） | 站点对外地址；反向代理后面**必须**设 |
| `PIXORA_TRUST_PROXY` | `1` | 信任几层代理的转发头；直接暴露公网时设 `0` |
| `PIXORA_COOKIE_SAMESITE` | `lax` | 会话 Cookie 的 SameSite：`lax` 或 `strict` |
| `PIXORA_SECURE_COOKIES` | 按 `BASE_URL` 是否 https | 本地 http 调试可显式设 false |
| `PIXORA_WORKERS` | CPU 核数 | 后台任务并发（邮件重试、Blob GC、对账） |
| `PIXORA_DB_MAX_CONNECTIONS` | SQLite 4 / 其它 10 | 连接池上限 |
| `PIXORA_WEB_DIR` | 空 | 开发用：前端读盘而不是读嵌入产物 |
| `PIXORA_ACCEL_PREFIX` | 空 | 让 Nginx 直接发文件的 `X-Accel-Redirect` 前缀 |
| `PIXORA_DEV_RETURN_CODES` | `false` | 开发用：把验证码放进接口响应（生产别开） |
| `RUST_LOG` | `info` | 日志级别 |

`PIXORA_TRUST_PROXY` 只决定“设置里没写过时用几层”；后台「IP 黑名单 → 策略设置」里保存过 `trust_proxy` / `proxy_hops` 之后，以数据库里的值为准。两处不要写成互相矛盾的配置。

`PIXORA_SECURE_COOKIES` 不填时按 `PIXORA_BASE_URL` 是否 `https://` 自动决定；反向代理终止 TLS、但应用看到的是 http 时，务必显式设 `true`，否则 Cookie 不带 Secure。

### 11.2 数据目录里有什么

```
data/
├── pixora.db           SQLite 数据库（另有 -wal / -shm）
├── master.key          自动生成的主密钥（注入了 PIXORA_MASTER_KEY 时没有这个文件）
├── blobs/              图片与缩略图的物理文件，按哈希分目录
└── users/              硬链接镜像树（按账号组织，可选，删了不影响数据）
```

用 PostgreSQL/MySQL 时，数据库数据在 `pgdata/` 或 `mysqldata/`，备份用 §4.7 的 dump；`data/` 里只剩图片和主密钥。对象存储上的图片不在本地，交给厂商的版本控制或生命周期规则。

备份清单：`data/` 整个目录 + `PIXORA_MASTER_KEY`（如果走环境变量）+ 数据库 dump（如果用外部库）。

### 11.3 版本与发版

版本号只写在 `Cargo.toml` 的 `[workspace.package]` 里。改版本号：

```bash
sh scripts/bump-version.sh 0.1.4     # 连同 Cargo.lock、web/package.json、web/package-lock.json 一起改
git add -A && git commit -m "chore: 0.1.4" && git push
```

推主干后，“发行版与镜像”工作流会读版本号：

- GitHub 上没有这个版本的 Release 时，出两个平台的二进制、建 `v<版本号>` 标签、发 Release，镜像带 `:<版本号>` 与 `:latest`；
- 已经发过的版本只刷新镜像的 `:edge` / 主干名 / `:sha-<短哈希>`。

镜像默认只构建 `linux/amd64`。要 ARM 镜像，在 Actions 里手动运行这个工作流，`platforms` 填 `linux/amd64,linux/arm64`。

GHCR 包默认私有；首次推上去后，去 Packages → 这个包 → Package settings 改可见性，或者给使用方发一个有 `read:packages` 的 PAT。

## 12. 上线前安全检查

这份清单来自 2026-10-17 的代码审计，部署完成后逐项过一遍。审计细节与残留风险见 [`docs/安全与性能审计-2026-10-17.md`](安全与性能审计-2026-10-17.md)。

| 检查项 | 怎么确认 | 不通过的处理 |
|---|---|---|
| 主密钥已备份 | `PIXORA_MASTER_KEY` 在密码管理器里，且随数据目录备份 | 重新生成并重填 SMTP、存储后端凭据；不要只备份数据库 |
| 站点地址是公开地址 | 浏览器打开直链，域名与 `PIXORA_BASE_URL` 一致 | 改环境变量或后台「站点 → 基础 URL」 |
| 代理层数正确 | 有反向代理时 `PIXORA_TRUST_PROXY=1`，直连公网时 `0`；后台 IP 策略与之一致 | 改环境变量并重启；后台策略同步 |
| Cookie 安全 | HTTPS 下响应头 `Set-Cookie` 带 `Secure`；会话 Cookie 带 `HttpOnly`、`SameSite=Lax` | 设 `PIXORA_SECURE_COOKIES=true` |
| 安装入口关闭 | `/api/v1/install/status` 返回 404 | 完成向导收尾；不要保留未安装实例在公网 |
| 管理员二次验证 | 账号 → 安全，TOTP 已开启，恢复码已离线保存 | 立即开启；恢复码只显示一次 |
| 申诉邮箱 | 系统设置 → 邮件 → 申诉邮箱非空，暂停/封禁通知模板可读 | 填运营者真实可收信的地址 |
| 服务条款 | 后台已发布当前版本；正文里的占位符已替换 | 发布前不要把 `【运营者名称】` 之类原样上线 |
| 安全响应头 | `curl -I https://你的域名/healthz` 能看到五个头（§7） | 如果是自建反代，确认没有把应用的响应头覆盖掉 |
| 限流可用 | 用 API Token 连续请求，超过每分钟额度返回 429 与 `Retry-After` | 检查用户策略是否被设成 `unlimited`，管理员默认豁免 |
| IP 规则 | 加一条测试 CIDR，确认命中后返回 403；删除后恢复 | 确认 `scope=all` 规则现在覆盖浏览与直链 |
| 直链权限 | 私有图片换浏览器/无 Cookie 打开应 404；公开图正常 | 检查图片可见性与相册可见性是否分开设置 |
| 备份可恢复 | 在测试目录恢复一次 `data/` 与数据库 dump，能登录、能看图 | 备份不可读等于没有备份 |
| 日志无敏感信息 | 日志里没有 DSN 密码、SMTP 密码、主密钥、Token 明文 | 调整 `RUST_LOG`，不要把请求体打进日志 |
| 多实例 | 只有一个实例跑 SQLite 与 GC；限流/会话共享方案已确认 | 改用 PostgreSQL/MySQL 并补数据库级锁，或保持单实例 |
| 资源上限 | 128M 内存下没有开启远端存储大文件 + 服务端缩略图兜底 | 提高到 256M，或把单文件上限降到 10MiB |

上线后建议每周看一眼：磁盘占用、`/healthz` 的 `db`、上传队列深度、邮件发件箱积压、IP 自动封禁记录。审计报告里列出的未修复项（多实例 GC、条款服务端强制、API 小时额度等）需要按实际部署规模决定是否补。
