import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import { resolve, join, extname } from "node:path";
import { readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { constants, gzipSync } from "node:zlib";

const outDir = resolve(import.meta.dirname, "dist-mobile");

/**
 * 构建完给文本文件（大于 1 KB 的 js、css、html、svg、json）各生成一份 .gz。
 * 手机入口在手机浏览器接受 gzip 时直接发压缩好的，第一次打开（尤其走 Tailscale 中转时）快很多。
 */
function gzipAssets(): Plugin {
  const exts = new Set([".js", ".css", ".html", ".svg", ".json"]);
  const walk = (dir: string) => {
    for (const name of readdirSync(dir)) {
      const path = join(dir, name);
      if (statSync(path).isDirectory()) { walk(path); continue; }
      if (!exts.has(extname(name))) continue;
      const raw = readFileSync(path);
      if (raw.length >= 1024) writeFileSync(`${path}.gz`, gzipSync(raw, { level: constants.Z_BEST_COMPRESSION }));
    }
  };
  return { name: "pierlink-gzip", apply: "build", closeBundle: () => walk(outDir) };
}

// 手机页面：单独打包到 dist-mobile，安装包里作为资源，由 Windows 上的手机入口提供给手机浏览器
export default defineConfig({
  root: resolve(import.meta.dirname, "mobile"),
  publicDir: false,
  plugins: [react(), gzipAssets()],
  clearScreen: false,
  server: { port: 1430, strictPort: true, host: "127.0.0.1", fs: { allow: [resolve(import.meta.dirname)] } },
  build: { target: "es2020", outDir, emptyOutDir: true, sourcemap: false,
    // 不把小文件内联成 data: 地址：手机入口的 CSP 只允许同源字体和图片
    assetsInlineLimit: 0 },
});
