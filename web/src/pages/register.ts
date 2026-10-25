/**
 * 注册 · 邮箱验证。
 *
 * 流程与设计稿一致：填邮箱 → 发验证码 → 填验证码与密码 → 创建账号。
 * 验证码走 SMTP 下发，本地没配 SMTP 时服务端会把码写进日志并（仅开发环境）
 * 在响应里带回来，这里也如实提示用户去看日志。
 *
 * 容量触顶时注册入口会关闭：这时**整页替换成提示**，而不是把按钮灰掉——
 * 灰按钮说不清「为什么不让我注册」。
 */

import { request, ApiError } from "../core/api";
import { h } from "../core/dom";
import { navigate, type PageContext } from "../core/router";
import { getState, loadMe } from "../core/state";
import { toastError } from "../core/toast";
import { brandLockup } from "../ui/brand";
import { field, passwordControl, qtip, textInput } from "../ui/widgets";
import { oauthButtons } from "../ui/login-methods";

export async function render(ctx: PageContext): Promise<void> {
  const site = getState().site;

  // 注册关闭：整页提示。只说「暂停」与「时间待定」——具体原因与恢复时间
  // 都是运维口径，而且随时会变，写在对外页面上只会误导
  if (site && !site.registration_open) {
    ctx.outlet.append(
      h(
        "div",
        { style: { maxWidth: "520px", margin: "0 auto", padding: "48px 18px", textAlign: "center" } },
        h("div", { style: { fontSize: "34px", marginBottom: "10px" }, text: "🚧" }),
        h("h2", { style: { fontSize: "19px", margin: "0 0 8px" }, text: "暂停新用户注册" }),
        h("p", {
          style: { color: "var(--muted)", fontSize: "13px", lineHeight: "1.9" },
          // 只通告「暂停了」：具体口径（占用多少、阈值多少）是运维信息，
          // 摆在注册页上对访客没用，而且天天在变
          text: "注册入口暂时关闭，重新开放时间待定。已注册用户不受影响，照常使用。",
        }),
        qtip("info", "已注册用户不受影响", "登录、上传、浏览一切照常。注册恢复后这里会重新出现注册表单。"),
        h(
          "div",
          { style: { marginTop: "18px" } },
          h("a.btn", { href: "#/login", text: "去登录" }),
        ),
      ),
    );
    return;
  }

  const emailInput = textInput({ placeholder: "chenyu@example.com", type: "email" });
  const domains = site?.register_email_domains ?? [];
  if (domains.length > 0) {
    // 白名单开着就先把话说在前面：填完邮箱、点了发码才被拒是最难受的
    emailInput.placeholder = `you@${domains[0]}`;
  }
  const codeInput = textInput({ placeholder: "6 位数字", name: "code" });
  const passwordInput = passwordControl({ placeholder: "至少 10 位，含大小写与数字" });
  const confirmInput = passwordControl({ placeholder: "再输入一次" });

  const sendButton = h("button.btn", {
    type: "button",
    text: "发送验证码",
    style: { flex: "0 0 132px" },
  });
  const errorBox = h("div", { style: { display: "none", marginBottom: "12px" } });
  const submit = h("button.btn.pri.blk", { type: "submit", text: "创建账号" });

  const quotaHint = h("div.tip", {
    text: domains.length > 0
      ? `本站只接受 ${domains.join("、")} 的邮箱注册。`
      : "注册后会自动获得管理员配置的默认配额，可在「账号信息」里查看。",
  });

  let cooldown = 0;
  let timer: number | undefined;

  function startCooldown(seconds: number): void {
    cooldown = seconds;
    sendButton.disabled = true;
    const tick = () => {
      sendButton.textContent = `重新发送 (${cooldown}s)`;
      if (cooldown <= 0) {
        sendButton.disabled = false;
        sendButton.textContent = "发送验证码";
        if (timer) window.clearInterval(timer);
        return;
      }
      cooldown -= 1;
    };
    tick();
    timer = window.setInterval(tick, 1000);
  }

  sendButton.addEventListener("click", async () => {
    const email = emailInput.value.trim();
    if (!email) {
      showError("请先填写邮箱");
      emailInput.focus();
      return;
    }
    sendButton.disabled = true;
    sendButton.textContent = "发送中…";
    try {
      const payload = await request<{ resend_after: number; debug_code?: string }>(
        "/auth/register/code",
        { json: { email } },
      );
      startCooldown(payload.resend_after || 60);
      if (payload.debug_code) {
        // 本地没配 SMTP：把码直接显示出来，省得用户去翻日志
        codeInput.value = payload.debug_code;
        quotaHint.textContent = `未配置 SMTP，验证码已填入（开发环境）。`;
      } else {
        quotaHint.textContent = "验证码已发送，10 分钟内有效，同一邮箱每小时最多 5 次。";
      }
      codeInput.focus();
    } catch (err) {
      sendButton.disabled = false;
      sendButton.textContent = "发送验证码";
      if (err instanceof ApiError) {
        if (err.code === "CONFLICT") {
          showError("该邮箱已经注册过，直接登录即可。");
        } else if (err.code === "FORBIDDEN") {
          showError(err.message);
        } else if (err.code === "RATE_LIMITED") {
          startCooldown(err.retryAfter ?? 60);
          showError(err.message);
        } else {
          showError(err.message);
        }
        return;
      }
      toastError(err);
    }
  });

  const form = h(
    "form",
    { class: "auth-card auth-card-wide" },
    h(
      "div.auth-head",
      null,
      h(
        "div.auth-brand",
        null,
        brandLockup((site?.name ?? "").trim() || "Pixora"),
      ),
      h("div.auth-eyebrow", { text: "Join the archive" }),
      h("h1.auth-title", { text: "创建账号" }),
      h("div.auth-sub", { text: "注册后即可上传、整理和分享图片。" }),
    ),
    errorBox,
    field("邮箱", emailInput),
    field("邮箱验证码", h("div.row", null, codeInput, sendButton), "10 分钟内有效，同一邮箱每小时最多 5 次。"),
    field("密码", passwordInput.node, "至少 10 位，需同时包含大写字母、小写字母和数字。"),
    field("确认密码", confirmInput.node),
    h("div.qtip.ok", null, h("span.ic", { text: "✓" }), h("div", null, h("b", { text: "注册后即可开始上传" }), quotaHint)),
    submit,
    h(
      "div.tip",
      { style: { textAlign: "center", marginTop: "16px" } },
      "已有账号？",
      h("a", { href: "#/login", style: { color: "var(--primary)", textDecoration: "none" }, text: "去登录" }),
    ),
  );

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    errorBox.style.display = "none";

    if (passwordInput.input.value !== confirmInput.input.value) {
      showError("两次输入的密码不一致");
      return;
    }
    if (passwordInput.input.value.length < 10) {
      showError("密码至少 10 位");
      return;
    }

    submit.disabled = true;
    submit.textContent = "创建中…";
    try {
      const payload = await request<{ uid: string; quota_bytes: number }>("/auth/register", {
        json: {
          email: emailInput.value.trim(),
          code: codeInput.value.trim(),
          password: passwordInput.input.value,
        },
      });
      void payload;

      // 注册即登录态：直接把会话拉进来，省用户再填一次密码
      const user = await loadMe();
      if (user) {
        navigate("/albums", { replace: true });
      } else {
        navigate("/login", { replace: true });
      }
    } catch (err) {
      if (err instanceof ApiError) {
        showError(err.message);
        return;
      }
      toastError(err);
      showError("注册失败，请稍后重试。");
    } finally {
      submit.disabled = false;
      submit.textContent = "创建账号";
    }
  });

  function showError(message: string): void {
    errorBox.replaceChildren(qtip("err", null, message));
    errorBox.style.display = "";
  }

  // 第三方注册：点下去走的是同一套授权，回调里没有账号就直接建一个
  const oauth = oauthButtons(site?.login_methods, "login", "/");
  if (oauth) form.append(oauth);

  ctx.outlet.append(form);
  emailInput.focus();
}
