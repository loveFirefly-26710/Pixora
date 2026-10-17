/**
 * 安装向导（`#/install`）。
 *
 * 未安装的实例上 `main.ts` 会把所有路由都导到这里，此时没有登录态，
 * 所以这一页不能依赖会话，也不能调用任何需要鉴权的接口。
 *
 * 五步：环境检查 → 数据库 → 管理员 → 站点与邮件 → 完成。
 * 五个面板一次建好，切步只改显示与步骤条，填过的值留在原节点上不丢；
 * 当前步按 `GET /install/status` 的 `step` 定位，刷新页面能接着填。
 */

import { ApiError, request } from "../core/api";
import { h, mount } from "../core/dom";
import { formatNumber } from "../core/format";
import type { PageContext } from "../core/router";
import { getState } from "../core/state";
import { toastError } from "../core/toast";
import type { InstallStatus } from "../core/types";
import { card, cardNode, field, kvRow, pageHead, qtip, select, spinner, textInput } from "../ui/widgets";

const STEP_TITLES = ["环境检查", "数据库", "管理员", "站点与邮件", "完成"];

/** 数据库类型。MariaDB 与 MySQL 共用驱动，提交时都写 mysql。 */
const DRIVERS: Array<{ value: string; label: string; port: number | null; note: string }> = [
  {
    value: "postgres",
    label: "PostgreSQL（默认）",
    port: 5432,
    note: "推荐：类型系统最严，条款与配额这类强约束字段不容易写歪。",
  },
  { value: "mysql", label: "MySQL", port: 3306, note: "" },
  { value: "mysql", label: "MariaDB", port: 3306, note: "与 MySQL 共用同一套驱动，提交时按 mysql 处理。" },
  {
    value: "sqlite",
    label: "SQLite（单文件，最省内存）",
    port: null,
    note: "单机小站够用，并发写入受限；「数据库名」填文件路径。",
  },
];

/** 加载态。 */
function loadingBlock(text: string): HTMLElement {
  return h(
    "div.card",
    { style: { display: "flex", alignItems: "center", gap: "8px", color: "var(--muted)", fontSize: "13px" } },
    spinner(),
    text,
  );
}

/** 错误态：服务端原话 + 重试。 */
function errorBlock(err: unknown, onRetry: () => void): HTMLElement {
  const message = err instanceof ApiError ? err.message : err instanceof Error ? err.message : String(err);
  const detail = err instanceof ApiError ? err.detail : null;
  return card(
    null,
    qtip("err", "读取安装状态失败", detail ? `${message}（${detail}）` : message),
    h("button.btn", { type: "button", text: "重试", on: { click: onRetry } }),
  );
}

function messageOf(err: unknown): string {
  if (err instanceof ApiError) return err.detail ? `${err.message}（${err.detail}）` : err.message;
  return err instanceof Error ? err.message : String(err);
}

/** 密码规则：≥10 位且同时含大小写与数字。前端先拦一道，规则本身由服务端兜底。 */
function passwordProblem(value: string): string | null {
  if (value.length < 10) return "密码至少 10 位";
  if (!/[a-z]/.test(value)) return "密码要含小写字母";
  if (!/[A-Z]/.test(value)) return "密码要含大写字母";
  if (!/[0-9]/.test(value)) return "密码要含数字";
  return null;
}

function emailProblem(value: string): string | null {
  return /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(value) ? null : "邮箱格式看起来不对";
}

/** 需要回头改标签文字的字段（SQLite 的「数据库名」要变成文件路径）。 */
function labeled(labelText: string, control: Node, tip?: string): { wrap: HTMLElement; label: HTMLLabelElement } {
  const label = h("label.f", { text: labelText });
  const wrap = h("div.fld", null, label, control, tip ? h("div.tip", { text: tip }) : null);
  return { wrap, label };
}

/** 提交型按钮：请求期间禁用，避免连点提交两次。 */
function actionButton(label: string, primary: boolean, task: () => Promise<void>): HTMLButtonElement {
  const button = h("button.btn", { type: "button", class: primary ? "pri" : "", text: label });
  button.addEventListener("click", async () => {
    button.disabled = true;
    button.textContent = "处理中…";
    try {
      await task();
    } finally {
      button.disabled = false;
      button.textContent = label;
    }
  });
  return button;
}

/** 每步底部按钮区：左边回退，右边前进。 */
function stepFooter(...children: Node[]): HTMLElement {
  const row = h("div.flexline", { style: { justifyContent: "space-between", marginTop: "16px" } });
  row.append(...children);
  return row;
}

export async function installPage(ctx: PageContext): Promise<void> {
  const root = h("div.pad");
  mount(ctx.outlet, root);
  root.append(loadingBlock("正在检查环境…"));
  const reload = (): void => void installPage(ctx);

  try {
    const status = await request<InstallStatus>("/install/status");
    mount(root, ...wizard(reload, status));
  } catch (err) {
    mount(root, errorBlock(err, reload));
  }
}

/**
 * 向导第 4 步预填的站点地址。
 *
 * 优先用后端已经知道的地址（来自 PIXORA_BASE_URL 或此前保存过的设置）：
 * 通过 IP 或内网地址打开向导时，不会把那个地址写进数据库、盖掉配置里的正式域名。
 * 后端没配时这里会拿到监听地址（0.0.0.0），它不能访问，仍用浏览器地址。
 */
function initialBaseUrl(): string {
  const configured = getState().site?.base_url?.trim().replace(/\/+$/, "") ?? "";
  if (!/^https?:\/\//i.test(configured)) return location.origin;
  if (/^https?:\/\/(0\.0\.0\.0|\[::\]|::)(:\d+)?$/i.test(configured)) return location.origin;
  return configured;
}

function wizard(reload: () => void, status: InstallStatus): Node[] {
  // 已经装好了：这组接口已经关闭，只能去登录
  if (status.installed) {
    return [
      pageHead("安装向导", "这台实例已经完成安装。"),
      cardNode(
        null,
        qtip("ok", "已安装", "安装向导已关闭（相关接口会返回 404）。要改配置请登录管理员后台。"),
        h(
          "div.flexline",
          { style: { marginTop: "12px" } },
          h("a.btn.pri", { href: "#/login", text: "去登录" }),
          h("a.btn", { href: "#/", text: "回到首页" }),
        ),
      ),
    ];
  }

  const startStep = Math.min(5, Math.max(1, status.step || 1));

  // ---------- 各步的输入 ----------
  const db = {
    driver: select(
      DRIVERS.map((item) => ({ value: item.value, label: item.label })),
      status.env.db_driver || "postgres",
    ),
    host: textInput({ value: "127.0.0.1", mono: true }),
    port: textInput({ value: "5432", mono: true }),
    database: textInput({ value: "pixora", mono: true }),
    username: textInput({ value: "pixora", mono: true }),
    password: textInput({ type: "password" }),
  };
  const admin = {
    email: textInput({ placeholder: "admin@example.com", mono: true }),
    password: textInput({ type: "password", placeholder: "至少 10 位，含大小写与数字" }),
    siteName: textInput({ value: "Pixora" }),
  };
  const site = {
    baseUrl: textInput({ value: initialBaseUrl(), mono: true }),
    prefix: textInput({ value: "images", mono: true }),
    smtpHost: textInput({ placeholder: "smtp.qiye.aliyun.com", mono: true }),
    smtpPort: textInput({ value: "465", mono: true }),
    smtpSecurity: select(
      [
        { value: "ssl", label: "SSL（465）" },
        { value: "starttls", label: "STARTTLS（587）" },
        { value: "plain", label: "不加密（内网自建）" },
      ],
      "ssl",
    ),
    smtpUser: textInput({ placeholder: "noreply@example.com", mono: true }),
    smtpPassword: textInput({ type: "password" }),
    fromName: textInput({ value: "Pixora 管理团队" }),
  };

  // ---------- 步骤条 ----------
  const stepNodes = STEP_TITLES.map((title, index) =>
    h("div.step", null, h("span.n", { text: String(index + 1) }), title),
  );
  const stepBar = h("div.steps", null, ...stepNodes);

  // ---------- 第 1 步：环境检查 ----------
  const envFacts = h("div");
  const renderEnvFacts = (): void => {
    mount(
      envFacts,
      kvRow("数据目录", status.env.data_dir || "—", true),
      kvRow("来源环境变量", status.env.env_source || "—", true),
      kvRow("数据库驱动", status.env.db_driver || "未配置", true),
      kvRow("数据库连接串", status.env.db_dsn_set ? "已配置" : "未配置（在第 2 步填，或由环境变量注入）"),
      kvRow("主密钥", status.env.master_key_set ? "已配置" : "未配置", true),
      kvRow("已检测到的表", `${formatNumber(status.tables)} 张`, true),
    );
  };
  renderEnvFacts();

  const panel1 = cardNode(
    null,
    h("h4", { text: "环境检查" }),
    envFacts,
    status.env.master_key_set
      ? qtip("ok", "环境看起来没问题", "主密钥已配置，安装后 Token、SMTP 密码这类敏感值才能加密存放。")
      : qtip(
          "err",
          "主密钥还没有配置",
          "没有主密钥，Token 与 SMTP 密码没法加密存放。继续安装会留下隐患，建议先配好环境变量再重启容器。",
        ),
    h("div.tip", { text: "数据目录来自容器环境变量，界面上不给改；要改路径请改挂载与环境变量后重启容器。" }),
    stepFooter(
      h("span.spacer"),
      actionButton("下一步", true, async () => {
        go(2);
      }),
    ),
  );

  // ---------- 第 2 步：数据库 ----------
  const dbResult = h("div", { style: { marginTop: "10px" } });
  const dbNameField = labeled("数据库名", db.database);
  const dbUserField = labeled("用户名", db.username);
  const dbHostRow = h("div.row", null, field("主机", db.host), field("端口", db.port));
  const dbAuthRow = h("div.row", null, dbNameField.wrap, dbUserField.wrap);
  const dbPasswordField = field("密码", db.password, "SQLite 不用密码");
  const driverNote = h("div");

  /** 结构与标签随驱动变：SQLite 只要一个文件路径。 */
  const syncDriver = (): void => {
    // select.value 区分不出 MySQL 与 MariaDB（值相同），用选项下标取精确的一条
    const meta = DRIVERS[db.driver.selectedIndex] ?? DRIVERS[0];
    const isSqlite = meta.value === "sqlite";
    dbHostRow.style.display = isSqlite ? "none" : "";
    dbUserField.wrap.style.display = isSqlite ? "none" : "";
    dbPasswordField.style.display = isSqlite ? "none" : "";
    dbNameField.label.textContent = isSqlite ? "数据库文件路径" : "数据库名";
    mount(driverNote, ...(meta.note ? [h("div.tip", { text: meta.note })] : []));
  };
  db.driver.addEventListener("change", () => {
    const meta = DRIVERS[db.driver.selectedIndex] ?? DRIVERS[0];
    // 换驱动时把端口带回该类数据库的默认值，省得手改
    if (meta.port !== null) db.port.value = String(meta.port);
    if (meta.value === "sqlite" && !db.database.value.includes("/")) db.database.value = "./data/pixora.db";
    if (meta.value !== "sqlite" && !/^\w+$/.test(db.database.value)) db.database.value = "pixora";
    syncDriver();
  });
  syncDriver();

  const submitDatabase = async (): Promise<boolean> => {
    const meta = DRIVERS[db.driver.selectedIndex] ?? DRIVERS[0];
    const isSqlite = meta.value === "sqlite";
    if (!db.database.value.trim()) {
      toastError(isSqlite ? "数据库文件路径不能为空" : "数据库名不能为空");
      return false;
    }
    if (!isSqlite && (!db.host.value.trim() || !db.username.value.trim())) {
      toastError("主机与用户名不能为空");
      return false;
    }
    const payload = {
      driver: meta.value,
      host: isSqlite ? null : db.host.value.trim(),
      port: isSqlite ? null : Number(db.port.value.trim()) || null,
      database: db.database.value.trim(),
      username: isSqlite ? null : db.username.value.trim(),
      password: isSqlite ? null : db.password.value,
    };
    try {
      const result = await request<{ ok: boolean; message: string }>("/install/database", {
        method: "POST",
        json: payload,
      });
      // 服务端那句话原样展示：版本、字符集、能不能建表都在这句里
      mount(dbResult, result.ok ? qtip("ok", "连接成功", result.message) : qtip("err", "连接失败", result.message));
      return result.ok;
    } catch (err) {
      mount(dbResult, qtip("err", "连接失败", messageOf(err)));
      return false;
    }
  };

  const testBtn = h("button.btn", { type: "button", text: "测试连接" });
  testBtn.addEventListener("click", async () => {
    testBtn.disabled = true;
    testBtn.textContent = "测试中…";
    await submitDatabase();
    testBtn.disabled = false;
    testBtn.textContent = "测试连接";
  });

  const panel2 = cardNode(
    null,
    h("h4", { text: "数据库" }),
    field("数据库类型", db.driver),
    driverNote,
    dbHostRow,
    dbAuthRow,
    dbPasswordField,
    dbResult,
    qtip("info", null, "存储路径不在这里填，由容器环境变量注入。本地直跑时默认取当前目录下的 ./data。"),
    stepFooter(
      actionButton("上一步", false, async () => {
        go(1);
      }),
      testBtn,
      actionButton("下一步", true, async () => {
        const ok = await submitDatabase();
        if (ok) go(3);
      }),
    ),
  );

  // ---------- 第 3 步：管理员 ----------
  const adminResult = h("div", { style: { marginTop: "10px" } });
  const panel3 = cardNode(
    null,
    h("h4", { text: "管理员" }),
    field("邮箱", admin.email, "这是唯一的超级管理员，密码丢了只能改数据库"),
    field("密码", admin.password, "至少 10 位，且必须同时含大写字母、小写字母与数字"),
    field("站点名称", admin.siteName, "显示在导航与邮件里，之后可以在系统设置里改"),
    adminResult,
    stepFooter(
      actionButton("上一步", false, async () => {
        go(2);
      }),
      actionButton("下一步", true, async () => {
        const email = admin.email.value.trim();
        const emailBad = emailProblem(email);
        if (emailBad) {
          toastError(emailBad);
          return;
        }
        const passwordBad = passwordProblem(admin.password.value);
        if (passwordBad) {
          toastError(passwordBad);
          return;
        }
        if (!admin.siteName.value.trim()) {
          toastError("站点名称不能为空");
          return;
        }
        try {
          await request("/install/admin", {
            method: "POST",
            json: { email, password: admin.password.value, site_name: admin.siteName.value.trim() },
          });
          mount(adminResult);
          go(4);
        } catch (err) {
          mount(adminResult, qtip("err", "管理员创建失败", messageOf(err)));
          toastError(err);
        }
      }),
    ),
  );

  // ---------- 第 4 步：站点与邮件 ----------
  const siteResult = h("div", { style: { marginTop: "10px" } });
  const finishResult = h("div");
  const panel4 = cardNode(
    null,
    h("h4", { text: "站点与邮件" }),
    field("基础 URL", site.baseUrl, "直链与邮件里的链接都用它。保存后以这里为准，末尾不要带斜杠"),
    field("直链前缀", site.prefix, "例如 /images/xxx.jpg 里的 images，之后可以改"),
    h("div.row", null, field("SMTP 服务器", site.smtpHost), field("端口", site.smtpPort)),
    h("div.row", null, field("安全方式", site.smtpSecurity), field("发信账号", site.smtpUser)),
    h("div.row", null, field("密码", site.smtpPassword), field("发信人名称", site.fromName)),
    h("div.tip", { text: "SMTP 这一段可以整段留空：先跳过安装，登录后在「系统设置 → 邮件与注册验证」里再配。" }),
    siteResult,
    finishResult,
    stepFooter(
      actionButton("上一步", false, async () => {
        go(3);
      }),
      actionButton("完成安装", true, async () => {
        const baseUrl = site.baseUrl.value.trim().replace(/\/+$/, "");
        const prefix = site.prefix.value.trim().replace(/^\/+|\/+$/g, "");
        if (!baseUrl) {
          toastError("基础 URL 不能为空，例如 https://img.example.com");
          return;
        }
        if (!prefix) {
          toastError("直链前缀不能为空，例如 images");
          return;
        }
        const smtpFilled = Boolean(
          site.smtpHost.value.trim() || site.smtpUser.value.trim() || site.smtpPassword.value,
        );
        try {
          await request("/install/site", {
            method: "POST",
            json: {
              base_url: baseUrl,
              upload_prefix: prefix,
              // 整段留空＝先跳过，之后再配
              smtp: smtpFilled
                ? {
                    host: site.smtpHost.value.trim(),
                    port: Number(site.smtpPort.value.trim()) || 465,
                    security: site.smtpSecurity.value,
                    user: site.smtpUser.value.trim(),
                    password: site.smtpPassword.value,
                    from_name: site.fromName.value.trim(),
                  }
                : null,
            },
          });
          const done = await request<{ ok: boolean; admin_uid: string; login_url: string }>("/install/finish", {
            method: "POST",
          });
          mount(finishResult, finishPanel(done.admin_uid, done.login_url));
          mount(siteResult);
          go(5);
        } catch (err) {
          mount(siteResult, qtip("err", "这一步没走通", messageOf(err)));
          toastError(err);
        }
      }),
    ),
  );

  // ---------- 第 5 步：完成 ----------
  const panel5 = cardNode(
    null,
    h("h4", { text: "完成" }),
    finishResult,
    h("div.tip", { text: "安装完成后这组接口会关闭（返回 404），之后请从管理员后台改配置。" }),
    stepFooter(
      actionButton("重新检查环境", false, async () => {
        reload();
      }),
      h("a.btn.pri", { href: "#/login", text: "去登录" }),
    ),
  );

  const panels = [panel1, panel2, panel3, panel4, panel5];
  let current = startStep;

  /** 切步：只改显示与步骤条类名，填过的值都留在原节点上。 */
  function go(step: number): void {
    current = Math.min(5, Math.max(1, step));
    panels.forEach((panel, index) => {
      panel.style.display = index + 1 === current ? "" : "none";
    });
    stepNodes.forEach((node, index) => {
      const number = index + 1;
      node.classList.toggle("on", number === current);
      node.classList.toggle("done", number < current);
      const marker = node.querySelector("span.n");
      if (marker) marker.textContent = number < current ? "✓" : String(number);
    });
    window.scrollTo({ top: 0 });
  }

  // 从第 5 步进来（服务端说已走到最后一步）说明安装已经完成过，
  // 完成接口此时会返回 404，拿不到管理员 UID，就把这件事说清楚
  if (startStep === 5 && finishResult.childElementCount === 0) {
    mount(
      finishResult,
      qtip(
        "info",
        "这台实例已经安装完成",
        "完成接口在安装结束后会关闭，所以刷新页面看不到管理员 UID；用安装时填的邮箱与密码登录即可。",
      ),
    );
  }

  const wrap = h("div.install-wrap", null, cardNode(null, ...panels));
  go(current);

  return [
    pageHead("安装向导", "五步走完就能用。数据库可选 PostgreSQL、MySQL、MariaDB 或 SQLite。"),
    stepBar,
    wrap,
  ];
}

/** 完成面板：管理员 UID 与登录入口。 */
function finishPanel(uid: string, loginUrl: string): HTMLElement {
  const href = loginUrl.startsWith("/") ? `#${loginUrl}` : loginUrl;
  return h(
    "div",
    null,
    qtip("ok", "安装完成", `管理员账号已创建，UID 是 ${uid}。用刚才填的邮箱与密码登录即可。`),
    kvRow("管理员 UID", uid, true),
    kvRow("登录入口", loginUrl || "/login", true),
    h("div.flexline", { style: { marginTop: "12px" } }, h("a.btn.pri", { href, text: "进入登录页" })),
  );
}
