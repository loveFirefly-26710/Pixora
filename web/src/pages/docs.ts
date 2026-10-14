/**
 * API 文档。
 *
 * 内容与 `docs/api-contract.md` 保持一致。刻意做成静态页面而不是从 OpenAPI
 * 生成：文档里的说明文字（为什么这么做、什么时候不用这么做）比结构本身更值钱，
 * 而生成器只会给你结构。改接口时同步改这里与契约文档，两处一起审。
 *
 * 页面上所有示例都带「复制」按钮——用户来这一页的目的就是复制一段能跑的命令。
 */

import { copyText, h } from "../core/dom";
import { getState } from "../core/state";
import { toast, toastWithCode } from "../core/toast";
import type { PageContext } from "../core/router";
import { pageHead, qtip } from "../ui/widgets";

interface Endpoint {
  method: "GET" | "POST" | "PATCH" | "DELETE";
  path: string;
  purpose: string;
  scope: string;
}

const ENDPOINTS: Endpoint[] = [
  { method: "POST", path: "/api/v1/upload", purpose: "上传单张图片", scope: "upload" },
  { method: "POST", path: "/api/v1/blobs/exists", purpose: "批量预检哈希，判断哪些文件已存在", scope: "upload" },
  { method: "GET", path: "/api/v1/images", purpose: "图片列表，支持分页与标签筛选", scope: "read" },
  { method: "GET", path: "/api/v1/images/{id}", purpose: "单张图片详情", scope: "read" },
  { method: "PATCH", path: "/api/v1/images/{id}", purpose: "改标签、可见性、所属相册", scope: "write" },
  { method: "DELETE", path: "/api/v1/images/{id}", purpose: "删除图片（引用计数归零才删物理文件）", scope: "write" },
  { method: "GET", path: "/api/v1/albums", purpose: "相册列表", scope: "read" },
  { method: "POST", path: "/api/v1/albums", purpose: "新建相册", scope: "write" },
  { method: "GET", path: "/api/v1/plaza", purpose: "广场搜索，公开端点不需要鉴权", scope: "公开" },
  { method: "GET", path: "/api/v1/fs/tree", purpose: "数据目录递归查询", scope: "read" },
  { method: "GET", path: "/api/v1/queue/{job_id}", purpose: "查询队列任务状态", scope: "read" },
  { method: "GET", path: "/api/v1/me", purpose: "当前用户信息与配额用量", scope: "read" },
  { method: "POST", path: "/upload", purpose: "兼容旧版图床的调用方式", scope: "upload" },
];

export async function render(ctx: PageContext): Promise<void> {
  const site = getState().site;
  const base = site?.base_url ?? location.origin;
  const uploadUrl = `${base}/api/v1/upload`;

  ctx.outlet.append(
    pageHead("API 文档", "列出全部可用端点、参数含义和调用示例，照着复制就能接上 PicGo、PicList 或自己的脚本。"),
  );

  ctx.outlet.append(
    h(
      "div.row",
      { style: { marginBottom: "16px" } },
      h(
        "div.card",
        null,
        h("h4", { text: "基础信息" }),
        h(
          "div",
          { style: { fontSize: "12.5px", lineHeight: "2.1", color: "#374151" } },
          "接口根地址 ", h("span.mono", { text: base }), h("br"),
          "鉴权方式 ", h("span.mono", { text: "Authorization: Bearer <token>" }), h("br"),
          "备用方式 ", h("span.mono", { text: "X-API-Key: <token>" }), h("br"),
          "请求格式 ", h("span.mono", { text: "multipart/form-data" }), " 或 ", h("span.mono", { text: "application/json" }), h("br"),
          "响应格式 ", h("span.mono", { text: "application/json" }),
        ),
      ),
      h(
        "div.card",
        null,
        h("h4", { text: "Token 与权限" }),
        h(
          "div",
          { style: { fontSize: "12.5px", lineHeight: "2.1", color: "#374151" } },
          "在「账号 → API Token」生成，只在创建时完整显示一次。",
        ),
        h(
          "div",
          { style: { marginTop: "6px" } },
          tag("upload", "上传与预检"),
          tag("read", "读取图片、相册、目录树"),
          tag("write", "修改与删除"),
          tag("admin", "管理接口，仅管理员 Token 可用"),
        ),
        h("div.tip", { text: "Token 在库里存的是哈希，服务端也拿不到明文。" }),
      ),
    ),
  );

  // 端点总表
  const rows = ENDPOINTS.map((endpoint) => [
    h("span.m", { class: methodClass(endpoint.method), text: endpoint.method }),
    h("span.mono", { text: endpoint.path }),
    h("span", { text: endpoint.purpose }),
    endpoint.scope === "公开"
      ? h("span.badge.mute", { text: "公开" })
      : h("span.tag", { text: endpoint.scope }),
  ]);
  ctx.outlet.append(
    h(
      "div.card",
      { style: { marginBottom: "16px", padding: "0", overflow: "hidden" } },
      table(["方法", "路径", "作用", "权限"], rows),
    ),
  );

  // 上传端点的详细参数与示例
  ctx.outlet.append(
    h(
      "div.endpoint",
      null,
      h(
        "div.ep-h",
        null,
        h("span.m.post", { text: "POST" }),
        h("span.p", { text: "/api/v1/upload" }),
        h("span.spacer"),
        h("span.badge.info", { text: "上传单张" }),
      ),
      h(
        "div.ep-b",
        null,
        table(
          ["参数", "类型", "必填", "说明"],
          [
            [h("span.mono", { text: "file" }), h("span", { text: "file" }), badge("必填", "err"), h("span", { text: "图片本体，字段名固定为 file" })],
            [h("span.mono", { text: "sha256" }), h("span", { text: "string" }), badge("可选", "mute"), h("span", { text: "客户端算好的哈希，服务端会用它做去重" })],
            [h("span.mono", { text: "thumb" }), h("span", { text: "file" }), badge("可选", "mute"), h("span", { text: "客户端生成的缩略图，服务端只存不缩放" })],
            [h("span.mono", { text: "album_id" }), h("span", { text: "int" }), badge("可选", "mute"), h("span", { text: "目标相册，省略则进默认相册" })],
            [h("span.mono", { text: "tags" }), h("span", { text: "string" }), badge("可选", "mute"), h("span", { text: "逗号分隔，如 风景,旅行" })],
            [h("span.mono", { text: "visibility" }), h("span", { text: "string" }), badge("可选", "mute"), h("span", { text: "public 或 private，默认 private" })],
            [h("span.mono", { text: "original_name" }), h("span", { text: "string" }), badge("可选", "mute"), h("span", { text: "保留原始文件名，仅内部记录，不出现在直链里" })],
          ],
        ),
        h(
          "div.row",
          null,
          codeBlock("请求示例", `# curl
curl -X POST "${uploadUrl}" \\
  -H "Authorization: Bearer px_live_xxx" \\
  -F "file=@./photo.jpg" \\
  -F "tags=风景,旅行" \\
  -F "visibility=public"`),
          codeBlock(
            "响应示例",
            `{
  "data": {
    "id": 88214,
    "job_id": 991,
    "url": "${base}/${site?.upload_prefix ?? "images"}/a3f9c1b8....webp",
    "sha256": "a3f9c1b8d2e4f6a7...",
    "size": 2411724,
    "dedup": false,
    "storage_type": "local",
    "fallback_message": null
  },
  "error": null
}`,
          ),
        ),
      ),
    ),
  );

  // 预检端点：省带宽的关键一步
  ctx.outlet.append(
    h(
      "div.endpoint",
      null,
      h(
        "div.ep-h",
        null,
        h("span.m.post", { text: "POST" }),
        h("span.p", { text: "/api/v1/blobs/exists" }),
        h("span.spacer"),
        h("span.badge.info", { text: "批量预检" }),
      ),
      h(
        "div.ep-b",
        null,
        table(
          ["参数", "类型", "必填", "说明"],
          [
            [h("span.mono", { text: "hashes" }), h("span", { text: "string[]" }), badge("必填", "err"), h("span", { text: "SHA-256 数组，单次最多 1000 个" })],
          ],
        ),
        h(
          "div.row",
          null,
          codeBlock("请求示例", `curl -X POST "${base}/api/v1/blobs/exists" \\
  -H "Authorization: Bearer $TOKEN" \\
  -H "Content-Type: application/json" \\
  -d '{"hashes":["a3f9c1b8...","7b21d0f4..."]}'`),
          codeBlock(
            "响应示例",
            `{
  "data": {
    "exist": ["a3f9c1b8..."],
    "missing": ["7b21d0f4..."]
  }
}`,
          ),
        ),
        h("div.tip", { text: "这是省带宽的关键一步：只把 missing 里的文件发出去，重复的连请求都不发。" }),
      ),
    ),
  );

  // 错误码表
  ctx.outlet.append(
    h(
      "div.card",
      { style: { marginBottom: "16px" } },
      h("h4", { text: "错误码" }),
      h("div.tip", { text: "客户端别只看 HTTP 状态码：暂停和封禁都是 403，靠 code 区分，两种情况的提示文案完全不一样。" }),
      table(
        ["code", "HTTP", "含义"],
        [
          ["ACCOUNT_SUSPENDED", "403", "账号被暂停，WEB 与 API 均不可用，可按指引申请解封"],
          ["ACCOUNT_BANNED", "403", "账号被封禁，需管理员解封，无法自助申请"],
          ["ACCOUNT_PENDING", "403", "邮箱未验证，账号未激活"],
          ["IP_BLOCKED", "403", "来源 IP 命中黑名单"],
          ["TOKEN_REVOKED", "401", "Token 已撤销或过期"],
          ["QUOTA_EXCEEDED", "413", "超出账号配额"],
          ["FILE_TOO_LARGE", "413", "超出单文件上限"],
          ["UNSUPPORTED_FORMAT", "415", "格式不在白名单内"],
          ["RATE_LIMITED", "429", "触发频率限制，带 Retry-After"],
          ["QUEUE_FULL", "429", "服务端队列已满，带 Retry-After"],
        ].map((row) => [h("span.mono", { text: row[0] }), badge(row[1], row[1] === "429" ? "warn" : "err"), h("span", { text: row[2] })]),
      ),
    ),
  );

  // 客户端接入
  ctx.outlet.append(
    h(
      "div.row",
      { style: { marginTop: "4px" } },
      h(
        "div.card",
        null,
        h("h4", { text: "PicGo / PicList 配置" }),
        h("div", { style: { fontSize: "12.5px", color: "var(--muted)", marginBottom: "10px" }, text: "装「自定义 Web 图床」插件后按下表填：" }),
        table(
          ["", ""],
          [
            ["API 地址", h("span.mono", { text: uploadUrl })],
            ["文件字段名", h("span.mono", { text: "file" })],
            ["额外请求头", h("span.mono", { text: "Authorization: Bearer <token>" })],
            ["返回 URL 路径", h("span.mono", { text: "data.url" })],
          ],
        ),
        h("button.btn.pri.sm", {
          type: "button",
          text: "复制配置 JSON",
          style: { marginTop: "10px" },
          on: {
            click: async () => {
              const json = JSON.stringify(
                { url: uploadUrl, field: "file", headers: { Authorization: "Bearer <token>" }, urlPath: "data.url" },
                null,
                2,
              );
              const ok = await copyText(json);
              if (ok) toast("✓ 已复制配置 JSON");
            },
          },
        }),
      ),
      h(
        "div.card",
        null,
        h("h4", { text: "兼容旧调用方式" }),
        h("div", { style: { fontSize: "12.5px", color: "var(--muted)", marginBottom: "8px" }, text: "沿用原来的字段名和响应结构，老脚本不用改。" }),
        codeBlock(
          null,
          `curl -X POST "${base}/upload" \\
  -H "X-API-Key: $TOKEN" \\
  -F "image=@./photo.jpg" \\
  -F "tags=风景"`,
        ),
        h("div.tip", { text: "响应 {success, message, url, storage_type, tags}，和旧版一致。" }),
      ),
    ),
  );

  ctx.outlet.append(
    qtip(
      "info",
      "关于这份文档",
      "它与仓库里的 docs/api-contract.md 同步维护。改接口时先改契约文档，再改实现与这一页——三处一致才不会让对接的人踩空。",
    ),
  );
}

function methodClass(method: Endpoint["method"]): string {
  return method === "GET" ? "get" : method === "POST" ? "post" : method === "PATCH" ? "patch" : "del";
}

function tag(name: string, description: string): HTMLElement {
  return h("div", { style: { marginBottom: "2px" } }, h("span.tag", { text: name }), h("span", { text: description }));
}

function badge(text: string, tone: "ok" | "warn" | "err" | "info" | "mute"): HTMLElement {
  return h("span.badge", { class: tone, text });
}

type Cell = Node | string;

function table(headers: string[], rows: Cell[][]): HTMLElement {
  const visibleHeaders = headers.filter((header) => header !== "");
  const thead =
    visibleHeaders.length > 0
      ? h("thead", null, h("tr", null, ...visibleHeaders.map((header) => h("th", { text: header }))))
      : null;
  const tbody = h(
    "tbody",
    null,
    ...rows.map((cells) =>
      h(
        "tr",
        null,
        ...cells.map((cell) => h("td", null, typeof cell === "string" ? document.createTextNode(cell) : cell)),
      ),
    ),
  );
  return h("div.table-wrap", null, h("table", null, thead, tbody));
}

function codeBlock(title: string | null, code: string): HTMLElement {
  const block = h("pre.code", { text: code });
  const copy = h("button.btn.sm.gho", {
    type: "button",
    text: "复制",
    style: { position: "absolute", top: "6px", right: "6px" },
    on: {
      click: async () => {
        const ok = await copyText(code);
        if (ok) toastWithCode("✓ 已复制", code.split("\n")[0].slice(0, 48));
      },
    },
  });
  return h(
    "div",
    { style: { flex: "1", minWidth: "260px" } },
    title ? h("div.tip", { style: { margin: "0 0 6px" }, text: title }) : null,
    h("div", { style: { position: "relative" } }, block, copy),
  );
}
