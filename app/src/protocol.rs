// 处理 idmshadow:// 协议链接。

use idm_shadow_bridge::protocol::{self, ProtocolAction};
use idm_shadow_engine::task::{TaskRequest, TaskSource};
use tauri::{AppHandle, Manager as TauriManager};

use crate::AppState;

/// 从命令行参数里找出协议链接并处理。
pub fn handle_args(app: &AppHandle, argv: &[String]) {
    for arg in argv {
        if !arg.starts_with("idmshadow:") {
            continue;
        }
        let Some(action) = protocol::parse(arg) else {
            continue;
        };
        match action {
            ProtocolAction::Show => {
                show_window(app);
            }
            ProtocolAction::Download { url, name } => {
                show_window(app);
                create_task(app, url, name);
            }
        }
    }
}

fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn create_task(app: &AppHandle, url: String, name: Option<String>) {
    let state = app.state::<AppState>();
    let manager = state.manager.clone();
    tauri::async_runtime::spawn(async move {
        let request = TaskRequest {
            url,
            filename: name,
            source: TaskSource::Protocol,
            ..Default::default()
        };
        match manager.add(request).await {
            Ok(task) => {
                if let Err(err) = manager.start(task.id).await {
                    tracing::error!("协议任务启动失败: {err}");
                }
            }
            Err(err) => tracing::error!("协议任务创建失败: {err}"),
        }
    });
}
