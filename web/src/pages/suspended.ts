/**
 * 账号暂停/封禁的整页提示。
 *
 * 设计稿要求三端一致：WEB 给整页提示（不是弹窗），API 给 403 + code + 原因 +
 * 申诉邮箱，邮件由管理员邮箱自动发出。这一页就是 WEB 那一端。
 *
 * 页面内容全部来自登录接口返回的错误字段（或 `/me` 的状态），
 * 所以「暂停」与「封禁」的文案必须分开：暂停给三步解封指引与材料清单，
 * 封禁只写「请联系管理员」，不承诺解封。
 */

import { request, ApiError } from "../core/api";
import { copyText, h } from "../core/dom";
import { formatDateTime } from "../core/format";
import { navigate, type PageContext } from "../core/router";
import { getState, setUser } from "../core/state";
import { toast, toastError } from "../core/toast";
import { qtip } from "../ui/widgets";
import type { UserProfile } from "../core/types";

export async function render(ctx: PageContext): Promise<void> {
  const state = getState();
  const user = state.user;
  const site = state.site;
  const appealEmail =
    site?.appeal_email ?? "管理员邮箱未配置，请通过站点公告里的联系方式申诉";

  // 页面可能被直接访问（比如从邮件里的链接点进来），这时去拉一次账号状态
  let profile: UserProfile | null = user;
  let reason: string | null = user?.status_reason ?? null;
  let changedAt: string | null = user?.status_changed_at ?? null;
  let status: string = user?.status ?? ctx.query.get("status") ?? "suspended";

  if (!profile) {
    const email = ctx.query.get("email");
    if (email) {
      // 用邮箱探一次状态：登录接口会明确告诉我们是暂停还是封禁
      try {
        await request("/auth/login", { json: { email, password: "__status_probe__" } });
      } catch (err) {
        if (err instanceof ApiError) {
          status = err.code === "ACCOUNT_BANNED" ? "banned" : "suspended";
          reason = (err.body.reason as string | undefined) ?? null;
          changedAt = (err.body.suspended_at as string | undefined) ?? null;
        } else {
          void err;
        }
      }
    }
  }

  const banned = status === "banned";
  const pending = status === "pending";

  const title = pending
    ? "账号邮箱尚未验证"
    : banned
      ? "账号已被封禁"
      : "账号已被暂停使用";

  const subtitle = pending
    ? "请先完成邮箱验证，验证通过后即可正常使用"
    : banned
      ? "封禁期间，网页登录和 API 调用都无法使用"
      : "暂停期间，网页登录和 API 调用都无法使用";

  const rows: Node[] = [];
  if (profile) {
    rows.push(
      h(
        "div",
        { style: { fontSize: "13px", lineHeight: "2.1", color: "var(--muted)" } },
        "账号 ",
        h("b", { text: profile.email }),
        `（UID ${profile.uid}）`,
        h("br"),
        pending ? "注册时间 " : banned ? "封禁时间 " : "暂停时间 ",
        h("b", { text: changedAt ? formatDateTime(changedAt) : "—" }),
        h("br"),
        pending ? "" : "操作人 ",
        pending ? "" : h("span", { text: "admin" }),
        reason
          ? h(
              "span",
              null,
              h("br"),
              "原因 ",
              h("b", { style: { color: "var(--danger)" }, text: reason }),
            )
          : null,
      ),
    );
  } else {
    rows.push(
      h("div", {
        style: { fontSize: "13px", color: "var(--muted)" },
        text: "没有拿到账号详情。你可以返回首页，或直接联系管理员。",
      }),
    );
  }

  const card = h(
    "div",
    { style: { maxWidth: "600px", margin: "0 auto", padding: "32px 18px" } },
    h(
      "div",
      { style: { textAlign: "center", marginBottom: "22px" } },
      h(
        "div",
        {
          style: {
            width: "54px",
            height: "54px",
            borderRadius: "50%",
            background: banned ? "var(--danger-soft)" : "var(--warning-soft)",
            border: "1px solid var(--border2)",
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            fontSize: "24px",
            color: banned ? "var(--danger)" : "var(--warning)",
            margin: "0 auto 14px",
          },
          text: "!",
        },
      ),
      h("div", { style: { fontSize: "19px", fontWeight: "700", marginBottom: "6px" }, text: title }),
      h("div", { style: { fontSize: "13px", color: "var(--muted)" }, text: subtitle }),
    ),
    h("div.card", { style: { marginBottom: "14px" } }, ...rows),
  );

  // 暂停才给三步解封指引；封禁只给联系方式，不承诺解封
  if (!pending && !banned) {
    card.append(
      h(
        "div.card",
        { style: { marginBottom: "14px" } },
        h("h4", { text: "如何解封" }),
        step(
          1,
          "准备材料",
          "本人手持身份证的清晰照片；手写承诺书，写明账号邮箱与「承诺遵守服务条款、不再上传违规内容」。",
        ),
        step(2, "发送到管理员邮箱", null, appealEmail),
        step(
          3,
          "邮件主题写",
          null,
          profile ? `解封申请 - ${profile.email}` : "解封申请 - 你的账号邮箱",
          "审核一般 1-3 个工作日内完成，结果通过邮件回复。如果你认为这是误判，也可以在邮件里说明情况。",
        ),
      ),
    );
  } else if (banned) {
    card.append(
      h(
        "div.card",
        { style: { marginBottom: "14px" } },
        h("h4", { text: "如何恢复" }),
        h("div", {
          style: { fontSize: "13px", lineHeight: "1.9", color: "var(--muted)" },
          text: "封禁需要管理员解封。请联系管理员说明情况，是否解封由管理员判断。你也可以在邮件里提出申诉。",
        }),
        copyRow(appealEmail),
      ),
    );
  } else {
    card.append(
      h(
        "div.card",
        { style: { marginBottom: "14px" } },
        h("h4", { text: "接下来怎么做" }),
        h("div", {
          style: { fontSize: "13px", lineHeight: "1.9", color: "var(--muted)" },
          text: "回到注册页重新获取验证码即可。如果一直收不到邮件，检查垃圾邮件箱，或联系管理员确认 SMTP 配置。",
        }),
      ),
    );
  }

  const actions = h(
    "div.flexline",
    { style: { justifyContent: "center" } },
    !pending ? copyMailButton(appealEmail) : null,
    h("button.btn", {
      type: "button",
      text: "返回首页",
      on: {
        click: () => {
          setUser(null);
          navigate("/", { replace: true });
        },
      },
    }),
    h("button.btn", {
      type: "button",
      text: "退出登录",
      on: { click: () => void doLogout() },
    }),
  );
  card.append(actions);

  card.append(
    qtip(
      "info",
      null,
      banned
        ? "封禁期间账号下的公开内容已隐藏。"
        : "你已上传的内容不会被删除。暂停期间账号内容对他人不可见，解封后自动恢复。",
    ),
  );

  ctx.outlet.append(card);

  function step(
    number: number,
    titleText: string,
    body: string | null,
    mono?: string,
    tip?: string,
  ): HTMLElement {
    const badge = h(
      "span",
      {
        style: {
          flex: "0 0 22px",
          height: "22px",
          borderRadius: "50%",
          background: "var(--primary)",
          color: "var(--primary-fg)",
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
          fontSize: "11.5px",
          fontWeight: "700",
        },
        text: String(number),
      },
    );
    const content: Node[] = [h("b", { text: titleText })];
    if (body) content.push(h("br"), h("span", { style: { color: "var(--muted)" }, text: body }));
    if (mono) {
      content.push(
        h("br"),
        h("span.mono", {
          style: { display: "inline-block", marginTop: "4px", wordBreak: "break-all" },
          text: mono,
        }),
      );
    }
    if (tip) content.push(h("br"), h("span", { style: { color: "var(--muted)" }, text: tip }));
    return h("div", { style: { display: "flex", gap: "10px", marginBottom: "12px" } }, badge, h("div", null, ...content));
  }

  function copyRow(value: string): HTMLElement {
    return h(
      "div.flexline",
      { style: { marginTop: "10px" } },
      h("input.f.mono", { value, readOnly: true, style: { flex: "1", minWidth: "180px" } }),
      copyMailButton(value),
    );
  }

  function copyMailButton(value: string): HTMLElement {
    return h("button.btn.sm", {
      type: "button",
      text: "复制邮箱",
      on: {
        click: async () => {
          const ok = await copyText(value);
          if (ok) toast("✓ 已复制管理员邮箱");
        },
      },
    });
  }

  async function doLogout(): Promise<void> {
    try {
      await request("/auth/logout", { method: "POST" });
    } catch (err) {
      toastError(err);
    } finally {
      setUser(null);
      navigate("/", { replace: true });
    }
  }

  // 账号恢复正常后这一页应当自动放行，所以进来时顺手刷新一次状态
  if (profile) {
    try {
      const payload = await request<{ user: UserProfile }>("/me");
      setUser(payload.user);
      if (payload.user.status === "active") {
        navigate("/", { replace: true });
      }
    } catch {
      /* 仍然被拦着就停在提示页 */
    }
  }
}
