/**
 * 最小可用的 QR 编码器：字节模式，纠错级别 M，版本 1–10。
 *
 * 为什么不引库、也不用第三方二维码服务：
 * 要编码的是 TOTP 的 `otpauth://` 链接，里面就是二次验证的种子。
 * 把种子拼进外部图片服务的 URL 等于把它交给别人；往这个零运行时依赖的
 * 项目里塞一个二维码库又是几千行没人看得懂的代码。所以只实现用得到的那部分：
 * 字节模式 + 纠错级别 M（屏幕扫描够用）+ 版本 1–10（够放两百多个字节的链接）。
 *
 * 面积上限：版本 10 = 57×57 模块，字节模式 M 级能放 213 字节。
 * 超出就报错，不静默截断——截断的 otpauth 链接扫出来是别的密钥，比报错更坏。
 */

/**
 * 纠错级别 M 的分块表：每个版本一组或多组 [块数, 块总码字, 块数据码字]。
 * 版本 8 以后同一版本里两种块长混用，所以要按组展开而不是乘一下完事。
 */
const M_BLOCKS: number[][][] = [
  [[1, 26, 16]],
  [[1, 44, 28]],
  [[1, 70, 44]],
  [[2, 50, 32]],
  [[2, 67, 43]],
  [[4, 43, 27]],
  [[4, 49, 31]],
  [[2, 60, 38], [2, 61, 39]],
  [[3, 58, 36], [2, 59, 37]],
  [[4, 69, 43], [1, 70, 44]],
];

/** 每个版本的对齐图案中心（版本 1 没有）。 */
const ALIGN_POSITIONS: number[][] = [
  [],
  [6, 18],
  [6, 22],
  [6, 26],
  [6, 30],
  [6, 34],
  [6, 22, 38],
  [6, 24, 42],
  [6, 26, 46],
  [6, 28, 50],
];

/** 纠错级别 M 在格式信息里的两位取值。 */
const ECC_FORMAT_BITS = 0b00;

/** GF(256) 乘法：QR 用的本原多项式是 0x11D。 */
function gfMul(a: number, b: number): number {
  let result = 0;
  for (let i = 7; i >= 0; i -= 1) {
    result = (result << 1) ^ ((result >>> 7) * 0x11d);
    result ^= ((b >>> i) & 1) * a;
  }
  return result & 0xff;
}

/** 生成 degree 次的生成多项式（首一）。 */
function generatorPoly(degree: number): number[] {
  let poly = [1];
  for (let i = 0; i < degree; i += 1) {
    const next = new Array<number>(poly.length + 1).fill(0);
    for (let j = 0; j < poly.length; j += 1) {
      next[j] ^= poly[j];
      next[j + 1] ^= gfMul(poly[j], gfExp(i));
    }
    poly = next;
  }
  return poly;
}

/** GF(256) 上的 2 的幂（alpha 指数表），惰性建一次。 */
const expTable: number[] = [];

function gfExp(power: number): number {
  if (expTable.length === 0) {
    let value = 1;
    for (let i = 0; i < 255; i += 1) {
      expTable[i] = value;
      value = (value << 1) ^ ((value >>> 7) * 0x11d);
    }
  }
  return expTable[power % 255]!;
}

/** 计算一块数据的纠错码字。 */
function eccCodewords(data: number[], eccLength: number): number[] {
  const generator = generatorPoly(eccLength);
  const remainder = new Array<number>(eccLength).fill(0);
  for (const byte of data) {
    const factor = byte ^ remainder[0]!;
    remainder.shift();
    remainder.push(0);
    for (let i = 0; i < eccLength; i += 1) {
      remainder[i] ^= gfMul(generator[i + 1]!, factor);
    }
  }
  return remainder;
}

/** UTF-8 编码（不依赖 TextEncoder，浏览器与 Node 都稳）。 */
function toUtf8(text: string): number[] {
  const out: number[] = [];
  for (const char of text) {
    const code = char.codePointAt(0)!;
    if (code < 0x80) out.push(code);
    else if (code < 0x800) out.push(0xc0 | (code >> 6), 0x80 | (code & 0x3f));
    else if (code < 0x10000) {
      out.push(0xe0 | (code >> 12), 0x80 | ((code >> 6) & 0x3f), 0x80 | (code & 0x3f));
    } else {
      out.push(
        0xf0 | (code >> 18),
        0x80 | ((code >> 12) & 0x3f),
        0x80 | ((code >> 6) & 0x3f),
        0x80 | (code & 0x3f),
      );
    }
  }
  return out;
}

/** 数据区的字节流：模式指示符 + 长度 + 内容 + 终止符 + 填充。 */
function buildDataCodewords(bytes: number[], version: number, capacity: number): number[] {
  const bits: number[] = [];
  const push = (value: number, length: number): void => {
    for (let i = length - 1; i >= 0; i -= 1) bits.push((value >>> i) & 1);
  };
  push(0b0100, 4);
  push(bytes.length, version < 10 ? 8 : 16);
  for (const byte of bytes) push(byte, 8);
  // 终止符最多 4 位，随后补零到字节边界
  const remaining = capacity * 8 - bits.length;
  push(0, Math.min(4, remaining));
  while (bits.length % 8 !== 0) bits.push(0);

  const codewords: number[] = [];
  for (let i = 0; i < bits.length; i += 8) {
    let value = 0;
    for (let j = 0; j < 8; j += 1) value = (value << 1) | bits[i + j]!;
    codewords.push(value);
  }
  // 交替填充字节，直到装满数据容量
  const pads = [0xec, 0x11];
  while (codewords.length < capacity) codewords.push(pads[(codewords.length - 1) % 2]!);
  return codewords;
}

/** 组装最终的码字序列（分块纠错后按列交错）。 */
function buildCodewords(bytes: number[], version: number): number[] {
  const groups = M_BLOCKS[version - 1]!;
  const dataCapacity = groups.reduce((sum, [count, , data]) => sum + count * data, 0);
  const data = buildDataCodewords(bytes, version, dataCapacity);

  const blocks: { data: number[]; ecc: number[] }[] = [];
  let offset = 0;
  for (const [count, totalPerBlock, dataPerBlock] of groups) {
    const eccPerBlock = totalPerBlock - dataPerBlock;
    for (let i = 0; i < count; i += 1) {
      const chunk = data.slice(offset, offset + dataPerBlock);
      offset += dataPerBlock;
      blocks.push({ data: chunk, ecc: eccCodewords(chunk, eccPerBlock) });
    }
  }

  const out: number[] = [];
  const maxData = Math.max(...blocks.map((block) => block.data.length));
  for (let i = 0; i < maxData; i += 1) {
    for (const block of blocks) {
      if (i < block.data.length) out.push(block.data[i]!);
    }
  }
  const maxEcc = Math.max(...blocks.map((block) => block.ecc.length));
  for (let i = 0; i < maxEcc; i += 1) {
    for (const block of blocks) {
      if (i < block.ecc.length) out.push(block.ecc[i]!);
    }
  }
  return out;
}

/** 15 位格式信息：5 位数据 + 10 位 BCH，再异或固定掩码。 */
function formatBits(mask: number): number {
  const data = (ECC_FORMAT_BITS << 3) | mask;
  let bits = data << 10;
  for (let i = 14; i >= 10; i -= 1) {
    if ((bits >>> i) & 1) bits ^= 0b10100110111 << (i - 10);
  }
  return ((data << 10) | bits) ^ 0b101010000010010;
}

/** 18 位版本信息（版本 ≥7 才写）。 */
function versionBits(version: number): number {
  let bits = version << 12;
  for (let i = 17; i >= 12; i -= 1) {
    if ((bits >>> i) & 1) bits ^= 0b1111100100101 << (i - 12);
  }
  return (version << 12) | bits;
}

function maskApplies(mask: number, row: number, col: number): boolean {
  switch (mask) {
    case 0:
      return (row + col) % 2 === 0;
    case 1:
      return row % 2 === 0;
    case 2:
      return col % 3 === 0;
    case 3:
      return (row + col) % 3 === 0;
    case 4:
      return (Math.floor(row / 2) + Math.floor(col / 3)) % 2 === 0;
    case 5:
      return ((row * col) % 2) + ((row * col) % 3) === 0;
    case 6:
      return (((row * col) % 2) + ((row * col) % 3)) % 2 === 0;
    default:
      return (((row + col) % 2) + ((row * col) % 3)) % 2 === 0;
  }
}

/** 掩码惩罚分：四条规则的实现，分低者胜。 */
function penalty(matrix: boolean[][]): number {
  const size = matrix.length;
  let score = 0;

  // 规则 1：同色连续 5 个以上
  const runScore = (line: boolean[]): number => {
    let total = 0;
    let run = 1;
    for (let i = 1; i < line.length; i += 1) {
      if (line[i] === line[i - 1]) {
        run += 1;
      } else {
        if (run >= 5) total += run - 2;
        run = 1;
      }
    }
    if (run >= 5) total += run - 2;
    return total;
  };
  for (let i = 0; i < size; i += 1) {
    score += runScore(matrix[i]!);
    score += runScore(matrix.map((row) => row[i]!));
  }

  // 规则 2：2×2 同色块
  for (let row = 0; row < size - 1; row += 1) {
    for (let col = 0; col < size - 1; col += 1) {
      const value = matrix[row]![col]!;
      if (
        matrix[row + 1]![col] === value &&
        matrix[row]![col + 1] === value &&
        matrix[row + 1]![col + 1] === value
      ) {
        score += 3;
      }
    }
  }

  // 规则 3：1:1:3:1:1 的定位线图案，左右或上下任意一侧有 4 个空白模块即算。
  // 两个 11 位窗口就是规范里那两种写法（空白在左 / 在右）。
  const windowA = [true, false, true, true, true, false, true, false, false, false, false];
  const windowB = [false, false, false, false, true, false, true, true, true, false, true];
  const matches = (line: boolean[], at: number): boolean => {
    const a = windowA.every((expected, index) => line[at + index] === expected);
    if (a) return true;
    return windowB.every((expected, index) => line[at + index] === expected);
  };
  for (let i = 0; i < size; i += 1) {
    const row = matrix[i]!;
    const col = matrix.map((line) => line[i]!);
    for (let at = 0; at + 11 <= size; at += 1) {
      if (matches(row, at)) score += 40;
      if (matches(col, at)) score += 40;
    }
  }

  // 规则 4：黑白比例偏离 50% 的程度
  let dark = 0;
  for (const row of matrix) for (const cell of row) if (cell) dark += 1;
  const percent = (dark * 100) / (size * size);
  score += Math.floor(Math.abs(percent - 50) / 5) * 10;
  return score;
}

/** 摆一个矩阵骨架（功能图案），返回矩阵与「此处是功能模块」的标记。 */
function buildMatrix(version: number): { matrix: boolean[][]; reserved: boolean[][] } {
  const size = version * 4 + 17;
  const matrix: boolean[][] = Array.from({ length: size }, () => new Array<boolean>(size).fill(false));
  const reserved: boolean[][] = Array.from({ length: size }, () => new Array<boolean>(size).fill(false));

  const set = (row: number, col: number, dark: boolean): void => {
    if (row < 0 || col < 0 || row >= size || col >= size) return;
    matrix[row]![col] = dark;
    reserved[row]![col] = true;
  };

  // 定位图案 + 分隔符
  for (const [row, col] of [
    [0, 0],
    [0, size - 7],
    [size - 7, 0],
  ] as const) {
    for (let r = -1; r <= 7; r += 1) {
      for (let c = -1; c <= 7; c += 1) {
        const inside = r >= 0 && r <= 6 && c >= 0 && c <= 6;
        const dark =
          inside &&
          (r === 0 || r === 6 || c === 0 || c === 6 || (r >= 2 && r <= 4 && c >= 2 && c <= 4));
        set(row + r, col + c, dark);
      }
    }
  }

  // 定时图案
  for (let i = 8; i < size - 8; i += 1) {
    set(6, i, i % 2 === 0);
    set(i, 6, i % 2 === 0);
  }

  // 对齐图案
  const positions = ALIGN_POSITIONS[version - 1]!;
  for (const row of positions) {
    for (const col of positions) {
      // 与定位图案重叠的位置不画
      if ((row === 6 && col === 6) || (row === 6 && col === size - 7) || (row === size - 7 && col === 6)) {
        continue;
      }
      for (let r = -2; r <= 2; r += 1) {
        for (let c = -2; c <= 2; c += 1) {
          const dark = Math.max(Math.abs(r), Math.abs(c)) !== 1;
          set(row + r, col + c, dark);
        }
      }
    }
  }

  // 预留格式信息与版本信息的位置（先占位，后面统一写）。
  // i === 6 那一格要让开：它是定时图案的两个端点 (8,6) 与 (6,8)，
  // 占位写 0 会把定时图案打断，扫描器认不出码。
  for (let i = 0; i < 9; i += 1) {
    if (i === 6) continue;
    set(8, i, false);
    set(i, 8, false);
  }
  for (let i = 0; i < 8; i += 1) {
    set(8, size - 1 - i, false);
    set(size - 1 - i, 8, false);
  }
  set(size - 8, 8, true); // 固定的深色模块
  if (version >= 7) {
    for (let i = 0; i < 6; i += 1) {
      for (let j = 0; j < 3; j += 1) {
        set(size - 11 + j, i, false);
        set(i, size - 11 + j, false);
      }
    }
  }
  return { matrix, reserved };
}

/** 把码字按锯齿形铺进矩阵。 */
function placeCodewords(matrix: boolean[][], reserved: boolean[][], codewords: number[]): void {
  const size = matrix.length;
  let bitIndex = 0;
  const totalBits = codewords.length * 8;
  const bitAt = (index: number): boolean => {
    if (index >= totalBits) return false; // 剩余位（remainder bits）补 0
    return ((codewords[index >> 3]! >>> (7 - (index & 7))) & 1) === 1;
  };

  let upward = true;
  for (let col = size - 1; col > 0; col -= 2) {
    // 第 6 列整列都是定时图案：这一组改成从第 5 列起（不是整组跳过——
    // 整组跳过会连带第 5 列一起丢掉，数据全部错位）
    if (col === 6) col -= 1;
    for (let step = 0; step < size; step += 1) {
      const row = upward ? size - 1 - step : step;
      for (const current of [col, col - 1]) {
        if (reserved[row]![current]) continue;
        matrix[row]![current] = bitAt(bitIndex);
        bitIndex += 1;
      }
    }
    upward = !upward;
  }
}

function applyMask(matrix: boolean[][], reserved: boolean[][], mask: number): void {
  for (let row = 0; row < matrix.length; row += 1) {
    for (let col = 0; col < matrix.length; col += 1) {
      if (reserved[row]![col]) continue;
      if (maskApplies(mask, row, col)) matrix[row]![col] = !matrix[row]![col];
    }
  }
}

function writeFormatInfo(matrix: boolean[][], mask: number): void {
  const size = matrix.length;
  const bits = formatBits(mask);
  for (let i = 0; i < 15; i += 1) {
    const dark = ((bits >>> i) & 1) === 1;
    // 第 8 列那一份（左上定位图案右侧 + 左下）
    if (i < 6) matrix[i]![8] = dark;
    else if (i < 8) matrix[i + 1]![8] = dark;
    else matrix[size - 15 + i]![8] = dark;
    // 第 8 行那一份（左上定位图案下方 + 右上）
    if (i < 8) matrix[8]![size - i - 1] = dark;
    else if (i < 9) matrix[8]![15 - i] = dark;
    else matrix[8]![15 - i - 1] = dark;
  }
}

function writeVersionInfo(matrix: boolean[][], version: number): void {
  if (version < 7) return;
  const size = matrix.length;
  const bits = versionBits(version);
  for (let i = 0; i < 18; i += 1) {
    const dark = ((bits >>> i) & 1) === 1;
    const row = Math.floor(i / 3);
    const col = size - 11 + (i % 3);
    matrix[row]![col] = dark;
    matrix[col]![row] = dark;
  }
}

/**
 * 编码结果：true 表示深色模块。
 *
 * `mask` 只在测试与实现对比时固定掩码用，正常调用不要传
 * （不传就是按惩罚分选最优掩码）。
 */
export function encodeQr(text: string, options: { mask?: number } = {}): boolean[][] {
  const bytes = toUtf8(text);
  let version = 0;
  for (let candidate = 1; candidate <= 10; candidate += 1) {
    const groups = M_BLOCKS[candidate - 1]!;
    const capacity = groups.reduce((sum, [count, , data]) => sum + count * data, 0);
    // 模式 4 位 + 长度（版本 10 起 16 位）+ 内容 + 终止符
    const overheadBits = 4 + (candidate < 10 ? 8 : 16);
    if (overheadBits + bytes.length * 8 <= capacity * 8) {
      version = candidate;
      break;
    }
  }
  if (version === 0) {
    throw new Error(`内容太长，放不进版本 10 的二维码（${bytes.length} 字节）`);
  }

  const codewords = buildCodewords(bytes, version);
  const { matrix, reserved } = buildMatrix(version);
  placeCodewords(matrix, reserved, codewords);

  // 八种掩码挑惩罚分最低的（指定了 mask 就只用那一种）
  let best: boolean[][] | null = null;
  let bestScore = Number.POSITIVE_INFINITY;
  const masks = options.mask !== undefined ? [options.mask] : [0, 1, 2, 3, 4, 5, 6, 7];
  for (const mask of masks) {
    const candidate = matrix.map((row) => row.slice());
    const candidateReserved = reserved.map((row) => row.slice());
    applyMask(candidate, candidateReserved, mask);
    writeFormatInfo(candidate, mask);
    writeVersionInfo(candidate, version);
    const score = penalty(candidate);
    if (score < bestScore) {
      bestScore = score;
      best = candidate;
    }
  }
  const result = best!;
  return result;
}

/**
 * 把矩阵画成 SVG。用 SVG 而不是 canvas：缩放不糊，深浅色主题下都能直接看。
 * `quiet` 是四周留几个模块的空白（规范要求 4 个，少了扫描器可能认不出边界）。
 */
export function qrSvg(matrix: boolean[][], options: { size?: number; quiet?: number } = {}): string {
  const quiet = options.quiet ?? 4;
  const size = options.size ?? 200;
  const modules = matrix.length;
  const total = modules + quiet * 2;
  const parts: string[] = [];
  for (let row = 0; row < modules; row += 1) {
    for (let col = 0; col < modules; col += 1) {
      if (matrix[row]![col]) parts.push(`M${col + quiet},${row + quiet}h1v1h-1z`);
    }
  }
  return (
    `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 ${total} ${total}" shape-rendering="crispEdges">` +
    `<rect width="${total}" height="${total}" fill="#ffffff"/>` +
    `<path d="${parts.join("")}" fill="#000000"/>` +
    `</svg>`
  );
}
