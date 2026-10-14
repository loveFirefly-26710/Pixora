/**
 * 找回密码。
 *
 * 与注册同样的两步：发码 → 换密码。区别是这里**不给「邮箱是否注册过」的线索**：
 * 服务端对不存在的邮箱也返回成功，页面上只说「如果该邮箱已注册，验证码已发出」。
 */

import { request, ApiError } from "../core/api";
import { h } from "../core/dom";
import { navigate, type PageContext } from "../core/router";
import { toastError } from "../core/toast";
import { field, qtip, textInput } from "../ui/widgets";

export async function render(ctx: PageContext): Promise<void> {
  const emailInput = textInput({ placeholder: "chenyu@example.com", type: "email" });
  const codeInput = textInput({ placeholder: "6 位数字", name: "code" });
  const passwordInput = textInput({ type: "password", placeholder: "至少 10 位，含大小写与数字" });
  const confirmInput = textInput({ type: "password", placeholder: "再输入一次" });

  const sendButton = h("button.btn", {
    type: "button",
    text: "发送验证码",
    style: { flex: "0 0 132px" },
  });
  const errorBox = h("div", { style: { display: "none", marginBottom: "12px" } });
  const infoBox = h("div", { style: { display: "none", marginBottom: "12px" } });
  const submit = h("button.btn.pri.blk", { type: "submit", text: "重置密码" });

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
        "/auth/password/code",
        { json: { email } },
      );
      startCooldown(payload.resend_after || 60);
      if (payload.debug_code) {
        codeInput.value = payload.debug_code;
        showInfo("未配置 SMTP，验证码已填入（开发环境）。");
      } else {
        // 刻意不确认「该邮箱已注册」——这是防账号探测的基本要求
        showInfo("如果该邮箱已注册，验证码已发出，10 分钟内有效。");
      }
      codeInput.focus();
    } catch (err) {
      sendButton.disabled = false;
      sendButton.textContent = "发送验证码";
      if (err instanceof ApiError) {
        if (err.code === "RATE_LIMITED") {
          startCooldown(err.retryAfter ?? 60);
          showError(err.message);
          return;
        }
        showError(err.message);
        return;
      }
      toastError(err);
    }
  });

  const form = h(
    "form",
    { style: { maxWidth: "440px", margin: "0 auto", padding: "34px 18px" } },
    h(
      "div",
      { style: { textAlign: "center", marginBottom: "20px" } },
      h("div.logo", { style: { fontSize: "19px" }, html: "Pi<em>x</em>ora" }),
      h("div.tip", { text: "重置密码" }),
    ),
    errorBox,
    infoBox,
    field("邮箱", emailInput),
    field("邮箱验证码", h("div.row", null, codeInput, sendButton), "10 分钟内有效。"),
    field("新密码", passwordInput, "至少 10 位，需同时包含大写字母、小写字母和数字。"),
    field("确认新密码", confirmInput),
    submit,
    qtip(
      "warn",
      "重置成功后所有设备会被踢下线",
      "密码都泄露了，旧会话不能继续留着。请用新密码重新登录。",
    ),
    h(
      "div.tip",
      { style: { textAlign: "center", marginTop: "16px" } },
      h("a", { href: "#/login", style: { color: "var(--primary)", textDecoration: "none" }, text: "返回登录" }),
    ),
  );

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    errorBox.style.display = "none";

    if (passwordInput.value !== confirmInput.value) {
      showError("两次输入的密码不一致");
      return;
    }

    submit.disabled = true;
    submit.textContent = "提交中…";
    try {
      await request("/auth/password/reset", {
        json: {
          email: emailInput.value.trim(),
          code: codeInput.value.trim(),
          password: passwordInput.value,
        },
      });
      showInfo("密码已重置，所有设备已下线。正在跳转登录…");
      window.setTimeout(() => navigate("/login", { replace: true }), 1200);
    } catch (err) {
      if (err instanceof ApiError) {
        showError(err.message);
        return;
      }
      toastError(err);
    } finally {
      submit.disabled = false;
      submit.textContent = "重置密码";
    }
  });

  function showError(message: string): void {
    infoBox.style.display = "none";
    errorBox.replaceChildren(qtip("err", null, message));
    errorBox.style.display = "";
  }

  function showInfo(message: string): void {
    errorBox.style.display = "none";
    infoBox.replaceChildren(qtip("ok", null, message));
    infoBox.style.display = "";
  }

  ctx.outlet.append(form);
  emailInput.focus();
}
