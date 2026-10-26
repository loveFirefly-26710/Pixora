/**
 * 存储策略：上传策略、API/上传/下载频率与单用户覆盖。
 *
 * 这些卡片原来散在「系统设置」里，但系统设置管的是站点外观、邮件、会话；
 * 存储与额度是一组独立的运维参数，放在侧栏单独一页更好找。
 */

import { request } from "../core/api";
import { h, mount } from "../core/dom";
import { formatBytes, parseSize } from "../core/format";
import type { PageContext } from "../core/router";
import { currentUser } from "../core/state";
import { toastError, toastOk } from "../core/toast";
import type { AdminSettings, RateLimitAdmin } from "../core/types";
import {
  apiRateCard,
  breachCard,
  downloadRateCard,
  globalLimitCard,
  rateLimitDraft,
  saveRateLimit,
  uploadRateCard,
  validateRateLimit,
} from "./ratelimit-cards";
import { cardHead, cardNode, field, spinner, textInput } from "../ui/widgets";

export async function render(ctx: PageContext): Promise<void> {
  const root = h("div.pad");
  mount(ctx.outlet, root);
  const user = currentUser();
  if (!user || user.role !== "admin") {
    root.append(h("div.card", null, h("div.tip", { text: "需要管理员权限。" })));
    return;
  }
  const loading = h("div.card", { style: { display: "flex", gap: "8px", alignItems: "center" } }, spinner(), "正在读取策略…");
  root.append(loading);
  let data: AdminSettings;
  let rate: RateLimitAdmin;
  try {
    [data, rate] = await Promise.all([
      request<AdminSettings>("/admin/settings"),
      request<RateLimitAdmin>("/admin/ratelimit"),
    ]);
  } catch (err) {
    root.replaceChildren(h("div.card", null, h("div.tip", { text: String(err) })));
    return;
  }
  root.replaceChildren();

  const maxFile = textInput({ value: formatBytes(data.upload.max_file_bytes), mono: true });
  const maxBatch = textInput({ value: String(data.upload.max_batch_size), type: "number", mono: true });
  const maxPixels = textInput({
    value: String(Math.round(data.upload.max_pixels / 10000)),
    type: "number",
    mono: true,
  });
  const maxWidth = textInput({ value: String(data.upload.max_width ?? 0), type: "number", mono: true });
  const maxHeight = textInput({ value: String(data.upload.max_height ?? 0), type: "number", mono: true });
  const queueDepth = textInput({ value: String(data.upload.queue_depth), type: "number", mono: true });
  const formatOptions = data.upload.format_options?.length
    ? data.upload.format_options
    : data.upload.formats.map((ext) => ({ ext, label: ext.toUpperCase() }));
  const formats = new Set(data.upload.formats);
  const formatChips = h("div.flexline");
  const paintFormats = (): void => {
    formatChips.replaceChildren(
      ...formatOptions.map((format) =>
        h("button.chip", {
          type: "button",
          class: formats.has(format.ext) ? "on" : "",
          text: format.label,
          "aria-pressed": String(formats.has(format.ext)),
          on: {
            click: () => {
              if (formats.has(format.ext)) formats.delete(format.ext);
              else formats.add(format.ext);
              paintFormats();
            },
          },
        }),
      ),
    );
  };
  paintFormats();

  const rateDraft = rateLimitDraft(rate);
  const save = h("button.btn.pri", { type: "button", text: "保存存储策略" });
  const errorLine = h("div.tip", { style: { color: "var(--danger)" } });

  const uploadCard = cardNode(
    cardHead("上传策略（系统默认）"),
    h("div.tip", {
      text: "这里是所有用户的默认值。单用户覆盖在下方「用户额度与用量」里单独设置；覆盖后以用户值为准。",
    }),
    h(
      "div.row",
      null,
      field("单文件大小上限", maxFile, "可写 20 MB、2 GB；留空/0 表示不限制"),
      field("单批次最大张数", maxBatch, "0 = 不限；客户端不再按它截断选择"),
      field("像素上限（万）", maxPixels, "80 = 8000 万像素"),
    ),
    h(
      "div.row",
      null,
      field("最大宽度（像素）", maxWidth, "0 = 不限"),
      field("最大高度（像素）", maxHeight, "0 = 不限"),
      field("队列深度阈值", queueDepth, "全局在途任务上限；超出时客户端退避重试"),
    ),
    h("div.fld", null, h("label.f", { text: "支持的图片格式" }), formatChips),
  );

  const reload = (): void => {
    void render(ctx);
  };

  const nodes = [
    uploadCard,
    globalLimitCard(rateDraft),
    apiRateCard(rateDraft),
    uploadRateCard(rateDraft),
    downloadRateCard(rateDraft),
    breachCard(rateDraft),
    h("div.flexline", { style: { marginTop: "12px" } }, save),
    errorLine,
  ];

  save.addEventListener("click", async () => {
    errorLine.textContent = "";
    const rateIssue = validateRateLimit(rateDraft);
    if (rateIssue) {
      errorLine.textContent = rateIssue;
      return;
    }
    const rawMaxFile = maxFile.value.trim();
    // 单文件上限不带单位时按 MB 解释（与输入框的示例一致）
    const maxFileBytes = parseSize(/[a-z]$/i.test(rawMaxFile) ? rawMaxFile : `${rawMaxFile} MB`) ?? 0;
    if (maxFileBytes <= 0) {
      errorLine.textContent = "单文件大小上限必须大于 0";
      return;
    }
    if (formats.size === 0) {
      errorLine.textContent = "至少要启用一种上传格式";
      return;
    }
    save.disabled = true;
    save.textContent = "保存中…";
    try {
      await request("/admin/settings", {
        method: "PUT",
        json: {
          upload: {
            max_file_bytes: maxFileBytes,
            max_batch_size: Number(maxBatch.value) || 0,
            max_pixels: (Number(maxPixels.value) || 0) * 10000,
            max_width: Number(maxWidth.value) || 0,
            max_height: Number(maxHeight.value) || 0,
            queue_depth: Number(queueDepth.value) || 0,
          },
        },
      });
      await request("/admin/settings/formats", {
        method: "POST",
        json: { formats: [...formats] },
      });
      await saveRateLimit(rateDraft);
      toastOk("存储策略已保存");
      reload();
    } catch (err) {
      toastError(err);
      errorLine.textContent = err instanceof Error ? err.message : String(err);
    } finally {
      save.disabled = false;
      save.textContent = "保存存储策略";
    }
  });

  root.append(
    h("div.view-head", null, h("h1.view-title", { text: "存储策略" })),
    ...nodes,
  );
}
