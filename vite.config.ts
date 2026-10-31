/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri 开发时固定端口，便于 tauri.conf.json 里的 devUrl 对应
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 1420, strictPort: true, host: "127.0.0.1" },
  build: { target: "es2022", outDir: "dist", sourcemap: false },
  // 回收站（.trash/）里是手动删除的旧文件，不参与测试
  test: { exclude: ["**/node_modules/**", "**/dist/**", ".trash/**"] },
});
