/**
 * 登录。
 *
 * 账号被暂停或封禁时，登录页必须给出明确原因和申诉入口，而不是只说
 * 「登录失败」——这是设计稿点名的三端一致性要求之一。这里的做法是：
 * 拿到 `ACCOUNT_SUSPENDED` / `ACCOUNT_BANNED` 就整页跳转到 `/suspended`，
 * 其它错误在表单下方就地显示。
 *
 * TOTP：开启了二次验证的账号第一次提交会拿到 `TOTP_REQUIRED`，
 * 此时就地展开动态码输入框，而不是把用户丢回上一步。
 */

import { request, ApiError } from "../core/api";
import { h } from "../core/dom";
import { navigate, type PageContext } from "../core/router";
import { getState, loadMe } from "../core/state";
import { toastError } from "../core/toast";
import { field, qtip, textInput } from "../ui/widgets";

export async function render(ctx: PageContext): Promise<void> {
  const next = ctx.query.get("next") ?? "/";
  const site = getState().site;

  const emailInput = textInput({ placeholder: "chenyu@example.com", type: "email" });
  const passwordInput = textInput({ type: "password", placeholder: "至少 10 位" });
  const remember = h("input", { type: "checkbox", checked: true });

  // 二次验证的输入框默认隐藏，收到 TOTP_REQUIRED 才展开
  const totpInput = textInput({ placeholder: "6 位动态码", name: "totp" });
  const totpHint = h("div.tip", {
    text: "打开验证器 App 输入当前动态码；也可以用一枚恢复码代替。",
  });
  const totpField = h("div.fld", { style: { display: "none" } }, h("label.f", { text: "动态验证码" }), totpInput, totpHint);

  const errorBox = h("div", { style: { display: "none", marginBottom: "12px" } });
  const submit = h("button.btn.pri.blk", { type: "submit", text: "登录" });

  const form = h(
    "form",
    { style: { maxWidth: "420px", margin: "0 auto", padding: "38px 18px" } },
    h(
      "div",
      { style: { textAlign: "center", marginBottom: "22px" } },
      h("div.logo", { style: { fontSize: "20px" }, html: "Pi<em>x</em>ora" }),
      h("div.tip", { text: "登录后管理你的相册" }),
    ),
    errorBox,
    field("邮箱", emailInput),
    field("密码", passwordInput),
    totpField,
    h(
      "div.flexline",
      { style: { justifyContent: "space-between", marginBottom: "16px", fontSize: "12.5px" } },
      h(
        "label",
        { style: { display: "flex", alignItems: "center", gap: "6px", color: "#4b5563" } },
        remember,
        h("span", { text: "记住我" }),
      ),
      h("a", {
        href: "#/reset-password",
        style: { color: "var(--primary)", textDecoration: "none" },
        text: "忘记密码？",
      }),
    ),
    submit,
    // 注册入口放在右下角：游客只剩「登录」一个入口，注册从这里进
    h(
      "div.flexline",
      { style: { justifyContent: "flex-end", marginTop: "16px" } },
      h("span", { style: { color: "var(--muted)", fontSize: "12.5px" }, text: "还没有账号？" }),
      h("a.btn", { href: "#/register", text: "注册" }),
    ),
    site && !site.registration_open
      ? qtip(
          "warn",
          "注册入口当前关闭",
          site.registration_closed_reason ?? "系统容量已达上限，暂停新用户注册。已注册用户不受影响。",
        )
      : qtip("info", null, "账号被管理员暂停或封禁时，这里会给出明确原因和申诉入口。"),
  );

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    errorBox.style.display = "none";
    submit.disabled = true;
    submit.textContent = "登录中…";

    try {
      const payload = await request<{ user: { status: string } }>("/auth/login", {
        json: {
          email: emailInput.value.trim(),
          password: passwordInput.value,
          totp: totpInput.value.trim() || undefined,
          remember: remember.checked,
        },
      });
      void payload;

      // 登录成功：把会话状态拉进来再跳转，避免目标页面闪一下未登录
      await loadMe();
      navigate(next.startsWith("/") ? next : "/", { replace: true });
    } catch (err) {
      if (err instanceof ApiError) {
        if (err.code === "TOTP_REQUIRED") {
          // 就地展开动态码输入，不把用户丢回上一步
          totpField.style.display = "";
          totpHint.textContent = "打开验证器 App 输入当前动态码；也可以用一枚恢复码代替。";
          totpInput.placeholder = "6 位动态码或恢复码";
          totpInput.focus();
          showError("该账号已开启二次验证，请输入动态验证码。");
          return;
        }
        if (err.needsAccountNotice) {
          navigate(`/suspended?email=${encodeURIComponent(emailInput.value.trim())}`, { replace: true });
          return;
        }
        if (err.code === "ACCOUNT_LOCKED") {
          const until = err.body.locked_until as string | undefined;
          showError(
            until
              ? `连续登录失败次数过多，账号已锁定到 ${new Date(until).toLocaleString("zh-CN")}。`
              : "连续登录失败次数过多，账号已被临时锁定。",
          );
          return;
        }
        if (err.code === "IP_BLOCKED") {
          showError("你的网络地址已被拒绝访问，请更换网络或联系管理员。");
          return;
        }
        if (err.code === "RATE_LIMITED") {
          showError(`${err.message}${err.retryAfter ? `（${err.retryAfter} 秒后可重试）` : ""}`);
          return;
        }
        showError(err.message);
        return;
      }
      toastError(err);
      showError("登录失败，请稍后重试。");
    } finally {
      submit.disabled = false;
      submit.textContent = "登录";
    }
  });

  function showError(message: string): void {
    errorBox.replaceChildren(qtip("err", null, message));
    errorBox.style.display = "";
  }

  ctx.outlet.append(form);
  emailInput.focus();
}
