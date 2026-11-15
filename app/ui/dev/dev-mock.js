// 开发时的 Tauri IPC 桩。
//
// 前端在浏览器里跑的时候没有 Tauri 的 invoke，会报 "window.__TAURI_INTERNALS__ 不存在"。
// 这个脚本在 index.html 里按需加载，把 invoke 换成本地假数据，
// 方便在浏览器里调界面。
//
// 只在开发模式（localhost:1420）生效，生产构建不加载。

(function () {
  if (!window.__TAURI_INTERNALS__) {
    const tasks = [
      {
        id: "demo-1",
        engine: "http",
        url: "https://example.com/movie.mp4",
        filename: "movie.mp4",
        path: "C:/Users/ROG/Downloads/movie.mp4",
        status: "downloading",
        total_size: 1024 * 1024 * 512,
        downloaded: 1024 * 1024 * 180,
        speed: 8 * 1024 * 1024,
        connections: 16,
        max_connections: 16,
        error: null,
        file_exists: true,
        created_at: "2026-11-13T00:00:00Z",
        updated_at: "2026-11-13T00:00:00Z"
      },
      {
        id: "demo-2",
        engine: "http",
        url: "https://example.com/archive.zip",
        filename: "archive.zip",
        path: "C:/Users/ROG/Downloads/archive.zip",
        status: "paused",
        total_size: 1024 * 1024 * 64,
        downloaded: 1024 * 1024 * 20,
        speed: 0,
        connections: 0,
        max_connections: 16,
        error: null,
        file_exists: true,
        created_at: "2026-11-13T00:00:00Z",
        updated_at: "2026-11-13T00:00:00Z"
      },
      {
        id: "demo-3",
        engine: "hls",
        url: "https://example.com/stream.m3u8",
        filename: "stream.mp4",
        path: "C:/Users/ROG/Downloads/stream.mp4",
        status: "completed",
        total_size: null,
        downloaded: 1024 * 1024 * 3,
        speed: 0,
        connections: 0,
        max_connections: 16,
        error: null,
        // 文件已经不在保存位置了——界面会把它置灰
        file_exists: false,
        created_at: "2026-11-13T00:00:00Z",
        updated_at: "2026-11-13T00:00:00Z"
      },
      {
        id: "demo-4",
        engine: "http",
        url: "https://example.com/awaiting.bin",
        filename: "awaiting.bin",
        path: "C:/Users/ROG/Downloads/awaiting.bin",
        status: "awaiting_confirm",
        total_size: 1024 * 1024 * 128,
        downloaded: 1024 * 1024 * 12,
        speed: 3 * 1024 * 1024,
        connections: 16,
        max_connections: 16,
        error: null,
        needs_confirm: true,
        file_exists: true,
        created_at: "2026-11-13T00:00:00Z",
        updated_at: "2026-11-13T00:00:00Z"
      }
    ];

    const settings = {
      port: 16888,
      token: "0123456789abcdef0123456789abcdef",
      download_dir: "C:\\Users\\ROG\\Downloads",
      max_connections: 16,
      max_concurrent_tasks: 3,
      default_download_tool: true,
      auto_start: true,
      version: "0.2.2",
      update_endpoint:
        "https://github.com/yefuchen1/idm-shadow/releases/latest/download/latest.json",
      update_check_on_start: true,
      release_notes: null
    };

    window.__TAURI_INTERNALS__ = {
      invoke: async (cmd, args) => {
        switch (cmd) {
          case "list_tasks":
            return tasks;
          case "get_settings":
            return settings;
          case "update_settings":
            // 真机的字段是 camelCase，这里按真机的名字回写。
            if (args?.downloadDir) settings.download_dir = args.downloadDir;
            if (typeof args?.autoStart === "boolean") {
              settings.auto_start = args.autoStart;
            }
            if (typeof args?.defaultDownloadTool === "boolean") {
              settings.default_download_tool = args.defaultDownloadTool;
            }
            if (typeof args?.updateEndpoint === "string") {
              settings.update_endpoint = args.updateEndpoint;
            }
            if (typeof args?.updateCheckOnStart === "boolean") {
              settings.update_check_on_start = args.updateCheckOnStart;
            }
            return null;
          case "check_update":
            return null;
          case "confirm_task": {
            // 真实实现会清掉 needs_confirm 并把状态改回 downloading
            const t = tasks.find((x) => x.id === args.id);
            if (t) {
              t.needs_confirm = false;
              t.status = "downloading";
            }
            return null;
          }
          case "cancel_confirmed_task": {
            const t = tasks.find((x) => x.id === args.id);
            if (t) {
              t.needs_confirm = false;
              t.status = "cancelled";
            }
            return null;
          }
          case "remove_task":
            return null;
          case "open_dialog":
          case "close_dialog":
          case "fit_dialog":
          case "open_file":
          case "show_in_folder":
          case "pause_task":
          case "resume_task":
          case "cancel_task":
          case "retry_task":
          case "set_global_limit":
          case "reveal_file":
          case "add_task":
            return null;
          default:
            console.warn("[dev-mock] 未处理的命令:", cmd);
            return null;
        }
      },
      transformCallback: (cb) => cb,
      convertFileSrc: (p) => p,
      metadata: {},
      plugins: {
        event: {
          listen: async () => () => {},
          emit: async () => {}
        }
      }
    };

    window.__TAURI__ = { event: { listen: async () => () => {} } };
  }
})();
