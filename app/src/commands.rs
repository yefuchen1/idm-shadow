// 给前端用的 Tauri 命令。

use idm_shadow_bridge::TaskDto;
use idm_shadow_engine::task::{TaskId, TaskRequest};
use serde::Serialize;
use tauri::State;

use crate::AppState;

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
) -> Result<String, String> {
    if url.trim().is_empty() {
        return Err("URL 不能为空".to_string());
    }

    let request = TaskRequest {
        url,
        filename,
        dir,
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
