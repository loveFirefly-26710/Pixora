# Pixora

自托管的图片托管服务。面向高并发、低内存占用，为单机 Docker 部署设计。

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
| 列表页为每张图查一次数据库改造 | 标签走关联表批量取，列表页没有 N+1 |

整栈内存预算：PostgreSQL 方案 145–225MB 常驻，SQLite 方案 80MB 以内。

## 快速开始

### 本地开发

```bash
# 后端（默认 SQLite，数据落在 ./data）
cargo run -p pixora-server

# 前端（另一个终端；改一行刷新即见，不必重编 Rust）
cd web && npm install && npm run build
PIXORA_WEB_DIR=./web/dist cargo run -p pixora-server
```

首次启动没有安装过时，访问 <http://127.0.0.1:8080/#/install> 走五步安装向导
（环境检查 → 数据库 → 管理员 → 站点与邮件 → 完成）。

没有配 SMTP 时，验证码会写进服务端日志，日志里也能直接看到码，
所以本地开发不需要真邮箱。

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

首次打开 <http://127.0.0.1:8080/#/install> 走安装向导。

Windows 上有三个坑，都踩过一遍：

1. **`.ps1` 要存成 UTF-8 with BOM。** Windows PowerShell 5.1 在没有 BOM 时按本地
   代码页读脚本，中文注释和字符串会变成乱码并直接报语法错误。
2. **不要把自己的函数命名成 `Curl`。** PowerShell 的命令解析顺序是「别名 > 函数」，
   内置别名 `curl` 指向 `Invoke-WebRequest`，同名函数会被它抢走，报
   「无法将 System.Object[] 转换为参数 Uri 所需的类型」。
3. **curl 的输出要按 UTF-8 解码。** 默认按本地代码页解，服务端返回的 UTF-8 JSON
   会变成乱码导致解析失败；`scripts\smoke.ps1` 里已经处理了这件事。

### Docker

```bash
cd deploy
cp .env.example .env       # 填域名、数据库密码、主密钥
docker compose up -d
```

数据库选了 PostgreSQL 时整栈约 200MB，换 SQLite（见 compose 里的注释）可压到 80MB 以内。

## 目录结构

```
crates/
├── pixora-crypto/    哈希、口令（Argon2id）、AES-256-GCM、令牌、TOTP
├── pixora-repo/      仓储层：SQL 方言差异收敛在这里（sqlx::Any）
├── pixora-storage/   存储驱动微内核：local / database / github / webdav / telegram / s3
├── pixora-mail/      SMTP 与邮件模板（含内置文案，库里没有就用内置的）
├── pixora-core/      领域层：配置、鉴权、上传、配额、限流、条款、管理
├── pixora-queue/     进程内后台任务：邮件重试、Blob GC、对账、缩略图兜底
└── pixora-server/    单二进制入口：路由、中间件、直链、静态资源
web/                  前端（无框架 TypeScript + esbuild，产物 embed 进二进制）
migrations/           三个方言各一套建表脚本
deploy/               Dockerfile、docker-compose、Caddyfile
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

### 账号处置

暂停与封禁都让网页登录与 API 同时失效，区别在后续处理：暂停可申请解封，
封禁要管理员手动解封。**API Token 不删**——鉴权时按账号状态拦，解封即自动恢复。
管理员点确认后由注册发信邮箱自动发出通知邮件，正文里写明解封材料清单
（暂停给清单，封禁只写「请联系管理员」，不承诺解封）。

### 存储路径只走环境变量

容器里的挂载点是编排文件决定的，界面改了也写不进去；配置文件一旦可写，
就等于给了一个远程改服务器目录的入口。所以 `PIXORA_DATA_DIR` 注入，
界面上只读展示来源与当前值（ADR-0003）。

## 存储后端

| 驱动 | 说明 | 备注 |
|---|---|---|
| `local` | 本地目录（默认） | 唯一支持硬链接镜像树与容量探测的驱动 |
| `database` | 存进数据库 BLOB | 适合小图或不想碰文件系统的场景 |
| `github` | Contents API 推送 | 需 Token；同名文件覆盖要先取 sha |
| `webdav` | PUT / GET / DELETE / HEAD | Basic Auth，路径逐段百分号编码 |
| `telegram` | Bot API 发到频道 | 存储键里带 chat 与 message id，删除才能撤回消息 |
| `s3` | S3 兼容（阿里云 OSS / MinIO） | SigV4 签名，含 AWS 官方测试向量核对 |

远端后端失败会自动回退本地，并在上传结果里带 `fallback_message` 让前端提示用户。

## 内存与容量

容量展示有三种模式：`auto` 探测文件系统（识别到 ossfs 这类网络挂载时自动降级，
因为 `statfs` 返回的是伪值）、`manual` 管理员手填总额、`hidden` 不展示。
已用量一律由库内文件实时汇总，不依赖文件系统。

配额是硬约束：所有用户配额合计 + 管理员账户配额 ≤ 系统总额，超额时拒绝保存
并给出还差多少。

## 测试

```bash
cargo test                     # 后端全部单元与集成测试（SQLite 临时库），200 项
cd web && npx tsc --noEmit     # 前端类型检查
bash scripts/smoke.sh          # 端到端冒烟：起真服务走完整条主链路
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

## 刻意不做的事

- **不用微服务**：500MB 内存预算下多进程是自相矛盾的。
- **不引入 Redis**：进程内队列够用，少一个组件少一份内存和一套运维。
- **不用 ORM**：sqlx 直接写 SQL 更可控，方言差异集中在仓储层。
- **服务端不做图像缩放**：缩放与哈希是浏览器的事。唯一例外是「缩略图兜底」
  这个可选开关，因为它服务的场景（API 上传）客户端本来就给不了缩略图。
- **不在界面开放存储路径**：见 ADR-0003。

## 已知边界

- 前端的手势交互（拖拽、长按多选、双指缩放）需要真机或浏览器手工验证，
  自动化测试覆盖不到。
- 缩略图与哈希的 WASM 解码器（HEIC/TIFF/JXL）按需加载，本项目没有内置解码器：
  这些格式的缩略图由客户端的原生解码能力决定，解不动就退化成「不带缩略图」。
- 多实例部署需要额外的共享设施（限流计数与会话表），当前按单实例设计。
