/**
 * 后台「登录方式」配置（系统设置里的一张卡）。
 *
 * 每个第三方平台一套：开关 + 客户端 ID + 客户端密钥，另外把**回调地址**直接
 * 摆在旁边让人复制——在对方平台登记时填错一个字符，回来就是一句「授权失败」，
 * 而排查起来毫无线索。
 *
 * 密钥是加密存储的，界面只显示「已配置」；输入框留空表示不改。
 */

import { request } from "../core/api";
import { h } from "../core/dom";
import { toastError, toastOk } from "../core/toast";
import { card, cardHead, cardNode, field, switchRow, textInput } from "../ui/widgets";

interface ProviderState {
  label: string;
  enabled: boolean;
  client_id: string;
  has_secret: boolean;
  callback: string;
}

export interface LoginMethodsAdmin {
  providers: Record<string, ProviderState>;
  phone: { enabled: boolean };
  sms: {
    enabled: boolean;
    access_key_id: string;
    has_secret: boolean;
    sign_name: string;
    template_code: string;
  };
}

interface OAuthEndpointHealth {
  ok: boolean;
  status: number | null;
  error: string | null;
}

interface OAuthHealth {
  proxy_configured: boolean;
  proxy_source: string | null;
  proxy_url: string | null;
  google: OAuthEndpointHealth;
  github: OAuthEndpointHealth;
}

interface Draft {
  providers: Record<string, { enabled: boolean; clientId: HTMLInputElement; secret: HTMLInputElement }>;
  phoneEnabled: boolean;
  smsEnabled: boolean;
  smsKeyId: HTMLInputElement;
  smsSecret: HTMLInputElement;
  smsSign: HTMLInputElement;
  smsTemplate: HTMLInputElement;
}

/** 登录方式那张卡的草稿：读一次配置，之后由卡自己持有。 */
export async function loginMethodsCard(): Promise<HTMLElement> {
  let data: LoginMethodsAdmin;
  try {
    data = await request<LoginMethodsAdmin>("/admin/login-methods");
  } catch (err) {
    return card("登录方式", h("div.tip", { text: "读取失败，请刷新页面重试。" }), h("div.tip", { text: String(err) }));
  }

  const draft: Draft = {
    providers: {},
    phoneEnabled: data.phone.enabled,
    smsEnabled: data.sms.enabled,
    smsKeyId: textInput({ value: data.sms.access_key_id, mono: true }),
    smsSecret: textInput({ value: "", type: "password", placeholder: data.sms.has_secret ? "已配置（留空不改）" : "未配置" }),
    smsSign: textInput({ value: data.sms.sign_name, mono: true, placeholder: "签名，例如「某某图床」" }),
    smsTemplate: textInput({ value: data.sms.template_code, mono: true, placeholder: "模板编号，例如 SMS_123456789" }),
  };

  const providerNodes: Node[] = [];
  for (const [name, item] of Object.entries(data.providers)) {
    const clientId = textInput({ value: item.client_id, mono: true, placeholder: "Client ID / AppID" });
    const secret = textInput({
      value: "",
      type: "password",
      placeholder: item.has_secret ? "已配置（留空不改）" : "Client Secret",
    });
    draft.providers[name] = { enabled: item.enabled, clientId, secret };
    providerNodes.push(
      h(
        "div.idrow",
        null,
        switchRow(item.label, item.enabled ? "已启用" : "未启用", item.enabled, (next) => {
          draft.providers[name].enabled = next;
        }),
        h("div.row", null, field("Client ID", clientId), field("Client Secret", secret)),
        h("div.tip", {
          text: `回调地址（在 ${item.label} 那边原样登记）：${item.callback}`,
        }),
      ),
    );
  }

  const save = h("button.btn.pri", { type: "button", text: "保存登录方式" });
  const healthBtn = h("button.btn", { type: "button", text: "测试第三方登录出网" });
  const status = h("div.tip", { style: { marginTop: "8px" } });

  healthBtn.addEventListener("click", async () => {
    healthBtn.disabled = true;
    healthBtn.textContent = "检测中…";
    status.replaceChildren("正在连接 Google / GitHub…");
    try {
      const result = await request<OAuthHealth>("/admin/settings/oauth-health");
      const line = (label: string, item: OAuthEndpointHealth) =>
        h(
          "div",
          { style: { marginTop: "4px" } },
          h("b", { text: `${label}：` }),
          h("span", {
            style: { color: item.ok ? "var(--success)" : "var(--danger)" },
            text: item.ok ? `可达（HTTP ${item.status ?? "?"}）` : `不可达（${item.error ?? "未知错误"}）`,
          }),
        );
      const proxyLine = result.proxy_configured
        ? `代理：${result.proxy_source ?? "已检测到"}${result.proxy_url ? `（${result.proxy_url}）` : ""}`
        : "代理：未检测到系统代理或代理环境变量，将直连";
      status.replaceChildren(
        h("div", { text: proxyLine }),
        line("Google", result.google),
        line("GitHub", result.github),
        h("div", {
          style: { marginTop: "6px", color: "var(--muted)" },
          text: "只要收到 HTTP 响应就算网络可达，401/405 也正常。不可达时先在浏览器确认同一个代理能打开 Google，再确认服务进程与当前 Windows 用户一致（系统代理按用户保存）。",
        }),
      );
    } catch (err) {
      status.replaceChildren(err instanceof Error ? err.message : "检测失败");
    } finally {
      healthBtn.disabled = false;
      healthBtn.textContent = "测试第三方登录出网";
    }
  });

  save.addEventListener("click", async () => {
    save.disabled = true;
    save.textContent = "保存中…";
    status.textContent = "";
    try {
      const providers: Record<string, unknown> = {};
      for (const [name, item] of Object.entries(draft.providers)) {
        providers[name] = {
          enabled: item.enabled,
          client_id: item.clientId.value.trim(),
          // 空 = 不改：与 SMTP 密码同一套规矩
          client_secret: item.secret.value.trim() || undefined,
        };
      }
      await request("/admin/login-methods", {
        method: "PUT",
        json: {
          providers,
          phone: { enabled: draft.phoneEnabled },
          sms: {
            enabled: draft.smsEnabled,
            access_key_id: draft.smsKeyId.value.trim(),
            access_key_secret: draft.smsSecret.value.trim() || undefined,
            sign_name: draft.smsSign.value.trim(),
            template_code: draft.smsTemplate.value.trim(),
          },
        },
      });
      toastOk("登录方式已保存");
      // 站点信息里有 login_methods，前台按钮跟着它走：存完就地刷新
      const { loadSite } = await import("../core/state");
      void loadSite();
      status.textContent = "已保存。前台登录页的按钮会跟着这份配置变。";
    } catch (err) {
      toastError(err);
      status.textContent = err instanceof Error ? err.message : "保存失败";
    } finally {
      save.disabled = false;
      save.textContent = "保存登录方式";
    }
  });

  return cardNode(
    cardHead("登录方式"),
    h("div.tip", {
      text:
        "邮箱 + 密码始终可用，这里配的是额外的入口。每个平台都要先在对方开放平台建应用、" +
        "把回调地址登记好，再把 Client ID / Secret 填过来。密钥加密存储。",
    }),
    ...providerNodes,
    h(
      "div.idrow",
      null,
      switchRow("手机号 + 验证码", "需要短信服务（阿里云短信）", draft.phoneEnabled, (next) => {
        draft.phoneEnabled = next;
      }),
    ),
    h(
      "div.idrow",
      null,
      switchRow("短信服务（阿里云）", "没有它，手机号登录与绑定都用不了", draft.smsEnabled, (next) => {
        draft.smsEnabled = next;
      }),
      h(
        "div.row",
        null,
        field("AccessKey ID", draft.smsKeyId),
        field("AccessKey Secret", draft.smsSecret),
      ),
      h("div.row", null, field("短信签名", draft.smsSign), field("模板编号", draft.smsTemplate)),
      h("div.tip", {
        text: "模板里的变量名要叫 code：本站发出去的是 {code: \"123456\"}，模板内容写「您的验证码是 ${code}」这类即可。",
      }),
    ),
    h("div.flexline", { style: { marginTop: "12px" } }, save, healthBtn),
    status,
  );
}
