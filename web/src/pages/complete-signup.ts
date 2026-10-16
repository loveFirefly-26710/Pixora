/**
 * 补全邮箱（`#/complete-signup`）。
 *
 * 走第三方登录时，对方不一定给邮箱（QQ、微信从来不给，GitHub 也可能不给）。
 * 而邮箱是账号本身：必需、不可解绑、也是找回账号的凭据，所以不能拿一句话充数——
 * 这里让用户填一个，并且照样发一次注册验证码验一遍。
 *
 * 令牌背在 URL 上（`?ticket=`），里面是「哪个平台的哪个身份」，签名保证没被改过。
 */

import { ApiError, request } from "../core/api";
import { h } from "../core/dom";
import { navigate, type PageContext } from "../core/router";
import { getState, loadMe } from "../core/state";
import { toastError, toastOk } from "../core/toast";
import { field, qtip, textInput } from "../ui/widgets";

export async function render(ctx: PageContext): Promise<void> {
  const ticket = ctx.query.get("ticket") ?? "";
  const providerName = ctx.query.get("provider") ?? "";
  const site = getState().site;

  const emailInput = textInput({ placeholder: "you@example.com", type: "email", name: "email" });
  const codeInput = textInput({ placeholder: "6 位验证码", name: "code" });
  codeInput.setAttribute("inputmode", "numeric");
  codeInput.setAttribute("maxlength", "6");

  const send = h("button.btn.gho.sm", { type: "button", text: "发送验证码" });
  const status = h("div.tip", { style: { margin: "6px 0 0" } });
  const errorBox = h("div", { style: { display: "none", marginBottom: "12px" } });
  const submit = h("button.btn.pri.blk", { type: "submit", text: "完成注册" });

  const label = providerName ? `（来自 ${providerName}）` : "";

  if (!ticket) {
    ctx.outlet.append(
      h(
        "div",
        { style: { maxWidth: "420px", margin: "0 auto", padding: "38px 18px" } },
        qtip("err", "链接不完整", "这次登录的凭据没带上，请回到登录页重新发起。"),
        h(
          "div.flexline",
          { style: { justifyContent: "center", marginTop: "12px" } },
          h("a.btn.pri", { href: "#/login", text: "回到登录页" }),
        ),
      ),
    );
    return;
  }

  send.addEventListener("click", async () => {
    const email = emailInput.value.trim();
    if (!email) {
      showError("先填邮箱");
      emailInput.focus();
      return;
    }
    send.disabled = true;
    send.textContent = "发送中…";
    status.textContent = "";
    try {
      // 复用注册发码：这一步要验的还是「这个邮箱是不是你的」
      await request("/auth/register/code", { json: { email } });
      status.textContent = "验证码已发到该邮箱，10 分钟内有效";
      codeInput.focus();
    } catch (err) {
      status.textContent = err instanceof ApiError ? err.message : "验证码没发出去，请稍后重试";
    } finally {
      send.disabled = false;
      send.textContent = "发送验证码";
    }
  });

  const form = h(
    "form",
    { style: { maxWidth: "420px", margin: "0 auto", padding: "38px 18px" } },
    h(
      "div",
      { style: { textAlign: "center", marginBottom: "22px" } },
      h("div.logo", { style: { fontSize: "20px" }, html: "Pi<em>x</em>ora" }),
      h("div.tip", { text: `还差一个邮箱就能建号${label}` }),
    ),
    errorBox,
    qtip(
      "info",
      "为什么要填邮箱",
      "邮箱是账号本身：找回密码、接收通知都靠它，所以本站要求每个账号都有且不可解绑。",
    ),
    field("邮箱", emailInput),
    h(
      "div",
      { style: { display: "flex", gap: "8px", alignItems: "flex-start" } },
      h("div", { style: { flex: "1" } }, field("验证码", codeInput)),
      h("div", { style: { paddingTop: "22px" } }, send),
    ),
    status,
    submit,
    h(
      "div.flexline",
      { style: { justifyContent: "center", marginTop: "14px" } },
      h("a", { href: "#/login", style: { color: "var(--primary)" }, text: "换一种方式登录" }),
    ),
  );

  function showError(message: string): void {
    errorBox.replaceChildren(qtip("err", null, message));
    errorBox.style.display = "";
  }

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const email = emailInput.value.trim();
    const code = codeInput.value.trim();
    if (!email || code.length !== 6) {
      showError("邮箱和 6 位验证码都要填");
      return;
    }
    submit.disabled = true;
    submit.textContent = "创建中…";
    try {
      await request("/auth/oauth/complete", { json: { ticket, email, code } });
      await loadMe();
      toastOk("账号已创建，欢迎");
      navigate("/", { replace: true });
    } catch (err) {
      if (err instanceof ApiError) {
        showError(err.message);
        return;
      }
      toastError(err);
      showError("没成功，请稍后重试");
    } finally {
      submit.disabled = false;
      submit.textContent = "完成注册";
    }
  });

  void site;
  ctx.outlet.append(form);
  emailInput.focus();
}
