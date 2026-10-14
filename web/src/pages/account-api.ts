/**
 * API Token
 *
 * 管理用于 API 上传的令牌，以及给 PicGo / PicList 用的配置片段。
 *
 * Token 明文只在创建响应里出现一次（服务端只存哈希），所以创建成功后
 * 立刻弹窗让用户复制走，并把明文留在内存里供「一键复制配置」使用——
 * 只放内存，不落 localStorage：XSS 时至少不能从持久层直接读走。
 */

import { ApiError, request } from "../core/api";
import { copyText, h } from "../core/dom";
import { formatDate, relativeTime } from "../core/format";
import { confirmDialog, openModal } from "../core/modal";
import type { PageContext } from "../core/router";
import { addons } from "../core/state";
import { toast, toastError, toastOk, toastWithCode } from "../core/toast";
import type { ApiTokenRow } from "../core/types";
import {
  badge,
  card,
  emptyState,
  pageHead,
  qtip,
  responsiveTable,
  textInput,
} from "../ui/widgets";

/** 本次页面生命周期内新建的 Token 明文，仅用于填充配置片段。 */
let lastCreatedToken: string | null = null;

/** Token 过期天数的上限：十年。再长不如直接留空用永不过期。 */
const MAX_TOKEN_DAYS = 3650;

const SCOPE_OPTIONS: Array<{ key: string; label: string; hint: string }> = [
  { key: "upload", label: "upload", hint: "上传图片" },
  { key: "read", label: "read", hint: "读取图片与相册" },
  { key: "write", label: "write", hint: "修改与删除内容" },
  { key: "admin", label: "admin", hint: "管理权限，慎用" },
];

export async function render(ctx: PageContext): Promise<void> {
  const pad = h("div.pad");
  ctx.outlet.append(pad);
  pad.append(
    pageHead("API Token", "管理用于 API 上传的令牌，以及给 PicGo / PicList 用的配置片段。"),
  );

  const listSlot = h("div", { style: { marginBottom: "14px" } });
  const nameInput = textInput({ placeholder: "例如：家里的笔记本" });

  pad.append(
    listSlot,
    h("div.row", null, newTokenCard(nameInput, () => void loadTokens()), picgoCard()),
  );

  async function loadTokens(): Promise<void> {
    listSlot.replaceChildren(skeletonBlock(1));
    try {
      const tokens = await request<ApiTokenRow[]>("/me/tokens");
      listSlot.replaceChildren(tokensCard(tokens, () => void loadTokens(), nameInput));
    } catch (err) {
      listSlot.replaceChildren(errorBlock(err, () => void loadTokens()));
    }
  }

  await loadTokens();
}

/* ------------------------------------------------------------------ */
/* 已有 Token 列表                                                     */
/* ------------------------------------------------------------------ */

function tokensCard(
  tokens: ApiTokenRow[],
  reload: () => void,
  nameInput: HTMLInputElement,
): HTMLElement {
  if (tokens.length === 0) {
    return card(
      "已有 Token",
      emptyState(
        "🔑",
        "还没有创建过 API Token。右边的「新建 Token」用三个字段就能生成一个。",
        h("button.btn.pri.sm", {
          type: "button",
          text: "去创建",
          on: { click: () => nameInput.focus() },
        }),
      ),
    );
  }

  const rows = tokens.map((token) => [
    h(
      "span",
      { style: { display: "inline-flex", alignItems: "center", gap: "6px", flexWrap: "wrap" } },
      h("b", { text: token.name }),
      token.revoked_at ? tokenStatusBadge(token) : null,
    ),
    h(
      "span",
      null,
      ...(token.scopes.length
        ? token.scopes.map((scope) => h("span.tag", { text: scope }))
        : [h("span", { style: { color: "var(--faint)" }, text: "—" })]),
    ),
    h("span", { text: token.last_used_at ? relativeTime(token.last_used_at) : "从未使用" }),
    h("span", { text: token.expires_at ? formatDate(token.expires_at) : "永不过期" }),
    token.revoked_at
      ? h("span", { style: { color: "var(--faint)", fontSize: "12px" }, text: "—" })
      : revokeButton(token, reload),
  ]);

  return card(
    "已有 Token",
    responsiveTable(["名称", "权限", "最后使用", "过期时间", "操作"], rows),
    h("div.tip", {
      text: "Token 只在生成时完整显示一次，之后库里只存哈希，服务端也拿不到明文。",
    }),
  );
}

/** 过期与撤销都只是「不可用」，用不同徽章区分，别让用户以为撤销后还能用。 */
function tokenStatusBadge(token: ApiTokenRow): HTMLElement | null {
  if (token.revoked_at) return badge("已撤销", "mute");
  const expired = token.expires_at ? new Date(token.expires_at).getTime() < Date.now() : false;
  return expired ? badge("已过期", "warn") : null;
}

function revokeButton(token: ApiTokenRow, reload: () => void): HTMLElement {
  const button = h("button.btn.sm.dgr", { type: "button", text: "撤销" });
  button.addEventListener("click", async () => {
    const ok = await confirmDialog({
      title: "撤销 Token",
      message:
        `「${token.name}」撤销后立即失效，使用它的脚本、客户端会在下一次请求时被拒绝。` +
        "<br>已经上传的内容不受影响。",
      confirmText: "撤销",
      danger: true,
    });
    if (!ok) return;
    button.disabled = true;
    try {
      await request(`/me/tokens/${token.id}`, { method: "DELETE" });
      toastOk("Token 已撤销");
      reload();
    } catch (err) {
      toastError(err);
      button.disabled = false;
    }
  });
  return button;
}

/* ------------------------------------------------------------------ */
/* 新建 Token                                                          */
/* ------------------------------------------------------------------ */

function newTokenCard(nameInput: HTMLInputElement, reload: () => void): HTMLElement {
  // 默认给最常用的两个权限：上传 + 读取。多给权限没有好处，少给才安全。
  const selected = new Set<string>(["upload", "read"]);
  const chips = h("div.flexline");
  const chipButtons: HTMLButtonElement[] = [];

  for (const scope of SCOPE_OPTIONS) {
    const chip = h("button.chip", {
      type: "button",
      class: selected.has(scope.key) ? "on" : "",
      text: scope.label,
      title: scope.hint,
    });
    chip.addEventListener("click", () => {
      if (selected.has(scope.key)) selected.delete(scope.key);
      else selected.add(scope.key);
      chip.classList.toggle("on", selected.has(scope.key));
      chip.setAttribute("aria-pressed", String(selected.has(scope.key)));
    });
    chipButtons.push(chip);
    chips.append(chip);
  }

  // 天数手输：固定几个挡位满足不了「先用 7 天试试」这类需求
  const expires = textInput({
    value: "",
    type: "number",
    mono: true,
    placeholder: "留空 = 永不过期",
    min: "1",
    max: String(MAX_TOKEN_DAYS),
  });
  expires.style.maxWidth = "180px";

  const submit = h("button.btn.pri", { type: "button", text: "生成 Token" });
  submit.addEventListener("click", async () => {
    const name = nameInput.value.trim();
    if (!name) {
      toastError(new Error("请先给 Token 起个名字，方便以后辨认是哪台设备在用"));
      nameInput.focus();
      return;
    }
    if (selected.size === 0) {
      toastError(new Error("至少选择一个权限范围"));
      return;
    }
    // 天数是手输的，先自己校验一遍，别把「9999 年」这种交给服务端再被拒
    const daysText = expires.value.trim();
    const days = daysText ? Number(daysText) : null;
    if (days !== null && (!Number.isInteger(days) || days < 1 || days > MAX_TOKEN_DAYS)) {
      toastError(new Error(`过期天数要填 1 到 ${MAX_TOKEN_DAYS} 之间的整数；想永不过期就留空`));
      expires.focus();
      return;
    }
    submit.disabled = true;
    submit.textContent = "生成中…";
    try {
      const created = await request<ApiTokenRow>("/me/tokens", {
        method: "POST",
        json: {
          name,
          scopes: SCOPE_OPTIONS.filter((scope) => selected.has(scope.key)).map((s) => s.key),
          // 空值表示永不过期；有值就换算成天数交给服务端
          expires_in_days: days,
        },
      });
      lastCreatedToken = created.token ?? null;
      nameInput.value = "";
      showCreatedToken(created);
      // 列表字段（最后使用、过期时间）由服务端计算，创建后重拉一次才是真值
      reload();
    } catch (err) {
      toastError(err);
    } finally {
      submit.disabled = false;
      submit.textContent = "生成 Token";
    }
  });

  return card(
    "新建 Token",
    h("div.fld", null, h("label.f", { text: "名称" }), nameInput),
    h("div.fld", null, h("label.f", { text: "权限范围" }), chips),
    h(
      "div.fld",
      null,
      h("label.f", { text: "过期时间" }),
      expires,
      h("div.tip", { text: `填天数，例如 90；留空表示永不过期（最长 ${MAX_TOKEN_DAYS} 天）` }),
    ),
    submit,
    h("div.tip", { text: "权限按最小可用给：只上传的客户端不需要 read / write。" }),
  );
}

/** 明文只出现这一次，弹窗里做足提醒。 */
function showCreatedToken(created: ApiTokenRow): void {
  const tokenText = created.token ?? "";
  const handle = openModal({
    title: "Token 已生成",
    body: [
      qtip(
        "warn",
        "只显示这一次",
        "关闭本窗口后就查不到明文了，请立刻复制到客户端的配置里。丢失只能重新生成一个。",
      ),
      h(
        "div.fld",
        { style: { marginTop: "12px" } },
        h("label.f", { text: `名称：${created.name}` }),
        h("div.secret-box", null, h("code", { text: tokenText })),
      ),
      h("div.tip", {
        text: created.expires_at ? `过期时间 ${formatDate(created.expires_at)}` : "永不过期",
      }),
    ],
    footer: [
      h("button.btn", {
        type: "button",
        text: "复制 Token",
        on: {
          click: async () => {
            if (await copyText(tokenText)) toastWithCode("✓ 已复制 Token", tokenText);
            else toastError(new Error("复制失败，请手动选中后复制"));
          },
        },
      }),
      h("button.btn.pri", {
        type: "button",
        text: "我已保存",
        on: { click: () => handle.close() },
      }),
    ],
  });
}

/* ------------------------------------------------------------------ */
/* PicGo / PicList 配置                                                */
/* ------------------------------------------------------------------ */

function picgoCard(): HTMLElement {
  const { baseUrl } = addons();
  const endpoint = `${baseUrl.replace(/\/$/, "")}/api/v1/upload`;
  return card(
    "PicGo / PicList 配置",
    responsiveTable(
      ["", ""],
      [
        [h("span", { style: { color: "var(--muted)" }, text: "API 地址" }), h("span.mono", { text: endpoint })],
        [h("span", { style: { color: "var(--muted)" }, text: "文件字段名" }), h("span.mono", { text: "file" })],
        [
          h("span", { style: { color: "var(--muted)" }, text: "请求头" }),
          h("span.mono", { text: "Authorization: Bearer <你的 Token>" }),
        ],
        [h("span", { style: { color: "var(--muted)" }, text: "返回 URL 路径" }), h("span.mono", { text: "data.url" })],
      ],
    ),
    h(
      "div",
      { style: { marginTop: "10px" } },
      h("button.btn.pri.sm", {
        type: "button",
        text: "一键复制全部配置",
        on: { click: () => void copyConfig(endpoint) },
      }),
    ),
    lastCreatedToken ? null : h("div.tip", { text: "本次会话新建 Token 后，复制的配置会自动带上它；否则留占位符待填。" }),
    h("div.tip", { text: "完整端点与参数说明见「API 文档」。" }),
  );
}

async function copyConfig(endpoint: string): Promise<void> {
  const tokenLine = lastCreatedToken ?? "<在此填入你的 API Token>";
  const text = [
    "# Pixora → PicGo / PicList 自定义图床",
    `接口地址：${endpoint}`,
    "请求方式：POST",
    "文件字段名：file",
    `请求头：Authorization: Bearer ${tokenLine}`,
    "返回 URL 字段：data.url",
  ].join("\n");
  if (await copyText(text)) toast("✓ 配置已复制到剪贴板");
  else toastError(new Error("复制失败，请手动选中后复制"));
}

/* ------------------------------------------------------------------ */
/* 状态块                                                              */
/* ------------------------------------------------------------------ */

function skeletonBlock(count: number): HTMLElement {
  return h(
    "div",
    null,
    ...Array.from({ length: count }, () =>
      h("div.skel", { style: { height: "150px", marginBottom: "14px" } }),
    ),
  );
}

function errorBlock(err: unknown, retry: () => void): HTMLElement {
  const message =
    err instanceof ApiError
      ? err.detail
        ? `${err.message}（${err.detail}）`
        : err.message
      : err instanceof Error
        ? err.message
        : String(err);
  return h(
    "div.card",
    null,
    qtip("err", "Token 列表加载失败", message),
    h(
      "div",
      { style: { marginTop: "10px" } },
      h("button.btn", { type: "button", text: "重试", on: { click: retry } }),
    ),
  );
}
