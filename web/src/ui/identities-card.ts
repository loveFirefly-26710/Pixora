/**
 * 登录方式（账号安全页的一张卡）。
 *
 * 邮箱排在最前面并标「必需」：它是账号本身，不能解绑——这一点必须写在界面上，
 * 否则用户会到处找「解绑邮箱」的按钮。其余方式（GitHub / Google / QQ / 微信 /
 * 手机号）都可以绑与解，但解绑前服务端会确认「还剩别的方式能进得来」。
 */

import { ApiError, request } from "../core/api";
import { h } from "../core/dom";
import { formatDateTime } from "../core/format";
import { confirmDialog } from "../core/modal";
import type { IdentitiesPayload, IdentityView, LoginMethods } from "../core/types";
import { toastError, toastOk } from "../core/toast";
import { oauthStartUrl } from "./login-methods";
import { badge, card, spinner } from "./widgets";

const PROVIDER_ORDER = ["github", "google", "qq", "wechat", "phone"] as const;

const PROVIDER_LABELS: Record<string, string> = {
  github: "GitHub",
  google: "Google",
  qq: "QQ",
  wechat: "微信",
  phone: "手机号",
};

export function loginMethodsCard(reload: () => void, onPasswordNeeded: () => void): HTMLElement {
  const body = h(
    "div",
    { style: { display: "flex", alignItems: "center", gap: "8px", color: "var(--muted)" } },
    spinner(),
    h("span", { text: "正在读取登录方式…" }),
  );
  const node = card("登录方式", body);
  void load(body, reload, onPasswordNeeded);
  return node;
}

async function load(
  body: HTMLElement,
  reload: () => void,
  onPasswordNeeded: () => void,
): Promise<void> {
  let data: IdentitiesPayload;
  try {
    data = await request<IdentitiesPayload>("/me/identities");
  } catch (err) {
    body.replaceChildren(
      h("div.tip", {
        text: err instanceof ApiError ? err.message : "读取失败，请刷新页面重试。",
      }),
    );
    return;
  }

  const bound = new Map<string, IdentityView>();
  for (const item of data.identities) bound.set(item.provider, item);

  const rows: Node[] = [
    // 邮箱：必需，不可解绑
    h(
      "div.idrow",
      null,
      h("div.idmain", null, h("b", { text: "邮箱" }), badge("必需", "ok")),
      h("div.tip", { text: data.email }),
      h("div.tip", {
        text: "邮箱是账号本身：找回密码、接收通知都靠它。可以换，但不能解绑——换邮箱在「账号信息」里。",
      }),
    ),
    // 密码：第三方建号的账号一开始没有口令，想解绑就得先设一个
    h(
      "div.idrow",
      null,
      h(
        "div.idmain",
        null,
        h("b", { text: "密码" }),
        data.password_set ? badge("已设置", "mute") : badge("未设置", "warn"),
      ),
      h("div.tip", {
        text: data.password_set
          ? "已设置。想换密码在「密码」那张卡里改。"
          : "这个账号是用第三方登录建的，还没设过密码。设一个之后，解绑第三方方式也进得来。",
      }),
      data.password_set
        ? null
        : h("button.btn.sm", {
            type: "button",
            text: "去设置密码",
            on: { click: () => onPasswordNeeded() },
          }),
    ),
  ];

  const available: LoginMethods = data.available ?? {
    github: false,
    google: false,
    qq: false,
    wechat: false,
    phone: false,
  };

  for (const provider of PROVIDER_ORDER) {
    const identity = bound.get(provider);
    const enabled = available[provider];
    // 已经绑过的一律列出来（哪怕管理员后来关了这个入口，也得能看见与解绑）
    if (!identity && !enabled) continue;

    const title = h(
      "div.idmain",
      null,
      h("b", { text: PROVIDER_LABELS[provider] ?? provider }),
      identity ? badge("已绑定", "ok") : badge("未绑定", "mute"),
    );

    const detail = identity
      ? h("div.tip", {
          text: `${identity.display ?? identity.provider} · 绑定于 ${formatDateTime(identity.created_at)}`,
        })
      : null;

    const actions = h("div.flexline", { style: { marginTop: "8px" } });
    if (identity) {
      actions.append(
        h("button.btn.sm.dgr", {
          type: "button",
          text: "解绑",
          on: { click: () => void unbind(provider, reload) },
        }),
      );
    } else if (provider === "phone") {
      // 手机号要验证码：点开输入框（不是跳转）
      actions.append(
        h("button.btn.sm", {
          type: "button",
          text: "绑定手机号",
          on: { click: () => void openPhoneBind(reload) },
        }),
      );
    } else {
      // 第三方：整页跳去对方站点确认一次（回来时带着 mode=bind 的会话）
      actions.append(
        h("a.btn.sm", {
          href: oauthStartUrl(provider, "bind", "/account/security"),
          text: "去绑定",
        }),
      );
    }

    rows.push(h("div.idrow", null, title, detail, actions));
  }

  body.replaceChildren(...rows);
}

async function unbind(provider: string, reload: () => void): Promise<void> {
  const label = PROVIDER_LABELS[provider] ?? provider;
  const ok = await confirmDialog({
    title: `解绑 ${label}`,
    message: `解绑后就不能用 ${label} 登录这个账号了（已经登录的会话不受影响）。<br>如果这是你唯一的登录方式，服务端会拒绝——先把密码设好再来。`,
    confirmText: "解绑",
    danger: true,
  });
  if (!ok) return;
  try {
    await request(`/me/identities/${encodeURIComponent(provider)}`, { method: "DELETE" });
    toastOk(`${label} 已解绑`);
    reload();
  } catch (err) {
    toastError(err);
  }
}

/**
 * 绑手机号：填号 → 收码 → 提交。
 *
 * 用弹窗而不是跳页：手机号这一步没有「对方站点授权」那一环，就地做完最省事。
 */
async function openPhoneBind(reload: () => void): Promise<void> {
  const { openModal } = await import("../core/modal");
  const { textInput } = await import("./widgets");
  const { phoneForm } = await import("./login-methods");

  const errorLine = h("div.tip", { style: { color: "var(--danger)" } });
  const form = phoneForm({
    mode: "bind",
    onError: (message) => {
      errorLine.textContent = message;
    },
    onDone: () => {
      toastOk("手机号已绑定");
      handle.close();
      reload();
    },
  });
  if (!form) {
    toastError(new Error("管理员还没开手机号登录"));
    return;
  }
  const handle = openModal({
    title: "绑定手机号",
    body: [form, errorLine],
    dismissable: true,
  });
  void textInput;
}
