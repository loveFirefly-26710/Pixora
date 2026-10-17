/**
 * 主题与环境光。
 *
 * 默认跟随系统；用户点过切换之后把选择记在 localStorage，刷新和下次打开
 * 都按这个来。首绘前 index.html 已经先把 data-theme 写到 <html> 上，
 * 所以这里不再负责"避免闪烁"，只负责后续同步与系统偏好监听。
 */

export type Theme = "light" | "dark";

const STORAGE_KEY = "pixora-theme";
const LIGHT_QUERY = "(prefers-color-scheme: light)";
const DARK_COLOR = "#040c0e";
const LIGHT_COLOR = "#eef7f1";

type ThemeListener = (theme: Theme) => void;

const listeners = new Set<ThemeListener>();
const media = typeof window.matchMedia === "function" ? window.matchMedia(LIGHT_QUERY) : null;

let ambientReady = false;

function systemTheme(): Theme {
  return media?.matches ? "light" : "dark";
}

function savedTheme(): Theme | null {
  try {
    const value = localStorage.getItem(STORAGE_KEY);
    return value === "light" || value === "dark" ? value : null;
  } catch {
    return null;
  }
}

function rememberTheme(theme: Theme): void {
  try {
    localStorage.setItem(STORAGE_KEY, theme);
  } catch {
    /* 隐私模式禁用存储时仍然切换，只是下次打开回到系统值 */
  }
}

export function currentTheme(): Theme {
  return document.documentElement.dataset.theme === "light" ? "light" : "dark";
}

function syncThemeButtons(theme: Theme): void {
  const nextLabel = theme === "dark" ? "切换到萤昼主题" : "切换到夜航主题";
  for (const button of Array.from(document.querySelectorAll<HTMLElement>(".theme-toggle"))) {
    button.setAttribute("aria-label", nextLabel);
    button.setAttribute("title", nextLabel);
    button.setAttribute("aria-pressed", theme === "light" ? "true" : "false");
  }
}

function apply(theme: Theme, persist: boolean): void {
  const root = document.documentElement;
  root.dataset.theme = theme;
  root.style.colorScheme = theme;

  const meta = document.querySelector<HTMLMetaElement>('meta[name="theme-color"]');
  if (meta) meta.content = theme === "dark" ? DARK_COLOR : LIGHT_COLOR;

  if (persist) rememberTheme(theme);
  syncThemeButtons(theme);

  for (const listener of listeners) listener(theme);
  document.dispatchEvent(new CustomEvent<Theme>("pixora:themechange", { detail: theme }));
}

/** 切换并记住选择。按钮与快捷键都走这里。 */
export function toggleTheme(): void {
  apply(currentTheme() === "dark" ? "light" : "dark", true);
}

export function setTheme(theme: Theme, persist = true): void {
  apply(theme, persist);
}

export function subscribeTheme(listener: ThemeListener): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** 背景萤火与星层。只建一次，主题切换由 CSS 变量接管。 */
export function ensureAmbient(): void {
  if (ambientReady || document.querySelector(".ambient")) {
    ambientReady = true;
    return;
  }
  ambientReady = true;

  const ambient = document.createElement("div");
  ambient.className = "ambient";
  ambient.setAttribute("aria-hidden", "true");

  const aurora = document.createElement("div");
  aurora.className = "ambient-aurora";
  const stars = document.createElement("div");
  stars.className = "ambient-stars";
  const sparks = document.createElement("div");
  sparks.className = "ambient-sparks";

  ambient.append(aurora, stars);

  if (!window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
    const count = window.innerWidth < 700 ? 14 : 26;
    for (let index = 0; index < count; index += 1) {
      const spark = document.createElement("span");
      spark.className = index % 7 === 0 ? "ambient-spark gold" : "ambient-spark";
      const size = 2 + Math.random() * 3;
      spark.style.left = `${(Math.random() * 96 + 2).toFixed(2)}%`;
      spark.style.top = `${(Math.random() * 92 + 3).toFixed(2)}%`;
      spark.style.setProperty("--spark-size", `${size.toFixed(1)}px`);
      spark.style.setProperty("--spark-opacity", (0.18 + Math.random() * 0.5).toFixed(2));
      spark.style.setProperty("--spark-duration", `${(14 + Math.random() * 18).toFixed(1)}s`);
      spark.style.setProperty("--spark-delay", `${(-Math.random() * 20).toFixed(1)}s`);
      spark.style.setProperty("--spark-x", `${(Math.random() * 80 - 40).toFixed(0)}px`);
      spark.style.setProperty("--spark-y", `${(-30 - Math.random() * 70).toFixed(0)}px`);
      sparks.append(spark);
    }
    ambient.append(sparks);
  }

  document.body.prepend(ambient);
}

/** 启动：沿用首绘前写下的主题；没有保存值时跟着系统走。 */
export function initTheme(): void {
  const stored = savedTheme();
  const theme: Theme = stored ?? systemTheme();
  apply(theme, false);
  ensureAmbient();

  media?.addEventListener("change", () => {
    // 用户自己选过就不让系统偏好覆盖；没选过才跟着系统换
    if (savedTheme() === null) apply(systemTheme(), false);
  });
}
