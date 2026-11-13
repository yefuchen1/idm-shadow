// 给前端用的 Tauri 命令。

use idm_shadow_bridge::TaskDto;
use idm_shadow_engine::task::{TaskId, TaskRequest};
use serde::Serialize;
use tauri::{AppHandle, Manager as TauriManager, State, WebviewUrl, WebviewWindowBuilder};

use crate::AppState;

/// 下载对话框的窗口 label。每个任务一个窗口。
pub fn dialog_label(task_id: &str) -> String {
    format!("dlg-{task_id}")
}

/// 打开（或聚焦）某个任务的下载对话框。
///
/// 下载对话框是独立的小窗口，不跟任务列表挤在一起——
/// 列表可能很长，对话框只有一屏，混在一起两边都不好用。
#[tauri::command]
pub async fn open_dialog(
    app: AppHandle,
    id: String,
) -> Result<(), String> {
    let label = dialog_label(&id);

    // 已经开着了就把它叫到前面
    if let Some(window) = app.get_webview_window(&label) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        return Ok(());
    }

    let url = WebviewUrl::App(format!("dlg.html?task={}", id).into());
    WebviewWindowBuilder::new(&app, &label, url)
        .title("IDM Shadow")
        .inner_size(460.0, 340.0)
        .min_inner_size(380.0, 280.0)
        .resizable(false)
        .always_on_top(true)
        .build()
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// 关闭某个任务的下载对话框。
#[tauri::command]
pub async fn close_dialog(app: AppHandle, id: String) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(&dialog_label(&id)) {
        window.close().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 打开下载好的文件。
#[tauri::command]
pub async fn open_file(app: AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_path(path, None::<&str>)
        .map_err(|e| e.to_string())
}

/// 在文件管理器里定位下载好的文件。
#[tauri::command]
pub async fn show_in_folder(app: AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .reveal_item_in_dir(&path)
        .map_err(|e| e.to_string())
}

#[derive(Debug, Serialize)]
pub struct SettingsDto {
    pub port: u16,
    pub token: String,
    pub download_dir: String,
    pub max_connections: u32,
    pub max_concurrent_tasks: u32,
    pub default_download_tool: bool,
    pub auto_start: bool,
}

fn parse_id(raw: &str) -> Result<TaskId, String> {
    raw.parse::<TaskId>()
        .map_err(|_| "任务 id 非法".to_string())
}

#[tauri::command]
pub async fn list_tasks(state: State<'_, AppState>) -> Result<Vec<TaskDto>, String> {
    let tasks = state.manager.list().map_err(|e| e.to_string())?;
    let live = state.manager.live_progress().await;
    let default_max = state.manager.engine().config().await.max_connections as u32;
    Ok(tasks
        .iter()
        .map(|t| TaskDto::from_task(t, live.get(&t.id), default_max))
        .collect())
}

#[tauri::command]
pub async fn add_task(
    state: State<'_, AppState>,
    url: String,
    filename: Option<String>,
    dir: Option<String>,
    needs_confirm: Option<bool>,
) -> Result<String, String> {
    if url.trim().is_empty() {
        return Err("URL 不能为空".to_string());
    }

    let request = TaskRequest {
        url,
        filename,
        dir,
        needs_confirm: needs_confirm.unwrap_or(false),
        ..Default::default()
    };

    let task = state.manager.add(request).await.map_err(|e| e.to_string())?;
    state
        .manager
        .start(task.id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(task.id.to_string())
}

/// 用户确认保存位置。`path` 为 None 表示用默认目录。
#[tauri::command]
pub async fn confirm_task(
    state: State<'_, AppState>,
    id: String,
    dir: Option<String>,
) -> Result<(), String> {
    let id = parse_id(&id)?;
    // 只给目录时，目录 + 原文件名才是最终落盘位置。
    //
    // 之前这里把目录直接当成了完整路径，结果文件会落在
    // "D:\chosen-dir" 这个目录名上，而不是 "D:\chosen-dir\原文件名"。
    let path = match dir.filter(|d| !d.trim().is_empty()) {
        Some(d) => {
            let task = state
                .manager
                .get(id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "任务不存在".to_string())?;
            Some(std::path::PathBuf::from(d).join(&task.filename))
        }
        None => None,
    };
    state
        .manager
        .confirm_path(id, path)
        .await
        .map_err(|e| e.to_string())
}

/// 用户在确认弹窗里点了取消：删掉已下载的暂存文件。
#[tauri::command]
pub async fn cancel_confirmed_task(
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let id = parse_id(&id)?;
    state
        .manager
        .cancel_with_cleanup(id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn pause_task(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let id = parse_id(&id)?;
    state.manager.pause(id).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn resume_task(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let id = parse_id(&id)?;
    state.manager.resume(id).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn cancel_task(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let id = parse_id(&id)?;
    state.manager.cancel(id).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn retry_task(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let id = parse_id(&id)?;
    state.manager.retry(id).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn remove_task(
    state: State<'_, AppState>,
    id: String,
    delete_file: bool,
) -> Result<(), String> {
    let id = parse_id(&id)?;
    state
        .manager
        .remove(id, delete_file)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> Result<SettingsDto, String> {
    let config = state.manager.engine().config().await;
    Ok(SettingsDto {
        port: state.bridge_port,
        token: state.token.clone(),
        download_dir: state.manager.default_dir().await.to_string_lossy().to_string(),
        max_connections: config.max_connections as u32,
        max_concurrent_tasks: state.manager.max_concurrent_tasks().await.unwrap_or(0) as u32,
        default_download_tool: idm_shadow_bridge::is_default_download_tool(&state.manager),
        auto_start: idm_shadow_bridge::system::is_auto_start(),
    })
}

/// 一次性更新所有设置。
#[tauri::command]
pub async fn update_settings(
    state: State<'_, AppState>,
    download_dir: Option<String>,
    global_limit: Option<u64>,
    max_connections: Option<u32>,
    max_concurrent_tasks: Option<u32>,
    default_download_tool: Option<bool>,
    auto_start: Option<bool>,
) -> Result<(), String> {
    if let Some(dir) = download_dir {
        let path = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
        state.manager.set_default_dir(path.clone()).await;
        state
            .manager
            .store()
            .set_setting("download_dir", &path.to_string_lossy())
            .map_err(|e| e.to_string())?;
    }
    if let Some(limit) = global_limit {
        state.manager.set_global_limit(if limit == 0 { None } else { Some(limit) });
        state
            .manager
            .store()
            .set_setting("global_limit", &limit.to_string())
            .map_err(|e| e.to_string())?;
    }
    if let Some(n) = max_connections {
        let n = n.clamp(1, 64);
        let mut config = state.manager.engine().config().await;
        config.max_connections = n as usize;
        state.manager.engine().set_config(config).await;
        state
            .manager
            .store()
            .set_setting("max_connections", &n.to_string())
            .map_err(|e| e.to_string())?;
    }
    if let Some(n) = max_concurrent_tasks {
        let limit = if n == 0 { None } else { Some(n as usize) };
        state.manager.set_max_concurrent_tasks(limit).await;
        state
            .manager
            .store()
            .set_setting("max_concurrent_tasks", &n.to_string())
            .map_err(|e| e.to_string())?;
    }
    if let Some(enabled) = default_download_tool {
        idm_shadow_bridge::set_default_download_tool(&state.manager, enabled)
            .map_err(|e| e.to_string())?;
    }
    if let Some(enabled) = auto_start {
        let exe = idm_shadow_bridge::system::current_exe()
            .ok_or_else(|| "找不到程序自身路径".to_string())?;
        idm_shadow_bridge::system::set_auto_start(enabled, &exe)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub async fn set_global_limit(
    state: State<'_, AppState>,
    bytes_per_second: Option<u64>,
) -> Result<(), String> {
    state.manager.set_global_limit(bytes_per_second);
    if let Some(limit) = bytes_per_second {
        state
            .manager
            .store()
            .set_setting("global_limit", &limit.to_string())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 在文件管理器里显示某个文件。
#[tauri::command]
pub async fn reveal_file(path: String) -> Result<(), String> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg("/select,")
            .arg(&path)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
    Ok(())
}
