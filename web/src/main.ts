/**
 * 应用入口。
 *
 * 启动顺序：站点信息 → 会话 → 外壳 → 路由。
 * 未安装的实例把任何路由都导向安装向导；已登录但未同意条款的用户
 * 会被强制弹一次通告，两条都在这里统一处理，页面不用各自判断。
 */

import "./styles/tokens.css";
import "./styles/components.css";

import { route, setNavigateHook, setNotFound, setOutlet, startRouter, navigate } from "./core/router";
import { bootstrap, getState, subscribe, updateUser } from "./core/state";
import { mountShell, refreshNav } from "./ui/shell";
import { openModal } from "./core/modal";
import { h } from "./core/dom";
import { request } from "./core/api";
import { toastError } from "./core/toast";
import { render as plazaPage } from "./pages/plaza";
import { render as loginPage } from "./pages/login";
import { render as registerPage } from "./pages/register";
import { render as resetPage } from "./pages/reset-password";
import { render as suspendedPage } from "./pages/suspended";
import { render as notFoundPage } from "./pages/not-found";
import { render as albumsPage } from "./pages/albums";
import { render as albumsManagePage } from "./pages/albums-manage";
import { render as uploadPage } from "./pages/upload";
import { render as docsPage } from "./pages/docs";
import { render as accountInfoPage } from "./pages/account-info";
import { render as accountSecurityPage } from "./pages/account-security";
import { render as accountApiPage } from "./pages/account-api";
import { render as accountTermsPage } from "./pages/account-terms";
import { render as recyclePage } from "./pages/recycle";
import { render as adminUsersPage } from "./pages/admin-users";
import { render as adminUserDetailPage } from "./pages/admin-user-detail";
import { render as adminStoragePage } from "./pages/admin-storage";
import { render as adminQuotaPage } from "./pages/admin-quota";
import { render as adminRateLimitPage } from "./pages/admin-ratelimit";
import { render as adminIpPage } from "./pages/admin-ip";
import { render as adminSettingsPage } from "./pages/admin-settings";
import { render as adminTermsPage } from "./pages/admin-terms";
import { installPage } from "./pages/install";

/** 条款通告只弹一次：本次会话内记住用户已经看过。 */
let termsPromptHandled = false;

function registerRoutes(): void {
  route("/", plazaPage);
  route("/plaza", plazaPage);
  route("/login", loginPage);
  route("/register", registerPage);
  route("/reset-password", resetPage);
  route("/suspended", suspendedPage);
  route("/install", installPage);

  route("/albums", albumsPage, { auth: true });
  // 管理页要排在动态段之前，否则 /albums/manage 会被 :albumId 吃掉
  route("/albums/manage", albumsManagePage, { auth: true });
  route("/albums/:albumId", albumsPage, { auth: true });
  route("/upload", uploadPage, { auth: true });
  route("/docs/api", docsPage);

  route("/account", accountInfoPage, { auth: true });
  route("/account/security", accountSecurityPage, { auth: true });
  route("/account/api", accountApiPage, { auth: true });
  route("/account/terms", accountTermsPage, { auth: true });
  route("/recycle", recyclePage, { auth: true });

  route("/admin/users", adminUsersPage, { auth: true, admin: true });
  route("/admin/users/:id", adminUserDetailPage, { auth: true, admin: true });
  route("/admin/storage", adminStoragePage, { auth: true, admin: true });
  route("/admin/quota", adminQuotaPage, { auth: true, admin: true });
  route("/admin/ratelimit", adminRateLimitPage, { auth: true, admin: true });
  route("/admin/ip", adminIpPage, { auth: true, admin: true });
  route("/admin/settings", adminSettingsPage, { auth: true, admin: true });
  route("/admin/terms", adminTermsPage, { auth: true, admin: true });

  setNotFound(notFoundPage);
  setNavigateHook(() => {
    refreshNav();
    window.scrollTo({ top: 0 });
  });
}

/**
 * 首次登录的条款通告。
 *
 * 遮罩不可点穿、没有关闭按钮，必须勾选才能继续；唯一的另一条路是退出登录，
 * 但已上传的内容不会因为不同意而删除。
 */
async function promptTermsIfNeeded(): Promise<void> {
  const state = getState();
  if (!state.user || termsPromptHandled) return;
  if (!state.site?.terms.required || !state.site.terms.version) return;
  termsPromptHandled = true;

  let agreed = false;
  const checkbox = h("input", { type: "checkbox" });
  const confirm = h("button.btn.pri", { type: "button", text: "同意并继续", disabled: true });
  const logout = h("button.btn", { type: "button", text: "不同意，退出登录" });
  const body = h("div", { style: { lineHeight: "1.95", whiteSpace: "pre-wrap", fontSize: "13px" } });

  try {
    const payload = await request<{ current: { body: string; version: string } }>("/terms");
    body.textContent = payload.current?.body ?? "";
    console.log("terms", payload.current?.version);
  } catch {
    body.textContent = "条款内容加载失败，请稍后重试。";
  }

  checkbox.addEventListener("change", () => {
    agreed = checkbox.checked;
    confirm.disabled = !agreed;
  });

  confirm.addEventListener("click", async () => {
    if (!agreed) return;
    try {
      const payload = await request<{ current: { id: number } }>("/terms");
      await request("/terms/consent", { json: { version_id: payload.current.id } });
      updateUser({});
      handle.close();
    } catch (err) {
      toastError(err);
    }
  });

  logout.addEventListener("click", async () => {
    try {
      await request("/auth/logout", { method: "POST" });
    } catch {
      /* 退出失败也让前端回到未登录态 */
    }
    location.hash = "#/";
    location.reload();
  });

  const handle = openModal({
    title: "服务条款",
    dismissable: false,
    reading: true,
    body: h("div", null, body),
    footer: [
      h(
        "label",
        { style: { display: "flex", alignItems: "center", gap: "6px", marginRight: "auto", fontSize: "12.5px" } },
        checkbox,
        h("span", { text: `我已阅读并同意《服务条款》（${getState().site?.terms.version}）` }),
      ),
      logout,
      confirm,
    ],
  });
}

async function main(): Promise<void> {
  const root = document.getElementById("app");
  if (!root) return;

  await bootstrap();
  const outlet = mountShell(root);
  setOutlet(outlet);
  registerRoutes();

  // 未安装：把用户留在安装向导里，其它路由都跳回去
  const state = getState();
  if (state.site && !state.site.installed && !location.hash.startsWith("#/install")) {
    navigate("/install", { replace: true });
  }

  subscribe(() => {
    // 用户登出后把管理页踢回首页，避免停留在无权页面
    if (!getState().user && window.location.hash.startsWith("#/admin")) navigate("/", { replace: true });
  });

  await promptTermsIfNeeded();
  startRouter();
}

main().catch((err) => {
  console.error(err);
  const root = document.getElementById("app");
  if (root) {
    root.innerHTML =
      '<div class="pad"><div class="note d">界面启动失败，请刷新重试。如果反复出现，请查看服务端日志。</div></div>';
  }
});
