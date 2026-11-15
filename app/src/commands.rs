// 给前端用的 Tauri 命令。

use idm_shadow_bridge::TaskDto;
use idm_shadow_engine::task::{TaskId, TaskRequest};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager as TauriManager, State, WebviewUrl, WebviewWindowBuilder};

use crate::AppState;

/// 检查是否有新版本。没有更新时 `version` 为空表示已是最新。
#[tauri::command]
pub async fn check_update(
    app: AppHandle,
    state: State<'_, AppState>,
    endpoint: Option<String>,
) -> Result<Option<crate::update::UpdateInfo>, String> {
    crate::update::check(&app, &state.manager, endpoint.as_deref()).await
}

/// 下载并安装更新。成功后界面会收到 `update-progress` 事件。
#[tauri::command]
pub async fn install_update(
    app: AppHandle,
    state: State<'_, AppState>,
    endpoint: Option<String>,
) -> Result<(), String> {
    crate::update::install(&app, &state.manager, endpoint.as_deref()).await
}

/// 用户关掉「本次更新内容」提示，不再重复弹。
#[tauri::command]
pub async fn dismiss_release_notes(state: State<'_, AppState>) -> Result<(), String> {
    crate::update::acknowledge_release_notes(&state.manager);
    Ok(())
}

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
    let window = WebviewWindowBuilder::new(&app, &label, url)
        .title("IDM Shadow")
        // 先给个大概的尺寸，等前端量完内容高度再收——
        // 前端会调 fit_dialog 把窗口改成刚好包住内容。
        // 一开始就隐藏，免得用户看到尺寸跳一下。
        .inner_size(440.0, 220.0)
        .min_inner_size(360.0, 120.0)
        .visible(false)
        .resizable(false)
        .always_on_top(true)
        .build()
        .map_err(|e| e.to_string())?;

    // 窗口被关掉（用户点 X，或者我们主动关）时通知前端。
    //
    // 前端靠这个事件把"这个任务已经开过对话框"的标记摘掉，
    // 队列里的下一个待确认任务才能接着弹。
    // 没有它的话，待确认任务一多就会一个接一个地开窗口，
    // 每个都还关不掉。
    let task_id = id.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::Destroyed = event {
            let _ = app.emit(
                "dlg-closed",
                serde_json::json!({ "id": task_id }),
            );
        }
    });

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

/// 把对话框窗口调整到刚好包住内容。
///
/// 每种状态的内容高度差很多：选保存位置时有进度条+两个按钮+目录行+
/// 勾选框，下完只有一行状态+三个按钮。固定 460x340 的话，
/// "下载完成"那屏底下会空出一大片——看着很松散。
///
/// 高度由前端量出来传进来（前端才知道自己多高），
/// 宽度保持建窗口时定的值。
#[tauri::command]
pub async fn fit_dialog(app: AppHandle, id: String, height: f64) -> Result<(), String> {
    let Some(window) = app.get_webview_window(&dialog_label(&id)) else {
        return Ok(());
    };

    let size = window.inner_size().map_err(|e| e.to_string())?;
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let width = size.width as f64 / scale;

    // 夹一下，别因为某个瞬间的测量结果把窗口搞成一条缝或者一大块。
    // set_size 设的是客户区（webview）尺寸，标题栏不算在内，
    // 所以前端量多少就用多少，不要再加。
    let height = height.clamp(110.0, 560.0);
    tracing::debug!(task = %id, height, width, "fit_dialog");
    window
        .set_size(tauri::LogicalSize::new(width, height))
        .map_err(|e| e.to_string())?;

    // 第一次量好之后才显示，避免用户看到窗口闪一下大小。
    if !window.is_visible().unwrap_or(true) {
        let _ = window.show();
        let _ = window.set_focus();
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
    pub version: String,
    pub update_endpoint: String,
    pub update_check_on_start: bool,
    /// 刚更新完、还没被用户关掉的发布说明。
    pub release_notes: Option<crate::update::ReleaseNotes>,
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
        auto_start: idm_shadow_bridge::is_auto_start_setting(&state.manager),
        version: env!("CARGO_PKG_VERSION").to_string(),
        update_endpoint: crate::update::endpoint_setting(&state.manager),
        update_check_on_start: crate::update::check_on_start_setting(&state.manager),
        release_notes: crate::update::pending_release_notes(&state.manager),
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
    update_endpoint: Option<String>,
    update_check_on_start: Option<bool>,
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
        idm_shadow_bridge::set_auto_start_setting(&state.manager, enabled)
            .map_err(|e| e.to_string())?;
    }
    if let Some(endpoint) = update_endpoint {
        crate::update::set_endpoint_setting(&state.manager, &endpoint)
            .map_err(|e| e.to_string())?;
    }
    if let Some(enabled) = update_check_on_start {
        state
            .manager
            .store()
            .set_setting("update_check_on_start", if enabled { "1" } else { "0" })
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
