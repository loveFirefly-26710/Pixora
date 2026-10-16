/**
 * 第三方登录与手机号验证码。
 *
 * 登录页、注册页、账号安全页三处都要用，收在一处免得三份各漂一点。
 *
 * 按钮只画 `/site` 里启用且凭据配全的那几个：摆一个点下去报「没配置」的按钮，
 * 比不摆更让人困惑。第三方授权是整页跳转（`<a href>` 到后端，后端 302 去对方
 * 站点），所以这里不需要任何 fetch。
 */

import { request } from "../core/api";
import { apiUrl } from "../core/config";
import { h } from "../core/dom";
import type { LoginMethods } from "../core/types";
import { toastError } from "../core/toast";
import { qtip, textInput } from "./widgets";

/** 后端发起授权的地址（`mode=bind` 时要带上登录态）。 */
export function oauthStartUrl(provider: string, mode: "login" | "bind", next: string): string {
  const params = new URLSearchParams({ mode, next });
  // 这是一个整页跳转的入口：跨域部署时必须指向后端，否则静态托管那边没有这个路由
  return apiUrl(`/api/v1/auth/oauth/${provider}/start?${params.toString()}`);
}

const PROVIDER_LABELS: Record<string, string> = {
  github: "GitHub",
  google: "Google",
  qq: "QQ",
  wechat: "微信",
};

/**
 * 第三方登录按钮组。一个都没启用时返回 `null`（调用方直接不摆这一块）。
 *
 * `mode=bind` 时按钮文案改成「绑定」，用于账号安全页。登录页上「扫码」那两家
 * （微信、QQ）由 `scanPanel` 出二维码，这里就不再重复摆一份按钮——`skipQr`
 * 就是给登录页用的。
 */
export function oauthButtons(
  methods: LoginMethods | null | undefined,
  mode: "login" | "bind",
  next: string,
  options: { skipQr?: boolean } = {},
): HTMLElement | null {
  if (!methods) return null;
  const candidates = options.skipQr
    ? (["github", "google"] as const)
    : (["github", "google", "qq", "wechat"] as const);
  const enabled = candidates.filter((name) => methods[name]);
  if (enabled.length === 0) return null;

  const buttons = enabled.map((name) =>
    h("a.btn.oauth-btn", {
      href: oauthStartUrl(name, mode, next),
      text: mode === "bind" ? `绑定 ${PROVIDER_LABELS[name]}` : `用 ${PROVIDER_LABELS[name]} 登录`,
    }),
  );

  return h(
    "div.oauth-box",
    null,
    h("div.oauth-sep", null, h("span", { text: mode === "bind" ? "或用第三方账号绑定" : "或用第三方账号登录" })),
    h("div.oauth-grid", null, ...buttons),
  );
}

/** 手机号 + 验证码登录 / 绑定。手机号没启用时返回 `null`。 */
export function phoneForm(
  hooks: {
    /** 拿到会话或绑定成功之后：调用方决定跳哪儿、刷什么 */
    onDone: () => Promise<void> | void;
    /** 就地显示错误（不弹 toast 的场合），不给就用 toast */
    onError?: (message: string) => void;
    /** 登录还是绑定：决定打哪个接口 */
    mode: "login" | "bind";
  },
): HTMLElement | null {
  const site = (window as unknown as { __pixoraSite?: LoginMethods }).__pixoraSite;
  void site;
  const phoneInput = textInput({ placeholder: "13800138000", type: "tel", name: "phone" });
  const codeInput = textInput({ placeholder: "6 位验证码", name: "phone-code" });
  codeInput.setAttribute("inputmode", "numeric");
  codeInput.setAttribute("maxlength", "6");

  const send = h("button.btn.gho.sm", { type: "button", text: "发送验证码" });
  const status = h("div.tip", { style: { margin: "6px 0 0" } });
  const submit = h("button.btn.pri", {
    type: "button",
    // 绑定走的是「我的登录方式」，登录走的是 /auth/phone/login
    text: hooks.mode === "bind" ? "绑定手机号" : "用手机号登录",
  });

  const fail = (message: string): void => {
    if (hooks.onError) hooks.onError(message);
    else toastError(new Error(message));
  };

  send.addEventListener("click", async () => {
    const phone = phoneInput.value.trim();
    if (!phone) {
      fail("先填手机号");
      phoneInput.focus();
      return;
    }
    send.disabled = true;
    send.textContent = "发送中…";
    status.textContent = "";
    try {
      const path = hooks.mode === "bind" ? "/me/identities/phone/code" : "/auth/phone/code";
      const result = await request<{ message?: string }>(path, { json: { phone } });
      status.textContent = result.message ?? "验证码已发送";
      codeInput.focus();
    } catch (err) {
      status.textContent = err instanceof Error ? err.message : "验证码没发出去，请稍后重试";
    } finally {
      send.disabled = false;
      send.textContent = "发送验证码";
    }
  });

  submit.addEventListener("click", async () => {
    const phone = phoneInput.value.trim();
    const code = codeInput.value.trim();
    if (!phone || code.length !== 6) {
      fail("手机号和 6 位验证码都要填");
      return;
    }
    submit.disabled = true;
    submit.textContent = "处理中…";
    try {
      if (hooks.mode === "bind") {
        // 绑定是在已登录状态下做的，走带 CSRF 的写接口
        await request("/me/identities", { method: "POST", json: { provider: "phone", phone, code } });
      } else {
        await request("/auth/phone/login", {
          json: { phone, code, remember: false },
        });
      }
      await hooks.onDone();
    } catch (err) {
      fail(err instanceof Error ? err.message : "没成功，请稍后重试");
    } finally {
      submit.disabled = false;
      submit.textContent = hooks.mode === "bind" ? "绑定手机号" : "用手机号登录";
    }
  });

  return h(
    "div.oauth-box",
    null,
    h("div.oauth-sep", null, h("span", { text: hooks.mode === "bind" ? "绑定手机号" : "或用手机号验证码登录" })),
    h(
      "div",
      { style: { display: "grid", gap: "8px" } },
      phoneInput,
      h("div", { style: { display: "flex", gap: "8px", flexWrap: "wrap" } }, codeInput, send),
      status,
      submit,
    ),
  );
}

/**
 * 扫码登录（微信 / QQ）。
 *
 * 把对方的授权页嵌进 iframe：微信的 `qrconnect` 与 QQ 的登录页本来就是
 * 「给人扫的二维码」，扫完由对方把结果推给我们的回调（`embed=1` 让回调去
 * 带走顶层窗口，而不是把站内页面塞进这一小块框里）。
 *
 * 对方要是拒绝被嵌（有些网络环境下会），下面的「在新窗口打开」是退路。
 */
export function scanPanel(
  methods: LoginMethods | null | undefined,
  next: string,
): HTMLElement | null {
  const available = (["wechat", "qq"] as const).filter((name) => methods?.[name]);
  if (available.length === 0) return null;

  let current: "wechat" | "qq" = available[0];
  const frameHost = h("div.qr-frame");
  const chips = h("div.flexline", { style: { justifyContent: "center", marginBottom: "10px" } });
  const fallback = h("a", { style: { color: "var(--primary)" }, text: "在新窗口打开" });

  const paintChips = (): void => {
    chips.replaceChildren(
      ...available.map((name) => {
        const chip = h("button.chip", {
          type: "button",
          class: current === name ? "on" : "",
          text: PROVIDER_LABELS[name],
        });
        chip.addEventListener("click", () => {
          if (current === name) return;
          current = name;
          paint();
        });
        return chip;
      }),
    );
  };

  const paint = (): void => {
    paintChips();
    const embed = apiUrl(
      `/api/v1/auth/oauth/${current}/start?mode=login&embed=1&next=${encodeURIComponent(next)}`,
    );
    frameHost.replaceChildren(
      h("iframe.qr-iframe", { src: embed, title: `${PROVIDER_LABELS[current]} 扫码登录` }),
    );
    fallback.setAttribute("href", oauthStartUrl(current, "login", next));
    fallback.setAttribute("target", "_blank");
    fallback.setAttribute("rel", "noopener");
  };
  paint();

  return h(
    "div.oauth-box",
    null,
    available.length > 1 ? chips : null,
    frameHost,
    h("div.tip", {
      style: { textAlign: "center", marginTop: "10px" },
      text: "用手机上的对应 App 扫码，在手机上确认即可登录",
    }),
    h("div.tip", { style: { textAlign: "center", margin: "4px 0 0" } }, fallback),
  );
}

/** 手机号 / 第三方登录都没启用时不摆那一块，返回 `null`。 */
export function hasAnyLoginMethod(methods: LoginMethods | null | undefined): boolean {
  if (!methods) return false;
  return methods.github || methods.google || methods.qq || methods.wechat || methods.phone;
}

/** 登录页顶部那条「上一次授权出了什么问题」的提示。 */
export function oauthErrorTip(message: string | null): HTMLElement | null {
  if (!message) return null;
  return qtip("err", "第三方登录没成", message);
}
