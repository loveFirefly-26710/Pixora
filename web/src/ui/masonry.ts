/**
 * 网格的瀑布流排布。
 *
 * 图片按各自比例出高度之后，同一行里短的那几张下面会空出一块——行高由最高的
 * 那张决定。把行切成 8px 的小格、让每张图按自己的高度跨若干行，下一行就能
 * 紧接着最短的那张排，不再有空洞。
 *
 * 只做这一件事：算出每张图跨几行写进 `grid-row-end`，行高与列数仍由 CSS 管。
 * 列宽变了（窗口缩放、列数设置变了）要重算，所以挂一个 ResizeObserver，
 * 宽度没变就不动 DOM。
 */

/** 行高。和 CSS 里 `.igrid { grid-auto-rows }` 保持一致。 */
const ROW_UNIT = 8;

/** 拿不到真实尺寸时的兜底比例，接近多数照片。 */
const FALLBACK_RATIO = 4 / 3;

/** 每个网格一份观察器：宽度变了才重算，免得自己动高度又触发观察形成循环。 */
const observers = new WeakMap<HTMLElement, ResizeObserver>();
const lastWidth = new WeakMap<HTMLElement, number>();

function ratioOf(tile: HTMLElement): number {
  const thumb = tile.querySelector<HTMLElement>(".ph");
  if (!thumb) return FALLBACK_RATIO;
  const raw = getComputedStyle(thumb).getPropertyValue("--ratio").trim();
  if (raw) {
    // --ratio 的写法是「1600 / 900」
    const match = raw.match(/^([\d.]+)\s*\/\s*([\d.]+)$/);
    if (match) {
      const width = Number(match[1]);
      const height = Number(match[2]);
      if (Number.isFinite(width) && Number.isFinite(height) && width > 0 && height > 0) {
        return width / height;
      }
    }
  }
  // 没写比例（老数据、接口没给尺寸）就用图片自己的尺寸，
  // 图片已经加载出来时这个比例最准
  const img = thumb.querySelector("img");
  if (img && img.naturalWidth > 0 && img.naturalHeight > 0) {
    return img.naturalWidth / img.naturalHeight;
  }
  return FALLBACK_RATIO;
}

function measure(grid: HTMLElement): void {
  const tiles = Array.from(grid.children).filter(
    (child): child is HTMLElement => child instanceof HTMLElement,
  );
  if (tiles.length === 0) return;

  const styles = getComputedStyle(grid);
  const gap = Number.parseFloat(styles.rowGap || styles.gap || "0") || 0;
  // 以第一张图的宽度为准：同列同宽，网格保证这一点
  const columnWidth = tiles[0].getBoundingClientRect().width;
  if (columnWidth <= 0) return;

  for (const tile of tiles) {
    const thumb = tile.querySelector<HTMLElement>(".ph");
    const meta = tile.querySelector<HTMLElement>(".meta");
    const thumbHeight = thumb ? columnWidth / ratioOf(tile) : 0;
    // 先按自然高度估，再让 span 往上取整：多出来的那几像素由 .ph 吸收
    // （它是 flex:1），所以不会在卡片里再留下一道缝
    const contentHeight = thumbHeight + (meta?.offsetHeight ?? 0);
    const span = Math.max(1, Math.ceil((contentHeight + gap) / (ROW_UNIT + gap)));
    const next = `span ${span}`;
    if (tile.style.gridRowEnd !== next) tile.style.gridRowEnd = next;
  }
}

/** 排一次，并盯着宽度变化。重复调用是安全的。 */
export function applyMasonry(grid: HTMLElement): void {
  const width = grid.getBoundingClientRect().width;
  lastWidth.set(grid, width);
  measure(grid);

  if (observers.has(grid)) return;
  const observer = new ResizeObserver(() => {
    const current = grid.getBoundingClientRect().width;
    if (Math.abs(current - (lastWidth.get(grid) ?? 0)) < 0.5) return;
    lastWidth.set(grid, current);
    measure(grid);
  });
  observer.observe(grid);
  observers.set(grid, observer);
}
