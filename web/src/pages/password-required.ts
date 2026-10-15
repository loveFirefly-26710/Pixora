/**
 * 必须改密码（`#/password-required`）。
 *
 * 管理员直接给账号设了新密码时，密码是经邮件送达的——可能落在聊天记录、
 * 转发的截图里。所以那个密码只能用来登录一次：改掉之前，服务端只放行
 * `/me` 与改密码本身，前端也把人钉在这一页。
 *
 * 与「安全」页的改密码共用同一个接口，多出来的一件事只是：改完把标记清掉，
 * 服务端在 `change_password` 里做。
 */

import { ApiError, request } from "../core/api";
import { h, mount } from "../core/dom";
import { navigate, type PageContext } from "../core/router";
import { getState, loadMe } from "../core/state";
import { toastError } from "../core/toast";
import { field, qtip, textInput } from "../ui/widgets";

export async function render(ctx: PageContext): Promise<void> {
  const user = getState().user;
  const currentInput = textInput({ type: "password", placeholder: "邮件里那个密码" });
  const nextInput = textInput({ type: "password", placeholder: "至少 10 位，含大小写与数字" });
  const repeatInput = textInput({ type: "password", placeholder: "再输入一次" });
  const errorBox = h("div", { style: { display: "none", marginBottom: "12px" } });
  const submit = h("button.btn.pri.blk", { type: "submit", text: "保存新密码" });
  const stateLine = h("div.tip", { style: { textAlign: "center" } });

  const form = h(
    "form",
    { style: { maxWidth: "440px", margin: "0 auto", padding: "34px 18px" } },
    h(
      "div",
      { style: { textAlign: "center", marginBottom: "20px" } },
      h("div.logo", { style: { fontSize: "19px" }, html: "Pi<em>x</em>ora" }),
      h("div.tip", { text: "设置你的新密码" }),
    ),
    errorBox,
    qtip(
      "info",
      "管理员重置了你的密码",
      `${user?.email ?? "这个账号"} 的密码由管理员直接设置过，那份密码是经邮件送达的：` +
        "请设置一个仅你本人知晓的新密码；完成之前，账号仅可访问本页。",
    ),
    field("当前密码", currentInput, "即邮件中收到的那一个"),
    field("新密码", nextInput, "至少 10 位，需同时包含大写字母、小写字母和数字。"),
    field("确认新密码", repeatInput),
    submit,
    stateLine,
    h(
      "div.tip",
      { style: { textAlign: "center", marginTop: "16px" } },
      h("a", {
        href: "#/account/security",
        style: { color: "var(--primary)", textDecoration: "none" },
        text: "前往账号安全设置",
      }),
    ),
  );

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    errorBox.style.display = "none";

    if (!currentInput.value) {
      showError("请填写当前密码（邮件中收到的那一个）");
      currentInput.focus();
      return;
    }
    if (nextInput.value !== repeatInput.value) {
      showError("两次输入的新密码不一致");
      repeatInput.focus();
      return;
    }
    if (nextInput.value === currentInput.value) {
      showError("新密码不能与当前密码相同");
      nextInput.focus();
      return;
    }

    submit.disabled = true;
    submit.textContent = "保存中…";
    try {
      await request("/me/password", {
        json: { current: currentInput.value, password: nextInput.value },
      });
      // 标记由服务端清掉，重拉一次身份才知道「现在能正常用了」
      await loadMe();
      mount(stateLine, qtip("ok", null, "新密码已生效，正在进入…"));
      window.setTimeout(() => navigate("/", { replace: true }), 900);
    } catch (err) {
      if (err instanceof ApiError) {
        showError(err.message);
      } else {
        toastError(err);
        showError("保存失败，请稍后重试。");
      }
    } finally {
      submit.disabled = false;
      submit.textContent = "保存新密码";
    }
  });

  function showError(message: string): void {
    errorBox.replaceChildren(qtip("err", null, message));
    errorBox.style.display = "";
  }

  ctx.outlet.append(form);
  currentInput.focus();
}
