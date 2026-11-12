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
}

fn parse_id(raw: &str) -> Result<TaskId, String> {
    raw.parse::<TaskId>()
        .map_err(|_| "任务 id 非法".to_string())
}

#[tauri::command]
pub async fn list_tasks(state: State<'_, AppState>) -> Result<Vec<TaskDto>, String> {
    let tasks = state.manager.list().map_err(|e| e.to_string())?;
    let live = state.manager.live_progress().await;
    Ok(tasks
        .iter()
        .map(|t| TaskDto::from_task(t, live.get(&t.id)))
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
    Ok(SettingsDto {
        port: state.bridge_port,
        token: state.token.clone(),
        download_dir: state.manager.default_dir().to_string_lossy().to_string(),
    })
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
