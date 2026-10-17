# Pixora

自托管的图片托管服务。浏览器算哈希、生成缩略图、排队上传，服务端只管存与发；
PostgreSQL 方案整栈常驻 145–225MB，SQLite 方案压到 80MB 以内，一台 1C1G 的小机器就能跑。
可以整栈放一台机器，或前面挂一层反向代理/CDN——[部署](#部署)一节把三条路写全了。

![广场](docs/screenshots/plaza.webp)

设计文档在 [`docs/Pixora-技术方案.md`](docs/Pixora-技术方案.md)，主题方案在
[`docs/Pixora-主题美化方案.md`](docs/Pixora-主题美化方案.md)，接口契约在
[`docs/api-contract.md`](docs/api-contract.md)，安全与性能审计在
[`docs/安全与性能审计-2026-10-17.md`](docs/安全与性能审计-2026-10-17.md)，
界面原型在 [`docs/ui-preview.html`](docs/ui-preview.html)。

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
- **广场**：未登录可浏览公开图片，标签筛选、排序、灯箱查看（可缩放拖动、显示缩放百分比）；自动漫游间隔后台可调，前台可暂停。
- **相册**：公开 / 私有；相册页支持网格与列表、拖拽排序、批量移动与标签。
- **直链**：`/{前缀}/{sha256}.{ext}`，前缀可在后台改且旧链仍有效；`?thumb=2` 走缩略图，`?download=1` 带原始文件名下载。
- **回收站**：删除先进回收站，保留期内可恢复，到期由后台任务彻底删除。
- **账号**：邮箱 + 密码、邮箱验证码、二次验证（TOTP，口令 + 恢复码）、会话与 API Token 管理、暂停/封禁申诉入口。
- **登录方式**：邮箱（必需）+ GitHub / Google / QQ / 微信 / 手机号验证码，用户可自行绑定与解绑，管理员能统一启停。
- **管理后台**：用户与配额、存储后端与容量、频率限制（含按用户覆盖）、IP 黑名单、内容审核、邮件模板、服务条款、系统日志。
- **存储后端**：本地目录、数据库 BLOB、S3 兼容（Cloudflare R2 / 阿里云 OSS / 腾讯云 COS / MinIO / B2）、WebDAV、GitHub 仓库、Telegram 频道。
- **部署形态**：单二进制（前端嵌在里面）、Docker 镜像（默认 amd64，需要时手动出 arm64）。前端与接口同源，不单独托管。

<img src="docs/screenshots/login.webp" alt="登录页" width="720">

登录方式可以切换（账密 / 手机号验证码 / 扫码），第三方入口只在配全凭据后才出现：

<table>
<tr>
<td width="50%"><img src="docs/screenshots/upload.webp" alt="上传页"><br><sub>上传：拖拽 / 选文件夹 / 粘贴，缩略图与哈希在浏览器里算</sub></td>
<td width="50%"><img src="docs/screenshots/albums.webp" alt="相册页"><br><sub>相册：公开或私有，网格或列表，批量操作</sub></td>
</tr>
<tr>
<td width="50%"><img src="docs/screenshots/account-security.webp" alt="账号安全"><br><sub>安全：TOTP、恢复码、会话设备、换绑邮箱</sub></td>
<td width="50%"><img src="docs/screenshots/admin-ip.webp" alt="IP 黑名单"><br><sub>IP 规则：全站 / 注册 / 登录 / 上传分场景拦截，白名单优先</sub></td>
</tr>
<tr>
<td width="50%"><img src="docs/screenshots/admin-quota.webp" alt="配额与容量"><br><sub>配额：系统总额分配、触顶自动关闭注册</sub></td>
<td width="50%"><img src="docs/screenshots/admin-users.webp" alt="用户管理"><br><sub>用户：角色、状态、用量、批量配额、免登录切换</sub></td>
</tr>
<tr>
<td width="50%"><img src="docs/screenshots/admin-settings.webp" alt="系统设置"><br><sub>设置：站点、注册、会话、防盗链、内容审核</sub></td>
<td width="50%"><img src="docs/screenshots/mobile-plaza.webp" alt="手机端广场"><br><sub>移动端：同一套前端，底部标签栏与触屏手势</sub></td>
</tr>
</table>

> 截图来自当前版本的真实页面，主题为「夜航」；浅色主题「萤昼」在右上角切换，跟随系统时会自动选择。

## 使用

### 从第一次登录开始

1. **装完向导**：用第 3 步创建的管理员账号登录。登录后先到「账号 → 安全」开启二次验证，恢复码只显示一次，离线保存。
2. **上传第一张图**：进「上传」，拖文件、选文件夹或直接 `Ctrl/Cmd + V` 粘贴。目标相册、标签、公开/私有、并发数可以整批设置；队列会逐张发，刷新页面不会丢已上传的记录。
3. **拿直链**：在相册里点图片打开查看器，复制直链或下载。`?thumb=2` 是缩略图，`?download=1` 会带原始文件名；域名在后台「系统设置 → 站点」里改，旧链不会失效。
4. **分享与 API**：公开图片会进广场；私有图片只有本人和管理员能通过直链查看。需要在 PicGo、PicList 或脚本里上传时，到「账号 → API Token」建一个 Token，范围默认是 `upload + read`，要改删再加 `write`。
5. **后台**：用户与配额、存储后端、频率限制、IP 黑名单、内容审核、邮件模板、服务条款都在左侧管理区。管理员默认不受频率限制，但仍受账号状态与 CSRF 约束。

API 上传示例（Token 只在创建时显示一次）：

```bash
curl -X POST https://img.example.com/api/v1/upload \
  -H "Authorization: Bearer px_live_xxxxxxxx" \
  -F "file=@photo.jpg" \
  -F "visibility=public" \
  -F "tags=旅行,夜景"
```

返回里的 `url` 是原图直链，`thumb_url` 是缩略图；`dedup=true` 表示服务器已有相同内容，没有重复占盘。完整的接口说明见 [`docs/api-contract.md`](docs/api-contract.md)。

### 安全与性能

- 口令用 Argon2id 哈希；会话 Cookie 是 HttpOnly + SameSite=Lax，HTTPS 下自动加 Secure。
- 所有写操作校验 CSRF；管理员接口额外判角色，跨用户查看会写审计日志。
- 上传按魔数识别真实格式，限制单文件、像素、长宽与缩略图像素；SVG 入库前净化，直链再带沙箱 CSP。
- API 与上传分别限流；IP 规则支持全站 / 注册 / 登录 / 上传，命中计数有节流，白名单优先。
- 直链只暴露哈希，私有图片按「有没有公开引用」和请求者身份判定；公开图长缓存，私有图不进共享缓存。
- 设置读取有短缓存，会话活动时间按分钟节流，随机图不再做全表随机排序。
- 最近一次安全与性能审计的修复项、残留风险和上线检查清单在 [`docs/安全与性能审计-2026-10-17.md`](docs/安全与性能审计-2026-10-17.md)。

## 部署

Pixora 由三部分组成：**后端**（一个 Rust 进程，负责接口、直链、上传与后台任务）、**前端**（静态文件，嵌在后端二进制里）、**数据**（数据库与图片，都在数据目录里）。只要起一个进程，网站就能用。

完整的部署方案单独成文：[**docs/Pixora-部署方案.md**](docs/Pixora-部署方案.md)。文档开头有一张从零到上线的清单，Docker、Linux 二进制、Windows 二进制三条路都给到每一步的命令、预期结果和排障对照；还包含三套完整 compose 与本机实测记录。当前版本 0.1.4。

| 你想怎么装 | 看哪儿 |
|---|---|
| 有 Docker，想最快跑起来 | 下面这一段 |
| Linux 服务器，不想装 Docker | [部署方案 §5](docs/Pixora-部署方案.md)（二进制 + systemd + Nginx） |
| 只有 Windows 机器 | [部署方案 §6](docs/Pixora-部署方案.md)（二进制 + NSSM 服务化） |

### 最快跑起来（Docker + SQLite）

把仓库 `deploy/docker-compose.yml` 的内容存成一个 `docker-compose.yml`，改里面标着「必改」的三处——域名、站点地址、主密钥（`openssl rand -hex 32` 的输出）——然后：

```bash
docker compose pull
docker compose up -d
docker compose ps                    # pixora 应为 Up (healthy)
docker compose logs --tail=50 pixora # 看到「Pixora 已启动」就说明起来了
```

镜像默认用 `:latest`；生产建议固定成具体版本号（如 `:0.1.4`），升级时再改。PostgreSQL/MySQL 请用 0.1.1 及以上：0.1.0 的镜像建表就会失败。

不需要 `.env`，也不用先建目录：数据落在同目录的 `./data`，Docker 会自己建；Caddy 的配置内嵌在编排文件末尾（`configs.content`），自动申请 https 证书。Compose 需要 2.23 以上（`docker compose version` 查）。

浏览器打开 `https://你的域名`，会进五步安装向导：环境检查 → 数据库（自检）→ 管理员 → 站点与邮件 → 完成。装完向导入口就锁上了。没配 SMTP 也能用，验证码在服务端日志里。

没有域名时：把编排里的 `caddy` 服务与末尾的 `configs:` 整段删掉，给 `pixora` 加 `ports: ["8080:8080"]`，`PIXORA_BASE_URL` 写成 `http://服务器IP:8080`。

### 数据库

默认 SQLite，数据文件就在数据目录里，备份就是拷目录。三种方言都跑过端到端冒烟；SQLite 当前 299 项全过，PostgreSQL 17.6 与 MySQL 8.4.0 上次实测 298 项（本次审计只复跑了 SQLite）。切换只改连接串；配置在[部署方案 §4.3 / §4.4](docs/Pixora-部署方案.md)，数据库已经单独部署了的接法（宿主机、局域网、面板、云上）见[§4.5](docs/Pixora-部署方案.md)，修复过哪些方言差异见[§8](docs/Pixora-部署方案.md)。这些修复从 0.1.1 开始进发行包和镜像。

### 常见问题

直链是内网地址、刷新掉登录、图片全裂、容器起来又退出——这些都有对照表，见[部署方案 §10](docs/Pixora-部署方案.md)。

### 从源码构建

需要 Rust stable 与 Node 20+。**前端要先构建**：它的产物会被 `rust-embed` 嵌进二进制，
仓库里提交了一份（所以全新克隆可以直接 `cargo build`），改了前端就得重新构建再编 Rust。

```bash
# 1. 前端
cd web
npm ci                # 或 npm install
npm run build
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
docs/                 技术方案、部署方案、接口契约、ADR、界面原型、截图
.github/workflows/    CI、Docker 镜像（可按需多架构）、发行版二进制
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
| [发行版与镜像](.github/workflows/release.yml) | 推主干、推 `v*` 标签、手动 | 按 Cargo.toml 里的版本号发 Release（Linux / Windows 可执行文件 + 校验和）并推容器镜像 |

### 版本号放在哪、怎么发版

版本号只有一个来源：`Cargo.toml` 里 `[workspace.package]` 的 `version`。二进制启动日志里那句
`version="0.1.4"`、发版的标签、容器镜像的标签，全都按它算。

```bash
sh scripts/version.sh              # 看当前版本号（0.1.4）
sh scripts/bump-version.sh 0.2.0   # 改成 0.2.0：Cargo.toml、Cargo.lock、web/package.json、package-lock 一起改
git add -A && git commit -m "chore: 版本号 0.2.0"
git push                           # 推上去 → 自动建 v0.2.0 标签、发 Release、推镜像
```

**推一次主干会发生什么**：工作流先看 GitHub 上有没有这一版的 Release——

- 没有 → 发一版：标签 `v<版本号>`、Linux 与 Windows 可执行文件、SHA-256 校验和，
  全挂到 Releases 页面；镜像同时带上 `:<版本号>` 与 `:latest`；
- 有 → 说明这一版早就发过了（改了代码没改版本号），跳过发版，
  只把镜像的 `:edge` / `:master` / `:sha-<短哈希>` 更新掉。

所以日常随便推，只有改了版本号那一次才会出 Release，不会一推就多一版。
想补发一版、或只想要个镜像，去 Actions → 发行版与镜像 → Run workflow：
version 留空就用文件里的版本号，填别的就按你填的发；platforms 里加上 `linux/arm64`
会一并出 ARM 镜像（QEMU 模拟，第一次很慢）。

**拉镜像**：

```bash
docker pull ghcr.io/<你的用户名>/pixora:latest      # 最近发的那一版
docker pull ghcr.io/<你的用户名>/pixora:0.2.0       # 指定版本
docker pull ghcr.io/<你的用户名>/pixora:edge        # 主干最新代码，未必发过版
```

两个小提醒：首次推成功后，**GHCR 上的包默认是私有的**，想让别人不登录就能 `docker pull`，
去 GitHub 左边 Packages → 这个包 → Package settings → Change visibility → Public；
想同时推 Docker Hub，把仓库密钥 `DOCKERHUB_USERNAME` 与 `DOCKERHUB_TOKEN` 配上即可，
没配就自动跳过 GHCR 照常推。

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
- **数据库方言**：SQLite 当前 299 项端到端冒烟全过；PostgreSQL 17.6 与 MySQL 8.4.0 上次实测 298 项，本次审计没有复跑这两套库。
  MariaDB 共用 MySQL 的迁移与查询，这次没有单独验证。
- 多实例部署需要额外的共享设施（限流计数、会话、GC 锁都按单实例设计），当前按单实例设计。
- 前端只嵌在二进制里，与接口同源部署；不支持把前端单独发到静态托管或 JS 边缘运行时。

## 许可

MIT。
