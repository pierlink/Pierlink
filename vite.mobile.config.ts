import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { resolve } from "node:path";

// 手机页面：单独打包到 dist-mobile，安装包里作为资源，由 Windows 上的手机入口提供给手机浏览器
export default defineConfig({
  root: resolve(import.meta.dirname, "mobile"),
  publicDir: false,
  plugins: [react()],
  clearScreen: false,
  server: { port: 1430, strictPort: true, host: "127.0.0.1", fs: { allow: [resolve(import.meta.dirname)] } },
  build: { target: "es2020", outDir: resolve(import.meta.dirname, "dist-mobile"), emptyOutDir: true, sourcemap: false,
    // 不把小文件内联成 data: 地址：手机入口的 CSP 只允许同源字体和图片
    assetsInlineLimit: 0 },
});
