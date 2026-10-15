/**
 * 登录。
 *
 * 账号被暂停或封禁时，登录页必须给出明确原因和申诉入口，而不是只说
 * 「登录失败」——这是设计稿点名的三端一致性要求之一。做法是：拿到
 * `ACCOUNT_SUSPENDED` / `ACCOUNT_BANNED` 就整页跳转到 `/suspended`，
 * 其它错误在表单下方就地显示。
 *
 * 二次验证走两步，而不是「提交一次报错、再展开一个输入框」：
 * 邮箱密码那一步照常提交，服务端回 `TOTP_REQUIRED` 时整页翻到第二步，
 * 由用户填动态码（或一枚恢复码）。两步之间可以退回去换账号。
 */

import { request, ApiError } from "../core/api";
import { h, mount } from "../core/dom";
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

  // 第二步：6 位动态码，或一枚恢复码
  const totpInput = textInput({ placeholder: "6 位动态码或恢复码", name: "totp" });
  const step2Note = h("div", { style: { marginBottom: "14px" } });
  const step2Error = h("div", { style: { display: "none", marginBottom: "12px" } });
  const step1Error = h("div", { style: { display: "none", marginBottom: "12px" } });

  const subtitle = h("div.tip", { text: "登录后管理你的相册" });
  const submit = h("button.btn.pri.blk", { type: "submit", text: "登录" });

  const step1 = h(
    "div",
    null,
    step1Error,
    field("邮箱", emailInput),
    field("密码", passwordInput),
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
  );

  // 第二步的两种方式：动态码（默认）或邮箱验证码
  let method: "totp" | "email" = "totp";
  const chips = h("div.flexline", { style: { marginBottom: "12px" } });
  const codeHint = h("div.tip");
  const sendCode = h("button.btn.sm", { type: "button", text: "发送邮箱验证码" });

  const paintMethod = (): void => {
    mount(
      chips,
      ...([["totp", "动态验证码"], ["email", "邮箱验证码"]] as const).map(([value, label]) => {
        const chip = h("button.chip", {
          type: "button",
          class: method === value ? "on" : "",
          text: label,
        });
        chip.addEventListener("click", () => {
          if (method === value) return;
          method = value;
          totpInput.value = "";
          paintMethod();
          totpInput.focus();
        });
        return chip;
      }),
    );
    codeHint.textContent =
      method === "email"
        ? "点「发送邮箱验证码」，把收到的 6 位数字填进来。"
        : "打开验证器 App 读当前那 6 位；恢复码也能用，一枚只能用一次。";
    sendCode.style.display = method === "email" ? "" : "none";
    totpInput.placeholder = method === "email" ? "6 位邮箱验证码" : "6 位动态码或恢复码";
  };

  sendCode.addEventListener("click", async () => {
    sendCode.disabled = true;
    sendCode.textContent = "发送中…";
    try {
      const result = await request<{ message: string }>("/auth/login/email-code", {
        method: "POST",
        json: { email: emailInput.value.trim() },
      });
      mount(step2Note, qtip("info", null, result.message));
      totpInput.focus();
    } catch (err) {
      mount(step2Note, qtip("err", null, err instanceof ApiError ? err.message : "验证码没能发出去，请稍后重试。"));
    } finally {
      sendCode.disabled = false;
      sendCode.textContent = "发送邮箱验证码";
    }
  });

  const step2 = h(
    "div",
    { style: { display: "none" } },
    step2Error,
    step2Note,
    chips,
    field(
      "验证码",
      h("div.row", { style: { margin: "0" } }, totpInput, sendCode),
      undefined,
    ),
    codeHint,
  );
  paintMethod();

  const back = h("button.btn.blk", {
    type: "button",
    text: "返回上一步",
    style: { marginTop: "8px" },
  });
  back.addEventListener("click", () => {
    totpInput.value = "";
    showStep(1);
    passwordInput.focus();
  });

  const backRow = h("div", { style: { display: "none" } }, back);
  const registerRow = h(
    "div.flexline",
    { style: { justifyContent: "flex-end", marginTop: "16px" } },
    h("span", { style: { color: "var(--muted)", fontSize: "12.5px" }, text: "还没有账号？" }),
    h("a.btn", { href: "#/register", text: "注册" }),
  );
  const firstStepTail = h(
    "div",
    null,
    registerRow,
    site && !site.registration_open
      ? qtip(
          "warn",
          "注册入口当前关闭",
          site.registration_closed_reason ?? "系统容量已达上限，暂停新用户注册。已注册用户不受影响。",
        )
      : qtip("info", null, "账号被管理员暂停或封禁时，这里会给出明确原因和申诉入口。"),
  );

  const form = h(
    "form",
    { style: { maxWidth: "420px", margin: "0 auto", padding: "38px 18px" } },
    h(
      "div",
      { style: { textAlign: "center", marginBottom: "22px" } },
      h("div.logo", { style: { fontSize: "20px" }, html: "Pi<em>x</em>ora" }),
      subtitle,
    ),
    step1,
    step2,
    submit,
    // 返回按钮归第二步，注册入口与提示条归第一步，各自收放
    backRow,
    firstStepTail,
  );

  let step: 1 | 2 = 1;

  /** 翻到第几步。第二步把「填动态码」摆成正常的一步，不用错误提示吓人。 */
  function showStep(which: 1 | 2): void {
    step = which;
    step1.style.display = which === 1 ? "" : "none";
    step2.style.display = which === 2 ? "" : "none";
    backRow.style.display = which === 2 ? "" : "none";
    firstStepTail.style.display = which === 1 ? "" : "none";
    subtitle.textContent = which === 1 ? "登录后管理你的相册" : "再填一步动态验证码";
    step1Error.style.display = "none";
    step2Error.style.display = "none";
  }

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const code = totpInput.value.trim();
    if (step === 2 && !code) {
      showError(step2Error, "请输入动态验证码，或改用一枚恢复码。");
      totpInput.focus();
      return;
    }

    step1Error.style.display = "none";
    step2Error.style.display = "none";
    submit.disabled = true;
    submit.textContent = "登录中…";

    try {
      await request<{ user: { status: string } }>("/auth/login", {
        json: {
          email: emailInput.value.trim(),
          password: passwordInput.value,
          code_method: method,
          // 两种码都从 `totp` 字段走，服务端按 code_method 决定怎么验；
          // 动态码那一路非 6 位的按恢复码发
          totp: code && (method === "email" || /^\d{6}$/.test(code)) ? code : undefined,
          recovery_code:
            method === "totp" && code && !/^\d{6}$/.test(code) ? code : undefined,
          remember: remember.checked,
        },
      });

      // 登录成功：把会话状态拉进来再跳转，避免目标页面闪一下未登录
      await loadMe();
      navigate(next.startsWith("/") ? next : "/", { replace: true });
    } catch (err) {
      if (err instanceof ApiError) {
        if (err.code === "TOTP_REQUIRED") {
          // 密码是对的，只是还差一步。摆成正常的一步，不显示成错误
          showStep(2);
          mount(
            step2Note,
            qtip(
              "info",
              "该账号已开启二次验证",
              `正在登录 ${emailInput.value.trim()}，输入验证器 App 上的 6 位动态码即可。`,
            ),
          );
          totpInput.focus();
          return;
        }
        if (err.needsAccountNotice) {
          navigate(`/suspended?email=${encodeURIComponent(emailInput.value.trim())}`, { replace: true });
          return;
        }
        if (err.code === "ACCOUNT_LOCKED") {
          const until = err.body.locked_until as string | undefined;
          showError(
            step === 2 ? step2Error : step1Error,
            until
              ? `连续登录失败次数过多，账号已锁定到 ${new Date(until).toLocaleString("zh-CN")}。`
              : "连续登录失败次数过多，账号已被临时锁定。",
          );
          return;
        }
        if (err.code === "IP_BLOCKED") {
          showError(step === 2 ? step2Error : step1Error, "你的网络地址已被拒绝访问，请更换网络或联系管理员。");
          return;
        }
        if (err.code === "RATE_LIMITED") {
          showError(
            step === 2 ? step2Error : step1Error,
            `${err.message}${err.retryAfter ? `（${err.retryAfter} 秒后可重试）` : ""}`,
          );
          return;
        }
        showError(step === 2 ? step2Error : step1Error, err.message);
        return;
      }
      toastError(err);
      showError(step === 2 ? step2Error : step1Error, "登录失败，请稍后重试。");
    } finally {
      submit.disabled = false;
      submit.textContent = "登录";
    }
  });

  function showError(box: HTMLElement, message: string): void {
    box.replaceChildren(qtip("err", null, message));
    box.style.display = "";
  }

  showStep(1);
  ctx.outlet.append(form);
  emailInput.focus();
}
