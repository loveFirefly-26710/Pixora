# ADR-0007：仓储层用 sqlx::Any 跑一套 SQL，三方言共享实现

**状态**：已接受

## 背景

设计稿要求元数据库支持 PostgreSQL（默认）、MySQL/MariaDB、SQLite，并且明确
「仓储层用 trait 隔离 SQL 方言」。原方案是：PostgreSQL 与 MySQL 走 `sqlx`，
SQLite 走 `rusqlite`（bundled，免系统依赖）。

落地的障碍有三个：

1. `rusqlite` 是同步库，在 tokio 应用里必须 `spawn_blocking` 包一层，仓储层会
   出现两种调用风格；
2. 三套数据库的 `Row` 类型互不相同（`PgRow` / `MySqlRow` / `SqliteRow`），
   行解码要么写三遍，要么用泛型把 `sqlx::Row` 的关联类型绑成一团；
3. 表结构里几处类型在三种方言里语义相同但写法不同（时间戳、JSON 载荷、
   IP/CIDR、数组），差异会渗到每一处解码。

## 决策

**用 `sqlx::Any` 驱动跑同一份 SQL 文本，三方言共享一套实现。**

配套的三条约定：

1. **占位符统一写 `?`**，`Db::sql()` 在 PostgreSQL 上改写为 `$1..$n`（有单测覆盖
   字符串常量与行注释里的问号）；
2. **可移植的列类型**：时间戳存 RFC3339 定长文本（字典序即时间序，按天/小时分桶
   用 `substr` 即可，不必为三种方言各写日期函数）、JSON 载荷存文本由 serde 解析、
   IP 与 CIDR 存字符串（网段匹配在进程内用 `ipnet` 做）、布尔存 0/1 整数；
3. **真正的方言差异只留三处**，用 `Db::sql_v(pg, mysql, sqlite)` 给三份变体：
   自增主键回填（`RETURNING` vs `LAST_INSERT_ID()`）、UPSERT 语法
   （`ON CONFLICT` vs `ON DUPLICATE KEY UPDATE`）、以及部分唯一索引的冲突目标。

标签从设计稿的「PostgreSQL 用 `text[]` + GIN，MySQL 用关联表」统一成
**关联表（`tags` + `image_tags`）**，三种方言共用——设计稿本来就为 MySQL
指定了关联表，统一之后「按标签筛选」「标签候选」「全局改名」只有一条代码路径。

## 备选方案

- **按方言各写一套仓储**：代码量翻三倍，修复一个 bug 要改三处，最容易出现
  「PostgreSQL 上是对的，SQLite 上是错的」。
- **只支持 PostgreSQL**：违反设计稿的三方言要求，也丢掉了「SQLite 是内存最优解」
  这条推荐路径。
- **用 `sqlx::Any` 但保留 TIMESTAMPTZ/JSONB/BOOLEAN 等原生类型**：`Any` 的类型映射
  在三种方言间不一致（实测 SQLite 上 `bool` 解码直接报「Rust type bool is not
  compatible with SQL type BIGINT」），要么在每处查询手动 CAST，要么在解码层处理
  分支，比统一成可移植类型复杂得多。

## 影响

**正面**

- 新增一种数据库要改的地方是可数的（占位符改写、三处方言变体、建表脚本），
  而不是翻倍；
- 测试只需要一套：SQLite 上跑真实建表与真实 SQL，集成测试全绿就说明 SQL 是通的；
- 行解码只有一条路径（`get_i64` / `get_str` / `get_ts` / `get_bool` …），
  不需要认识三个 `Row` 类型。

**负面**

- 失去原生类型的好处：时间戳在 PostgreSQL 里是文本，范围查询与日期函数不如
  `timestamptz` 顺手（按天分桶靠 `substr` 绕过去了，代价是索引选择性略差）；
  JSON 列在 PostgreSQL 里是 `text` 而不是 `jsonb`，不能用 JSONB 的操作符与 GIN 索引；
- 因为要照顾 SQLite，全文检索统一用 `LIKE`（`tsvector` 的中文分词本来也不可用），
  标签多了以后搜索性能靠索引而不是倒排；
- 布尔存 0/1 让 PostgreSQL 的表结构看起来不够「地道」，可读性略降。

**技术债**

- 如果将来只保留 PostgreSQL，可以把时间戳、JSON、布尔换回原生类型，
  但那时标签关联表与 `substr` 分桶已经稳定工作，收益不大；
- `sqlx::Any` 的类型映射在 MySQL 上仍有一处需要实测确认（`TINYINT(1)` 与整数
  的互操作），已在 `docs/api-contract.md` 之外的测试计划里标为「上线前跑一遍」。

## 与设计稿的其它偏差（同批次落地）

| 项 | 设计稿 | 实现 | 原因 |
|---|---|---|---|
| SQLite 驱动 | `rusqlite`（bundled） | `sqlx` 的 sqlite 特性（同样 bundled，无系统依赖） | 保持异步与单一调用风格 |
| 布尔列 | `BOOLEAN` | 0/1 整数 | `sqlx::Any` 在 SQLite 上不能把整数列解码成 `bool` |
| 时间列 | `TIMESTAMPTZ` | RFC3339 定长文本 | 三种方言语义一致，避免日期函数分叉 |
| JSON 列 | `JSONB` | 文本 | `Any` 不支持 jsonb 的原生绑定 |
| 标签 | PG `text[]` + GIN | `tags` + `image_tags` 关联表 | 三方言统一（设计稿已为 MySQL 指定关联表） |
| IP/CIDR | `INET` / `CIDR` | 字符串 + 进程内 `ipnet` 匹配 | 三种数据库的 CIDR 包含语法各不相同 |
| `settings.key` | `key` | `name` | `key` 在 MySQL 里是保留字 |
| 缩略图 | 未明确落库方式 | `blobs.kind = 'thumb'` + `images.thumb_blob_id` | 缩略图也走内容寻址，同图共享一份 |
| 上传队列 | 队列表 + worker 取任务 | 请求内处理 + `upload_jobs` 记账 + 深度背压 | 客户端已排队，服务端再入队会让响应无法同步返回直链 |
