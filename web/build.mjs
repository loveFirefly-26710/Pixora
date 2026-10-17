// 前端构建脚本。
//
// 不引入打包框架：esbuild 一次性把 main.ts 打成单个 ESM 文件，
// 两个 Worker 单独打包（它们要能独立 close，不能混进主包）。
// 产物落在 web/dist，由 pixora-server 用 rust-embed 嵌进二进制——
// 部署时只有一个可执行文件，不需要挂载静态目录。

import { build, context } from "esbuild";
import { cp, mkdir, rm, readdir } from "node:fs/promises";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.dirname(fileURLToPath(import.meta.url));
const outdir = path.join(root, "dist");
const watch = process.argv.includes("--watch");
const production = process.argv.includes("--production");

/** 公共选项：目标浏览器覆盖到 Safari 15（移动端要能跑）。 */
const shared = {
  bundle: true,
  format: "esm",
  target: ["es2022", "safari15", "chrome100", "firefox100"],
  sourcemap: production ? false : "inline",
  minify: production,
  logLevel: "info",
  legalComments: "none",
};

const entries = [
  { in: "src/main.ts", out: "main" },
  { in: "src/worker/hash.worker.ts", out: "workers/hash.worker" },
  { in: "src/worker/thumb.worker.ts", out: "workers/thumb.worker" },
];

/**
 * 静态文件：首页、图标与 manifest。
 *
 * 前端只跟后端同源部署，接口走相对路径。
 */
async function copyStatic() {
  await mkdir(outdir, { recursive: true });
  await cp(path.join(root, "src/index.html"), path.join(outdir, "index.html"));
  for (const asset of ["favicon.svg", "manifest.webmanifest"]) {
    const from = path.join(root, "src", asset);
    if (existsSync(from)) await cp(from, path.join(outdir, asset));
  }
}

async function run() {
  if (production) await rm(outdir, { recursive: true, force: true });
  await copyStatic();

  if (watch) {
    for (const entry of entries) {
      const ctx = await context({
        ...shared,
        entryPoints: [path.join(root, entry.in)],
        outfile: path.join(outdir, `${entry.out}.js`),
      });
      await ctx.watch();
    }
    console.log("web: 监听中（dist/ 为产物目录）");
    return;
  }

  await Promise.all(
    entries.map((entry) =>
      build({
        ...shared,
        entryPoints: [path.join(root, entry.in)],
        outfile: path.join(outdir, `${entry.out}.js`),
      }),
    ),
  );

  const produced = await readdir(outdir, { recursive: true });
  const js = produced.filter((file) => file.endsWith(".js"));
  console.log(`web: 构建完成，${js.length} 个脚本入口 → ${outdir}`);
}

run().catch((err) => {
  console.error(err);
  process.exit(1);
});
