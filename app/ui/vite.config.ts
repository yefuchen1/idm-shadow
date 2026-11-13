import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join, resolve } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));

export default defineConfig({
  plugins: [
    react(),
    {
      // 开发时在浏览器里跑，注入 Tauri IPC 的桩，方便单独调界面。
      // 生产构建不注入——真实环境��� Tauri 自己会提供 invoke。
      name: "dev-tauri-mock",
      apply: "serve",
      transformIndexHtml(html: string) {
        // 直接把桩代码内联进去，dev 目录里的文件就不会被打进生产包
        const mock = readFileSync(join(here, "dev", "dev-mock.js"), "utf8");
        return {
          html,
          tags: [
            {
              tag: "script",
              children: mock,
              injectTo: "body"
            }
          ]
        };
      }
    }
  ],
  clearScreen: false,
  // 两个入口：主窗口（任务列表）和下载对话框（独立小窗口）
  build: {
    outDir: "dist",
    target: "es2021",
    rollupOptions: {
      input: {
        main: resolve(here, "index.html"),
        dlg: resolve(here, "dlg.html")
      }
    },
    sourcemap: !!process.env.TAURI_ENV_DEBUG
  },
  server: {
    port: 1420,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**"] }
  }
});
