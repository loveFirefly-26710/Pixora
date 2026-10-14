/**
 * 存储管理（`#/admin/storage`）。
 *
 * 三条边界写死在界面上，别在别处再放宽：
 *   1. 数据目录只读。路径由容器环境变量 `PIXORA_DATA_DIR` 注入，
 *      界面上不给写入口——能改路径等于能让上传写到任意目录，这是设计稿点名的安全约束。
 *   2. 直链一级目录可以改。它是 URL 前缀不是路径，改完旧链依然有效（服务端两种前缀都解析）。
 *   3. 容量探测不可信时（网络挂载的 statfs 是挂载伪值）降级为手工填写，
 *      并把 `f_type` 摆到界面上：为什么降级要看得见、可核对。
 */

import { ApiError, request } from "../core/api";
import { h, mount } from "../core/dom";
import { formatBytes, formatNumber, toGigabytes } from "../core/format";
import { confirmDialog, openModal } from "../core/modal";
import type { PageContext } from "../core/router";
import { toastError, toastOk } from "../core/toast";
import type { BackendRow, StorageAdmin } from "../core/types";
import {
  badge,
  card,
  cardHead,
  cardNode,
  emptyState,
  field,
  pageHead,
  qtip,
  select,
  spinner,
  textInput,
} from "../ui/widgets";

interface DriverMeta {
  driver: string;
  label: string;
  desc: string;
  /** 存储分布上的色点，取值与原型一致 */
  color: string;
}

const DRIVERS: DriverMeta[] = [
  { driver: "local", label: "本地磁盘", desc: "存入 PIXORA_DATA_DIR，默认", color: "#2563eb" },
  { driver: "database", label: "数据库内", desc: "以 BLOB 形式存进元数据库", color: "#7c3aed" },
  { driver: "github", label: "GitHub 仓库", desc: "Contents API 推送", color: "#16a34a" },
  { driver: "webdav", label: "WebDAV", desc: "PUT 上传，Basic Auth", color: "#d97706" },
  { driver: "telegram", label: "Telegram", desc: "Bot API 发到频道", color: "#dc2626" },
  { driver: "s3", label: "S3 / 阿里云 OSS", desc: "对象存储，兼容 S3 协议", color: "#0891b2" },
];

function driverMeta(driver: string): DriverMeta {
  return (
    DRIVERS.find((item) => item.driver === driver) ?? {
      driver,
      label: driver,
      desc: "自定义后端",
      color: "#94a3b8",
    }
  );
}

interface ConfigFieldSpec {
  key: string;
  label: string;
  tip?: string;
  /** 密钥类字段：不回显明文，留空即表示不修改 */
  secret?: boolean;
  /** 布尔类字段渲染成下拉，避免出现手写 "true"/"false" 这种字符串 */
  kind?: "text" | "bool";
  placeholder?: string;
}

/** 各驱动的配置字段。字段名与后端 `*Config` 结构体一一对应，改这里先改后端。 */
const CONFIG_FIELDS: Record<string, ConfigFieldSpec[]> = {
  local: [],
  database: [],
  github: [
    { key: "token", label: "GitHub Token", secret: true, placeholder: "ghp_…", tip: "AES-256-GCM 加密存储，这里不回显明文" },
    { key: "owner", label: "仓库所有者", placeholder: "example-org" },
    { key: "repo", label: "仓库名称", placeholder: "img-bed" },
    { key: "prefix", label: "存储路径", tip: "仓库内路径，跟本地路径无关" },
    { key: "branch", label: "分支", placeholder: "main" },
  ],
  webdav: [
    { key: "base_url", label: "根地址", placeholder: "https://dav.example.com/remote.php/dav/files/me" },
    { key: "username", label: "用户名" },
    { key: "password", label: "密码 / 应用专用密码", secret: true, tip: "加密存储，留空表示不修改" },
    { key: "prefix", label: "路径前缀", tip: "根地址下的目录，可留空" },
  ],
  telegram: [
    { key: "bot_token", label: "Bot Token", secret: true, placeholder: "123456:ABC-DEF…" },
    { key: "chat_id", label: "频道 / 群组 chat id", placeholder: "-1001234567890" },
    { key: "api_base", label: "API 地址", placeholder: "https://api.telegram.org", tip: "走自建反代时改这里，留空用官方地址" },
    { key: "proxy", label: "代理地址", placeholder: "http://127.0.0.1:7890", tip: "支持 http 与 socks5，可留空" },
  ],
  s3: [
    { key: "endpoint", label: "Endpoint", placeholder: "https://oss-cn-hangzhou.aliyuncs.com" },
    { key: "region", label: "Region", placeholder: "cn-hangzhou" },
    { key: "bucket", label: "Bucket" },
    { key: "access_key", label: "Access Key ID" },
    { key: "secret_key", label: "Secret Access Key", secret: true, tip: "加密存储，留空表示不修改" },
    { key: "prefix", label: "路径前缀", tip: "桶内目录，可留空" },
    { key: "path_style", label: "路径风格", kind: "bool", tip: "MinIO、OSS 用「路径风格」，AWS 新桶可能只支持虚拟主机风格" },
    { key: "public_base_url", label: "公网访问前缀", placeholder: "https://cdn.example.com", tip: "配了它直链直接指向对象存储，绕过本站" },
  ],
};

/**
 * 带富文本提示的字段。`field()` 的 tip 只吃字符串，
 * 而路径类说明必须有等宽片段才好读，所以这里自己拼一个。
 */
function fieldNode(label: string, control: Node, tip?: Node | string): HTMLElement {
  return h("div.fld", null, h("label.f", { text: label }), control, tip ? h("div.tip", null, tip) : null);
}

/** 加载态：数据没回来之前先占位，避免页面先闪一下空白再跳内容。 */
function loadingBlock(text: string): HTMLElement {
  return h(
    "div.card",
    { style: { display: "flex", alignItems: "center", gap: "8px", color: "var(--muted)", fontSize: "13px" } },
    spinner(),
    text,
  );
}

/** 错误态：显示服务端原话 + 重试入口，不写「出错了」这种无用文案。 */
function errorBlock(err: unknown, onRetry: () => void): HTMLElement {
  const message = err instanceof ApiError ? err.message : err instanceof Error ? err.message : String(err);
  const detail = err instanceof ApiError ? err.detail : null;
  return card(
    null,
    qtip("err", "加载失败", detail ? `${message}（${detail}）` : message),
    h("button.btn", { type: "button", text: "重试", on: { click: onRetry } }),
  );
}

/** 十六进制 f_type：后端已经格式化过就用原值，给到数字就补上 `0x`。 */
function hexFsType(raw: string): string {
  const text = String(raw ?? "").trim();
  if (!text) return "—";
  if (text.toLowerCase().startsWith("0x")) return text;
  const value = Number(text);
  return Number.isFinite(value) ? `0x${value.toString(16)}` : text;
}

/** 添加后端按钮。空状态里要再放一个，所以做成工厂而不是复用同一个节点。 */
function addBackendButton(reload: () => void): HTMLButtonElement {
  return h("button.btn.pri.sm", {
    type: "button",
    text: "＋ 添加后端",
    on: { click: () => openBackendModal(reload) },
  });
}

export async function render(ctx: PageContext): Promise<void> {
  const root = h("div.pad");
  mount(ctx.outlet, root);
  root.append(loadingBlock("正在读取存储配置…"));
  const reload = (): void => void render(ctx);
  try {
    const data = await request<StorageAdmin>("/admin/storage");
    mount(root, ...view(reload, data));
  } catch (err) {
    mount(root, errorBlock(err, reload));
  }
}

function view(reload: () => void, data: StorageAdmin): Node[] {
  return [
    pageHead(
      "存储管理",
      "默认存储类型、各后端配置、容量展示模式。存储路径由环境变量注入，界面上只做展示，不给改。",
    ),
    h(
      "div",
      { style: { marginBottom: "14px" } },
      qtip(
        "info",
        "路径来自容器环境变量",
        h(
          "span",
          null,
          "数据目录由 ",
          h("span.mono", { text: data.paths.source }),
          " 注入，这里只读展示。要改路径请改 ",
          h("span.mono", { text: "docker-compose.yml" }),
          " 的挂载与环境变量后重启容器。",
        ),
      ),
    ),
    h("div.row", { style: { marginBottom: "14px" } }, pathsCard(reload, data), defaultDriverCard(reload, data)),
    h("div.row", { style: { marginBottom: "14px" } }, distributionCard(data), capacityCard(reload, data)),
    backendsCard(reload, data),
  ];
}

/** 运行时路径：数据目录只读、直链前缀可改、目录结构只读。 */
function pathsCard(reload: () => void, data: StorageAdmin): HTMLElement {
  const prefixInput = textInput({ value: data.paths.upload_prefix, mono: true });
  const saveBtn = h("button.btn.sm", { type: "button", text: "保存前缀" });
  saveBtn.addEventListener("click", async () => {
    const prefix = prefixInput.value.trim().replace(/^\/+|\/+$/g, "");
    if (!prefix) {
      toastError("直链目录不能为空，例如 images");
      return;
    }
    saveBtn.disabled = true;
    saveBtn.textContent = "保存中…";
    try {
      await request("/admin/storage/prefix", { method: "PUT", json: { upload_prefix: prefix } });
      toastOk("已保存，改完旧链依然有效");
      reload();
    } catch (err) {
      toastError(err);
    } finally {
      saveBtn.disabled = false;
      saveBtn.textContent = "保存前缀";
    }
  });

  return card(
    "运行时路径（只读）",
    fieldNode(
      "数据目录",
      textInput({ value: data.paths.data_dir, readonly: true, mono: true }),
      h(
        "span",
        null,
        "来自环境变量 ",
        h("span.mono", { text: data.paths.source }),
        "，改路径要改容器挂载与变量后重启",
      ),
    ),
    field("直链一级目录", prefixInput, "这是 URL 前缀，可以在这里改，改完旧链依然有效"),
    h("div", { style: { margin: "-6px 0 13px" } }, saveBtn),
    fieldNode(
      "目录结构",
      textInput({ value: data.paths.layout, readonly: true, mono: true }),
      h(
        "span",
        null,
        "实际字节存在 ",
        h("span.mono", { text: `${data.paths.data_dir.replace(/\/+$/, "")}/blobs/` }),
        "，两层之间用硬链接关联，去重不重复占盘。",
      ),
    ),
  );
}

/** 默认存储类型：六选一，切换即落库（PUT /admin/storage/default）。 */
function defaultDriverCard(reload: () => void, data: StorageAdmin): HTMLElement {
  const list = h("div");
  for (const meta of DRIVERS) {
    const on = data.default_driver === meta.driver;
    // 原型画的是开关观感，但语义是单选：点一行就等于换默认后端
    const row = h("div.swrow.pickable", { role: "radio", "aria-checked": String(on), tabindex: "0" });
    row.append(
      h("div.txt", null, h("b", { text: meta.label }), h("small", { text: meta.desc })),
      h("span.sw", { class: on ? "" : "off", "aria-hidden": "true" }),
    );
    const choose = async (): Promise<void> => {
      if (on) return;
      const backend = data.backends.find((item) => item.driver === meta.driver);
      const body: Record<string, unknown> = { driver: meta.driver };
      if (backend) body.backend_id = backend.id;
      try {
        await request("/admin/storage/default", { method: "PUT", json: body });
        toastOk(`默认存储已切换为「${meta.label}」`);
        reload();
      } catch (err) {
        toastError(err);
      }
    };
    row.addEventListener("click", () => void choose());
    row.addEventListener("keydown", (event) => {
      const key = (event as KeyboardEvent).key;
      if (key === "Enter" || key === " ") {
        event.preventDefault();
        void choose();
      }
    });
    list.append(row);
  }
  return cardNode(
    cardHead("默认存储类型", badge("单选", "mute")),
    list,
    h("div.tip", {
      text: "切换后新上传走选中的后端；远端后端失败时服务端会自动回退本地，并在上传结果里提示。",
    }),
  );
}

/** 存储分布：各驱动张数与占用，底下一行总计（含去重省下的量）。 */
function distributionCard(data: StorageAdmin): HTMLElement {
  const body = h("div", { style: { fontSize: "13px", lineHeight: "2.2" } });
  const rows = data.distribution ?? [];
  if (rows.length === 0) {
    body.append(h("div", { style: { color: "var(--muted)" }, text: "还没有任何图片入库。" }));
  }
  for (const item of rows) {
    const meta = driverMeta(item.driver);
    body.append(
      h(
        "div.flexline",
        null,
        h("span.dot", { style: { background: meta.color } }),
        meta.label,
        h("span.spacer"),
        h("span", {
          style: { color: "var(--muted)" },
          text: `${formatNumber(item.count)} 张 · ${formatBytes(item.bytes)}`,
        }),
      ),
    );
  }
  return card(
    "存储分布",
    body,
    h("div", {
      style: {
        marginTop: "10px",
        paddingTop: "10px",
        borderTop: "1px solid #f3f4f6",
        fontSize: "12.5px",
        color: "var(--muted)",
      },
      text: `总计 ${formatNumber(data.totals.count)} 张 · ${formatBytes(data.totals.bytes)} · 去重省下 ${formatBytes(data.totals.dedup_saved_bytes)}`,
    }),
  );
}

/** 容量展示模式：自动探测 / 手动填写 / 不显示。探测不可信时把降级原因与 f_type 摆出来。 */
function capacityCard(reload: () => void, data: StorageAdmin): HTMLElement {
  const probe = data.capacity.probe;
  const modeSelect = select(
    [
      { value: "auto", label: "自动探测（本地盘 / 云盘的真实值）" },
      { value: "manual", label: "手动填写总量（OSS 等网络挂载）" },
      { value: "hidden", label: "不显示磁盘容量" },
    ],
    data.capacity.mode,
  );

  const manualWrap = h("div");
  const manualInput = textInput({
    value: toGigabytes(data.capacity.manual_total ?? data.capacity.total ?? 0),
    mono: true,
  });
  manualWrap.append(
    field("系统总额（GB）", manualInput, "这里只决定展示口径，具体分配在「配额与容量」页配置"),
  );

  const syncManual = (): void => {
    manualWrap.style.display = modeSelect.value === "manual" ? "" : "none";
  };
  syncManual();
  modeSelect.addEventListener("change", syncManual);

  const saveBtn = h("button.btn.sm", { type: "button", text: "应用" });
  saveBtn.addEventListener("click", async () => {
    const mode = modeSelect.value as "auto" | "manual" | "hidden";
    const body: Record<string, unknown> = { mode };
    if (mode === "manual") {
      const total = Number(manualInput.value.trim());
      if (!Number.isFinite(total) || total <= 0) {
        toastError("请填写大于 0 的系统总额");
        return;
      }
      body.manual_total = Math.round(total * 1024 * 1024 * 1024);
    }
    saveBtn.disabled = true;
    try {
      await request("/admin/storage/capacity", { method: "PUT", json: body });
      toastOk("容量展示模式已更新");
      reload();
    } catch (err) {
      toastError(err);
    } finally {
      saveBtn.disabled = false;
    }
  });

  const nodes: Node[] = [cardHead("容量展示模式"), field("展示方式", modeSelect), manualWrap];

  if (probe && !probe.trustworthy) {
    // 降级说明：说清「为什么不信 statfs」，并给出原始 f_type 供核对
    nodes.push(
      qtip(
        "warn",
        data.capacity.mode === "manual" ? "已降级为「手动填写总量」" : "探测结果不可信，建议改成「手动填写总量」",
        h(
          "span",
          null,
          `数据目录挂在 ${probe.fs_name}，statfs 返回的是挂载伪值而不是真实容量（f_type ${hexFsType(probe.fs_type)}）。`,
          probe.note ? ` ${probe.note}` : "",
        ),
      ),
    );
  } else if (probe) {
    nodes.push(
      qtip(
        "ok",
        "容量探测可信",
        h("span", null, `${probe.fs_name}，当前读到的系统总额是 ${formatBytes(data.capacity.total)}。`),
      ),
    );
  }

  nodes.push(h("div", { style: { marginTop: "10px" } }, saveBtn));
  nodes.push(h("div.tip", { text: "具体总额与分配到「配额与容量」页配置，这里只切换展示方式。" }));
  return cardNode(null, ...nodes);
}

/** 后端配置：按驱动渲染不同字段，密钥不回显，可测试连接。 */
function backendsCard(reload: () => void, data: StorageAdmin): HTMLElement {
  const addBtn = addBackendButton(reload);
  const blocks: Node[] = [];
  if (!data.backends || data.backends.length === 0) {
    blocks.push(emptyState("💾", "还没有登记任何存储后端，先添加一个再上传。", addBackendButton(reload)));
  } else {
    for (const backend of data.backends) blocks.push(backendBlock(reload, backend));
  }
  return cardNode(
    cardHead("后端配置", addBtn),
    ...blocks,
    h("div.tip", { text: "远端后端失败时会自动回退本地，并在上传结果里提示。" }),
  );
}

function backendBlock(reload: () => void, backend: BackendRow): HTMLElement {
  const meta = driverMeta(backend.driver);
  const specs = CONFIG_FIELDS[backend.driver] ?? [];
  const inputs = new Map<string, HTMLInputElement | HTMLSelectElement>();
  const config: Record<string, unknown> = (backend.config ?? {}) as Record<string, unknown>;

  const head = h(
    "div.flexline",
    { style: { marginBottom: "10px" } },
    h("b", { text: backend.label || meta.label }),
    badge(meta.label, "info"),
    backend.is_default ? badge("默认", "ok") : null,
    backend.enabled ? null : badge("已停用", "mute"),
  );

  const body = h("div");
  if (specs.length === 0) {
    body.append(
      h("div.tip", {
        text:
          backend.driver === "local"
            ? "本地磁盘没有额外配置，路径与目录结构见上方「运行时路径」。"
            : "该驱动没有额外配置项。",
      }),
    );
  } else {
    const grid = h("div.row");
    for (const spec of specs) {
      const raw = config[spec.key];
      if (spec.kind === "bool") {
        const control = select(
          [
            { value: "true", label: "路径风格（path-style）" },
            { value: "false", label: "虚拟主机风格（virtual-hosted）" },
          ],
          raw === false ? "false" : "true",
        );
        inputs.set(spec.key, control);
        grid.append(field(spec.label, control, spec.tip));
        continue;
      }
      // 密钥字段：后端只回 secret_set: true，绝不回显明文；留空即不修改
      const configured = raw === true || (typeof raw === "string" && raw.length > 0);
      const plain = typeof raw === "string" ? raw : raw === undefined || raw === null ? "" : String(raw);
      const control = textInput({
        value: spec.secret ? "" : plain,
        type: spec.secret ? "password" : "text",
        placeholder: spec.secret && configured ? "已配置（留空表示不修改）" : (spec.placeholder ?? ""),
        mono: true,
      });
      inputs.set(spec.key, control);
      grid.append(field(spec.label, control, spec.tip));
    }
    body.append(grid);
  }

  const result = h("div");
  const testBtn = h("button.btn.sm", { type: "button", text: "测试连接" });
  const saveBtn = h("button.btn.sm.pri", { type: "button", text: "保存配置" });
  const removeBtn = h("button.btn.sm.dgr", { type: "button", text: "删除后端" });

  testBtn.addEventListener("click", async () => {
    testBtn.disabled = true;
    testBtn.textContent = "测试中…";
    try {
      const payload = await request<{ ok: boolean; message: string }>(
        `/admin/storage/backends/${backend.id}/test`,
        { method: "POST" },
      );
      // 服务端返回的那句话原样展示，不做二次加工
      mount(result, payload.ok ? qtip("ok", "连通", payload.message) : qtip("err", "不通", payload.message));
    } catch (err) {
      const message = err instanceof ApiError ? err.message : String(err);
      mount(result, qtip("err", "测试失败", message));
    } finally {
      testBtn.disabled = false;
      testBtn.textContent = "测试连接";
    }
  });

  saveBtn.addEventListener("click", async () => {
    const patch: Record<string, unknown> = {};
    for (const spec of specs) {
      const input = inputs.get(spec.key);
      if (!input) continue;
      if (spec.kind === "bool") {
        patch[spec.key] = input.value === "true";
        continue;
      }
      const value = input.value.trim();
      if (spec.secret) {
        // 留空＝不动这个字段：省略键，服务端保留库里已有的密文
        if (value) patch[spec.key] = value;
        continue;
      }
      patch[spec.key] = value;
    }
    saveBtn.disabled = true;
    saveBtn.textContent = "保存中…";
    try {
      await request(`/admin/storage/backends/${backend.id}`, { method: "PATCH", json: { config: patch } });
      toastOk("后端配置已保存");
      reload();
    } catch (err) {
      toastError(err);
    } finally {
      saveBtn.disabled = false;
      saveBtn.textContent = "保存配置";
    }
  });

  removeBtn.addEventListener("click", async () => {
    const ok = await confirmDialog({
      title: "删除这个后端",
      message: `删除「${backend.label || meta.label}」后，已存在该后端的图片仍按记录的后端读取，但新上传不会再走它。`,
      confirmText: "删除后端",
      danger: true,
    });
    if (!ok) return;
    try {
      await request(`/admin/storage/backends/${backend.id}`, { method: "DELETE" });
      toastOk("后端已删除");
      reload();
    } catch (err) {
      toastError(err);
    }
  });

  body.append(
    h("div.flexline", { style: { marginTop: "4px" } }, testBtn, saveBtn, backend.is_default ? null : removeBtn),
    result,
  );

  return h(
    "div",
    {
      style: {
        border: "1px solid var(--border)",
        borderRadius: "10px",
        padding: "12px",
        marginBottom: "12px",
        background: backend.enabled ? "#fff" : "#fafafa",
      },
    },
    head,
    body,
  );
}

/** 添加后端：驱动变更时重建字段区，字段定义与编辑表单共用同一份。 */
function openBackendModal(reload: () => void): void {
  const driverSelect = select(
    DRIVERS.map((meta) => ({ value: meta.driver, label: meta.label })),
    "local",
  );
  const labelInput = textInput({ placeholder: "例如「GitHub 主仓库」，留空用驱动名" });
  const fieldsWrap = h("div");
  const inputs = new Map<string, HTMLInputElement | HTMLSelectElement>();

  const rebuild = (): void => {
    inputs.clear();
    const specs = CONFIG_FIELDS[driverSelect.value] ?? [];
    if (specs.length === 0) {
      mount(fieldsWrap, h("div.tip", { text: "该驱动没有额外配置项。" }));
      return;
    }
    const grid = h("div.row");
    for (const spec of specs) {
      const control =
        spec.kind === "bool"
          ? select(
              [
                { value: "true", label: "路径风格（path-style）" },
                { value: "false", label: "虚拟主机风格（virtual-hosted）" },
              ],
              "true",
            )
          : textInput({ type: spec.secret ? "password" : "text", placeholder: spec.placeholder ?? "" });
      inputs.set(spec.key, control);
      grid.append(field(spec.label, control, spec.tip));
    }
    mount(fieldsWrap, grid);
  };
  driverSelect.addEventListener("change", rebuild);
  rebuild();

  const submit = h("button.btn.pri", { type: "button", text: "添加" });
  const handle = openModal({
    title: "添加存储后端",
    body: [
      field("驱动", driverSelect),
      field("名称", labelInput, "只用于后台显示"),
      fieldsWrap,
      qtip("info", null, "密钥类字段会加密存储，之后界面上不再回显明文。"),
    ],
    footer: [h("button.btn", { type: "button", text: "取消", on: { click: () => handle.close() } }), submit],
  });

  submit.addEventListener("click", async () => {
    const driver = driverSelect.value;
    const specs = CONFIG_FIELDS[driver] ?? [];
    const config: Record<string, unknown> = {};
    for (const spec of specs) {
      const input = inputs.get(spec.key);
      if (!input) continue;
      if (spec.kind === "bool") {
        config[spec.key] = input.value === "true";
        continue;
      }
      const value = input.value.trim();
      if (value) config[spec.key] = value;
    }
    submit.disabled = true;
    submit.textContent = "添加中…";
    try {
      await request("/admin/storage/backends", {
        method: "POST",
        json: { driver, label: labelInput.value.trim() || driverMeta(driver).label, config },
      });
      handle.close();
      toastOk("后端已添加");
      reload();
    } catch (err) {
      toastError(err);
      submit.disabled = false;
      submit.textContent = "添加";
    }
  });
}
