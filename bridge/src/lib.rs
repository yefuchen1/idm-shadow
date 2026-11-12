//! 本地 HTTP API：油猴脚本与浏览器扩展的接入口。
//!
//! 监听 `127.0.0.1`，CORS 全开（`127.0.0.1` 在 Chrome 里属于可信来源，
//! HTTPS 页面可以直连）。鉴权用 token，防止本机其它网页乱投任务。

pub mod protocol;
pub mod system;

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tower_http::cors::{Any, CorsLayer};

use idm_shadow_engine::manager::DownloadManager;
use idm_shadow_engine::task::{EngineKind, Task, TaskId, TaskRequest};

pub const TOKEN_HEADER: &str = "x-idm-shadow-token";

#[derive(Clone)]
pub struct BridgeState {
    pub manager: Arc<DownloadManager>,
    pub token: Arc<String>,
    pub port: u16,
}

/// 统一错误响应。
#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: ErrorBody,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

impl ApiError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: code.to_string(),
                message: message.into(),
            },
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, Json(self)).into_response()
    }
}

/// 任务摘要。
#[derive(Debug, Serialize)]
pub struct TaskDto {
    pub id: String,
    pub engine: String,
    pub url: String,
    pub filename: String,
    pub path: String,
    pub status: String,
    pub total_size: Option<u64>,
    pub downloaded: u64,
    pub speed: u64,
    pub connections: usize,
    pub max_connections: u32,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl TaskDto {
    pub fn from_task(
        task: &Task,
        live: Option<&idm_shadow_engine::http::TaskProgress>,
    ) -> Self {
        Self {
            id: task.id.to_string(),
            engine: task.engine.as_str().to_string(),
            url: task.url.clone(),
            filename: task.filename.clone(),
            path: task.target_path().to_string_lossy().to_string(),
            status: task.status.as_str().to_string(),
            total_size: task.total_size,
            downloaded: task.downloaded,
            speed: live.map(|p| p.speed).unwrap_or(0),
            connections: live.map(|p| p.connections).unwrap_or(0),
            max_connections: task.max_connections,
            error: task.error.as_ref().map(|e| e.message.clone()),
            created_at: task.created_at.to_rfc3339(),
            updated_at: task.updated_at.to_rfc3339(),
        }
    }
}

/// 新建任务的请求体。字段都能缺省。
#[derive(Debug, Default, Deserialize)]
pub struct CreateTaskBody {
    pub url: String,
    pub filename: Option<String>,
    pub dir: Option<String>,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    pub cookies: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    pub max_connections: Option<u32>,
    pub engine: Option<String>,
    /// 是否立即开始下载，默认 true。
    #[serde(default = "default_true")]
    pub autostart: bool,
}

fn default_true() -> bool {
    true
}

impl CreateTaskBody {
    pub fn into_request(self) -> TaskRequest {
        TaskRequest {
            url: self.url,
            filename: self.filename,
            dir: self.dir,
            referer: self.referer,
            user_agent: self.user_agent,
            cookies: self.cookies,
            headers: self.headers.into_iter().collect(),
            max_connections: self.max_connections.unwrap_or(8).clamp(1, 64),
            engine: self
                .engine
                .as_deref()
                .and_then(EngineKind::from_str_opt)
                .unwrap_or(EngineKind::Auto),
        }
    }
}

/// 鉴权中间件。除了健康检查，其它接口都要带 token。
async fn auth_middleware(
    State(state): State<BridgeState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let provided = headers
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| query.get("token").cloned());

    match provided {
        Some(token) if token == *state.token => next.run(request).await,
        _ => {
            let err = ApiError::new("unauthorized", "token 不正确");
            (StatusCode::UNAUTHORIZED, Json(err)).into_response()
        }
    }
}

/// 构造路由。
pub fn router(state: BridgeState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let protected = Router::new()
        .route("/api/v1/tasks", post(create_task))
        .route("/api/v1/tasks", get(list_tasks))
        .route("/api/v1/tasks/:id", get(get_task))
        .route("/api/v1/tasks/:id", patch(patch_task))
        .route("/api/v1/tasks/:id", delete(delete_task))
        .route("/api/v1/tasks/:id/pause", post(pause_task))
        .route("/api/v1/tasks/:id/resume", post(resume_task))
        .route("/api/v1/tasks/:id/cancel", post(cancel_task))
        .route("/api/v1/tasks/:id/retry", post(retry_task))
        .route("/api/v1/settings", get(get_settings))
        .route("/api/v1/settings", post(update_settings))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware));

    Router::new()
        .route("/api/v1/health", get(health))
        .merge(protected)
        .layer(cors)
        .with_state(state)
}

/// 启动本地 HTTP 服务。端口被占用时返回错误。
pub async fn serve(state: BridgeState, port: u16) -> anyhow::Result<()> {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!(port, "bridge 已启动");
    axum::serve(listener, app).await?;
    Ok(())
}

/// 从 `start` 开始找一个空闲端口。
pub async fn find_free_port(start: u16) -> Option<u16> {
    for port in start..start.saturating_add(64) {
        if tokio::net::TcpListener::bind(("127.0.0.1", port)).await.is_ok() {
            return Some(port);
        }
    }
    None
}

async fn health(State(state): State<BridgeState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "port": state.port,
    }))
}

async fn create_task(
    State(state): State<BridgeState>,
    Json(body): Json<CreateTaskBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if body.url.trim().is_empty() {
        return Err(ApiError::new("invalid_request", "URL 不能为空"));
    }

    let autostart = body.autostart;
    let request = body.into_request();
    let task = state
        .manager
        .add(request)
        .await
        .map_err(|e| ApiError::new("add_failed", e.to_string()))?;

    if autostart {
        state
            .manager
            .start(task.id)
            .await
            .map_err(|e| ApiError::new("start_failed", e.to_string()))?;
    }

    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": task.id.to_string(), "status": task.status.as_str() })),
    ))
}

async fn list_tasks(State(state): State<BridgeState>) -> Result<Json<Value>, ApiError> {
    let tasks = state
        .manager
        .list()
        .map_err(|e| ApiError::new("internal", e.to_string()))?;
    let live = state.manager.live_progress().await;

    let list: Vec<TaskDto> = tasks
        .iter()
        .map(|t| TaskDto::from_task(t, live.get(&t.id)))
        .collect();
    Ok(Json(json!({ "tasks": list })))
}

async fn get_task(
    State(state): State<BridgeState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let id = parse_id(&id)?;
    let task = state
        .manager
        .get(id)
        .map_err(|e| ApiError::new("internal", e.to_string()))?
        .ok_or_else(|| ApiError::new("not_found", "任务不存在"))?;

    let live = state.manager.live_progress().await;
    let dto = TaskDto::from_task(&task, live.get(&id));
    Ok(Json(json!({ "task": dto })))
}

#[derive(Debug, Default, Deserialize)]
pub struct PatchTaskBody {
    pub filename: Option<String>,
    pub dir: Option<String>,
    pub max_connections: Option<u32>,
}

async fn patch_task(
    State(state): State<BridgeState>,
    Path(id): Path<String>,
    Json(body): Json<PatchTaskBody>,
) -> Result<Json<Value>, ApiError> {
    let id = parse_id(&id)?;
    let mut task = state
        .manager
        .get(id)
        .map_err(|e| ApiError::new("internal", e.to_string()))?
        .ok_or_else(|| ApiError::new("not_found", "任务不存在"))?;

    if let Some(name) = body.filename {
        task.filename = idm_shadow_engine::task::sanitize_filename(&name);
    }
    if let Some(dir) = body.dir {
        task.dir = dir.into();
    }
    if let Some(n) = body.max_connections {
        task.max_connections = n.clamp(1, 64);
    }
    task.updated_at = chrono::Utc::now();

    state
        .manager
        .store()
        .update_task(&task)
        .map_err(|e| ApiError::new("internal", e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Default, Deserialize)]
pub struct DeleteQuery {
    pub delete_file: Option<bool>,
}

async fn delete_task(
    State(state): State<BridgeState>,
    Path(id): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> Result<Json<Value>, ApiError> {
    let id = parse_id(&id)?;
    state
        .manager
        .remove(id, q.delete_file.unwrap_or(false))
        .await
        .map_err(|e| ApiError::new("internal", e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn pause_task(
    State(state): State<BridgeState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let id = parse_id(&id)?;
    state
        .manager
        .pause(id)
        .await
        .map_err(|e| ApiError::new("internal", e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn resume_task(
    State(state): State<BridgeState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let id = parse_id(&id)?;
    state
        .manager
        .resume(id)
        .await
        .map_err(|e| ApiError::new("internal", e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn cancel_task(
    State(state): State<BridgeState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let id = parse_id(&id)?;
    state
        .manager
        .cancel(id)
        .await
        .map_err(|e| ApiError::new("internal", e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn retry_task(
    State(state): State<BridgeState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let id = parse_id(&id)?;
    state
        .manager
        .retry(id)
        .await
        .map_err(|e| ApiError::new("internal", e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn get_settings(State(state): State<BridgeState>) -> Result<Json<Value>, ApiError> {
    let settings = state
        .manager
        .store()
        .all_settings()
        .map_err(|e| ApiError::new("internal", e.to_string()))?;
    let config = state.manager.engine().config().await;
    Ok(Json(json!({
        "port": state.port,
        "download_dir": state.manager.default_dir().await.to_string_lossy(),
        "max_connections": config.max_connections,
        "max_concurrent_tasks": state.manager.max_concurrent_tasks().await,
        "default_download_tool": is_default_download_tool(&state.manager),
        "auto_start": system::is_auto_start(),
        "settings": settings,
    })))
}

/// "设为默认下载工具"这个开关存在数据库里。
/// 单一事实来源在桌面应用，扩展被动服从。
pub fn is_default_download_tool(manager: &DownloadManager) -> bool {
    manager
        .store()
        .get_setting("default_download_tool")
        .ok()
        .flatten()
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// 打开"设为默认下载工具"。
/// 按约定：默认下载工具意味着开机就在后台待命，所以同时打开开机自启。
pub fn set_default_download_tool(manager: &DownloadManager, enabled: bool) -> anyhow::Result<()> {
    manager
        .store()
        .set_setting("default_download_tool", if enabled { "1" } else { "0" })?;

    if enabled {
        // 默认下载工具 ⇒ 开机自启
        if let Some(exe) = system::current_exe() {
            system::set_auto_start(true, &exe)?;
        }
        // 注册自定义协议，扩展唤起应用时用得上
        if let Some(exe) = system::current_exe() {
            let _ = protocol::register(&exe);
        }
    }
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
pub struct UpdateSettingsBody {
    pub download_dir: Option<String>,
    pub global_limit: Option<u64>,
    /// 单任务连接数上限。
    pub max_connections: Option<u32>,
    /// 同时下载的任务数上限。0 = 不限。
    pub max_concurrent_tasks: Option<u32>,
    /// 设为默认下载工具。打开它会连带打开开机自启。
    pub default_download_tool: Option<bool>,
    /// 开机自启。
    pub auto_start: Option<bool>,
}

async fn update_settings(
    State(state): State<BridgeState>,
    Json(body): Json<UpdateSettingsBody>,
) -> Result<Json<Value>, ApiError> {
    if let Some(dir) = body.download_dir {
        let path = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&path).ok();
        state
            .manager
            .store()
            .set_setting("download_dir", &path.to_string_lossy())
            .map_err(|e| ApiError::new("internal", e.to_string()))?;
    }
    if let Some(limit) = body.global_limit {
        state
            .manager
            .set_global_limit(if limit == 0 { None } else { Some(limit) });
        state
            .manager
            .store()
            .set_setting("global_limit", &limit.to_string())
            .map_err(|e| ApiError::new("internal", e.to_string()))?;
    }
    if let Some(n) = body.max_connections {
        let n = n.clamp(1, 64);
        let mut config = state.manager.engine().config().await;
        config.max_connections = n as usize;
        state.manager.engine().set_config(config).await;
        state
            .manager
            .store()
            .set_setting("max_connections", &n.to_string())
            .map_err(|e| ApiError::new("internal", e.to_string()))?;
    }
    if let Some(n) = body.max_concurrent_tasks {
        let limit = if n == 0 { None } else { Some(n as usize) };
        state.manager.set_max_concurrent_tasks(limit).await;
        let value = n.to_string();
        state
            .manager
            .store()
            .set_setting("max_concurrent_tasks", &value)
            .map_err(|e| ApiError::new("internal", e.to_string()))?;
    }
    if let Some(enabled) = body.default_download_tool {
        set_default_download_tool(&state.manager, enabled)
            .map_err(|e| ApiError::new("internal", e.to_string()))?;
    }
    if let Some(enabled) = body.auto_start {
        let exe = system::current_exe().ok_or_else(|| {
            ApiError::new("internal", "找不到程序自身路径")
        })?;
        system::set_auto_start(enabled, &exe)
            .map_err(|e| ApiError::new("internal", e.to_string()))?;
    }
    Ok(Json(json!({ "ok": true })))
}

fn parse_id(raw: &str) -> Result<TaskId, ApiError> {
    raw.parse::<TaskId>()
        .map_err(|_| ApiError::new("invalid_id", "任务 id 非法"))
}

/// 任务状态的中文名，给桥接接口用（方便扩展直接展示）。
pub fn status_label(status: &str) -> &'static str {
    match status {
        "queued" => "排队中",
        "probing" => "探测中",
        "downloading" => "下载中",
        "retrying" => "重试中",
        "paused" => "已暂停",
        "completed" => "已完成",
        "error" => "出错",
        "cancelled" => "已取消",
        _ => "未知",
    }
}
