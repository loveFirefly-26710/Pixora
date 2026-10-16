/**
 * 登录方式（账号安全页的一张卡）。
 *
 * 邮箱排在最前面并标「必需」：它是账号本身，不能解绑——这一点必须写在界面上，
 * 否则用户会到处找「解绑邮箱」的按钮。其余方式（GitHub / Google / QQ / 微信 /
 * 手机号）都可以绑与解，但解绑前服务端会确认「还剩别的方式能进得来」。
 */

import { ApiError, request } from "../core/api";
import { h } from "../core/dom";
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

  // 一格一行：登录方式 + 状态 + 账号名（没绑就没有这一行）+ 操作。
  // 说明性的段落一律不摆：卡片本身就是一张「我有哪些门能进来」的清单，
  // 摆满解释反而看不出重点
  const rows: Node[] = [
    h(
      "div.idrow",
      null,
      h("div.idmain", null, h("b", { text: "邮箱" }), badge("必需", "ok")),
      h("div.idvalue.mono", { text: data.email }),
      // 第三方登录建的账号一开始没有口令，想解绑第三方方式得先能靠邮箱进来
      data.password_set
        ? null
        : h(
            "div.idactions",
            null,
            h("button.btn.sm", {
              type: "button",
              text: "设置密码",
              on: { click: () => onPasswordNeeded() },
            }),
          ),
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

    const actions = h("div.idactions");
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

    rows.push(
      h(
        "div.idrow",
        null,
        h(
          "div.idmain",
          null,
          h("b", { text: PROVIDER_LABELS[provider] ?? provider }),
          identity ? badge("已绑定", "ok") : badge("未绑定", "mute"),
        ),
        // 没绑定就没有账号名可显示
        identity ? h("div.idvalue.mono", { text: identity.display ?? identity.provider }) : null,
        actions,
      ),
    );
  }

  body.replaceChildren(h("div.idgrid", null, ...rows));
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
