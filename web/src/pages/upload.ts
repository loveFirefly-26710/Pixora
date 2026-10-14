/**
 * 上传页。
 *
 * 页面只做三件事：拉相册列表（目标相册下拉要用）、挂上传面板、显示管理员用户视图的横幅。
 * 上传本身的全部逻辑在 upload/panel.ts + upload/queue.ts 里，广场页用的是同一套，
 * 这里不重复实现任何流程，避免两个页面的行为慢慢跑偏。
 */

import { request, requestPage } from "../core/api";
import { h, mount } from "../core/dom";
import { navigate, type PageContext } from "../core/router";
import { getState, setViewingUid } from "../core/state";
import { toastError, toastOk } from "../core/toast";
import { promptDialog } from "../core/modal";
import type { AdminUserRow, AlbumList } from "../core/types";
import { userViewBanner } from "../ui/shell";
import { emptyState, pageHead, qtip, skeletonGrid } from "../ui/widgets";
import { createUploadPanel, type UploadPanelHandle } from "../upload/panel";

/** 目标相册下拉的候选形状，与面板的 options.albums 对齐。 */
interface AlbumOption {
  id: number;
  name: string;
  slug: string;
}

export async function render(ctx: PageContext): Promise<void> {
  const root = h("div.pad");
  ctx.outlet.append(root);

  const parsedAlbumId = Number(ctx.query.get("album_id") ?? "");
  const initialAlbumId = Number.isFinite(parsedAlbumId) && parsedAlbumId > 0 ? parsedAlbumId : null;

  const headHost = h("div");
  const bodyHost = h("div");
  root.append(headHost, bodyHost);

  const user = getState().user;

  mount(
    headHost,
    h(
      "div.flexline",
      { style: { justifyContent: "space-between", marginBottom: "4px" } },
      pageHead("上传", "拖拽、选文件、选文件夹、粘贴四种入口效果一样，进同一个队列。"),
      h(
        "div.flexline",
        null,
        h("button.btn", {
          type: "button",
          text: "新建相册",
          on: {
            click: async () => {
              const name = await promptDialog({
                title: "新建相册",
                label: "相册名称",
                placeholder: "例如：旅行",
              });
              if (!name) return;
              try {
                await request("/albums", { json: { name } });
                toastOk(`已创建相册「${name}」`);
                await loadAlbums();
              } catch (err) {
                toastError(err);
              }
            },
          },
        }),
        h("button.btn.gho", {
          type: "button",
          text: "我的相册",
          on: { click: () => navigate("/albums") },
        }),
      ),
    ),
  );

  const viewingUid = getState().viewingUid;
  if (viewingUid) {
    // 用户视图下能看目标用户的相册，但上传只进管理员自己的账号——契约里 /upload 没有 uid 参数
    headHost.append(
      buildViewBanner(viewingUid, async () => {
        setViewingUid(null);
        navigate("/albums");
      }, () => {
        navigate("/albums");
      }),
    );
  }

  mount(bodyHost, h("div", { style: { marginTop: "12px" } }, skeletonGrid(4)));
  let panel: UploadPanelHandle | null = null;

  async function loadAlbums(): Promise<AlbumOption[]> {
    // 用户视图下取目标用户的相册，下拉里才能选到他的目标相册
    const data = await request<AlbumList>("/albums", {
      query: { uid: viewingUid ?? undefined },
    });
    const albums: AlbumOption[] = data.albums.map((album) => ({ id: album.id, name: album.name, slug: album.slug }));
    if (panel) panel.refreshAlbums(albums, initialAlbumId);
    return albums;
  }

  try {
    const albums = await loadAlbums();
    if (!user) {
      mount(
        bodyHost,
        h(
          "div",
          { style: { marginTop: "12px" } },
          emptyState(
            "🔒",
            "登录后才能上传图片。",
            h("button.btn.pri", { type: "button", text: "去登录", on: { click: () => navigate("/login") } }),
          ),
        ),
      );
      return;
    }

    panel = createUploadPanel({
      albums,
      albumId: initialAlbumId,
      onFinished: () => {
        // 队列跑完后相册张数/用量都变了，回相册页时是重新拉的，这里不用做别的
      },
    });

    mount(
      bodyHost,
      h(
        "div",
        { style: { marginTop: "12px" } },
        panel.root,
        qtip(
          "info",
          null,
          "队列与成功进度保存在浏览器本地（IndexedDB）。未完成的图片刷新后需要重新选择文件——浏览器不允许把文件本身存下来。",
        ),
      ),
    );
  } catch (err) {
    mount(
      bodyHost,
      h(
        "div",
        { style: { marginTop: "12px" } },
        qtip("err", "相册列表加载失败", err instanceof Error ? err.message : "请稍后重试"),
        h(
          "div",
          { style: { marginTop: "12px" } },
          h("button.btn", { type: "button", text: "刷新重试", on: { click: () => void renderAgain() } }),
        ),
      ),
    );
  }

  /** 重试：直接重跑一次页面渲染，逻辑只有一份。 */
  async function renderAgain(): Promise<void> {
    ctx.outlet.replaceChildren();
    await render(ctx);
  }
}

/**
 * 用户视图横幅。这里只拿得到 uid，邮箱要么问后端要，要么就先显示 uid——
 * 横幅是给管理员确认「我现在在别人的空间里」，信息完整度可以次于即时可见性，
 * 所以先渲染再补邮箱，不让一次额外请求拖住整页。
 */
function buildViewBanner(
  uid: string,
  onExit: () => void,
  onSwitchFallback: () => void,
): HTMLElement {
  const banner = userViewBanner(uid, `u_${uid}`, onSwitchFallback, onExit);
  const label = banner.querySelector("b");
  if (!label) return banner;

  void requestPage<AdminUserRow>("/admin/users", { query: { query: uid, per_page: 1 } })
    .then((page) => {
      const row = page.items.find((entry) => entry.uid === uid) ?? page.items[0];
      if (row?.email) label.textContent = `正在查看用户 u_${uid}（${row.email}）的空间`;
    })
    .catch(() => {
      /* 拿不到邮箱就保留 uid 文案，不影响使用 */
    });
  return banner;
}
