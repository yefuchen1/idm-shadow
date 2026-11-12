// IDM Shadow 桌面应用。
// 一个 exe 同时干三件事：托盘常驻、本地 HTTP API（油猴/扩展的入口）、
// 以及给前端用的 Tauri 命令。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod protocol;
mod tray;

use std::sync::Arc;

use idm_shadow_engine::http::engine::DownloadConfig;
use idm_shadow_engine::manager::{DownloadManager, TaskEvent};
use idm_shadow_engine::store::Store;
use tauri::{Emitter, Manager as TauriManager};

pub struct AppState {
    pub manager: Arc<DownloadManager>,
    pub bridge_port: u16,
    pub token: String,
}

/// 应用数据目录：%APPDATA%\IDM Shadow
fn data_dir() -> std::path::PathBuf {
    let base = std::env::var("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
    base.join("IDM Shadow")
}

/// 生成一个够用的随机 token。
fn make_token() -> String {
    rand_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

fn rand_bytes() -> [u8; 24] {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    seed ^= std::process::id() as u64;
    let mut out = [0u8; 24];
    for slot in out.iter_mut() {
        // xorshift64
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *slot = (seed >> 24) as u8;
    }
    out
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // 已经有实例在跑：把协议链接转交给它
            protocol::handle_args(app, &argv);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let dir = data_dir();
            std::fs::create_dir_all(&dir).ok();

            let store = Arc::new(Store::open(dir.join("idm-shadow.db"))?);

            let config = DownloadConfig::default();
            let manager = Arc::new(DownloadManager::new(store.clone(), config)?);
            manager.recover_interrupted()?;

            // 恢复上次的设置
            if let Some(dir) = store.get_setting("download_dir")? {
                tauri::async_runtime::block_on(
                    manager.set_default_dir(std::path::PathBuf::from(dir)),
                );
            }
            if let Some(limit) = store
                .get_setting("global_limit")?
                .and_then(|v| v.parse::<u64>().ok())
            {
                manager.set_global_limit(if limit == 0 { None } else { Some(limit) });
            }
            if let Some(n) = store
                .get_setting("max_connections")?
                .and_then(|v| v.parse::<usize>().ok())
            {
                tauri::async_runtime::block_on(async {
                    let mut config = manager.engine().config().await;
                    config.max_connections = n.clamp(1, 64);
                    manager.engine().set_config(config).await;
                });
            }
            if let Some(n) = store
                .get_setting("max_concurrent_tasks")?
                .and_then(|v| v.parse::<usize>().ok())
            {
                tauri::async_runtime::block_on(
                    manager.set_max_concurrent_tasks(if n == 0 { None } else { Some(n) }),
                );
            }

            // token：环境变量 > 上次存的 > 新生成的。
            // 环境变量方便脚本化调用和自动化测试。
            let token = match std::env::var("IDM_SHADOW_TOKEN") {
                Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
                _ => match store.get_setting("bridge_token")? {
                    Some(t) if !t.is_empty() => t,
                    _ => {
                        let t = make_token();
                        store.set_setting("bridge_token", &t)?;
                        t
                    }
                },
            };

            // 本地 HTTP API
            // setup 回调是同步的，这里用 Tauri 的阻塞运行时跑一小段异步。
            let port = tauri::async_runtime::block_on(idm_shadow_bridge::find_free_port(16888))
                .unwrap_or(16888);
            store.set_setting("bridge_port", &port.to_string())?;

            let bridge_state = idm_shadow_bridge::BridgeState {
                manager: manager.clone(),
                token: Arc::new(token.clone()),
                port,
            };
            tauri::async_runtime::spawn(async move {
                if let Err(err) = idm_shadow_bridge::serve(bridge_state, port).await {
                    tracing::error!("bridge 启动失败: {err}");
                }
            });

            // 注册自定义协议
            if let Ok(exe) = std::env::current_exe() {
                if let Err(err) = idm_shadow_bridge::protocol::register(&exe) {
                    tracing::warn!("注册 idmshadow:// 协议失败: {err}");
                }
            }

            app.manage(AppState {
                manager: manager.clone(),
                bridge_port: port,
                token: token.clone(),
            });

            // 调度器：定期把排队里的任务捞起来
            let scheduler = manager.clone();
            tauri::async_runtime::spawn(async move {
                scheduler.run_scheduler().await;
            });

            // 把引擎事件转发给前端
            let handle = app.handle().clone();
            let mut events = manager.subscribe();
            tauri::async_runtime::spawn(async move {
                while let Ok(event) = events.recv().await {
                    match event {
                        TaskEvent::Progress(id, progress) => {
                            let _ = handle.emit(
                                "task-progress",
                                serde_json::json!({
                                    "id": id.to_string(),
                                    "downloaded": progress.downloaded,
                                    "total": progress.total,
                                    "speed": progress.speed,
                                    "connections": progress.connections,
                                }),
                            );
                        }
                        TaskEvent::StatusChanged(id, status) => {
                            let _ = handle.emit(
                                "task-status",
                                serde_json::json!({
                                    "id": id.to_string(),
                                    "status": status.as_str(),
                                }),
                            );
                        }
                    }
                }
            });

            tray::build(app.handle())?;

            // 启动时处理协议链接
            let argv: Vec<String> = std::env::args().collect();
            protocol::handle_args(app.handle(), &argv);

            Ok(())
        })
        .on_window_event(|window, event| {
            // 关窗口只是隐藏，下载继续在托盘里跑
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_tasks,
            commands::add_task,
            commands::pause_task,
            commands::resume_task,
            commands::cancel_task,
            commands::retry_task,
            commands::remove_task,
            commands::get_settings,
            commands::update_settings,
            commands::set_global_limit,
            commands::reveal_file,
        ])
        .run(tauri::generate_context!())
        .expect("应用启动失败");
}
