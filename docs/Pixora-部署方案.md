# Pixora 部署方案

这份文档把 Pixora 的部署拆成三条路线：纯 Docker、Linux 单二进制、Windows 单二进制，外加可选的「前后端分离」。每条路线都从「准备好这几样东西」讲到「怎么确认装好了」，命令可以直接复制。

第 10 节是实测记录：这台机器上真的跑过哪些组合、各自什么结果、哪些因为环境限制没跑。第 9 节单独说数据库的现状——**目前只有 SQLite 是端到端验证过的**，PostgreSQL 与 MySQL 的迁移能跑通、运行时还差一层适配，细节和报错原文都在那一节。

## 0. 怎么用这份文档

先看第 3 节的选型表，挑一条路线，然后按编号往下读。文档里的约定：

| 占位 | 换成什么 |
|---|---|
| `img.example.com` | 你实际使用的域名 |
| `<你生成的主密钥>` | 第 2.2 节那条命令的输出 |
| `<数据库密码>` | 自己设一个强密码 |
| `ghcr.io/lovefirefly-26710/pixora` | 镜像地址。如果你 fork 了仓库，换成 `ghcr.io/<你的用户名>/pixora` |

全文用 `$` 开头的行表示在服务器（Linux/macOS/WSL）上执行，`PS>` 开头表示在 Windows PowerShell 上执行。

## 1. 先认三样东西

| 角色 | 是什么 | 住在哪 |
|---|---|---|
| **后端** | 一个 Rust 进程（可执行文件名 `pixora`），负责接口、直链、上传、后台任务 | 你的服务器 / 容器 |
| **前端** | 一堆静态文件（`index.html`、`main.js`…），用户看到的界面 | 默认嵌在后端二进制里；也可以单独放到静态托管 |
| **数据** | 数据库（账号、相册、标签）+ 图片文件 | 数据目录（本地磁盘 / 卷），数据库可以换成自建或托管 |

默认形态里前端被 `rust-embed` 嵌进了后端二进制，所以**只要起一个进程，网站就能用**。只有想把前端也放上 CDN 时，才需要把两者拆开（第 8 节）。

### 名词速查

| 词 | 一句话解释 |
|---|---|
| 环境变量 | 写成 `KEY=VALUE` 的配置，进程启动时读取；这个项目所有部署参数都走它 |
| 容器 / 镜像 | 镜像是打包好的程序，容器是它的运行实例 |
| 反向代理 | 站在最前面的那个服务，负责 https 证书与转发（本项目的编排里是 Caddy） |
| 直链 | 图片的永久地址，形如 `https://你的域名/images/<哈希>.jpg` |
| 直链前缀 | 直链里 `images` 那一段，后台可以改，旧链不会失效 |
| 安装向导 | 第一次打开网站时的五步引导，用来验证数据库、建管理员、写站点配置 |
| 主密钥 | 加密存储后端凭据与 SMTP 密码的那把钥匙，丢了只能重新填一遍凭据 |

### 部署顺序

```
先把后端跑起来 → 打开网站走安装向导 → 能登录、能传图、能看图 → 再考虑把前端放 CDN
```

顺序反了（先拆前端再调后端）会同时面对跨域和接口两套问题，不好定位。

## 2. 开工前准备好这四样

### 2.1 一台机器

云服务器（1 核 1G 就能跑）、家里的小主机、NAS 都行。系统推荐 Ubuntu 22.04/24.04 或 Debian 12。Windows 上也有一等公民的支持（第 6 节），不是靠 WSL 转的。

### 2.2 主密钥

```bash
openssl rand -hex 32
```

输出 64 位十六进制，就是 `PIXORA_MASTER_KEY`。把它存进密码管理器，并**随数据目录一起备份**。

它加密的是数据库里的敏感配置：S3/R2 的 AccessKey、WebDAV 密码、SMTP 密码、GitHub Token。换了它，这些值全部解不开，只能进后台一项项重填。图片文件和账号密码不受影响（口令是单向哈希）。

不设这个变量也能启动：首次运行会在数据目录里生成 `master.key` 并照着用。单机自托管够用，但多实例、迁移容器、备份恢复时容易丢，生产环境建议显式注入。

### 2.3 站点对外地址 `PIXORA_BASE_URL`

写用户浏览器里看到的那个地址，例如：

```dotenv
PIXORA_BASE_URL=https://img.example.com
```

只有 IP、没有域名就写 `http://203.0.113.10:8080`。

**这是最容易漏、症状最明显的一个变量。** 直链、邮件里的链接、OAuth 回跳地址都按它拼。忘了设置时，服务端会按请求头推断，在反向代理后面推出来的是容器内网地址（`http://172.x.x.x/...`），现象是「页面能开，图片全裂」。

### 2.4 数据目录

一个可以读写的目录，用来放数据库文件（SQLite 时）、图片、主密钥文件。建议：

- Docker：`./data` 挂到容器里的 `/data`；
- Linux：`/srv/pixora/data`，属主是运行服务的那个用户；
- Windows：`C:\pixora\data` 或任何一个普通目录（不要放 `C:\Program Files` 下，权限会打架）。

这个目录就是全部身家，备份就备份它（第 4.7 节）。

## 3. 选一条路线

| 你的情况 | 走哪条 | 看哪节 |
|---|---|---|
| 有 Docker，想最快跑起来 | 纯 Docker 单机 | 第 4 节 |
| 有台 Linux 服务器，不想装 Docker | Linux 单二进制 + systemd | 第 5 节 |
| 只有 Windows 机器 / Windows 小主机 | Windows 单二进制 + 服务 | 第 6 节 |
| 后端在自己机器上，想让前端走 CDN | 先按 4/5/6 跑通后端，再拆前端 | 第 8 节 |
| 想用云数据库 | Docker 或二进制都行，把 `PIXORA_DB_DSN` 换掉 | 第 4.5 / 9 节 |

数据库怎么选：

| 数据库 | 适合 | 现状（2026-10） |
|---|---|---|
| **SQLite**（默认） | 单机、个人、最省事 | **端到端验证过**，298 项冒烟全过 |
| PostgreSQL | 想用正经数据库、单机多人 | 迁移能跑通（9 条、26 张表），运行时还差类型映射，见第 9 节 |
| MySQL / MariaDB | 公司统一用 MySQL | 迁移能跑通，运行时还差类型映射，见第 9 节 |

**当前版本请选 SQLite。** 它并不是「凑数的单机库」：WAL 模式、外键、真实事务都有，几万张图片、几十个用户完全没问题；数据文件就是一个 `pixora.db`，备份就是拷文件。

## 4. 方案 A：纯 Docker

一个文件跑完整站：下面每份 `docker-compose.yml` 里写着应用、数据库（SQLite 版没有单独的数据库服务）和自动 https 的反向代理。Caddy 的配置内嵌在文件末尾，**没有 `.env`、没有单独的 Caddyfile、也不用先建目录**——Docker 会自己建 `./data`。

三步：

1. 把整段 YAML 存成 `docker-compose.yml`；
2. 改文件里标着「必改」的几处（见 4.1）；
3. `docker compose up -d`。

需要 Docker Compose 2.23 以上（用了 `configs.content` 内嵌 Caddy 配置）。版本这么查：

```bash
docker compose version
```

### 4.1 要改的就这几处

| 占位符 | 换成 | 在哪 |
|---|---|---|
| `img.example.com` | 你的域名 | `PIXORA_DOMAIN`，Caddy 用它申请证书 |
| `https://img.example.com` | 站点对外地址，和域名一致 | `PIXORA_BASE_URL` |
| `REPLACE_WITH_openssl_rand_hex_32` | `openssl rand -hex 32` 的输出 | `PIXORA_MASTER_KEY` |
| `REPLACE_WITH_A_STRONG_PASSWORD` | 一个强密码 | 只有 PostgreSQL / MySQL 版有，出现 2～3 处，改成同一个值 |

主密钥那个占位符故意不是合法的密钥格式：忘了改，应用启动时直接报错，不会拿占位文字当密钥用。

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
# 想从源码构建镜像，见 README 的「从源码构建」一节。

services:
  pixora:
    # 换成你自己的用户名（fork 过的话）；没发过版的仓库只有 :edge
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

# Caddy 的配置就写在这里，不用另外放一个 Caddyfile
configs:
  caddyfile:
    content: |
      {$PIXORA_DOMAIN} {
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

启动与验证：

```bash
docker compose up -d
docker compose ps                              # 两个服务都应是 running / healthy
docker compose logs -f pixora                  # 看到「Pixora 已启动」按 Ctrl+C

# 证书批下来之前，直接问应用本身
docker compose exec pixora /pixora --health-probe && echo ok
# 证书就绪之后
curl -s https://img.example.com/healthz        # {"db":"ok","status":"ok",...}
```

然后浏览器打开 `https://img.example.com`，走安装向导（第 7 节）。

> 镜像标签：发过版的版本号才有 `:0.1.0`、`:latest`；主干最新构建是 `:edge`。fork 过仓库就把 `lovefirefly-26710` 换成自己的用户名。

### 4.3 A2：PostgreSQL 版

> ⚠ 先看第 9 节：PostgreSQL 的迁移能跑通，运行期还差一层类型适配，接口会报 `NUMERIC` 解码错误。配置先留着，修好之后可以直接用。

```yaml
# Pixora 单机部署 —— PostgreSQL 版（一个文件跑完整站）。
#
# ⚠ 状态说明（2026-10）：迁移能建完，运行期还差 sqlx Any 的类型映射
#   （SUM 在 PG 返回 NUMERIC）。当前版本请用 SQLite 版，见
#   docs/Pixora-部署方案.md 的「数据库支持现状」。
#
# 用法三步：存成 docker-compose.yml → 改三处「必改」与数据库密码 → docker compose up -d
# 数据库密码在文件里出现 2 处（DSN 与 POSTGRES_PASSWORD），改成同一个值。
#
# 需要 Docker Compose 2.23 以上（configs.content 内嵌了 Caddy 配置）。

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

configs:
  caddyfile:
    content: |
      {$PIXORA_DOMAIN} {
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

启动同 4.2，第一次会先初始化数据库，多等十几秒。备份用 `pg_dump`，见 4.7。

### 4.4 A3：MySQL 版

> ⚠ 同 4.3：迁移能跑通，运行期还差类型适配（MySQL 的 `TEXT` 被当成 BLOB、`TINYINT` 不识别），见第 9 节。

```yaml
# Pixora 单机部署 —— MySQL 8.0.13+ / MariaDB 版（一个文件跑完整站）。
#
# ⚠ 状态说明（2026-10）：迁移能建完，运行期还差 sqlx Any 的类型映射
#   （MySQL 的 TEXT 被当成 BLOB、TINYINT 不识别）。当前版本请用 SQLite 版，
#   见 docs/Pixora-部署方案.md 的「数据库支持现状」。
#
# 用法三步：存成 docker-compose.yml → 改三处「必改」与数据库密码 → docker compose up -d
# 数据库密码在文件里出现 3 处（DSN、MYSQL_PASSWORD、MYSQL_ROOT_PASSWORD），
# 三处改成同一个值。
#
# 需要 Docker Compose 2.23 以上（configs.content 内嵌了 Caddy 配置）。

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

configs:
  caddyfile:
    content: |
      {$PIXORA_DOMAIN} {
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

字符集固定 `utf8mb4`，否则中文标签、emoji 相册名插不进去。MySQL 比 PostgreSQL 重一些，同配置下多留 100MB 内存。MariaDB 把镜像换成 `mariadb:11`、环境变量换成 `MARIADB_DATABASE` / `MARIADB_USER` / `MARIADB_PASSWORD` / `MARIADB_ROOT_PASSWORD`，迁移共用一套，但同样受第 9 节的限制。

### 4.5 A4：托管数据库

不用自己维护数据库时，从 4.2 的 SQLite 版出发改两行，其它不动：

```yaml
    environment:
      PIXORA_DB_DRIVER: postgres        # 云的 MySQL 写 mysql
      # 厂商给的连接串，照抄，别自己拼
      PIXORA_DB_DSN: postgres://user:pass@ep-xxx-pooler.region.aws.neon.tech/pixora?sslmode=require
```

常见连接串长这样：

```dotenv
# Neon / Vercel Postgres / Supabase
postgres://user:pass@ep-xxx-pooler.region.aws.neon.tech/pixora?sslmode=require
# 阿里云 RDS / 腾讯云 CDB（PostgreSQL）
postgres://user:pass@pgm-xxx.pg.rds.aliyuncs.com:5432/pixora?sslmode=require
# 云厂商 MySQL / PlanetScale
mysql://user:pass@host:3306/pixora?ssl-mode=REQUIRED
```

建表在应用启动时自动执行，不用手动跑 SQL。

### 4.6 不用 Caddy

两种情况会走到这里：只有 IP、没有域名；或者前面已经有一层反向代理。

做法：删掉 `caddy` 服务与文件末尾的 `configs:` 整段，给 `pixora` 加一行端口映射——

```yaml
    ports: ["8080:8080"]
```

再把 `PIXORA_BASE_URL` 改成实际访问地址：

```dotenv
# 只有 IP 时
http://203.0.113.10:8080
# 平台给了域名（Railway / Render / Koyeb 这类）
https://pixora-production.up.railway.app
```

前面有 Nginx / 平台负载均衡时，`PIXORA_TRUST_PROXY` 保持 `1`。平台给了 https 域名却把地址写成 http，表现是「登录成功，刷新就掉登录」——Cookie 的 Secure 推导跟着 Base URL 走。

### 4.7 升级、备份、恢复

升级：

```bash
docker compose pull
docker compose up -d          # 数据库迁移在启动时自动执行
```

备份：

```bash
# 1) 数据目录（SQLite 库、图片、主密钥）
docker compose stop pixora
tar -czf pixora-data-$(date +%F).tar.gz data/
docker compose start pixora

# 2) PostgreSQL
docker compose exec db pg_dump -U pixora pixora > pixora-$(date +%F).sql

#    MySQL（密码换成你填的那个）
docker compose exec db mysqldump -u pixora -p'你的密码' pixora > pixora-$(date +%F).sql
```

恢复就是把 `data/` 解回去、或把 dump 导入空库，再 `docker compose up -d`。`PIXORA_MASTER_KEY` 单独记一份：数据库里的存储凭据与 SMTP 密码是它加密的，丢了只能重填。

### 4.8 Docker 这条路的常见坑

| 现象 | 原因与处理 |
|---|---|
| 启动报「主密钥格式不对」 | compose 里的 `PIXORA_MASTER_KEY` 还是占位符，换成 `openssl rand -hex 32` 的输出 |
| `additional properties 'content' not allowed` | Docker Compose 低于 2.23，不认内嵌的 Caddy 配置。升级 compose；或者把 `configs:` 那段删掉，给 caddy 服务加一行 `command: caddy reverse-proxy --from https://你的域名 --to http://pixora:8080`（不用任何配置文件，代价是没有那几行安全响应头） |
| 容器起来又退出 | `docker compose logs pixora` 看最后几行；最常见是主密钥没换，或者数据目录写不进去 |
| `80/443 端口被占用` | 宿主机上另有 Nginx/Apache；改 Caddy 的端口，或先停掉别的服务 |
| 浏览器打开是 502 | Caddy 起来了但应用没起来，`docker compose ps` 看 `pixora` 是否 healthy |
| 图片裂开、地址是 `172.x.x.x` | `PIXORA_BASE_URL` 没填对 |
| 上传报 413 | 内嵌配置里的 `request_body` 上限被改小了；默认 64MB，与应用侧一致 |
| `./data` 权限报错 | 镜像基于 `scratch`、以 root 运行，宿主机上一般不用改权限；只有开了 rootless / 用户命名空间时才按映射的 UID 授权 |

## 5. 方案 B：Linux 单二进制

发行版提供静态链接（musl）的 Linux 可执行文件，不依赖 glibc，Alpine、群晖、老 CentOS 都能跑。

### 5.1 下载与校验

在 [Releases](https://github.com/loveFirefly-26710/Pixora/releases) 下载两个文件和一个校验文件：

```bash
curl -LO https://github.com/loveFirefly-26710/Pixora/releases/download/v0.1.0/pixora-v0.1.0-linux-x86_64.tar.gz
curl -LO https://github.com/loveFirefly-26710/Pixora/releases/download/v0.1.0/SHA256SUMS.txt
sha256sum -c SHA256SUMS.txt        # 期望：...: OK
tar -xzf pixora-v0.1.0-linux-x86_64.tar.gz
ls pixora/                          # pixora  README.md  .env.example  deploy/
```

### 5.2 目录与用户

```bash
sudo useradd --system --home /srv/pixora --shell /usr/sbin/nologin pixora
sudo mkdir -p /srv/pixora/data
sudo mv pixora/pixora /srv/pixora/pixora
sudo chown -R pixora:pixora /srv/pixora
sudo chmod 750 /srv/pixora/data
```

### 5.3 环境变量文件

把配置集中到 `/etc/pixora.env`（`chmod 600`，属主 root），这样 systemd 单元里不用塞密钥：

```bash
sudo tee /etc/pixora.env > /dev/null <<'EOF'
PIXORA_BIND=127.0.0.1:8080
PIXORA_DATA_DIR=/srv/pixora/data
PIXORA_DB_DRIVER=sqlite
PIXORA_DB_DSN=/srv/pixora/data/pixora.db
PIXORA_BASE_URL=https://img.example.com
PIXORA_MASTER_KEY=<你生成的主密钥>
PIXORA_TRUST_PROXY=1
PIXORA_WORKERS=2
RUST_LOG=info
EOF
sudo chmod 600 /etc/pixora.env
```

监听 `127.0.0.1` 是为了只让本机的反向代理访问；想直接对外就写 `0.0.0.0:8080`，并自己开防火墙。

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
# 进程崩了要能留下日志
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

**Nginx**（`/etc/nginx/sites-available/pixora`，链接到 `sites-enabled`）：

```nginx
server {
    listen 443 ssl http2;
    server_name img.example.com;

    # 略大于应用的单文件上限（默认 20MiB）
    client_max_body_size 64m;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        # 只信最靠近服务端的一层，客户端伪造的 X-Forwarded-For 会被覆盖
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_read_timeout 300s;
    }
}
```

证书可以用 certbot 申请：`sudo certbot --nginx -d img.example.com`。

**Caddy** 更简单（自动证书），`/etc/caddy/Caddyfile`：

```
img.example.com {
	reverse_proxy 127.0.0.1:8080
}
```

### 5.6 验证

```bash
curl -s http://127.0.0.1:8080/healthz        # {"db":"ok","status":"ok",...}
curl -s https://img.example.com/api/v1/site | head -c 200
```

浏览器打开 `https://img.example.com`，走安装向导（第 7 节）。

### 5.7 升级与回滚

```bash
sudo systemctl stop pixora
sudo mv /srv/pixora/pixora /srv/pixora/pixora.bak-$(date +%F)
sudo tar -xzf pixora-新版本-linux-x86_64.tar.gz
sudo mv pixora/pixora /srv/pixora/pixora
sudo chown pixora:pixora /srv/pixora/pixora
sudo systemctl start pixora
```

数据库迁移在启动时自动执行、只向前不回头。**回滚二进制之前先备份数据库**：新版本迁过的库，旧版本未必认。

## 6. 方案 C：Windows 单二进制

发行包里是 `pixora\pixora.exe` 加上 `README.md`、`.env.example`、`deploy\`（Caddyfile 与 compose 样例）。没有运行时依赖，不需要装 Rust、Node 或 VC++ 运行库。

### 6.1 下载与解压

在 [Releases](https://github.com/loveFirefly-26710/Pixora/releases) 下载 `pixora-v0.1.0-windows-x86_64.zip` 与 `SHA256SUMS.txt`，然后：

```powershell
PS> Expand-Archive pixora-v0.1.0-windows-x86_64.zip C:\pixora
PS> Get-FileHash C:\pixora\pixora\pixora.exe -Algorithm SHA256   # 与 SHA256SUMS.txt 对照
PS> cd C:\pixora\pixora
```

目录结构：

```
C:\pixora\pixora\
├── pixora.exe
├── README.md
├── .env.example
└── deploy\           # Caddyfile 与 docker-compose 样例，纯二进制部署用不上
```

### 6.2 先跑起来看看

最小启动（不配数据库、不配反向代理），数据落到当前目录的 `data\`：

```powershell
PS> $env:PIXORA_BIND = "127.0.0.1:8080"
PS> $env:PIXORA_BASE_URL = "http://127.0.0.1:8080"
PS> .\pixora.exe
```

预期输出（截取）：

```
INFO Pixora 已启动 version="0.1.0" listen=127.0.0.1:8080 data_dir=./data db="sqlite"
     storage_default=local master_key=首次启动自动生成，已写入 ./data\master.key web=已嵌入二进制
WARN 主密钥是自动生成的（…）：单机自托管够用，生产环境建议改用 PIXORA_MASTER_KEY 注入并纳入备份
```

另开一个窗口验证：

```powershell
PS> Invoke-RestMethod http://127.0.0.1:8080/healthz
db               status uptime_seconds
--               ------ --------------
ok               ok                  3

PS> (Invoke-RestMethod http://127.0.0.1:8080/api/v1/site).data.base_url
http://127.0.0.1:8080
```

健康探针也可以单独跑（容器编排与监控脚本用它）：

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
2. 以管理员身份打开 PowerShell：

```powershell
PS> C:\pixora\nssm.exe install Pixora "C:\pixora\pixora\pixora.exe"
PS> C:\pixora\nssm.exe set Pixora AppDirectory "C:\pixora\pixora"
PS> C:\pixora\nssm.exe set Pixora AppEnvironmentExtra `
      PIXORA_BIND=0.0.0.0:8080 `
      PIXORA_BASE_URL=http://192.168.1.10:8080 `
      PIXORA_DATA_DIR=C:\pixora\data `
      PIXORA_MASTER_KEY=<你生成的主密钥>
PS> C:\pixora\nssm.exe set Pixora AppStdout "C:\pixora\logs\pixora.log"
PS> C:\pixora\nssm.exe set Pixora AppStderr "C:\pixora\logs\pixora.log"
PS> Start-Service Pixora
```

`sc.exe` 也能注册服务，但它要求 exe 自己实现服务协议，这个二进制不是服务程序，直接用会报「服务没有及时响应启动或控制请求」。所以要么用 NSSM，要么用任务计划程序（`Register-ScheduledTask`，触发器选「计算机启动时」，勾选「不管用户是否登录都要运行」）。

查看/停止：

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
PS> New-NetFirewallRule -DisplayName "Pixora 8080" -Direction Inbound -Protocol TCP -LocalPort 8080 -Action Allow
```

然后用 `http://192.168.1.10:8080` 打开。**直链与登录都按 `PIXORA_BASE_URL` 拼**，改成 IP 后要重启服务让它生效。

想对外提供 https，就在 IIS 或 Caddy for Windows 前面挂一层反向代理，`PIXORA_BASE_URL` 写那个 https 地址，并保留 `PIXORA_TRUST_PROXY=1`。

### 6.6 升级与备份

升级：停服务 → 备份 `data\` → 用新版 `pixora.exe` 覆盖 → 启动。数据库迁移在启动时自动执行。

```powershell
PS> C:\pixora\nssm.exe stop Pixora
PS> Compress-Archive -Path C:\pixora\data -DestinationPath C:\pixora\backup-data-$(Get-Date -Format yyyyMMdd).zip
PS> Copy-Item C:\pixora\new\pixora.exe C:\pixora\pixora\pixora.exe -Force
PS> C:\pixora\nssm.exe start Pixora
```

备份三样：`data\`、`master.key`（在 data 里）、`PIXORA_MASTER_KEY`（如果走环境变量注入）。

### 6.7 Windows 上容易踩的坑

| 现象 | 原因与处理 |
|---|---|
| 启动报 `unable to open database file`，且数据目录里没有 `master.key` | 数据目录所在位置不可写（例如放在 `C:\Program Files` 下，或安全软件拦了写文件）。换到 `C:\pixora\data` 这类普通目录，并给当前用户写权限 |
| 从 Git Bash 里启动，各种诡异 | Git Bash 会把 `TEMP` 设成 `/tmp` 这类 MSYS 路径，原生程序解析不了。用 PowerShell 或 cmd 启动，别在 MSYS 里跑 |
| 服务启动后立刻停止 | NSSM 的 `AppDirectory` 没设，工作目录不对；或环境变量没通过 `AppEnvironmentExtra` 传进去 |
| 浏览器提示证书不安全 | 直接在 http 下访问是正常的；要 https 就前面挂反代并配域名 |
| 端口被占用 | 8080 常被别的开发工具占用，换 `PIXORA_BIND=0.0.0.0:8081` 并同步改 `PIXORA_BASE_URL` |

如果你是在源码目录里折腾，注意**别把 exe 和 data 放在 cargo 的 `target\` 下面**——那是构建系统的工作区，权限和清理规则都不适合放运行数据（本机验证时就撞过这个坑）。把 exe 拷到一个普通目录再跑。

## 7. 安装向导：所有路线都要走一遍

浏览器打开 `PIXORA_BASE_URL`，会跳到 `/#/install`，五步：

1. **环境检查**：数据目录能不能写、数据库连接串是否配置、主密钥来源（环境变量 / 密钥文件 / 自动生成）。这一页还会显示「已检测到 N 张表」。
2. **数据库**：这一步是**连接自检**，不是配置。数据库在进程启动时就由 `PIXORA_DB_DRIVER` / `PIXORA_DB_DSN` 定好了。选上你实际用的类型、点「测试连接」，成功就下一步。如果你填的类型和进程实际连的不一致，它会直说「改完环境变量并重启才会切换」。
3. **管理员**：填邮箱与密码。这个账号是最高权限：改不了角色、不能被处置、不能被删除。密码有强度要求。
4. **站点与邮件**：站点名、对外地址、SMTP。没配 SMTP 也能用，只是注册验证码、找回密码的邮件发不出去——验证码会写进服务端日志，开发/自用时去日志里拿。
5. **完成**：标记已安装，安装接口从此整体下线（再访问返回 404）。这是有意的：否则任何人都能重跑向导重设管理员。

装完就能用：登录 → 传图 → 复制直链。到这一步后端部分就结束了。

## 8. 前后端分开部署（可选）

这一步能拿到两个好处：前端静态资源走 CDN 边缘节点、后端那台机器只扛接口。代价是多一处要维护的配置。

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

- **CI 构建**：推主干之后，`CI` 工作流会上传 `web-dist` 产物，下载解压就是静态站；
- **本地构建**：

```bash
cd web
npm ci
PIXORA_API_BASE=https://img-api.example.com npm run build   # 边缘反代接法就留空
# 产物在 web/dist/
```

`PIXORA_API_BASE` 会被写进产物根目录的 `config.js`，前端在运行时读取它。各平台填变量位置：

| 平台 | 变量名 | 填在哪儿 |
|---|---|---|
| Vercel | `PIXORA_API_BASE` | 项目 → Settings → Environment Variables（勾 Production，**改完要重新部署**） |
| Cloudflare Pages | 同上 | 项目 → Settings → Variables and Secrets → Production |
| EdgeOne Makers | 同上 | 项目 → 构建设置 → 环境变量 |
| GitHub Pages | 同上 | 仓库 Settings → Secrets and variables → Actions → **Variables** 页 |
| 本地 / 自建 CI | 同上 | 构建命令前加：`PIXORA_API_BASE=https://… npm run build` |

不想在平台后台改也行：部署完直接编辑线上的 `config.js`，刷新即生效（运行时读取，不用重新构建）。

### 8.3 Vercel

Vercel 只能放前端：它的函数是 JS 运行时，跑不了这个需要常驻进程的二进制。后端按第 4/5/6 节放在你自己的服务器上。

1. 把仓库推到 GitHub，用 GitHub 账号登录 [vercel.com](https://vercel.com)；
2. **Add New → Project → Import**，选这个仓库；
3. 构建配置有两种填法。省事的一种：把仓库里的 `deploy/static/vercel.json` 复制到仓库根目录，它已经写明安装命令、构建命令与输出目录，导入后什么都不用填；不想复制就在项目设置里手填 **Install Command** `npm --prefix web ci`、**Build Command** `npm --prefix web run build`、**Output Directory** `web/dist`，Root Directory 留空；
4. **跨域接法**：在 Environment Variables 里加 `PIXORA_API_BASE=https://后端地址`，然后 Deploy；
5. **边缘反代接法**：把 `vercel.json` 的 `rewrites` 换成下面这份（填你的后端地址），提交后 Vercel 自动重新部署：

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

把两处 `img-api.example.com` 换成你的后端地址；`/images/` 换成你在后台设的直链前缀（默认就是 `images`）。最后一条是 SPA 回落：让 `/some-page` 这类陌生路径也进前端。

命令行等价：

```bash
npx vercel login
npx vercel --prod          # 在仓库根目录执行，会读 vercel.json
```

跨域接法记得回后端放行前端域名：

```dotenv
PIXORA_CORS_ORIGINS=https://你的项目.vercel.app
```

改完重启后端。放行名单是精确匹配（含 https、端口），`*` 会被忽略。

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

命令行等价（本地构建好再发）：

```bash
npx wrangler pages deploy web/dist --project-name pixora
```

**别往产物里放 `404.html`**：Pages 的机制是「没有 `404.html` 时，没命中静态文件的路径自动回 `index.html`（200）」，前端路由正好接住；一旦存在 `404.html`，它就按真 404 处理。

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

R2 是 S3 兼容的，后台「存储管理」加一个 S3 后端：

| 字段 | 值 |
|---|---|
| 驱动 | S3 |
| Endpoint | `https://<账号ID>.r2.cloudflarestorage.com` |
| Region | `auto` |
| Bucket | 你建的桶名 |
| AccessKey / Secret | R2 的 API 令牌（对象读写权限） |

**想把后端也放 Cloudflare**：用 Containers 跑这个镜像。三条硬约束先记住——容器只跑 x86_64、磁盘每次冷启动都是新的（所以图片必须进 R2、数据必须用托管库）、镜像要推到 Cloudflare 自己的 registry（GHCR 不能直接拉，得先 `docker pull` 再 `npx wrangler containers push`）。

### 8.5 EdgeOne Makers

EdgeOne Pages 现在叫 EdgeOne Makers，账号与命令行都兼容。

1. 控制台从 Git 导入仓库：构建命令 `npm --prefix web ci && npm --prefix web run build`，输出目录 `web/dist`，Node 版本 20；跨域接法加环境变量 `PIXORA_API_BASE`；
2. 想让它自动读配置，把仓库里的 `deploy/static/edgeone.json` 复制到仓库根目录（`edgeone.json` 只认根目录），里面已经写好构建配置、SPA 回落与缓存头；不复制就在控制台手填上面那三行：

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

纯静态、免费，适合「前端就用仓库发」的场景。仓库里带了工作流，三步：

1. Settings → Pages → Source 选 **GitHub Actions**；
2. Settings → Secrets and variables → Actions → **Variables** 新增两个：`PIXORA_DEPLOY_FRONTEND=true` 和 `PIXORA_API_BASE=https://后端地址`；
3. Actions →「前端静态站点」→ Run workflow，target 选 `github-pages`。

工作流会把 `index.html` 复制一份成 `404.html`——Pages 没有 SPA 回落，陌生路径会命中这个 404 页面，但页面内容还是前端本体，哈希路由照常接管。

**一条硬限制**：Pages 的项目站点挂在 `https://<用户名>.github.io/<仓库名>/` 子路径下，而前端产物引用的是绝对路径（`/main.js`、`/config.js`），子路径下这些资源会 404（第 10 节的实测里专门验了这一点）。要么绑自定义域名，要么把仓库命名为 `<用户名>.github.io` 发到根路径。

### 8.7 后端还能放哪儿

前端放静态托管之后，后端只要满足「能长跑一个进程、能给一个地址」就行：

| 去处 | 怎么放 | 注意 |
|---|---|---|
| 自己的云服务器 / 小主机 / NAS | 第 4 或第 5 节 | 最稳，数据在自己手里 |
| Railway / Render / Fly.io / Koyeb / Zeabur | 用本仓库镜像，挂一个持久卷 | 没卷就别用 SQLite 与本地图片，改托管库 + R2 |
| Oracle 免费 ARM 实例 | 直接用多架构镜像（`linux/arm64`） | 一年免费额度足够 |
| Cloudflare Containers | 见 8.4 C 小节 | 必须配 R2 + 托管库，且只跑 x86_64 |
| 群晖 / 威联通 NAS | 容器管理界面里填 compose | 数据目录指到 NAS 的存储池 |

平台特有的端口变量（有的叫 `PORT`、有的要你在面板里填容器端口）直接对应 `PIXORA_BIND`。没有持久磁盘的平台一定要把数据库指向托管库，否则容器一重建数据就没了。

### 8.8 验收清单

```bash
# 1) 后端自己的接口通（换成你的域名）
curl -sI https://img-api.example.com/api/v1/site | head -1      # 期望 200

# 2) 前端的接口通道通（边缘反代接法才有这条）
curl -sI https://你的前端域名/api/v1/site | head -1              # 期望 200

# 3) 跨域接法：预检要放行前端来源
curl -s -D - -o /dev/null -X OPTIONS \
  -H "Origin: https://你的前端域名" \
  -H "Access-Control-Request-Method: POST" \
  https://img-api.example.com/api/v1/auth/login | head -5       # 期望 204
```

浏览器里再确认三件事：广场能出图；登录后刷新不掉线；能传一张图并复制直链打开。

## 9. 数据库支持现状

这一节是实测出来的，不是设计意图。结论先放：**当前版本只有 SQLite 可以端到端使用**。

| 数据库 | 迁移（自动建表） | 接口可用 | 说明 |
|---|---|---|---|
| SQLite 3 | ✅ 9/9 | ✅ | 298 项冒烟全过（第 10 节） |
| PostgreSQL 17.6 | ✅ 9/9、26 张表 | ❌ | 运行时 sqlx Any 驱动解不了 `NUMERIC` |
| MySQL 8.4.0 | ✅ 9/9 | ❌ | 运行时 sqlx Any 驱动不认 `TEXT` 与 `TINYINT` |

### PostgreSQL：差在 `SUM(...)` 的返回类型

现象（登录后任意读接口）：

```
数据访问失败：数据库错误：error occurred while decoding column total:
error in Any driver mapping: Any driver does not support the Postgres type PgTypeInfo(Numeric)
```

原因：`SUM(bigint)` 在 PostgreSQL 里返回 `NUMERIC`（不是 bigint），而本项目统一用 `sqlx::Any` + `i64` 解码。修法是给那些求和表达式包一层方言相关的 `CAST(... AS BIGINT)`，或者在 PostgreSQL 上改用 `count`/分组聚合的写法。涉及的大约十来处查询（容量统计、限流额度、任务计数）。

这次顺带修掉的 PostgreSQL 迁移问题（修之前连启动都过不去）：

- `storage_backends` 的部分唯一索引写了 `WHERE is_default`，而这一列是 `BIGINT`，PG 要求布尔表达式 → 改为 `WHERE is_default = 1`；
- `must_change_password` 声明成 `BOOLEAN`，代码按 `i64` 解码 → 改为 `BIGINT 0/1`；
- 几处 `INTEGER`（int4）列被按 `i64` 读取 → 改为 `BIGINT`；
- `user_identities` 的时间列写成 `TIMESTAMPTZ`、代码按文本写入/读取 → 改为 `TEXT`。

### MySQL：差在 `TEXT` 与 `TINYINT` 的类型映射

现象（登录后任意读接口）：

```
error in Any driver mapping: Any driver does not support MySql type ... type: Tiny ...
数据解码失败（value）：mismatched types; Rust type `Option<String>` is not compatible with SQL type `BLOB`
```

原因：MySQL 把 `TEXT` 列报告成 BLOB 家族的类型，`TINYINT` 也不在 sqlx Any 的映射表里。而代码到处用 `Option<String>` 读文本、用 `i64` 读布尔。这一条比 PostgreSQL 那条更深：要么把 MySQL 侧的长文本列改成 `VARCHAR`（受行宽限制，邮件模板这类放不下），要么在仓储层按字节读文本再转字符串。

这次顺带修掉的 MySQL 迁移问题（修之前建表就失败）：

- `rate_limit_policies` 的生成列基列上挂了 `ON DELETE CASCADE`（MySQL 8 直接报 1215）→ 改成 `RESTRICT`，删账号前由应用显式清掉策略行；
- `albums.deleted_at` 用 `TEXT` 又建了索引（报 1170）→ 改为 `VARCHAR(32)`；
- `must_change_password` 声明成 `BOOLEAN`（即 `TINYINT`）→ 改为 `BIGINT 0/1`；
- `user_identities` 的时间列用 `DATETIME(3) DEFAULT CURRENT_TIMESTAMP(3)`，代码写入的是 RFC3339 文本 → 改为 `VARCHAR(32)`。

### 在那之前怎么办

用 SQLite。它不是玩具库：WAL 模式、外键约束、真实事务都有，单机几十个用户、几万张图完全够。将来要换 PostgreSQL/MySQL，数据迁移路径是「导出 → 建新库 → 改环境变量 → 重启」，图片文件不用动。

如果你自己有动手能力，想接着把这两条方言修完，最小验证路径是：

```bash
# PostgreSQL（需要一台能连的库）
PIXORA_TEST_DB_DSN="postgres://user:pass@127.0.0.1/pixora" \
  cargo test -p pixora-repo --test dialects -- --nocapture

# MySQL
PIXORA_TEST_DB_DSN="mysql://root:pass@127.0.0.1/pixora" \
  cargo test -p pixora-repo --test dialects -- --nocapture
```

这套方言用例覆盖布尔往返、唯一冲突识别、UPSERT、内容寻址复用、引用计数归零、批量预检的占位符改写、标签筛选与时间戳比较——方言差异最容易出问题的几处都在里面。跑绿了，再拿第 10 节那套冒烟（298 项）过一遍，基本就稳了。

## 10. 本次实测记录

环境：Windows 11（10.0.22631）、x86_64；便携版 PostgreSQL 17.6（EDB 二进制包）、MySQL 8.4.0（Community zip），分别跑在 127.0.0.1 的 55432 / 33306 端口，独立数据目录；没有 Docker 守护进程，也没有 WSL。所有服务实例都放在仓库外的 `E:\Develop\db\` 下，不碰仓库里的 `data/`。

| 场景 | 怎么验的 | 结果 |
|---|---|---|
| 发行包完整性 | 下载 `v0.1.0` 的 linux/win 两个包与 `SHA256SUMS.txt`，`sha256sum -c` | ✅ 两个包都 OK |
| Windows 二进制（发行包） | 解压 zip → 全新数据目录启动 → `/healthz`、`/api/v1/site`、`--health-probe` | ✅ 自动生成主密钥、healthz 返回 `{"db":"ok"}`、探针退出码 0 |
| SQLite 端到端 | 起服务 → 安装向导 → CI 同款冒烟 `scripts/smoke-new.sh` | ✅ **298 项全过** |
| 单元与集成测试 | `cargo test --workspace` | ✅ 279 项全过 |
| PostgreSQL 17.6 | 建库 → 启动 → 迁移 9 条/26 张表 → 冒烟 | ⚠ 迁移通过；接口层报 `NUMERIC` 解码错误（第 9 节） |
| MySQL 8.4.0 | 建库 → 启动 → 迁移 9 条 → 冒烟 | ⚠ 迁移通过；接口层报 `TEXT`/`TINYINT` 映射错误（第 9 节） |
| 前后端分离：Cloudflare Pages 式跨域 | 静态站 8098（含 SPA 回落）+ 后端 8099（`PIXORA_CORS_ORIGINS` 放行 8098），用真实 Cookie 走完向导、登录、上传、取直链 | ✅ 首页/回落/预检/凭据 Cookie（`SameSite=None; Secure`）/跨域上传全部通过；名单外来源拿不到跨域头 |
| 前后端分离：Vercel / EdgeOne 式边缘反代 | 站点 8097 把 `/api/*`、`/images/*` 转发到 8099，浏览器只看到 8097 | ✅ 向导、登录（`SameSite=Lax`）、上传、直链（按站点地址拼）、取图全部通过 |
| 前后端分离：GitHub Pages 子路径 | 把产物放到 `/Pixora/` 下，用 `404.html` 做回落 | ✅ 子路径下能开、陌生路径命中 404 页面但内容是前端；`/main.js` 在根上 404 —— 印证了 8.6 的限制 |
| 三份 compose（单文件内嵌 Caddy 配置） | 解析成功后逐项检查：服务清单、`depends_on` 指向、`healthcheck.test`、端口与卷形状、`configs` 引用是否都有定义、四个占位符是否齐全 | ✅ 三份都通过；**容器实跑未做**（这台机器没有 Docker 守护进程，`docker compose config` 也没跑成：compose 独立二进制在这台机器上下载反复断流） |

没能在这台机器上验的，以及原因：

- **Docker 容器实跑**：没有 Docker Desktop（也没装 WSL2/Hyper-V），只有编排文件的静态校验。镜像本身的构建在 CI 里跑通过（`发行版与镜像` 工作流的 `image` job），所以「镜像能编出来」是验证过的，「按 compose 起三个容器」没有。
- **Linux 二进制执行**：没有 WSL 或 Linux 环境，无法在这台 Windows 上直接跑 musl 产物。发行包里那个 Linux 二进制是同一份代码、同一次构建产出的，Windows 那份的运行行为已经验证；Linux 的准备步骤（systemd、Nginx）按发行包里的二进制位置写好，但没有在 Linux 上实际启动过。
- **平台真实部署**（Vercel / Cloudflare / EdgeOne / GitHub Pages）：没有这些平台的账号与令牌，因此用本机模拟器复刻了它们的路由规则（SPA 回落、`/api` 转发、`404.html`），把前后端链路完整跑通；真正点「Deploy」那一步没有做。

## 11. 排障

| 现象 | 多半是 |
|---|---|
| 直链地址是 `http://172.x.x.x/...` 或容器内网地址 | 没设 `PIXORA_BASE_URL` |
| 登录成功，一刷新又变未登录 | 站点是 https 但 Cookie 没带 `Secure`；或跨域接法却没配 `PIXORA_CORS_ORIGINS` |
| 前端口页面写着「服务暂时不可用」 | 后端没起来，或浏览器到后端被拦（跨域没配、域名写错） |
| 跨域请求报 `failed to fetch` | 前端域名没写进 `PIXORA_CORS_ORIGINS`（要带 `https://`，端口写全） |
| 静态托管上页面能开、图片全裂 | 后端没设 `PIXORA_BASE_URL`，或跨域接法没填 `PIXORA_API_BASE` |
| 前端 404 / 白屏，控制台报 `/main.js` 404 | 站点挂在子路径下（如 `/<仓库名>/`），产物引用的是绝对路径 |
| 注册页说「注册入口已暂停」 | 管理员关了注册；或磁盘占用触发了「容量触顶自动关闭」，去「配额与容量」看 |
| 验证码收不到 | SMTP 没配好（服务端日志会写原因）；开发时验证码也写进日志 |
| 上传报「域名收不到邮件」 | 收件域名查不到 MX 记录，注册类发信被拦（`antispam.mx_check` 可关） |
| 容器起来又退出 | `docker compose logs pixora` 看最后几行；常见是主密钥还是占位符，或者数据目录写不进去 |
| Windows 下报 `unable to open database file` | 数据目录不可写（放到普通目录，别放 `Program Files` / `target\`）；也别从 Git Bash 启动 |
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
| `PIXORA_BASE_URL` | 空（按请求推断） | 站点对外地址；反代后面**必须**设 |
| `PIXORA_TRUST_PROXY` | `1` | 信任几层代理的转发头；设大了等于允许伪造 |
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

备份清单：`data/` 整个目录 + `PIXORA_MASTER_KEY`（如果走环境变量）。对象存储上的图片不在本地，交给厂商的版本控制或生命周期规则。

### 12.3 版本与发版

版本号只写在 `Cargo.toml` 的 `[workspace.package]` 里。改版本号：

```bash
sh scripts/bump-version.sh 0.2.0     # 连同 Cargo.lock 与 web/package.json 一起改
git add -A && git commit -m "chore: 0.2.0" && git push
```

推主干后，`发行版与镜像` 工作流会读版本号：GitHub 上没有这个版本的 Release 时，出两个平台的二进制、建 `v<版本号>` 标签、发 Release，镜像带 `:<版本号>` 与 `:latest`；已经发过的版本只刷新镜像的 `:edge` / 主干名 / `:sha-<短哈希>`。
