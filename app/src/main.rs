// IDM Shadow 桌面应用。
// 一个 exe 同时干三件事：托盘常驻、本地 HTTP API（油猴/扩展的入口）、
// 以及给前端用的 Tauri 命令。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod extension;
mod protocol;
mod tray;
mod update;

use std::sync::Arc;

use idm_shadow_engine::http::engine::DownloadConfig;
use idm_shadow_engine::manager::{DownloadManager, TaskEvent};
use idm_shadow_engine::store::Store;
use tauri::{Emitter, Manager as TauriManager};

pub struct AppState {
    pub manager: Arc<DownloadManager>,
    pub bridge_port: u16,
    pub token: String,
    /// 浏览器扩展最近一次带着 token 调接口的时间，界面用它显示连接状态。
    pub extension_seen: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    /// 扩展最近一次自报的版本号。
    ///
    /// 浏览器会缓存已解压扩展的注册信息，磁盘文件换了新版它照样跑旧代码。
    /// 靠这个版本号才能发现"跑着旧代码的扩展"——那和"扩展没装"是两回事，
    /// 但对用户来说症状一样：点下载一点反应都没有。
    pub extension_version: Arc<std::sync::Mutex<Option<String>>>,
}

/// 应用数据目录：%APPDATA%\IDM Shadow
fn data_dir() -> std::path::PathBuf {
    // 自动化测试要跑在独立目录里——不然测试的清理动作会动到用户真实的任务库。
    if let Ok(dir) = std::env::var("IDM_SHADOW_DATA_DIR") {
        if !dir.trim().is_empty() {
            return std::path::PathBuf::from(dir);
        }
    }
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

    // 接管 BT 库的调试日志。
    //
    // 库会把自己每一条调试信息无条件写进当前工作目录的 `debug.log`，
    // 没有级别过滤、没有轮转，每写一条还 flush 一次。
    // 一个卡住的 BT 任务半小时能写 9 万多行、7.4MB，全是噪音。
    //
    // 接管之后库不再写文件，消息改由 `tracing` 转发——
    // 默认级别是 info，看不到；用户要排查时设 `RUST_LOG=debug` 就能拿到。
    idm_shadow_engine::bt::init_logging();

    // 自动化测试要能跟用户正在跑的实例并存，所以给它一个独立端口。
    // 单实例插件会直接把第二个进程转交给第一个、让它啥也不干——
    // 测试里表现就是"bridge 起不来"，很难查。
    let isolated = std::env::var("IDM_SHADOW_PORT")
        .ok()
        .and_then(|p| p.trim().parse::<u16>().ok())
        .filter(|p| *p > 0)
        .is_some();

    let mut builder = tauri::Builder::default();
    if !isolated {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // 已经有实例在跑：把协议链接转交给它
            protocol::handle_args(app, &argv);
        }));
    }

    builder
        .plugin(tauri_plugin_updater::Builder::new().build())
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
            //
            // 端口可以由环境变量指定（IDM_SHADOW_PORT）。
            // 这样自动化测试能跑在别的端口上，不用先把用户正在用的实例关掉。
            let start_port = std::env::var("IDM_SHADOW_PORT")
                .ok()
                .and_then(|p| p.trim().parse::<u16>().ok())
                .filter(|p| *p > 0)
                .unwrap_or(16888);
            let port = tauri::async_runtime::block_on(idm_shadow_bridge::find_free_port(start_port))
                .unwrap_or(start_port);
            store.set_setting("bridge_port", &port.to_string())?;

            // AppHandle 是 Send + Sync，能进 Arc 闭包；App 不是。
            let app_handle = app.handle().clone();
            let extension_seen = Arc::new(std::sync::Mutex::new(None));
            let extension_version = Arc::new(std::sync::Mutex::new(None));
            // 随包发布的扩展版本号。扩展拿它跟自己比的版本对一下，
            // 就知道自己是不是旧版。
            let shipped_extension_version = Arc::new(extension::shipped_version());
            let bridge_state = idm_shadow_bridge::BridgeState {
                manager: manager.clone(),
                token: Arc::new(token.clone()),
                port,
                // 扩展点「打开主界面」时调它，把窗口叫到前面。
                // 用 AppHandle 而不是 App——后者不是 Sync，进不了 Arc。
                show_ui: Some(Arc::new(move || {
                    let handle = app_handle.clone();
                    if let Some(window) = handle.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.unminimize();
                        let _ = window.set_focus();
                    }
                })),
                extension_seen: extension_seen.clone(),
                extension_version: extension_version.clone(),
                shipped_extension_version: shipped_extension_version.clone(),
            };

            // 扩展文件先落地，bridge 再开。
            //
            // 顺序反了会出事：扩展可能先从应用那儿问到"有新版"，
            // 立刻把自己重载——那会儿文件还没写完，它重载起来还是旧代码，
            // 然后又发现落后，又重载…… 变成个死循环。
            extension::sync_on_startup();

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
                extension_seen,
                extension_version,
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
                                    "note": progress.note,
                                    "dht_nodes": progress.dht_nodes,
                                    "segments": progress.segments.iter().map(|s| serde_json::json!({
                                        "start": s.start,
                                        "end": s.end,
                                        "speed": s.speed,
                                    })).collect::<Vec<_>>(),
                                    "pieces": progress.pieces.as_ref().map(|ps| serde_json::json!({
                                        "total": ps.total,
                                        "verified": ps.verified,
                                        "corrupted": ps.corrupted,
                                        "verification_failures": ps.verification_failures,
                                        "cells": ps.cells.iter().map(|c| serde_json::json!({
                                            "from": c.from,
                                            "to": c.to,
                                            "state": match c.state {
                                                idm_shadow_engine::http::engine::PieceState::Missing => "missing",
                                                idm_shadow_engine::http::engine::PieceState::Verified => "verified",
                                                idm_shadow_engine::http::engine::PieceState::Partial => "partial",
                                                idm_shadow_engine::http::engine::PieceState::Corrupted => "corrupted",
                                            },
                                            "availability": c.availability,
                                        })).collect::<Vec<_>>(),
                                    })),
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
                        TaskEvent::ConfirmNeeded(id) => {
                            // 有下载在等用户确认保存位置。
                            // 下载对话框是独立的小窗口，不在这里弹——
                            // 主窗口是任务列表，可能很长，盖一个模态框
                            // 两边都别扭。这里只发事件，主窗口那边去开小窗口。
                            let _ = handle.emit(
                                "task-confirm-needed",
                                serde_json::json!({ "id": id.to_string() }),
                            );
                        }
                    }
                }
            });

            tray::build(app.handle())?;

            // 启动时处理协议链接
            let argv: Vec<String> = std::env::args().collect();
            protocol::handle_args(app.handle(), &argv);

            // 开机自启只进托盘；正常启动才显示主窗口。
            // 窗口在 tauri.conf.json 里是 visible:false，等这里决定。
            let autostart = argv.iter().any(|arg| arg == idm_shadow_bridge::system::AUTOSTART_ARG);
            if !autostart {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }

            // 注册表按数据库里的选择对账（路径变了会重写）。
            idm_shadow_bridge::reconcile_auto_start(&manager);

            // 启动后静默检查一次更新，不自动安装。
            update::spawn_startup_check(app.handle().clone(), manager.clone());

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
            commands::confirm_task,
            commands::cancel_confirmed_task,
            commands::pause_task,
            commands::resume_task,
            commands::cancel_task,
            commands::retry_task,
            commands::remove_task,
            commands::get_settings,
            commands::update_settings,
            commands::set_global_limit,
            commands::reveal_file,
            commands::open_dialog,
            commands::close_dialog,
            commands::fit_dialog,
            commands::open_file,
            commands::show_in_folder,
            commands::check_update,
            commands::install_update,
            commands::dismiss_release_notes,
            commands::extension_status,
            commands::install_extension,
            commands::open_extension_folder,
        ])
        .run(tauri::generate_context!())
        .expect("应用启动失败");
}
