# Pixora 部署方案

当前版本 0.1.1。本文按部署方式分三条主线：纯 Docker、Linux 单二进制、Windows 单二进制，前后端分离单独一章。每条线都从准备工作写到验收，命令可以直接复制。

第一次部署建议先走 Docker + SQLite（§4.2），跑通后再换数据库或拆前端。遇到报错先看 §11 的对照表。

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
| 前端 | 静态文件（`index.html`、`main.js` 等），用户看到的界面 | 默认嵌在后端二进制里，也可以单独放静态托管 |
| 数据 | 数据库（账号、相册、标签、设置）与图片文件 | 数据目录；数据库可以换成自建或托管 |

默认形态下前端被 `rust-embed` 嵌进二进制，启动一个进程网站就能用。只有想把前端放到 CDN 时，才需要拆开（§8）。

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

先让后端跑起来，走完安装向导，确认能登录、能上传、能打开直链，再考虑拆前端。顺序反过来会同时面对跨域和接口两套问题，不好定位。

## 2. 开工前准备好这四样

### 2.1 机器、域名与端口

云服务器 1 核 1G 就能跑，推荐 Ubuntu 22.04/24.04 或 Debian 12。Windows 也支持原生部署（§6），不需要 WSL。NAS、小主机同样可以。

域名要做 A（IPv4）或 AAAA（IPv6）记录，指向服务器公网 IP。Caddy 自动申请证书要用到 80 和 443，云厂商的安全组和系统防火墙都要放行。国内云主机用域名对外提供 80/443 服务，通常要先完成备案。

登录服务器先确认三件事：

```bash
uname -m                       # x86_64 或 aarch64
dig +short img.example.com     # 应返回你的服务器 IP
sudo ufw status                # 或 firewall-cmd --list-all
```

没有 `dig` 就用 `getent hosts img.example.com`。域名还没解析好之前，Caddy 申请证书会失败，先把这一步做完。

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
| 想让前端走 CDN | 先按 §4/§5/§6 跑通后端，再拆前端 | §8 |
| 数据库已经单独部署了 | Docker 或二进制都行，把连接指向它 | §4.5 / §9 |

数据库怎么选：

| 数据库 | 适合 | 验证情况（2026-10） |
|---|---|---|
| **SQLite**（默认） | 单机、个人、最省事 | 298 项冒烟全过；备份就是拷文件 |
| PostgreSQL | 想用正经数据库、单机多人 | 298 项冒烟全过（PostgreSQL 17.6） |
| MySQL | 团队统一用 MySQL | 298 项冒烟全过（MySQL 8.4.0） |

三种都实测过，切换只改 `PIXORA_DB_DRIVER` 与 `PIXORA_DB_DSN`，细节见 §9。

几条限制先说清楚：

- **SQLite 只能一个实例用**。不要让两个容器或两个进程指向同一个 `pixora.db`，SQLite 的写锁会把它们互相挡住。要横向扩展就换 PostgreSQL/MySQL。
- 发行包只有 `linux-x86_64` 与 `windows-x86_64`。ARM Linux 需要从源码构建（README 的“从源码构建”）。
- 容器镜像默认只出 `linux/amd64`。要 ARM 镜像，手动跑“发行版与镜像”工作流，`platforms` 填 `linux/amd64,linux/arm64`。
- MariaDB 与 MySQL 共用同一套迁移和驱动，但这次没有单独跑 MariaDB，见 §9。

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
docker pull ghcr.io/lovefirefly-26710/pixora:0.1.1
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
| `0.1.1` | 固定版本，生产推荐 |
| `edge` | 主干最新代码，包含尚未发版的修复 |
| `master` / `sha-<短哈希>` | 调试与追溯用 |

生产环境建议把 compose 里的 `:latest` 换成具体版本号，升级时再改，出问题好回滚。

部署前先确认要用的标签真的存在（`docker pull ...:0.1.1`）。GHCR 推送失败时 `latest` 会停在上一个版本；这时要么先修好权限，要么临时用 `:edge`，不要拿 0.1.0 跑 PostgreSQL/MySQL。

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
# 镜像默认用 :latest（最近一次发版）。生产环境建议固定成具体版本号（如 :0.1.1），
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
              max_size 64MB
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
- 日志里有 `INFO Pixora 已启动 version="0.1.1" ... db="sqlite"`；
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
# 镜像默认用 :latest；生产建议固定成具体版本号（如 :0.1.1）。
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
              max_size 64MB
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
# 镜像默认用 :latest；生产建议固定成具体版本号（如 :0.1.1）。
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
              max_size 64MB
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

MariaDB 把镜像换成 `mariadb:11`，环境变量换成 `MARIADB_DATABASE` / `MARIADB_USER` / `MARIADB_PASSWORD` / `MARIADB_ROOT_PASSWORD`。迁移共用一套 MySQL 的实现，但 MariaDB 这次没有单独验证，见 §9。

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
# Neon / Vercel Postgres / Supabase
postgres://user:pass@ep-xxx-pooler.region.aws.neon.tech/pixora?sslmode=require
# 阿里云 RDS / 腾讯云 CDB（PostgreSQL）
postgres://user:pass@pgm-xxx.pg.rds.aliyuncs.com:5432/pixora?sslmode=require
# 云厂商 MySQL / PlanetScale
mysql://user:pass@host:3306/pixora?ssl-mode=REQUIRED
```

启动后先看日志最后几行，确认迁移执行完、`Pixora 已启动`，再跑一次 `/healthz`。连不上时按 §4.8 的“连不上单独部署的库”逐项对。

### 4.6 不用 Caddy

两种情况会走到这里：只有 IP、没有域名；或者前面已经有一层反向代理。

删掉 `caddy` 服务与文件末尾的 `configs:` 整段，给 `pixora` 加端口映射：

```yaml
    # 直接对外：
    ports: ["8080:8080"]
    # 前面是本机 Nginx/Caddy，只让本机访问：
    # ports: ["127.0.0.1:8080:8080"]
```

`PIXORA_BASE_URL` 改成实际访问地址：

```dotenv
# 只有 IP
http://203.0.113.10:8080
# 平台给了域名（Railway / Render / Koyeb 这类）
https://pixora-production.up.railway.app
```

前面有 Nginx、平台负载均衡或 CDN 时，`PIXORA_TRUST_PROXY` 保持 `1`；直接暴露到公网、前面没有代理时设 `0`，否则客户端可以伪造转发头。

平台给了 https 域名却把地址写成 http，表现是“登录成功，刷新就掉登录”，因为 Cookie 的 Secure 推导跟着 Base URL 走。

### 4.7 升级、备份、恢复

升级：

```bash
cd /opt/pixora
docker compose pull
docker compose up -d
docker compose logs --tail=50 pixora
```

数据库迁移在启动时自动执行，只向前不回头。升级前先做备份。生产环境建议在 compose 里固定版本号，而不是一直用 `latest`。

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
| 上传报 413 | 内嵌配置里的 `request_body` 上限被改小了；默认 64MB，与应用侧一致 |
| 日志把磁盘写满 | 本文编排已经给每个服务加了 10MB × 3 的日志轮转；自己改过的话检查 `logging` 段 |
| `database is locked`（SQLite） | 有两个进程或容器在写同一个 `pixora.db`。SQLite 只能单实例 |
| `./data` 权限报错 | 镜像以 root 运行，通常不用改；只有 rootless Docker 或用户命名空间才需要按映射的 UID 授权 |

## 5. 方案 B：Linux 单二进制

发行版提供静态链接（musl）的 Linux 可执行文件，不依赖 glibc，Alpine、群晖、老 CentOS 都能跑。发行包只有 x86_64；ARM Linux 要从源码构建。

### 5.1 下载与校验

在 [Releases](https://github.com/loveFirefly-26710/Pixora/releases) 下载对应版本。以 0.1.1 为例：

```bash
VERSION=0.1.1
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
PIXORA_MASTER_KEY=<主密钥>
# 前面有 Nginx/Caddy 时填 1；直接暴露到公网填 0
PIXORA_TRUST_PROXY=1
PIXORA_WORKERS=2
RUST_LOG=info
EOF
sudo chown root:root /etc/pixora.env
sudo chmod 600 /etc/pixora.env
```

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

在 [Releases](https://github.com/loveFirefly-26710/Pixora/releases) 下载 `pixora-v0.1.1-windows-x86_64.zip` 与 `SHA256SUMS.txt`：

```powershell
PS> Expand-Archive .\pixora-v0.1.1-windows-x86_64.zip C:\pixora -Force
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
INFO Pixora 已启动 version="0.1.1" listen=127.0.0.1:8080 data_dir=C:\pixora\data db="sqlite"
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

地址是 `https://你的域名/#/install`，未安装时打开首页也会自动跳过去。五步：

1. **环境检查**：数据目录能不能写、数据库驱动与连接串来源、主密钥来源（环境变量 / 密钥文件 / 自动生成）。页面上会显示“已检测到 N 张表”。
2. **数据库**：这是**连接自检**，不是配置。数据库在进程启动时就由 `PIXORA_DB_DRIVER` / `PIXORA_DB_DSN` 定好了。选你实际用的类型，点“测试连接”。如果填的类型和进程实际连的不一致，它会提示“改完环境变量并重启才会切换”。
3. **管理员**：填邮箱与密码。这个账号是最高权限：改不了角色、不能被处置、不能被删除。密码有强度要求。
4. **站点与邮件**：站点名、基础 URL、直链前缀、SMTP。基础 URL 会预填 `PIXORA_BASE_URL`，保持它等于对外地址；保存后以数据库里的值为准。SMTP 可以整段留空，之后在后台再配；没配 SMTP 时，验证码会写进服务端日志。
5. **完成**：标记已安装，安装接口从此整体下线（再访问返回 404）。这是有意的，否则任何人都能重跑向导重设管理员。

装完立即做四项验收：

| 检查 | 怎么做 | 通过标准 |
|---|---|---|
| 登录 | 用第 3 步的管理员账号登录 | 能进后台，刷新后仍是登录状态 |
| 上传 | 上传一张图片 | 返回直链，复制到新标签页能打开 |
| 直链域名 | 看直链开头 | 是 `https://你的域名/...`，不是内网地址 |
| 重启不丢 | `docker compose restart pixora`（二进制则 `systemctl restart pixora`） | 账号、图片、设置都还在，`/healthz` 的 `db` 为 ok |

到这一步后端部分就结束了。要拆前端再看 §8。

## 8. 前后端分开部署（可选）

这一步能拿到两个好处：前端静态资源走 CDN 边缘节点，后端那台机器只扛接口。代价是多一处要维护的配置。先把 §4/§5/§6 的后端和 §7 的向导跑通，再拆。

### 8.1 两条接法

| | 边缘反代（推荐） | 跨域直连 |
|---|---|---|
| 浏览器看到 | 只有一个域名，`/api/*` 由平台转发给后端 | 两个域名，前端直接请求后端 |
| 前端要填 | 什么都不用填 | `PIXORA_API_BASE` = 后端地址 |
| 后端要填 | `PIXORA_BASE_URL` = 前端域名 | `PIXORA_BASE_URL` = 后端域名 + `PIXORA_CORS_ORIGINS` = 前端域名 |
| 会话 Cookie | 第一方，`SameSite=Lax` | 跨站，自动切 `SameSite=None; Secure`，两边都必须 https |
| 直链域名 | 前端域名（`/images/...` 转发） | 后端域名 |
| 什么时候选 | 平台支持改写/代理时优先 | 平台不支持代理外部源时 |

### 8.2 前端产物与 `PIXORA_API_BASE`

前端产物来自两处，任选其一：

- **CI 构建**：推主干后，CI 工作流会上传 `web-dist` 产物，下载解压就是静态站；
- **本地构建**：

```bash
cd web
npm ci
PIXORA_API_BASE=https://img-api.example.com npm run build   # 边缘反代接法留空
# 产物在 web/dist/
```

`PIXORA_API_BASE` 会写进产物根目录的 `config.js`，前端运行时读取。各平台填变量的位置：

| 平台 | 变量名 | 填在哪儿 |
|---|---|---|
| Vercel | `PIXORA_API_BASE` | 项目 → Settings → Environment Variables（勾 Production，**改完要重新部署**） |
| Cloudflare Pages | 同上 | 项目 → Settings → Variables and Secrets → Production |
| EdgeOne Makers | 同上 | 项目 → 构建设置 → 环境变量 |
| GitHub Pages | 同上 | 仓库 Settings → Secrets and variables → Actions → **Variables** 页 |
| 本地 / 自建 CI | 同上 | 构建命令前加：`PIXORA_API_BASE=https://… npm run build` |

不想在平台后台改也行：部署完直接编辑线上的 `config.js`，刷新即生效。仓库里的 `deploy/static/` 放着各平台的配置样例。

### 8.3 Vercel

Vercel 只能放前端：它的函数是 JS 运行时，跑不了这个需要常驻进程的二进制。后端按 §4/§5/§6 放在自己的服务器上。

1. 把仓库推到 GitHub，用 GitHub 账号登录 [vercel.com](https://vercel.com)；
2. **Add New → Project → Import**，选这个仓库；
3. 构建配置二选一：
   - 省事：把仓库里的 `deploy/static/vercel.json` 复制到仓库根目录，导入后什么都不用填。这份默认是同源/跨域接法，边缘反代要按下面第 5 步改 `rewrites`；
   - 手填：**Install Command** `npm --prefix web ci`、**Build Command** `npm --prefix web run build`、**Output Directory** `web/dist`，Root Directory 留空；
4. **跨域接法**：Environment Variables 里加 `PIXORA_API_BASE=https://后端地址`，然后 Deploy；
5. **边缘反代接法**：把 `vercel.json` 的 `rewrites` 换成下面这份（填后端地址），提交后自动重新部署：

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

把两处 `img-api.example.com` 换成后端地址；`/images/` 换成后台设的直链前缀（默认就是 `images`）。最后一条是 SPA 回落。

命令行等价：

```bash
npx vercel login
npx vercel --prod          # 在仓库根目录执行，会读 vercel.json
```

跨域接法记得回后端放行前端域名：

```dotenv
PIXORA_CORS_ORIGINS=https://你的项目.vercel.app
```

改完重启后端。名单精确匹配（含 https、端口），`*` 会被忽略。

### 8.4 Cloudflare

三块能力按需取用：Pages 放前端、Worker 做反代、R2 存图片。

**A. Pages 放前端**

1. Cloudflare 控制台 → Workers & Pages → Create → Pages → Connect to Git，选仓库；
2. 构建配置：

   | 字段 | 值 |
   |---|---|
   | Framework preset | None |
   | Build command | `npm --prefix web ci && npm --prefix web run build` |
   | Build output directory | `web/dist` |
   | 环境变量 | 跨域接法才填 `PIXORA_API_BASE`；反代接法留空 |

3. Save and Deploy，拿到 `https://<项目名>.pages.dev`；
4. 自定义域名在项目 → Custom domains 里加。

命令行等价：

```bash
npx wrangler pages deploy web/dist --project-name pixora
```

别往产物里放 `404.html`：Pages 的机制是“没有 404.html 时，没命中静态文件的路径自动回 index.html（200）”，前端路由正好接住；一旦存在 `404.html`，它就按真 404 处理。

**B. Worker 反代（反代接法才需要）**

`deploy/static/worker-proxy.js` 是现成的：`/api/*`、`/healthz` 与直链转发给后端，其余交给静态产物。配一个 `wrangler.toml` 再部署：

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

**C. R2 存图片**（后端在哪都能用）

R2 是 S3 兼容的，后台“存储管理”加一个 S3 后端：

| 字段 | 值 |
|---|---|
| 驱动 | S3 |
| Endpoint | `https://<账号ID>.r2.cloudflarestorage.com` |
| Region | `auto` |
| Bucket | 你建的桶名 |
| AccessKey / Secret | R2 的 API 令牌（对象读写权限） |

想把后端也放 Cloudflare：用 Containers 跑这个镜像。三条约束——容器只跑 x86_64、磁盘每次冷启动都是新的（图片必须进 R2、数据库必须用托管库）、镜像要推到 Cloudflare 自己的 registry（GHCR 不能直接拉，得先 `docker pull` 再 `npx wrangler containers push`）。

### 8.5 EdgeOne Makers

EdgeOne Pages 现在叫 EdgeOne Makers，账号与命令行都兼容。

1. 控制台从 Git 导入仓库：构建命令 `npm --prefix web ci && npm --prefix web run build`，输出目录 `web/dist`，Node 版本 20；跨域接法加环境变量 `PIXORA_API_BASE`；
2. 想让它自动读配置，把仓库里的 `deploy/static/edgeone.json` 复制到仓库根目录（`edgeone.json` 只认根目录），里面已经写好构建配置、SPA 回落与缓存头；不复制就在控制台手填上面三行：

```json
{
  "name": "pixora",
  "installCommand": "npm --prefix web ci",
  "buildCommand": "npm --prefix web run build",
  "outputDirectory": "./web/dist",
  "rewrites": [
    { "source": "/*", "destination": "/index.html" }
  ],
  "headers": [
    {
      "source": "/(.*)",
      "headers": [
        { "key": "Cache-Control", "value": "no-cache" },
        { "key": "X-Content-Type-Options", "value": "nosniff" }
      ]
    }
  ]
}
```

3. 命令行（CI 里用 API Token）：

```bash
npx edgeone pages deploy ./web/dist -n pixora -t "$EDGEONE_API_TOKEN"
```

接口怎么通：EdgeOne 的 `rewrites` 是站内路径改写，指向外部域名的反代要看版本，所以这里建议**跨域接法**——后端 `PIXORA_CORS_ORIGINS=https://你的EdgeOne域名`，前端 `PIXORA_API_BASE` 填后端地址。绑定域名后 https 自动生效。

### 8.6 GitHub Pages

纯静态、免费，适合“前端就用仓库发”的场景。仓库里带了工作流，三步：

1. Settings → Pages → Source 选 **GitHub Actions**；
2. Settings → Secrets and variables → Actions → **Variables** 新增两个：`PIXORA_DEPLOY_FRONTEND=true` 和 `PIXORA_API_BASE=https://后端地址`；
3. Actions →“前端静态站点”→ Run workflow，target 选 `github-pages`。

工作流会把 `index.html` 复制一份成 `404.html`——Pages 没有 SPA 回落，陌生路径会命中这个 404 页面，但页面内容还是前端本体，哈希路由照常接管。

一条硬限制：Pages 的项目站点挂在 `https://<用户名>.github.io/<仓库名>/` 子路径下，而前端产物引用的是绝对路径（`/main.js`、`/config.js`），子路径下这些资源会 404。要么绑自定义域名，要么把仓库命名为 `<用户名>.github.io` 发到根路径。

### 8.7 后端还能放哪儿

前端放静态托管之后，后端只要满足“能长跑一个进程、能给一个地址”就行：

| 去处 | 怎么放 | 注意 |
|---|---|---|
| 自己的云服务器 / 小主机 / NAS | §4 或 §5 | 最稳，数据在自己手里 |
| Railway / Render / Fly.io / Koyeb / Zeabur | 用本仓库镜像，挂一个持久卷 | 没卷就别用 SQLite 与本地图片，改托管库 + R2 |
| Oracle 免费 ARM 实例 | 手动构建 arm64 镜像，或从源码编译 | 发行镜像默认只有 amd64 |
| Cloudflare Containers | 见 §8.4 C 小节 | 必须配 R2 + 托管库，且只跑 x86_64 |
| 群晖 / 威联通 NAS | 容器管理界面里填 compose | 数据目录指到 NAS 的存储池 |

平台特有的端口变量（有的叫 `PORT`、有的要你在面板里填容器端口）直接对应 `PIXORA_BIND`。没有持久磁盘的平台一定要把数据库指向托管库，否则容器一重建数据就没了。

### 8.8 验收清单

```bash
# 1) 后端自己的接口通（换成你的域名）
curl -sI https://img-api.example.com/api/v1/site | head -1      # 期望 200

# 2) 前端的接口通道通（边缘反代接法才有这条）
curl -sI https://你的前端域名/api/v1/site | head -1             # 期望 200

# 3) 跨域接法：预检要放行前端来源
curl -s -D - -o /dev/null -X OPTIONS \
  -H "Origin: https://你的前端域名" \
  -H "Access-Control-Request-Method: POST" \
  https://img-api.example.com/api/v1/auth/login | head -5       # 期望 204
```

浏览器里再确认三件事：广场能出图；登录后刷新不掉线；能传一张图并复制直链打开。

## 9. 数据库支持现状

三种方言都跑过同一套端到端冒烟（298 项），结果一致：

| 数据库 | 验证版本 | 迁移 | 冒烟 | 服务端 5xx |
|---|---|---|---|---|
| SQLite | 内置 | 9/9 | 298/298 | 0 |
| PostgreSQL | 17.6 | 9/9，26 张表 | 298/298 | 0 |
| MySQL | 8.4.0 | 9/9 | 298/298 | 0 |

SQLite 是默认，也是依赖最少的一条；PostgreSQL / MySQL 按 §4.3 / §4.4 填连接串，数据库已经单独部署的接法见 §4.5。

MariaDB 共用 MySQL 的驱动与迁移，这次没有单独跑，不能算“已验证”。用 MariaDB 时建议先在自己的环境跑一遍 §9 末尾的方言用例和冒烟。

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

## 10. 本次实测记录

环境：Windows 11（10.0.22631）、x86_64；便携版 PostgreSQL 17.6（EDB 二进制包）、MySQL 8.4.0（Community zip），分别跑在 127.0.0.1 的 55432 / 33306 端口，独立数据目录；没有 Docker 守护进程，也没有 WSL。所有服务实例都放在仓库外的 `E:\Develop\db\` 下，不碰仓库里的 `data/`。

下表是实际跑过的内容。发行包那两行用的是 v0.1.0（本次数据库修复前的最后一版）；0.1.1 的二进制与镜像由推送后的工作流构建，发布后建议按 §5.1 再校验一次 SHA256。

| 场景 | 怎么验的 | 结果 |
|---|---|---|
| 发行包完整性（v0.1.0） | 下载 v0.1.0 的 linux/win 两个包与 `SHA256SUMS.txt`，`sha256sum -c` | 两个包都 OK |
| Windows 二进制（v0.1.0） | 解压 zip → 全新数据目录启动 → `/healthz`、`/api/v1/site`、`--health-probe` | 自动生成主密钥、healthz 返回 `db: ok`、探针退出码 0 |
| SQLite 端到端 | 起服务 → 安装向导 → CI 同款冒烟 `scripts/smoke-new.sh` | 298 项全过 |
| 单元与集成测试 | `cargo test --workspace` | 279 项全过 |
| PostgreSQL 17.6 | 建库 → 启动 → 迁移 → 冒烟（`SMOKE_DB_CLIENT` 指向 psql） | 迁移 9 条/26 张表、冒烟 298/298、服务端 0 个 5xx |
| MySQL 8.4.0 | 建库 → 启动 → 迁移 → 冒烟（客户端带 utf8mb4） | 迁移 9 条、冒烟 298/298、服务端 0 个 5xx |
| 方言用例 | `PIXORA_TEST_DB_DSN=pg,mysql cargo test -p pixora-repo --test dialects` | 两种方言一起通过 |
| 前后端分离：Cloudflare Pages 式跨域 | 静态站 8098（含 SPA 回落）+ 后端 8099（`PIXORA_CORS_ORIGINS` 放行 8098），用真实 Cookie 走完向导、登录、上传、取直链 | 首页/回落/预检/凭据 Cookie（`SameSite=None; Secure`）/跨域上传全部通过；名单外来源拿不到跨域头 |
| 前后端分离：Vercel / EdgeOne 式边缘反代 | 站点 8097 把 `/api/*`、`/images/*` 转发到 8099，浏览器只看到 8097 | 向导、登录（`SameSite=Lax`）、上传、直链、取图全部通过 |
| 前后端分离：GitHub Pages 子路径 | 把产物放到 `/Pixora/` 下，用 `404.html` 做回落 | 子路径下能开、陌生路径命中 404 页面但内容是前端；`/main.js` 在根上 404，印证了 §8.6 的限制 |
| 三份 compose | 用 Docker Compose v5.6.0 CLI 执行 `config` 渲染，检查服务清单、`depends_on`、健康检查、端口与卷、`configs` 引用、占位符 | 三份都通过；**容器实跑未做**（本机没有 Docker 守护进程） |
| Caddy 配置 | 从 compose 渲染出 Caddyfile，用 Caddy 2.11.7 `validate` | 三份都是 `Valid configuration` |

没能在这台机器上验的，以及原因：

- **Docker 容器实跑**：没有 Docker Desktop，也没有 WSL2/Hyper-V。编排用真实 Compose CLI 渲染通过，Caddyfile 也用 Caddy 校验通过；但“按 compose 起三个容器、Caddy 真的申请证书”没有在这台机器上做。镜像构建步骤在 CI 里跑过，上次卡在 GHCR 的写权限（§4.8），是否已推到 Registry 要看 Actions 结果。
- **Linux 二进制执行**：没有 WSL 或 Linux 环境，无法直接跑 musl 产物。Linux 的准备步骤按发行包里的二进制位置写好，但没有在 Linux 上实际启动过。
- **平台真实部署**（Vercel / Cloudflare / EdgeOne / GitHub Pages）：没有这些平台的账号与令牌，因此用本机模拟器复刻了它们的路由规则（SPA 回落、`/api` 转发、`404.html`），把前后端链路完整跑通；真正点 Deploy 那一步没有做。

0.1.0 的镜像和二进制早于这次的 PostgreSQL/MySQL 修复；0.1.1 起包含。用非 SQLite 数据库时不要拉 0.1.0。

## 11. 排障

| 现象 | 多半是 |
|---|---|
| 直链地址是 `http://172.x.x.x/...` 或容器内网地址 | 没设 `PIXORA_BASE_URL`，或安装向导里基础 URL 写错了 |
| 登录成功，一刷新又变未登录 | 站点是 https 但 Cookie 没带 Secure；或跨域接法却没配 `PIXORA_CORS_ORIGINS` |
| 前端页面写着“服务暂时不可用” | 后端没起来，或浏览器到后端被拦（跨域没配、域名写错） |
| 跨域请求报 `failed to fetch` | 前端域名没写进 `PIXORA_CORS_ORIGINS`（要带 `https://`，端口写全） |
| 静态托管上页面能开、图片全裂 | 后端没设 `PIXORA_BASE_URL`，或跨域接法没填 `PIXORA_API_BASE` |
| 前端 404 / 白屏，控制台报 `/main.js` 404 | 站点挂在子路径下（如 `/<仓库名>/`），产物引用的是绝对路径 |
| 注册页说“注册入口已暂停” | 管理员关了注册；或磁盘占用触发了“容量触顶自动关闭”，去“配额与容量”看 |
| 验证码收不到 | SMTP 没配好（服务端日志会写原因）；开发时验证码也写进日志 |
| 上传报“域名收不到邮件” | 收件域名查不到 MX 记录，注册类发信被拦（`antispam.mx_check` 可关） |
| 容器起来又退出 | `docker compose logs pixora` 看最后几行；常见是主密钥还是占位符，或数据库连不上 |
| Caddy 日志里域名是空的 / 启动即退出 | 编排里的 `{$$PIXORA_DOMAIN}` 被改成了单个 `$`，或者数据库密码里的 `$` 被 Compose 插值了 |
| 证书申请失败 | 域名没解析、80/443 没放行、云安全组没开，或域名被别的服务占用 |
| `pull denied` / `unauthorized` | GHCR 包私有或未登录；PAT 要 `read:packages`；CI 推不上去看 §4.8 |
| `exec format error` | 镜像架构与机器不符；ARM 要手动构建 arm64 |
| Windows 下报 `unable to open database file` | 数据目录不可写（放到普通目录，别放 `Program Files` / `target\`）；也别从 Git Bash 启动 |
| SQLite 报 `database is locked` | 有两个进程/容器在写同一个库；SQLite 只能单实例 |
| 磁盘满了 | 看 `docker system df` 与数据目录；编排已加日志轮转，图片要自己清理或上对象存储 |
| `Cache-Control` 看起来不对 | 直链是 `immutable` 一年，带 `?thumb=1` 的是缩略图；私有图片是 `private, max-age=300` |

## 12. 附录

### 12.1 环境变量全表

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
| `PIXORA_CORS_ORIGINS` | 空 | 跨域前端来源，逗号分隔；精确匹配，不接受 `*` |
| `PIXORA_COOKIE_SAMESITE` | 配了 CORS 就 `none`，否则 `lax` | 会话 Cookie 的跨站策略 |
| `PIXORA_SECURE_COOKIES` | 按 `BASE_URL` 是否 https | 本地 http 调试可显式设 false |
| `PIXORA_WORKERS` | CPU 核数 | 后台任务并发（邮件重试、Blob GC、对账） |
| `PIXORA_DB_MAX_CONNECTIONS` | SQLite 4 / 其它 10 | 连接池上限 |
| `PIXORA_WEB_DIR` | 空 | 开发用：前端读盘而不是读嵌入产物 |
| `PIXORA_ACCEL_PREFIX` | 空 | 让 Nginx 直接发文件的 `X-Accel-Redirect` 前缀 |
| `PIXORA_DEV_RETURN_CODES` | `false` | 开发用：把验证码放进接口响应（生产别开） |
| `RUST_LOG` | `info` | 日志级别 |
| `PIXORA_API_BASE` | 空 | **构建期变量**，不是服务端变量；写进前端的 `config.js` |

### 12.2 数据目录里有什么

```
data/
├── pixora.db           SQLite 数据库（另有 -wal / -shm）
├── master.key          自动生成的主密钥（注入了 PIXORA_MASTER_KEY 时没有这个文件）
├── blobs/              图片与缩略图的物理文件，按哈希分目录
└── users/              硬链接镜像树（按账号组织，可选，删了不影响数据）
```

用 PostgreSQL/MySQL 时，数据库数据在 `pgdata/` 或 `mysqldata/`，备份用 §4.7 的 dump；`data/` 里只剩图片和主密钥。对象存储上的图片不在本地，交给厂商的版本控制或生命周期规则。

备份清单：`data/` 整个目录 + `PIXORA_MASTER_KEY`（如果走环境变量）+ 数据库 dump（如果用外部库）。

### 12.3 版本与发版

版本号只写在 `Cargo.toml` 的 `[workspace.package]` 里。改版本号：

```bash
sh scripts/bump-version.sh 0.1.2     # 连同 Cargo.lock、web/package.json、web/package-lock.json 一起改
git add -A && git commit -m "chore: 0.1.2" && git push
```

推主干后，“发行版与镜像”工作流会读版本号：

- GitHub 上没有这个版本的 Release 时，出两个平台的二进制、建 `v<版本号>` 标签、发 Release，镜像带 `:<版本号>` 与 `:latest`；
- 已经发过的版本只刷新镜像的 `:edge` / 主干名 / `:sha-<短哈希>`。

镜像默认只构建 `linux/amd64`。要 ARM 镜像，在 Actions 里手动运行这个工作流，`platforms` 填 `linux/amd64,linux/arm64`。

GHCR 包默认私有；首次推上去后，去 Packages → 这个包 → Package settings 改可见性，或者给使用方发一个有 `read:packages` 的 PAT。
