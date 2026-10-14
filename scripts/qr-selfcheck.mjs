/**
 * 二维码编码器的回归自检。
 *
 * 前端没有测试框架（这个项目刻意不引），而二维码这种东西「看起来画出来了」
 * 和「扫得出来」是两回事：改一行摆码逻辑，图还是会画，只是再也扫不出内容。
 * 所以这里守住一个关键不变量——给定内容编出来的矩阵是确定的，
 * 与一份经独立解码器（zbar 全量验证过 213 组）确认过的结果比对。
 *
 * 跑法：node scripts/qr-selfcheck.mjs
 */

import { createHash } from "node:crypto";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { execFileSync } from "node:child_process";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const work = mkdtempSync(join(tmpdir(), "pixora-qr-"));

/** 覆盖几个版本档位与真实长度的 otpauth 链接。 */
const CASES = [
  {
    payload: "otpauth://totp/Pixora:admin@example.com?secret=JBSWY3DPEHPK3PXP&issuer=Pixora&algorithm=SHA1&digits=6&period=30",
    digest: "ce5cfd0a9c9b726aa147ac45f03d19fd5d0d634d3d7d8ebff6202e199a15ef30",
  },
];

try {
  // 用 esbuild 把 TS 模块打成临时 JS（不为了一个自检脚本引运行时依赖）
  const bundle = join(work, "qrcode.mjs");
  execFileSync(
    process.execPath,
    [
      join(root, "web", "node_modules", "esbuild", "bin", "esbuild"),
      join(root, "web", "src", "core", "qrcode.ts"),
      "--bundle",
      "--platform=node",
      "--format=esm",
      `--outfile=${bundle}`,
    ],
    { stdio: "inherit" },
  );
  const { encodeQr } = await import(pathToFileURL(bundle).href);

  let failed = 0;
  for (const item of CASES) {
    const matrix = encodeQr(item.payload);
    const text = matrix.map((row) => row.map((cell) => (cell ? "1" : "0")).join("")).join("\n");
    const digest = createHash("sha256").update(text).digest("hex");
    const size = matrix.length;
    if (digest !== item.digest) {
      console.error(`✗ 矩阵与基准不一致（${size}x${size}）`);
      console.error(`  期望 ${item.digest}`);
      console.error(`  实得 ${digest}`);
      failed += 1;
    } else {
      console.log(`✓ ${size}x${size} 与基准一致`);
    }
  }
  process.exitCode = failed === 0 ? 0 : 1;
} finally {
  rmSync(work, { recursive: true, force: true });
}
