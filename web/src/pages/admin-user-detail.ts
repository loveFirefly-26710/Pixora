/**
 * 用户详情与处置
 *
 * 一页管完一个账号：基本信息、密码、账号状态与处置、处置记录、限流覆盖、危险操作。
 *
 * 处置确认框做成模态而不是新页面：原因、解封要求、是否通知这些参数要对着
 * 页面上看到的状态填；跳走再回来，管理员大概率已经忘了自己要处置的是什么。
 */

import { ApiError, request } from "../core/api";
import { h, nodes } from "../core/dom";
import {
  describeMinutes,
  formatBytes,
  formatDateTime,
  formatNumber,
  toGigabytes,
} from "../core/format";
import { confirmDialog, openModal } from "../core/modal";
import { navigate } from "../core/router";
import type { PageContext } from "../core/router";
import { accountLabel, canOperateAccount, getState } from "../core/state";
import { toast, toastError, toastOk } from "../core/toast";
import type { AdminUserRow, IdentityView, PolicyFields, UserActionRow } from "../core/types";
import { openRateLogs, toMegabytes } from "./ratelimit-cards";
import {
  badge,
  card,
  dangerZone,
  emptyState,
  field,
  pageHead,
  qtip,
  responsiveTable,
  roleBadge,
  select,
  statusBadge,
  switchRow,
  textInput,
  textarea,
} from "../ui/widgets";

/** `/admin/users/{id}` 返回的用户字段比列表多几个（契约里没有逐条列出，按可选处理）。 */
interface DetailUser extends AdminUserRow {
  email_verified?: boolean;
  status_reason?: string | null;
  status_changed_at?: string | null;
  last_login_ip?: string | null;
  totp_enabled?: boolean;
}

interface UserDetailPayload {
  user: DetailUser;
  actions: UserActionRow[];
  /** 生效的三层解析结果，展示用（含继承来的值） */
  policy: PolicyFields | null;
  /** 该用户自己的覆盖行（未解析，0 / 空 = 跟随）；编辑回传时用它 */
  policy_override?: PolicyFields | null;
  /** 当前生效的限流参数（含继承结果） */
  limits: PolicyFields | null;
  /** 这个账号挂着的第三方登录方式（邮箱不在其中，它是账号本身） */
  identities?: IdentityView[];
}

interface StatusResult {
  status: string;
  revoked_sessions: number;
  mail_sent: boolean;
}

export async function render(ctx: PageContext): Promise<void> {
  const id = Number(ctx.params.id);
  const pad = h("div.pad");
  ctx.outlet.append(pad);

  // 返回入口放在最上面：处置完一个账号，下一步大概率是回列表看下一个
  pad.append(
    h(
      "div",
      { style: { marginBottom: "10px" } },
      h("a.btn.sm.gho", { href: "#/admin/users", text: "← 返回用户管理" }),
    ),
  );
  pad.append(
    pageHead("用户详情与处置", "改邮箱密码、调配额、暂停或封禁。每一步都写进审计日志。"),
  );

  const body = h("div");
  pad.append(body);

  if (!Number.isFinite(id) || id <= 0) {
    body.append(card(null, qtip("err", "无效的用户 ID", "地址里的用户编号不是数字，无法查询。")));
    return;
  }

  async function load(): Promise<void> {
    body.replaceChildren(skeletonBlock(3));
    try {
      const detail = await request<UserDetailPayload>(`/admin/users/${id}`);
      // `identitiesCard()` 在没有第三方绑定时返回 null：由 nodes() 过滤掉，
      // 别把 null 交给原生 replaceChildren（它会把 null 当成字符串插进去）
      body.replaceChildren(
        ...nodes(
          h(
            "div.row",
            { style: { marginBottom: "14px" } },
            basicCard(detail, () => void load()),
            passwordCard(detail.user),
          ),
          h(
            "div.row",
            { style: { marginBottom: "14px" } },
            statusCard(detail, {
              onDispose: (preset) => openDispositionDialog(detail.user, preset, () => void load()),
              onRestore: () => void restoreUser(detail.user, () => void load()),
            }),
            actionsCard(detail),
          ),
          identitiesCard(detail, () => void load()),
          rateLimitCard(detail, () => void load()),
          h("div.note.d", {
            text:
              "暂停与封禁都会让网页登录和 API 同时失效，区别在后续：暂停可申请解封、随时恢复；" +
              "封禁会隐藏公开内容并阻断重新注册，要管理员手动解封。",
          }),
        ),
      );
    } catch (err) {
      body.replaceChildren(errorBlock(err, () => void load()));
    }
  }

  await load();
}

/* ------------------------------------------------------------------ */
/* 基本信息                                                            */
/* ------------------------------------------------------------------ */

function basicCard(detail: UserDetailPayload, reload: () => void): HTMLElement {
  const user = detail.user;
  const uid = textInput({ value: user.uid, readonly: true, mono: true });
  const email = textInput({ value: user.email });
  const role = select(
    [
      { value: "user", label: "普通用户" },
      { value: "admin", label: "管理员" },
    ],
    user.role,
  );
  const quota = textInput({ value: toGigabytes(user.quota_bytes), mono: true });

  // 安装时创建的那个管理员：别的管理员不能改它的邮箱、角色与配额。
  // 灰掉输入框而不是让它点了报错——服务端同样会拒
  const operable = canOperateAccount(user.uid);
  if (!operable) {
    email.readOnly = true;
    role.disabled = true;
    quota.readOnly = true;
  }
  // 访客账号是系统账号、不接受登录：角色不能改（下面那张密码卡同理）。
  // 邮箱可以改——它只是个记号，用来认「这只相册是谁的」，不走验证也不发信；
  // 配额也留着，管理员用它可以按住游客上传
  if (user.guest) {
    role.disabled = true;
  }

  const errorLine = h("div.tip", { style: { color: "var(--danger)" } });
  const save = h("button.btn.pri", { type: "button", text: "保存修改" });
  if (!operable) save.disabled = true;

  save.addEventListener("click", async () => {
    errorLine.textContent = "";
    const patch: { email?: string; role?: "admin" | "user" } = {};
    const nextEmail = email.value.trim();
    if (nextEmail !== user.email) {
      if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(nextEmail)) {
        errorLine.textContent = "邮箱格式不正确。";
        email.focus();
        return;
      }
      patch.email = nextEmail;
    }
    if (role.value !== user.role) {
      // 降级自己会让当前会话立刻失去管理权限，属于自锁操作，直接拒绝
      if (user.id === getState().user?.id) {
        errorLine.textContent = "不能修改自己的角色，请让另一位管理员操作。";
        return;
      }
      patch.role = role.value === "admin" ? "admin" : "user";
    }

    const quotaBytes = parseQuota(quota.value);
    const quotaChanged = quotaBytes !== null && quotaBytes !== user.quota_bytes;
    if (quota.value.trim() && quotaBytes === null) {
      errorLine.textContent = "配额格式不正确，填「20」表示 20 GB，也可以写「500 MB」。";
      quota.focus();
      return;
    }
    if (Object.keys(patch).length === 0 && !quotaChanged) {
      errorLine.textContent = "没有任何修改。";
      return;
    }

    save.disabled = true;
    save.textContent = "保存中…";
    const done: string[] = [];
    try {
      if (Object.keys(patch).length > 0) {
        const payload = await request<{ mail?: { delivered: boolean; detail?: string | null } }>(
          `/admin/users/${user.id}`,
          { method: "PATCH", json: patch },
        );
        if (patch.email) {
          const mail = payload?.mail ?? null;
          // 变更通知发不发得出去要如实说：管理员容易以为「改完他就知道了」。
          // 访客账号没有通知这回事（它不收信、也不登录），照实说没有
          done.push(
            user.guest
              ? `邮箱已改为 ${patch.email}（访客账号的邮箱只是个记号，不发验证与通知）`
              : mail?.delivered
                ? `邮箱已改为 ${patch.email}，变更通知已发到新旧两个地址`
                : `邮箱已改为 ${patch.email}，但变更通知没发出去：${
                    mail?.detail ?? "已留在发件箱等重试"
                  }`,
          );
        }
        if (patch.role) done.push(`角色已改为${patch.role === "admin" ? "管理员" : "普通用户"}`);
      }
      if (quotaChanged && quotaBytes !== null) {
        await request(`/admin/users/${user.id}/quota`, {
          method: "POST",
          json: { quota_bytes: quotaBytes },
        });
        done.push(`配额已改为 ${formatBytes(quotaBytes)}`);
      }
      toastOk(done.join("；"));
      // 服务端才掌握最终值（例如邮箱冲突会被拒绝），保存完重拉一次才准
      reload();
    } catch (err) {
      toastError(err);
    } finally {
      save.disabled = false;
      save.textContent = "保存修改";
    }
  });

  return card(
    "基本信息",
    h(
      "div.row",
      null,
      h("div.fld", null, h("label.f", { text: "UID" }), uid),
      h("div.fld", null, h("label.f", { text: "邮箱" }), email),
    ),
    h(
      "div.row",
      null,
      h("div.fld", null, h("label.f", { text: "角色" }), role),
      h("div.fld", null, h("label.f", { text: "配额（GB）" }), quota),
    ),
    h(
      "div.flexline",
      { style: { marginTop: "4px" } },
      save,
      badge(user.email_verified ? "邮箱已验证" : "邮箱未验证", user.email_verified ? "ok" : "warn"),
      user.totp_enabled ? badge("已开启二次验证", "info") : null,
    ),
    errorLine,
    h("div.tip", {
      text: user.guest
        ? "访客账号是系统账号，不接受登录（没有可设的密码，也发不出重置邮件），因此邮箱只起个记号作用：改了它不会验证、不会发通知，也不能拿它登录。游客上传的图片都归在它名下。"
        : operable
          ? "改邮箱后账号立即以新地址登录（管理员填写即视为可信），并给新旧两个地址各发一封变更通知。"
          : "这是安装时创建的管理员账号，只有它自己能改这些；要换主账号，先给另一个账号管理员权限。",
    }),
    h("div.tip", {
      text: `注册于 ${formatDateTime(user.created_at)} · 最近登录 ${
        user.last_login_at ? `${formatDateTime(user.last_login_at)}（${user.last_login_ip ?? "IP 未记录"}）` : "从未登录"
      }`,
    }),
  );
}

/* ------------------------------------------------------------------ */
/* 密码                                                                */
/* ------------------------------------------------------------------ */

function passwordCard(user: DetailUser): HTMLElement {
  const next = textInput({ type: "password", placeholder: "至少 10 位，含大小写与数字" });
  const repeat = textInput({ type: "password", placeholder: "再输一次" });
  const errorLine = h("div.tip", { style: { color: "var(--danger)" } });
  // 安装账号的密码不给别的管理员重置，理由与改邮箱一样：那是主钥匙
  const operable = canOperateAccount(user.uid);

  const reset = h("button.btn", { type: "button", text: "重置密码" });
  if (!operable) {
    next.readOnly = true;
    repeat.readOnly = true;
    reset.disabled = true;
    reset.title = "安装时创建的管理员账号，只有它自己能改密码";
  }
  // 访客账号不接受登录（见 core/guest.rs）：给它设密码或发重置邮件都没有意义，
  // 两张卡一起停掉。服务端同样会拒，这里只是不让管理员白点
  if (user.guest) {
    next.readOnly = true;
    repeat.readOnly = true;
    reset.disabled = true;
    reset.title = "访客账号是系统账号，不接受登录";
  }
  reset.addEventListener("click", async () => {
    errorLine.textContent = "";
    if (
      next.value.length < 10 ||
      !/[a-z]/.test(next.value) ||
      !/[A-Z]/.test(next.value) ||
      !/\d/.test(next.value)
    ) {
      errorLine.textContent = "新密码至少 10 位，且需包含大小写字母与数字。";
      return;
    }
    if (next.value !== repeat.value) {
      errorLine.textContent = "两次输入的新密码不一致。";
      return;
    }
    reset.disabled = true;
    reset.textContent = "重置中…";
    try {
      const payload = await request<{ mail?: { delivered: boolean; detail?: string | null } }>(
        `/admin/users/${user.id}`,
        { method: "PATCH", json: { password: next.value } },
      );
      const mail = payload?.mail ?? null;
      // 如实说明邮件出去没有：管理员容易以为「设完他就知道了」，
      // 而实际上对方要收到那封信才拿得到密码
      if (mail?.delivered) {
        toastOk(`密码已重置，新密码已发到 ${user.email}；他下次登录会被要求自己改一个`);
      } else if (mail) {
        toast(
          `密码已重置，但邮件没投出去：${mail.detail ?? "已留在发件箱等重试"}。` +
            "配好 SMTP 后会自动重发，也可以先口头告诉他密码。",
        );
      } else {
        toastOk("密码已重置");
      }
      next.value = "";
      repeat.value = "";
    } catch (err) {
      toastError(err);
    } finally {
      reset.disabled = false;
      reset.textContent = "重置密码";
    }
  });

  const sendMail = h("button.btn", { type: "button", text: "发送密码重置邮件" });
  if (user.guest) {
    sendMail.disabled = true;
    sendMail.title = "访客账号是系统账号，不接受登录";
  }
  sendMail.addEventListener("click", async () => {
    sendMail.disabled = true;
    sendMail.textContent = "发送中…";
    try {
      const result = await request<{ delivered: boolean; message: string }>(
        `/admin/users/${user.id}/password-reset-mail`,
        { method: "POST" },
      );
      // 发信结果原样告诉管理员：投出去了，或者卡在哪一步
      if (result.delivered) toastOk(`重置邮件已投递到 ${user.email}`);
      else toast(result.message);
    } catch (err) {
      toastError(err);
    } finally {
      sendMail.disabled = false;
      sendMail.textContent = "发送密码重置邮件";
    }
  });

  return card(
    "密码",
    h("div.fld", null, h("label.f", { text: "直接设置新密码" }), next),
    h("div.fld", null, h("label.f", { text: "确认新密码" }), repeat),
    h("div.flexline", null, reset),
    errorLine,
    h("div.tip", {
      style: { marginTop: "10px" },
      text:
        "直接设的密码会随邮件发给用户，并要求他下次登录时自己改一个——" +
        "不想经手密码就用下面那个按钮，让他在邮件里点链接自己设。",
    }),
    h("div", { style: { marginTop: "6px" } }, sendMail),
  );
}

/* ------------------------------------------------------------------ */
/* 账号状态与处置                                                      */
/* ------------------------------------------------------------------ */

interface StatusHooks {
  onDispose: (preset: "suspend" | "ban") => void;
  onRestore: () => void;
}

function statusCard(detail: UserDetailPayload, hooks: StatusHooks): HTMLElement {
  const user = detail.user;
  const isAdminAccount = user.role === "admin";
  const active = user.status === "active";

  const suspend = h("button.btn.wrn", { type: "button", text: "暂停使用" });
  const ban = h("button.btn.dgr", { type: "button", text: "封禁账号" });
  const restore = h("button.btn", { type: "button", text: "恢复正常" });

  if (isAdminAccount) {
    // 管理员不能被处置：把自己锁在外面是最难恢复的运维事故，服务端也会拒绝
    suspend.disabled = true;
    ban.disabled = true;
  } else {
    suspend.addEventListener("click", () => hooks.onDispose("suspend"));
    ban.addEventListener("click", () => hooks.onDispose("ban"));
  }
  if (active) {
    restore.disabled = true;
    restore.title = "当前状态正常，无需恢复";
  } else {
    restore.addEventListener("click", () => hooks.onRestore());
  }

  return card(
    "账号状态",
    h(
      "div.flexline",
      { style: { marginBottom: "12px" } },
      h(
        "span",
        { style: { display: "inline-flex", alignItems: "center", gap: "8px" } },
        statusBadge(user.status, `当前：${user.status_label}`),
        roleBadge(user.role),
      ),
      h("span.tip", {
        style: { margin: "0" },
        text: user.status_changed_at
          ? `处置于 ${formatDateTime(user.status_changed_at)}`
          : "从未被处置过",
      }),
    ),
    h(
      "div.table-wrap",
      null,
      h(
        "table.responsive",
        null,
        h(
          "thead",
          null,
          h("tr", null, h("th", { style: { width: "110px" }, text: "状态" }), h("th", { text: "效果" })),
        ),
        h(
          "tbody",
          null,
          effectRow("active", "正常", "可以登录、上传、访问广场"),
          effectRow(
            "suspended",
            "暂停使用",
            "网页登录和 API 调用同时失效，已上传内容保留但对外不可见。可随时恢复",
          ),
          effectRow(
            "banned",
            "封禁",
            "网页登录和 API 全部失效，隐藏其全部公开内容，同邮箱与常用 IP 无法重新注册。需要管理员解封",
          ),
        ),
      ),
    ),
    h(
      "div.fld",
      { style: { marginTop: "12px" } },
      h("label.f", { text: "处置原因（会展示给用户）" }),
      // 只读展示：原因是处置动作的一部分，改它要去确认框里改，避免两处状态打架
      h("div", {
        style: {
          border: "1px solid var(--border2)",
          borderRadius: "8px",
          padding: "7px 11px",
          fontSize: "13px",
          background: "#f9fafb",
          color: "#6b7280",
        },
        text: user.status_reason ?? "—",
      }),
      h("div.tip", { text: "修改原因在「暂停使用 / 封禁账号」的确认框里填写，随处置一起提交。" }),
    ),
    h(
      "div.flexline",
      null,
      suspend,
      ban,
      restore,
      isAdminAccount
        ? h("span.tip", {
            style: { margin: "0" },
            text: "管理员账号不能被暂停或封禁，先降级为普通用户才能处置。",
          })
        : null,
    ),
  );
}

function effectRow(status: string, label: string, effect: string): HTMLElement {
  return h(
    "tr",
    null,
    h("td", { "data-label": "状态" }, statusBadge(status, label)),
    h("td", { "data-label": "效果", text: effect }),
  );
}

/* ------------------------------------------------------------------ */
/* 处置记录与危险操作                                                  */
/* ------------------------------------------------------------------ */

function actionsCard(detail: UserDetailPayload): HTMLElement {
  const actions = detail.actions ?? [];
  const user = detail.user;

  const deleteButton = h("button.btn.dgr.sm", { type: "button", text: "删除账号" });
  const operable = canOperateAccount(detail.user.uid);
  if (!operable) {
    deleteButton.disabled = true;
    deleteButton.title = "安装时创建的管理员账号，不能删";
  }
  // 访客账号不能删：它是全站游客上传的归属地，删掉等于把那些图一次性清除，
  // 而且下一次游客上传又会把它建回来。想停游客上传有开关与暂停两条路
  if (user.guest) {
    deleteButton.disabled = true;
    deleteButton.title = "访客账号是系统账号，不能删除；要停止游客上传就关掉「允许游客上传」或暂停它";
  }
  deleteButton.addEventListener("click", async () => {
    if (user.id === getState().user?.id) {
      toastError(new Error("不能删除自己的账号"));
      return;
    }
    const ok = await confirmDialog({
      title: "删除账号",
      message:
        `删除后 ${user.email}（u_${user.uid}）的相册、图片、令牌与会话一起消失，` +
        "不再被别人引用的文件当场从磁盘上删掉，且无法撤销。" +
        "<br>照着输入邮箱确认。",
      confirmText: "删除账号",
      danger: true,
      requireText: user.email,
    });
    if (!ok) return;
    deleteButton.disabled = true;
    try {
      // 服务端要求把邮箱原样带回来：一步不可逆的操作，值得两道确认
      await request(`/admin/users/${user.id}?confirm=${encodeURIComponent(user.email)}`, {
        method: "DELETE",
      });
      toastOk(`${user.email} 及它的数据已删除`);
      navigate("/admin/users");
    } catch (err) {
      toastError(err);
      deleteButton.disabled = false;
    }
  });

  return card(
    "处置记录",
    h(
      "div.flexline",
      { style: { marginBottom: "10px", gap: "8px" } },
      h("button.btn.sm", {
        type: "button",
        text: "查看全部账号历史",
        on: { click: () => navigate(`/admin/logs?uid=${encodeURIComponent(user.uid)}`) },
      }),
      h("span.tip", {
        style: { margin: "0" },
        text: "下面这张表随账号一起消失；「系统日志」里的那份不随账号删除，含登录记录与是谁操作的。",
      }),
    ),
    actions.length > 0
      ? responsiveTable(
          ["时间", "动作", "说明"],
          actions.map((record) => [
            h("span", { text: formatDateTime(record.created_at) }),
            badge(record.action_label, actionTone(record.action)),
            h("span", { text: describeAction(record) }),
          ]),
        )
      : emptyState("🗂", "这个账号还没有任何处置记录。"),
    h(
      "div",
      { style: { marginTop: "14px" } },
      dangerZone(
        "危险操作",
        user.guest
          ? "访客账号是系统账号，不能删除：它名下是全站游客上传的图片，删掉等于一次性清空那些图。要停止游客上传，关掉「允许游客上传」或者暂停这个账号。"
          : "删除账号会同时删除该用户的全部相册与图片引用。引用计数归零的物理文件才会真正释放。",
        deleteButton,
      ),
    ),
  );
}

/** 动作 → 徽章色调。契约的 action 取值见 schema 的 user_actions.action。 */
function actionTone(action: string): "ok" | "warn" | "err" | "info" | "mute" {
  switch (action) {
    case "ban":
    case "delete":
      return "err";
    case "suspend":
      return "warn";
    case "restore":
    case "unban":
    case "unsuspend":
      return "ok";
    case "register":
      return "mute";
    default:
      // reset_password / change_email / quota_change 这类中性记录
      return "info";
  }
}

/** 处置记录的「说明」列：优先用原因，再补 detail 里的变更与通知状态。 */
function describeAction(record: UserActionRow): string {
  const parts: string[] = [];
  if (record.reason) parts.push(record.reason);
  const detail = (record.detail ?? {}) as Record<string, unknown>;
  const pair = formatPair(detail);
  if (pair) parts.push(pair);
  if ("duration_minutes" in detail) {
    const minutes = detail.duration_minutes;
    parts.push(
      typeof minutes === "number" && minutes > 0 ? `${describeMinutes(minutes)}内有效` : "永久生效",
    );
  }
  if (record.actor_email) parts.push(`操作人 ${record.actor_email}`);
  if (record.mail_sent) parts.push("通知邮件已发送");
  return parts.join(" · ") || "—";
}

/** detail 的键由后端定义，这里按常见的几组命名容错取「变更前后」。 */
function formatPair(detail: Record<string, unknown>): string | null {
  const candidates: Array<[string, string]> = [
    ["quota_bytes_from", "quota_bytes_to"],
    ["quota_from", "quota_to"],
    ["old_email", "new_email"],
    ["from", "to"],
    ["old", "new"],
    ["before", "after"],
  ];
  for (const [fromKey, toKey] of candidates) {
    if (fromKey in detail || toKey in detail) {
      return `${shortValue(fromKey, detail[fromKey])} → ${shortValue(toKey, detail[toKey])}`;
    }
  }
  return null;
}

function shortValue(key: string, value: unknown): string {
  if (value === null || value === undefined || value === "") return "空";
  if (typeof value === "number") {
    // 字节数（配额）与计数（次数）都会出现在 detail 里，靠键名和量级分辨；
    // 纯启发式，但比把 10737418240 直接摆给管理员看强
    return /bytes|size/i.test(key) || value >= 1048576 ? formatBytes(value) : formatNumber(value);
  }
  if (typeof value === "string") return value;
  return JSON.stringify(value);
}

/* ------------------------------------------------------------------ */
/* 登录方式                                                            */
/* ------------------------------------------------------------------ */

/**
 * 这个账号挂了哪些登录方式（第三方身份）。
 *
 * 管理员能摘掉某一条：用户换号、平台账号丢了这类情况下只能从这里动手。
 * 摘之前服务端会确认「还剩别的方式能进得来」，唯一的入口摘不掉。
 */
function identitiesCard(detail: UserDetailPayload, reload: () => void): HTMLElement | null {
  const list = detail.identities ?? [];
  const rows = list.map((item) => {
    const row = responsiveRow(item.label, item.display ?? "—", item.created_at);
    row.append(
      h("button.btn.sm.dgr", {
        type: "button",
        text: "摘除",
        on: {
          click: async () => {
            const ok = await confirmDialog({
              title: `摘除 ${item.label}`,
              message: `${detail.user.email} 将不能用 ${item.label} 登录。如果这是他唯一的登录方式，服务端会拒绝。`,
              confirmText: "摘除",
              danger: true,
            });
            if (!ok) return;
            try {
              await request(`/admin/users/${detail.user.id}/identities/${item.provider}`, {
                method: "DELETE",
              });
              toastOk(`${item.label} 已摘除`);
              reload();
            } catch (err) {
              toastError(err);
            }
          },
        },
      }),
    );
    return row;
  });

  return card(
    "登录方式",
    rows.length === 0
      ? h("div.tip", { text: "这个账号只用邮箱 + 密码登录，没有额外的登录方式。" })
      : h("div", { style: { display: "grid", gap: "8px" } }, ...rows),
    h("div.tip", { text: "邮箱是账号本身，不可摘除；摘除前会确认还剩别的方式能登进来。" }),
  );
}

/** 一行：名称 + 展示值 + 操作按钮（窄屏自动折成竖排）。 */
function responsiveRow(title: string, value: string, createdAt: string): HTMLElement {
  return h(
    "div",
    { style: { display: "flex", alignItems: "center", gap: "10px", flexWrap: "wrap" } },
    h("b", { style: { minWidth: "72px" }, text: title }),
    h("span.mono", { style: { flex: "1", color: "var(--muted)" }, text: value }),
    h("span.tip", { style: { margin: "0" }, text: createdAt.slice(0, 10) }),
  );
}

/* ------------------------------------------------------------------ */
/* 频率限制覆盖                                                        */
/* ------------------------------------------------------------------ */

function rateLimitCard(detail: UserDetailPayload, reload: () => void): HTMLElement {
  const user = detail.user;
  const custom = detail.policy !== null && detail.policy !== undefined;
  const effective = detail.policy ?? detail.limits ?? null;

  const apiPerMinute = textInput({ value: String(effective?.api_per_minute ?? ""), mono: true });
  const uploadPerMinute = textInput({ value: String(effective?.upload_per_minute ?? ""), mono: true });
  const uploadPerDay = textInput({ value: String(effective?.upload_per_day ?? ""), mono: true });
  const bytesPerDay = textInput({
    value: effective?.upload_bytes_per_day ? toGigabytes(effective.upload_bytes_per_day) : "",
    mono: true,
  });
  // 单张图片最大大小：0（空）表示跟随全局默认。上传时由服务端按生效值卡住，
  // 客户端队列也拿 /me 里的 limits 提前拦——两处读的是同一份解析结果
  const maxFileSize = textInput({
    value: effective?.max_file_size ? toMegabytes(effective.max_file_size) : "",
    mono: true,
  });
  const downloadPerMinute = textInput({
    value: String(effective?.download_per_minute ?? ""),
    mono: true,
  });
  const downloadPerDay = textInput({
    value: String(effective?.download_per_day ?? ""),
    mono: true,
  });
  const errorLine = h("div.tip", { style: { color: "var(--danger)" } });

  const save = h("button.btn.sm.pri", { type: "button", text: "保存覆盖" });
  save.addEventListener("click", async () => {
    errorLine.textContent = "";
    const values = {
      api_per_minute: Number(apiPerMinute.value),
      upload_per_minute: Number(uploadPerMinute.value),
      upload_per_day: Number(uploadPerDay.value),
      upload_bytes_per_day: parseQuota(bytesPerDay.value) ?? NaN,
      // 单张图片最大大小：空 = 跟随全局默认（0）；填数字按 MB，也可以写 20 MB
      max_file_size: maxFileSize.value.trim() ? (parseQuota(`${maxFileSize.value} MB`) ?? NaN) : 0,
      download_per_minute: Number(downloadPerMinute.value),
      download_per_day: Number(downloadPerDay.value),
    };
    for (const [key, value] of Object.entries(values)) {
      if (!Number.isFinite(value) || value < 0) {
        errorLine.textContent = `${key} 不是有效数字，单日流量可写「0.5 GB」或字节数，单张大小可写「20 MB」。`;
        return;
      }
    }
    save.disabled = true;
    try {
      // PUT 是整体覆盖：body 里没给的字段会继承全局默认，而这个人可能还有
      // 别的自定义项（比如单文件上限）。所以先把他自己的覆盖行原样带上——
      // 注意用 policy_override（未解析的原始值）而不是 policy（解析结果），
      // 后者会把「跟随全局」悄悄固化成当前值。路径里的标识是 uid，不是自增 id。
      //
      // `unlimited` 保持服务端返回的原值：以前这里写死 false，结果是
      // 「在这一页点一次保存，别处设好的不限额度就被悄悄取消了」
      await request(`/admin/ratelimit/users/${encodeURIComponent(user.uid)}`, {
        method: "PUT",
        json: { ...(detail.policy_override ?? {}), ...values },
      });
      toastOk("该账号的频率限制已更新");
      reload();
    } catch (err) {
      toastError(err);
    } finally {
      save.disabled = false;
    }
  });

  const reset = h("button.btn.sm", {
    type: "button",
    text: "恢复默认",
    disabled: !custom,
    title: custom ? "删除用户级覆盖，改为继承全局默认" : "当前已经在继承全局默认",
  });
  reset.addEventListener("click", async () => {
    const ok = await confirmDialog({
      title: "恢复默认频率限制",
      message: `${user.email} 会改为继承全局默认参数，用户级覆盖被删除。`,
      confirmText: "恢复默认",
    });
    if (!ok) return;
    reset.disabled = true;
    try {
      // 路径里是 uid 而不是自增 id：服务端按 uid 找用户，
      // 传数字 id 会得到一句「用户不存在」
      await request(`/admin/ratelimit/users/${encodeURIComponent(user.uid)}`, { method: "DELETE" });
      toastOk("已恢复全局默认");
      reload();
    } catch (err) {
      toastError(err);
      reset.disabled = false;
    }
  });

  const logs = h("button.btn.sm", {
    type: "button",
    text: "查看超限日志",
    // 日志弹窗就在这一页打开：频率限制已经没有自己的页面了
    on: { click: () => void openRateLogs(1, user.uid) },
  });

  const summary = effective
    ? `API ${formatNumber(effective.api_per_minute)} 次/分 · 上传 ${formatNumber(
        effective.upload_per_minute,
      )} 张/分 · 单日流量 ${formatBytes(effective.upload_bytes_per_day)} · 单张上限 ${
        effective.max_file_size > 0 ? formatBytes(effective.max_file_size) : "跟随默认"
      }`
    : "接口没有返回该账号的限流参数";

  return card(
    "频率限制",
    h(
      "div.flexline",
      { style: { marginBottom: "12px" } },
      custom ? badge("当前：自定义", "info") : badge("当前：继承默认", "mute"),
      effective?.unlimited ? badge("不限", "warn") : null,
      h("span.tip", { style: { margin: "0" }, text: summary }),
      h("span.spacer"),
      reset,
      logs,
    ),
    h(
      "div.row",
      null,
      h("div.fld", { style: { margin: "0" } }, h("label.f", { text: "API 次 / 分钟" }), apiPerMinute),
      h(
        "div.fld",
        { style: { margin: "0" } },
        h("label.f", { text: "上传 张 / 分钟" }),
        uploadPerMinute,
      ),
      h("div.fld", { style: { margin: "0" } }, h("label.f", { text: "上传 张 / 天" }), uploadPerDay),
      h("div.fld", { style: { margin: "0" } }, h("label.f", { text: "单日流量（GB）" }), bytesPerDay),
      h(
        "div.fld",
        { style: { margin: "0" } },
        h("label.f", { text: "单张图片最大大小" }),
        maxFileSize,
      ),
    ),
    h(
      "div.row",
      { style: { marginTop: "10px" } },
      h("div.fld", { style: { margin: "0" } }, h("label.f", { text: "下载 次 / 分钟" }), downloadPerMinute),
      h("div.fld", { style: { margin: "0" } }, h("label.f", { text: "下载 次 / 天" }), downloadPerDay),
      h("div.fld", { style: { margin: "0" } }, h("span.tip", {
        style: { margin: "0" },
        text: "单张大小留空 = 跟随全局默认；下载填 0 = 这一项不限。",
      })),
    ),
    h("div", { style: { marginTop: "12px" } }, save),
    errorLine,
    h("div.tip", {
      text: "全局默认、超限处理与所有用户的额度都在「系统设置」里；这一栏只改这个账号自己的这几项。单张图片最大大小直接管住这个账号能传多大的图。",
    }),
  );
}

/* ------------------------------------------------------------------ */
/* 处置确认框                                                          */
/* ------------------------------------------------------------------ */

function openDispositionDialog(
  user: DetailUser,
  preset: "suspend" | "ban",
  reload: () => void,
): void {
  let kind: "suspend" | "ban" = preset;

  const chips = h("div.flexline");
  const makeChip = (value: "suspend" | "ban", label: string): HTMLButtonElement => {
    const chip = h("button.chip", {
      type: "button",
      class: kind === value ? "on" : "",
      text: label,
    });
    chip.addEventListener("click", () => {
      kind = value;
      for (const child of Array.from(chips.children)) child.classList.toggle("on", child === chip);
      refreshLabel();
    });
    return chip;
  };
  chips.append(makeChip("suspend", "暂停使用（禁用）"), makeChip("ban", "封禁账号"));

  // 生效时长：自己填，留空就是永久。单位给小时与天两档——
  // 「封 12 小时」和「暂停 30 天」都是常见口径，只给天会逼人写小数
  const durationValue = textInput({
    type: "number",
    mono: true,
    placeholder: "留空 = 永久",
    min: "1",
  });
  const durationUnit = select([
    { value: "days", label: "天" },
    { value: "hours", label: "小时" },
  ]);

  /** 手填的时长换算成分钟；空、0、非数字一律按「永久」处理（不传这个字段）。 */
  function durationMinutes(): number | null {
    const value = Number(durationValue.value.trim());
    if (!Number.isFinite(value) || value <= 0) return null;
    const scale = durationUnit.value === "hours" ? 60 : 1440;
    return Math.max(1, Math.round(value * scale));
  }
  // 申诉邮箱默认取站点配置的注册发信邮箱，可改
  const appeal = textInput({ value: getState().site?.appeal_email ?? "", mono: true });
  const reason = textarea({
    rows: 3,
    placeholder: "例如：上传违规内容，违反服务条款第 3 条",
    value: user.status_reason ?? "",
  });
  const requirements = textarea({
    rows: 4,
    placeholder:
      "例如：请提供本人手持身份证的清晰照片，以及手写承诺书（写明账号邮箱与「承诺遵守服务条款、不再上传违规内容」），作为附件发送至管理员邮箱。",
  });

  let notify = true;
  const notifyRow = switchRow(
    "发送通知邮件",
    `由站点发信邮箱发出，收件人 ${user.email}`,
    true,
    (next) => {
      notify = next;
    },
  );

  // 访客账号：只填时长与备注。它登不进来，邮箱又是保留域（发出去必退信），
  // 申诉邮箱、解封要求、通知邮件这几项对它都没有意义
  if (user.guest) notify = false;

  const errorLine = h("div.tip", { style: { color: "var(--danger)" } });
  const cancel = h("button.btn", { type: "button", text: "取消" });
  const confirm = h("button.btn.wrn", { type: "button" });

  function refreshLabel(): void {
    const ban = kind === "ban";
    confirm.textContent = ban ? "确认封禁" : "确认暂停";
    confirm.className = `btn ${ban ? "dgr" : "wrn"}`;
  }
  refreshLabel();

  const handle = openModal({
    title: `处置 ${accountLabel(user)}`,
    body: [
      h("div.fld", null, h("label.f", { text: "处置类型" }), chips),
      h(
        "div.row",
        null,
        h(
          "div.fld",
          null,
          h("label.f", { text: "生效时长" }),
          h(
            "div.row",
            { style: { margin: "0", alignItems: "center" } },
            durationValue,
            h("div", { style: { flex: "0 0 90px" } }, durationUnit),
          ),
          h("div.tip", { text: "留空表示永久，只能手动解封；填了就到期自动恢复。" }),
        ),
        ...(user.guest
          ? []
          : [
              h(
                "div.fld",
                null,
                h("label.f", { text: "申诉接收邮箱" }),
                appeal,
                h("div.tip", { text: "默认取注册发信邮箱，可改" }),
              ),
            ]),
      ),
      h(
        "div.fld",
        null,
        h("label.f", { text: user.guest ? "备注（写进账号历史）" : "处置原因（展示给用户，也写进邮件）" }),
        reason,
      ),
      ...(user.guest
        ? [
            h("div.tip", {
              text: "访客账号不发通知邮件：它的邮箱收不到信，账号本身也登不进来。暂停它就是按住游客上传，恢复即重新开放。",
            }),
          ]
        : [field("解封要求（写进通知邮件的「如何解封」部分）", requirements), notifyRow]),
      h(
        "div",
        { style: { marginTop: "12px" } },
        qtip(
          "warn",
          null,
          "确认后立刻生效：所有会话踢下线，API Token 进入暂停；解封时自动恢复，不用重新签发。",
        ),
      ),
      errorLine,
    ],
    footer: [cancel, confirm],
  });

  cancel.addEventListener("click", () => handle.close());
  confirm.addEventListener("click", async () => {
    if (!reason.value.trim()) {
      errorLine.textContent = user.guest
        ? "请填写备注：它会写进账号历史。"
        : "请填写处置原因：它会展示给用户，也会写进通知邮件。";
      reason.focus();
      return;
    }
    errorLine.textContent = "";
    confirm.disabled = true;
    cancel.disabled = true;
    const minutes = durationMinutes();
    try {
      const result = await request<StatusResult>(`/admin/users/${user.id}/status`, {
        method: "POST",
        json: {
          action: kind,
          reason: reason.value.trim(),
          duration_minutes: minutes,
          requirements: requirements.value.trim() || null,
          appeal_email: appeal.value.trim() || null,
          notify,
        },
      });
      handle.close();
      toastOk(
        `已踢下线 ${result.revoked_sessions ?? 0} 个会话、通知邮件${
          result.mail_sent ? "已发送" : "未发送"
        }${minutes ? `；${describeMinutes(minutes)}后自动恢复` : "；未填时长，永久生效"}`,
      );
      reload();
    } catch (err) {
      toastError(err);
      confirm.disabled = false;
      cancel.disabled = false;
    }
  });

  reason.focus();
}

async function restoreUser(user: DetailUser, reload: () => void): Promise<void> {
  const ok = await confirmDialog({
    title: user.status === "banned" ? "解封账号" : "恢复账号",
    message:
      `${user.email}（u_${user.uid}）恢复正常后可以重新登录并使用 API，` +
      "已被暂停的 API Token 自动恢复，不需要重新签发。",
    confirmText: "恢复正常",
  });
  if (!ok) return;
  try {
    const result = await request<StatusResult>(`/admin/users/${user.id}/status`, {
      method: "POST",
      json: {
        action: "restore",
        reason: "管理员手动恢复账号",
        duration_minutes: null,
        requirements: null,
        appeal_email: getState().site?.appeal_email ?? null,
        notify: true,
      },
    });
    toastOk(
      `已恢复 ${user.email} · 踢下线 ${result.revoked_sessions ?? 0} 个会话${
        result.mail_sent ? " · 通知邮件已发送" : ""
      }`,
    );
    reload();
  } catch (err) {
    toastError(err);
  }
}

/* ------------------------------------------------------------------ */
/* 小工具                                                              */
/* ------------------------------------------------------------------ */

/** 配额输入解析：带单位按单位算；不带单位一律按 GB 算。 */
function parseQuota(input: string): number | null {
  const text = input.trim();
  if (!text) return null;
  const match = text.match(/^([\d.]+)\s*(B|KB|MB|GB|TB)?$/i);
  if (!match) return null;
  const value = Number(match[1]);
  if (!Number.isFinite(value) || value < 0) return null;
  // 输入框的标签写的是 GB：把「20」当成 20 字节会让管理员一不留神就给用户 20 字节配额
  const unit = (match[2] ?? "GB").toUpperCase();
  const units = ["B", "KB", "MB", "GB", "TB"];
  const index = units.indexOf(unit);
  return Math.round(value * Math.pow(1024, index < 0 ? 0 : index));
}

function skeletonBlock(count: number): HTMLElement {
  return h(
    "div",
    null,
    ...Array.from({ length: count }, () =>
      h("div.skel", { style: { height: "160px", marginBottom: "14px" } }),
    ),
  );
}

function errorBlock(err: unknown, retry: () => void): HTMLElement {
  const message =
    err instanceof ApiError
      ? err.detail
        ? `${err.message}（${err.detail}）`
        : err.message
      : err instanceof Error
        ? err.message
        : String(err);
  return h(
    "div.card",
    null,
    qtip("err", "用户详情加载失败", message),
    h(
      "div",
      { style: { marginTop: "10px" } },
      h("button.btn", { type: "button", text: "重试", on: { click: retry } }),
    ),
  );
}
