/**
 * 安全
 *
 * 三块内容一页：密码、验证与提醒、活跃会话。
 *
 * 所有写操作走「改完就地重拉」：会话是否被吊销、恢复码还剩几个、
 * 二次验证到底开没开，只有服务端知道。本地推测出来的状态一定会和真相对不上。
 */

import { ApiError, request } from "../core/api";
import { copyText, h } from "../core/dom";
import { formatDateTime, relativeTime } from "../core/format";
import { confirmDialog, openModal } from "../core/modal";
import { encodeQr, qrSvg } from "../core/qrcode";
import type { PageContext } from "../core/router";
import { getState, updateUser } from "../core/state";
import { toastError, toastOk, toastWithCode, toast } from "../core/toast";
import type { SessionRow, UserProfile } from "../core/types";
import {
  badge,
  card,
  emptyState,
  pageHead,
  qtip,
  responsiveTable,
  switchRow,
  textInput,
} from "../ui/widgets";

/** 页面级状态：用户档案、恢复码剩余数量、会话列表。 */
let user: UserProfile | null = null;
let recoveryRemaining = 0;

export async function render(ctx: PageContext): Promise<void> {
  const pad = h("div.pad");
  ctx.outlet.append(pad);
  pad.append(
    pageHead("安全", "密码、二次验证、登录提醒和活跃会话管理。三块内容都在这一页，不再分栏。"),
  );

  const verifySlot = h("div");
  const sessionsSlot = h("div");

  pad.append(h("div.row", { style: { marginBottom: "14px" } }, passwordCard(), verifySlot), sessionsSlot);

  await load();

  async function load(): Promise<void> {
    verifySlot.replaceChildren(skeletonBlock(1));
    sessionsSlot.replaceChildren(skeletonBlock(1));
    let me: { user: UserProfile };
    try {
      me = await request<{ user: UserProfile }>("/me");
    } catch (err) {
      const block = errorBlock("安全设置加载失败", err, () => void load());
      verifySlot.replaceChildren(block);
      sessionsSlot.replaceChildren();
      return;
    }
    user = me.user;
    updateUser(user);
    // 先用 /me 里冗余的恢复码数量渲染，再异步取精确值，避免多一次串行等待
    recoveryRemaining = user.recovery_codes_remaining;
    renderVerify();
    void refreshRecoveryCount();
    void refreshSessions();
  }

  /** 重新渲染「验证与提醒」卡（开关写入后就地刷新，不动页面上其它内容）。 */
  function renderVerify(): void {
    if (!user) return;
    verifySlot.replaceChildren(verifyCard(user, recoveryRemaining, {
      onRecoveryChanged: (next) => {
        recoveryRemaining = next;
        renderVerify();
      },
      onReload: () => void load(),
    }));
  }

  async function refreshRecoveryCount(): Promise<void> {
    try {
      const payload = await request<{ remaining: number }>("/me/recovery-codes");
      if (typeof payload.remaining === "number" && payload.remaining !== recoveryRemaining) {
        recoveryRemaining = payload.remaining;
        renderVerify();
      }
    } catch {
      // 拿不到精确值就继续用 /me 里的冗余字段，不值得为这个打扰用户
    }
  }

  async function refreshSessions(): Promise<void> {
    sessionsSlot.replaceChildren(skeletonBlock(1));
    try {
      const list = await request<SessionRow[]>("/me/sessions");
      sessionsSlot.replaceChildren(
        list.length === 0
          ? emptySessions()
          : sessionsCard(list, { onChanged: () => void refreshSessions() }),
      );
    } catch (err) {
      sessionsSlot.replaceChildren(errorBlock("会话列表加载失败", err, () => void refreshSessions()));
    }
  }
}

/* ------------------------------------------------------------------ */
/* 密码                                                                */
/* ------------------------------------------------------------------ */

function passwordCard(): HTMLElement {
  const current = textInput({ type: "password", placeholder: "当前密码" });
  const next = textInput({ type: "password", placeholder: "至少 10 位，含大小写与数字" });
  const repeat = textInput({ type: "password", placeholder: "再输一次" });
  const submit = h("button.btn", { type: "button", text: "修改密码" });

  submit.addEventListener("click", async () => {
    // 先做本地检查：明显不合规的输入没必要占一次服务端往返，也别让用户等往返才发现打错了
    if (next.value !== repeat.value) {
      toastError(new Error("两次输入的新密码不一致"));
      return;
    }
    if (next.value.length < 10 || !/[a-z]/.test(next.value) || !/[A-Z]/.test(next.value) || !/\d/.test(next.value)) {
      toastError(new Error("新密码至少 10 位，且需包含大小写字母与数字"));
      return;
    }
    submit.disabled = true;
    submit.textContent = "提交中…";
    try {
      await request("/me/password", {
        method: "POST",
        json: { current: current.value, password: next.value },
      });
      // 契约：改密成功会吊销其他设备会话（保留当前会话），所以顺手刷新会话表
      toastOk("密码已修改，其他设备已被踢下线");
      current.value = "";
      next.value = "";
      repeat.value = "";
    } catch (err) {
      toastError(err);
    } finally {
      submit.disabled = false;
      submit.textContent = "修改密码";
    }
  });

  return card(
    "密码",
    h("div.fld", null, h("label.f", { text: "当前密码" }), current),
    h(
      "div.row",
      null,
      h("div.fld", null, h("label.f", { text: "新密码" }), next),
      h("div.fld", null, h("label.f", { text: "确认新密码" }), repeat),
    ),
    submit,
    // 「上次修改时间」接口没给，不编造；能确定的行为是改完踢其他设备
    h("div.tip", { text: "改完后其他设备上的会话会被踢下线，当前设备保持登录。" }),
  );
}

/* ------------------------------------------------------------------ */
/* 验证与提醒                                                          */
/* ------------------------------------------------------------------ */

interface VerifyHooks {
  onRecoveryChanged: (remaining: number) => void;
  onReload: () => void;
}

function verifyCard(profile: UserProfile, remaining: number, hooks: VerifyHooks): HTMLElement {
  const totpSwitch = h("button.sw", {
    type: "button",
    class: profile.totp_enabled ? "" : "off",
    role: "switch",
    "aria-checked": String(profile.totp_enabled),
    "aria-label": "二次验证（TOTP）",
    on: {
      click: () => {
        if (profile.totp_enabled) void openDisableTotp(hooks);
        else void openEnableTotp(hooks);
      },
    },
  });

  return card(
    "验证与提醒",
    h(
      "div.swrow",
      null,
      h(
        "div.txt",
        null,
        h("b", { text: "二次验证（TOTP）" }),
        h("small", {
          text: profile.totp_enabled
            ? "已开启 · 登录时需要验证器动态码"
            : "未开启 · 开启后即使密码泄露也无法直接登录",
        }),
      ),
      totpSwitch,
    ),
    switchWriteRow(
      "登录提醒邮件",
      "新设备登录时发送通知",
      profile.login_alert,
      "login_alert",
    ),
    switchWriteRow(
      "异常登录拦截",
      "短时间内异地登录时要求二次验证",
      profile.new_device_guard,
      "new_device_guard",
    ),
    h(
      "div.flexline",
      { style: { marginTop: "12px" } },
      h("button.btn.sm", {
        type: "button",
        text: "重新生成恢复码",
        on: { click: () => void regenerateRecoveryCodes(hooks) },
      }),
      // 剩余 0 个意味着验证器丢了就进不来了，标黄提醒
      badge(`剩余 ${remaining} 个`, remaining > 0 ? "mute" : "warn"),
    ),
    h("div.tip", { text: "恢复码用于验证器丢失时登录，每个只能用一次，请离线保存。" }),
  );
}

/**
 * 可写开关行：先由组件乐观切换，失败再翻回来。
 *
 * 不整页重渲染：开关是高频操作，重渲染会把用户正在看的会话表也刷掉。
 */
function switchWriteRow(
  title: string,
  description: string,
  checked: boolean,
  key: "login_alert" | "new_device_guard",
): HTMLElement {
  let current = checked;
  const row = switchRow(title, description, checked, (next) => void save(next));
  const found = row.querySelector<HTMLButtonElement>("button.sw");
  // switchRow 一定带开关按钮；这里收一次引用，闭包里用得着且不会是 null
  if (!found) return row;
  const button: HTMLButtonElement = found;

  async function save(next: boolean): Promise<void> {
    button.disabled = true;
    try {
      const updated = await request<UserProfile>("/me", {
        method: "PATCH",
        json: { [key]: next },
      });
      current = next;
      user = updated;
      updateUser(updated);
      toastOk(`${title}已${next ? "开启" : "关闭"}`);
    } catch (err) {
      // 翻回切换前的样子，否则界面会和真相不一致
      button.classList.toggle("off", !current);
      button.setAttribute("aria-checked", String(current));
      toastError(err);
    } finally {
      button.disabled = false;
    }
  }

  return row;
}

/** TOTP 开启：三步走的第一步只拿密钥，带对 6 位码才算真正开启。 */
async function openEnableTotp(hooks: VerifyHooks): Promise<void> {
  const body = h("div", null, h("div.skel", { style: { height: "120px" } }));
  const cancel = h("button.btn", { type: "button", text: "取消" });
  const confirm = h("button.btn.pri", { type: "button", text: "验证并开启", disabled: true });

  const handle = openModal({
    title: "开启二次验证（TOTP）",
    body,
    footer: [cancel, confirm],
    dismissable: true,
  });
  cancel.addEventListener("click", () => handle.close());

  let code = "";
  try {
    const setup = await request<{ secret: string; otpauth_url: string }>("/me/totp", {
      method: "POST",
      json: { action: "setup" },
    });

    const codeInput = textInput({ placeholder: "6 位数字", name: "totp-code" });
    codeInput.setAttribute("inputmode", "numeric");
    codeInput.setAttribute("maxlength", "6");
    codeInput.setAttribute("autocomplete", "one-time-code");
    codeInput.addEventListener("input", () => {
      code = codeInput.value.replace(/\D/g, "").slice(0, 6);
      codeInput.value = code;
      confirm.disabled = code.length !== 6;
    });

    // 二维码在本地画：密钥一旦拼进外部 QR 服务就等于把种子交给了别人。
    // 画不出来（链接过长之类）就退回手输密钥，功能不缺。
    let qrBlock: HTMLElement | null = null;
    try {
      qrBlock = h(
        "div.qr-box",
        null,
        h("div", { html: qrSvg(encodeQr(setup.otpauth_url), { size: 196 }) }),
        h("div.tip", { style: { marginTop: "6px" }, text: "用验证器 App 扫码即可，不必手输" }),
      );
    } catch {
      qrBlock = null;
    }

    const blocks: Node[] = [
      h("div.tip", {
        text: "用验证器 App（Google Authenticator / 1Password / 微软验证器等）添加账号，再输入它显示的动态码。",
      }),
    ];
    if (qrBlock) blocks.push(qrBlock);
    blocks.push(
      h(
        "div.fld",
        { style: { marginTop: "12px" } },
        h("label.f", { text: qrBlock ? "密钥（扫码不成时手动输入）" : "密钥（手动输入）" }),
        secretBox(setup.secret, "密钥"),
      ),
      h(
        "details",
        { style: { marginTop: "10px", fontSize: "12.5px" } },
        h("summary", { text: "显示 otpauth 链接（部分客户端可直接导入）" }),
        h("div", { style: { marginTop: "8px" } }, secretBox(setup.otpauth_url, "otpauth 链接")),
      ),
    );
    blocks.push(h("div.fld", { style: { marginTop: "14px" } }, h("label.f", { text: "动态码" }), codeInput));
    body.replaceChildren(...blocks);

    confirm.addEventListener("click", async () => {
      if (code.length !== 6) return;
      confirm.disabled = true;
      confirm.textContent = "校验中…";
      try {
        await request("/me/totp", { method: "POST", json: { action: "enable", code } });
        toastOk("二次验证已开启");
        handle.close();
        hooks.onReload();
      } catch (err) {
        toastError(err);
        confirm.disabled = false;
        confirm.textContent = "验证并开启";
      }
    });

    codeInput.focus();
  } catch (err) {
    body.replaceChildren(errorBlock("无法生成密钥", err, () => handle.close()));
  }
}

/** TOTP 关闭：要求动态码或邮箱验证码，防止会话被盗后被顺手关掉二次验证。 */
function openDisableTotp(hooks: VerifyHooks): void {
  const codeInput = textInput({ placeholder: "6 位动态码或邮箱验证码", name: "totp-code" });
  codeInput.setAttribute("inputmode", "numeric");
  codeInput.setAttribute("maxlength", "6");
  codeInput.setAttribute("autocomplete", "one-time-code");

  const mailBtn = h("button.btn", { type: "button", text: "改用邮箱验证码" });
  const mailStatus = h("div.tip", { style: { margin: "6px 0 0" } });
  const cancel = h("button.btn", { type: "button", text: "取消" });
  const confirm = h("button.btn.dgr", { type: "button", text: "关闭二次验证", disabled: true });

  const handle = openModal({
    title: "关闭二次验证",
    body: [
      qtip(
        "warn",
        "关闭后账号只靠密码保护",
        "建议先确认恢复码可用，再决定是否关闭。",
      ),
      h(
        "div.fld",
        { style: { marginTop: "12px" } },
        h("label.f", { text: "动态码" }),
        codeInput,
        h("div.tip", {
          text:
            "填验证器 App 里的 6 位动态码；手机丢了、换机没导出过验证器的话，" +
            "点下面的按钮收一个邮箱验证码，用它也能关。",
        }),
      ),
      h("div.flexline", { style: { marginTop: "4px" } }, mailBtn),
      mailStatus,
    ],
    footer: [cancel, confirm],
    dismissable: true,
  });

  let code = "";
  codeInput.addEventListener("input", () => {
    code = codeInput.value.replace(/\D/g, "").slice(0, 6);
    codeInput.value = code;
    confirm.disabled = code.length !== 6;
  });

  mailBtn.addEventListener("click", async () => {
    mailBtn.disabled = true;
    mailBtn.textContent = "发送中…";
    mailStatus.textContent = "";
    try {
      const result = await request<{ delivered: boolean; message: string }>("/me/totp", {
        method: "POST",
        json: { action: "send_disable_code" },
      });
      // 发信结果原样说：投出去了，或者卡在哪一步（SMTP 没配、投递失败）
      mailStatus.textContent = result.delivered
        ? `验证码已发到你的邮箱，10 分钟内有效。`
        : result.message;
      codeInput.focus();
    } catch (err) {
      mailStatus.textContent = err instanceof ApiError ? err.message : "验证码没能发出去，请稍后重试。";
    } finally {
      mailBtn.disabled = false;
      mailBtn.textContent = "改用邮箱验证码";
    }
  });

  cancel.addEventListener("click", () => handle.close());
  confirm.addEventListener("click", async () => {
    confirm.disabled = true;
    confirm.textContent = "处理中…";
    try {
      await request("/me/totp", { method: "POST", json: { action: "disable", code } });
      toastOk("二次验证已关闭");
      handle.close();
      hooks.onReload();
    } catch (err) {
      toastError(err);
      confirm.disabled = false;
      confirm.textContent = "关闭二次验证";
    }
  });
  codeInput.focus();
}

/** 重新生成恢复码：明文只出现这一次，弹窗里就让用户复制走。 */
async function regenerateRecoveryCodes(hooks: VerifyHooks): Promise<void> {
  const ok = await confirmDialog({
    title: "重新生成恢复码",
    message:
      "重新生成后，旧的恢复码会立即全部失效。" +
      "<br>新的一组恢复码只显示一次，请准备好保存它们（离线保管，别只存在同一台设备上）。",
    confirmText: "重新生成",
    danger: true,
  });
  if (!ok) return;

  try {
    const payload = await request<{ codes: string[]; remaining: number }>("/me/recovery-codes", {
      method: "POST",
    });
    hooks.onRecoveryChanged(payload.remaining);

    const list = h("div.code-list", null, ...payload.codes.map((item) => h("code", { text: item })));
    const handle = openModal({
      title: "新的恢复码",
      body: [
        qtip("warn", "只显示这一次", "关闭本窗口后就查不到了，请立刻复制保存。每个恢复码只能使用一次。"),
        h("div", { style: { marginTop: "12px" } }, list),
      ],
      footer: [
        h("button.btn", {
          type: "button",
          text: "复制全部",
          on: {
            click: async () => {
              const text = payload.codes.join("\n");
              if (await copyText(text)) toast("✓ 恢复码已复制到剪贴板");
              else toastError(new Error("复制失败，请手动选中后复制"));
            },
          },
        }),
        h("button.btn.pri", {
          type: "button",
          text: "我已保存",
          on: { click: () => handle.close() },
        }),
      ],
    });
  } catch (err) {
    toastError(err);
  }
}

/* ------------------------------------------------------------------ */
/* 活跃会话                                                            */
/* ------------------------------------------------------------------ */

interface SessionHooks {
  onChanged: () => void;
}

function sessionsCard(list: SessionRow[], hooks: SessionHooks): HTMLElement {
  const others = list.filter((item) => !item.current).length;
  const rows = list.map((item) => [
    h(
      "span",
      { style: { display: "inline-flex", alignItems: "center", gap: "6px", flexWrap: "wrap" } },
      h("span", { text: item.device ?? "未知设备" }),
      item.current ? badge("当前", "ok") : null,
    ),
    h("span.mono", { text: item.ip ?? "—" }),
    h("span", { text: relativeTime(item.last_seen_at) }),
    h("span", { text: formatDateTime(item.created_at) }),
    item.current
      ? // 当前会话不给下线的入口：把正在用的设备踢掉，页面立刻失效，这不是用户想要的
        h("span", { style: { color: "var(--faint)", fontSize: "12px" }, text: "—" })
      : revokeButton(item, hooks),
  ]);

  return card(
    "活跃会话",
    responsiveTable(["设备", "IP", "最近活动", "登录时间", "操作"], rows),
    h(
      "div.flexline",
      { style: { marginTop: "10px" } },
      h("button.btn.sm.dgr", {
        type: "button",
        text: "下线所有其他设备",
        disabled: others === 0,
        title: others === 0 ? "没有其他需要下线的设备" : `将下线 ${others} 个会话`,
        on: { click: (event) => void revokeOthers(event.currentTarget as HTMLButtonElement, hooks) },
      }),
      h("span.tip", {
        style: { margin: "0" },
        text: others === 0 ? "当前只有这一台设备在线。" : `除当前设备外还有 ${others} 个会话。`,
      }),
    ),
    sessionPolicyTip(),
  );
}

/**
 * 会话有效期的说明。
 *
 * 「多久没动就要重新登录」是用户能感知到的规则，写在列表下面比只放在
 * 后台设置里合适：掉线之后被弹到登录页时，至少知道为什么。
 */
/** 「120 分钟」写成「2 小时」——设置里填的是分钟，读的人按小时想。 */
function describeMinutes(minutes: number): string {
  if (minutes >= 60 && minutes % 60 === 0) return `${minutes / 60} 小时`;
  if (minutes >= 60) return `${Math.floor(minutes / 60)} 小时 ${minutes % 60} 分钟`;
  return `${minutes} 分钟`;
}

function sessionPolicyTip(): HTMLElement | null {
  const policy = getState().site?.session;
  if (!policy) return null;
  const parts: string[] = [];
  if (policy.idle_minutes > 0) {
    parts.push(`${describeMinutes(policy.idle_minutes)}不操作会自动退出登录`);
  }
  if (policy.absolute_days > 0) parts.push(`单次登录最长 ${policy.absolute_days} 天`);
  if (parts.length === 0) return h("div.tip", { text: "当前未设置会话超时。" });
  return h("div.tip", { text: `${parts.join("，")}。届时需要重新输入密码。` });
}

function revokeButton(item: SessionRow, hooks: SessionHooks): HTMLElement {
  const button = h("button.btn.sm.dgr", { type: "button", text: "下线" });
  button.addEventListener("click", async () => {
    const ok = await confirmDialog({
      title: "下线该会话",
      message: `「${item.device ?? "未知设备"}」（${item.ip ?? "未知 IP"}）会立即退出登录，需要重新输入密码。`,
      confirmText: "下线",
      danger: true,
    });
    if (!ok) return;
    button.disabled = true;
    try {
      await request(`/me/sessions/${item.id}`, { method: "DELETE" });
      toastOk("该会话已下线");
      hooks.onChanged();
    } catch (err) {
      toastError(err);
      button.disabled = false;
    }
  });
  return button;
}

async function revokeOthers(button: HTMLButtonElement, hooks: SessionHooks): Promise<void> {
  const ok = await confirmDialog({
    title: "下线所有其他设备",
    message: "除当前设备外的全部会话会立即失效，其他设备需要重新登录。",
    confirmText: "全部下线",
    danger: true,
  });
  if (!ok) return;
  button.disabled = true;
  try {
    const result = await request<{ revoked: number }>("/me/sessions", {
      method: "DELETE",
      query: { keep_current: "true" },
    });
    toastOk(`已下线 ${result.revoked ?? 0} 个其他会话`);
    hooks.onChanged();
  } catch (err) {
    toastError(err);
    button.disabled = false;
  }
}

/* ------------------------------------------------------------------ */
/* 小组件                                                              */
/* ------------------------------------------------------------------ */

/** 密钥 / Token 这类短串：等宽展示 + 一键复制。 */
function secretBox(value: string, label: string): HTMLElement {
  const button = h("button.btn.sm", { type: "button", text: "复制" });
  button.addEventListener("click", async () => {
    if (await copyText(value)) toastWithCode(`✓ 已复制${label}`, value);
    else toastError(new Error("复制失败，请手动选中后复制"));
  });
  return h("div.secret-box", null, h("code", { text: value }), button);
}

function skeletonBlock(count: number): HTMLElement {
  return h(
    "div",
    null,
    ...Array.from({ length: count }, () =>
      h("div.skel", { style: { height: "180px", marginBottom: "14px" } }),
    ),
  );
}

function errorBlock(title: string, err: unknown, retry: () => void): HTMLElement {
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
    qtip("err", title, message),
    h(
      "div",
      { style: { marginTop: "10px" } },
      h("button.btn", { type: "button", text: "重试", on: { click: retry } }),
    ),
  );
}

// 空状态：会话表里至少该有一条「当前会话」，真空了说明服务端异常，给个兜底文案
function emptySessions(): HTMLElement {
  return card("活跃会话", emptyState("🖥", "暂时没有活跃会话"));
}
