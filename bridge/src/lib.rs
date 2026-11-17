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
/// 扩展在每个请求上打这个头，用来判断"浏览器接管是不是真的在工作"。
///
/// 光看进程/浏览器版本判断不了：扩展没启用、被禁用、被删掉，
/// 应用这边都看不出来。让扩展自报身份最省事。
pub const CLIENT_HEADER: &str = "x-idm-shadow-client";
/// 扩展自报自己的版本号。
///
/// 浏览器会缓存已解压扩展的注册信息：磁盘上的文件换了新版，浏览器
/// 照样跑着旧代码。旧代码连 `CLIENT_HEADER` 都不发，所以光看"连没连上"
/// 分不清"扩展没装"和"跑着旧代码的扩展"——而这两者的症状对用户来说
/// 是一样的：点下载一点反应都没有。
///
/// 有了版本号，应用就能明确告诉用户该去点「重新加载」。
pub const VERSION_HEADER: &str = "x-idm-shadow-version";

#[derive(Clone)]
pub struct BridgeState {
    pub manager: Arc<DownloadManager>,
    pub token: Arc<String>,
    pub port: u16,
    /// 把桌面应用的窗口叫到前面。
    ///
    /// 扩展点「打开主界面」时会调它。bridge 本身不该知道 Tauri，
    /// 所以这里放个回调，由 app 那边接上。
    pub show_ui: Option<Arc<dyn Fn() + Send + Sync>>,
    /// 扩展最近一次带 token 调接口的时间。
    ///
    /// 界面用它显示"已连接 / 未连接"。用 `Instant` 而不是时间戳，
    /// 免得用户改系统时间把状态搞乱。
    pub extension_seen: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    /// 扩展最近一次自报的版本号。
    ///
    /// 三种状态：
    ///  - None —— 扩展还没连上过；
    ///  - Some("") —— 连上了，但没报版本号。那是老版本扩展
    ///    （0.1.3 之前连身份头都不发，0.1.3~0.1.4 发身份头但不报版本），
    ///    本身就是"跑着旧代码"的信号；
    ///  - Some(v) —— 扩展自报的版本号。
    pub extension_version: Arc<std::sync::Mutex<Option<String>>>,
    /// 应用随包发布的扩展版本号。
    ///
    /// 扩展拿它跟自己报的版本比一下，就知道自己是不是旧版，
    /// 可以在界面上提醒用户去点「重新加载」。
    pub shipped_extension_version: Arc<String>,
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
    /// 是否在等用户确认保存位置。
    pub needs_confirm: bool,
    /// 当前各连接持有的区间（分段可视化用）。
    pub segments: Vec<SegmentDto>,
    /// BT 任务的分片状态（piece 校验可视化用）。
    pub pieces: Option<PieceStatusDto>,
    /// 文件现在还在不在保存位置。
    ///
    /// 任务列表用它把"文件已经没了"的条目标灰——
    /// 用户可能手动删了，或者换了机器。
    /// 只有真正下完过的任务才检查，下载中的不查（文件还没落地）。
    pub file_exists: Option<bool>,
    pub created_at: String,
    pub updated_at: String,
}

/// 一条连接的区间。
#[derive(Debug, Serialize)]
pub struct SegmentDto {
    pub start: u64,
    pub end: u64,
    pub speed: u64,
}

/// BT 分片状态。
#[derive(Debug, Serialize)]
pub struct PieceCellDto {
    /// 这一格覆盖的分片区间（含端点）
    pub from: u32,
    pub to: u32,
    /// missing / partial / verified / corrupted
    pub state: String,
    /// 这一格里最稀缺的那块，有几个 peer 持有
    pub availability: u16,
}

/// BT 分片状态汇总。
#[derive(Debug, Serialize)]
pub struct PieceStatusDto {
    /// 一共多少块
    pub total: u32,
    /// 已经校验通过的
    pub verified: u32,
    /// 当前判定为坏块的（正在重传）
    pub corrupted: u32,
    /// 累计校验失败次数（重传统计）
    pub verification_failures: u64,
    /// 降采样后的格子，用来画图
    pub cells: Vec<PieceCellDto>,
}

impl TaskDto {
    /// 检查下载好的文件还在不在保存位置。
    ///
    /// 只对真正下完的任务查：下载中的文件还没落地，
    /// 查了也是 false，界面上会误标成"文件没了"。
    fn file_exists(task: &Task) -> Option<bool> {
        if task.status != idm_shadow_engine::task::TaskStatus::Completed {
            return None;
        }
        let path = task.final_path.clone().unwrap_or_else(|| task.target_path());
        Some(path.exists())
    }

    /// `default_max_connections` 是全局设置，任务没单独指定时用它。
    pub fn from_task(
        task: &Task,
        live: Option<&idm_shadow_engine::http::TaskProgress>,
        default_max_connections: u32,
    ) -> Self {
        Self {
            id: task.id.to_string(),
            engine: task.engine.as_str().to_string(),
            url: task.url.clone(),
            filename: task.filename.clone(),
            // 用户在确认弹窗里选了别的位置时，文件其实落在 final_path。
            // 这里要报最终位置，不然"在文件夹中显示"会指错地方。
            path: task
                .final_path
                .clone()
                .unwrap_or_else(|| task.target_path())
                .to_string_lossy()
                .to_string(),
            file_exists: Self::file_exists(task),
            status: task.status.as_str().to_string(),
            total_size: task.total_size,
            downloaded: task.downloaded,
            speed: live.map(|p| p.speed).unwrap_or(0),
            connections: live.map(|p| p.connections).unwrap_or(0),
            max_connections: task
                .max_connections
                .unwrap_or(default_max_connections),
            error: task.error.as_ref().map(|e| e.message.clone()),
            needs_confirm: task.needs_confirm,
            segments: live
                .map(|p| {
                    p.segments
                        .iter()
                        .map(|s| SegmentDto {
                            start: s.start,
                            end: s.end,
                            speed: s.speed,
                        })
                        .collect()
                })
                .unwrap_or_default(),
            pieces: live.and_then(|p| p.pieces.as_ref()).map(|ps| PieceStatusDto {
                total: ps.total,
                verified: ps.verified,
                corrupted: ps.corrupted,
                verification_failures: ps.verification_failures,
                cells: ps
                    .cells
                    .iter()
                    .map(|c| PieceCellDto {
                        from: c.from,
                        to: c.to,
                        state: match c.state {
                            idm_shadow_engine::http::engine::PieceState::Missing => {
                                "missing".to_string()
                            }
                            idm_shadow_engine::http::engine::PieceState::Verified => {
                                "verified".to_string()
                            }
                            idm_shadow_engine::http::engine::PieceState::Partial => {
                                "partial".to_string()
                            }
                            idm_shadow_engine::http::engine::PieceState::Corrupted => {
                                "corrupted".to_string()
                            }
                        },
                        availability: c.availability,
                    })
                    .collect(),
            }),
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
    /// 浏览器侧习惯用驼峰，所以两种拼法都认。
    #[serde(alias = "userAgent")]
    pub user_agent: Option<String>,
    pub cookies: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// 浏览器侧习惯用驼峰，所以两种拼法都认。
    #[serde(alias = "maxConnections")]
    pub max_connections: Option<u32>,
    pub engine: Option<String>,
    /// 是否立即开始下载，默认 true。
    #[serde(default = "default_true")]
    pub autostart: bool,
    /// 是否弹出保存位置确认（IDM 式流程）。默认 false。
    #[serde(default, alias = "needsConfirm")]
    pub needs_confirm: bool,
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
            // None = 跟随全局设置
            max_connections: self.max_connections.map(|v| v.clamp(1, 64)),
            needs_confirm: self.needs_confirm,
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
        Some(token) if token == *state.token => {
            if is_extension_client(&headers) {
                if let Ok(mut slot) = state.extension_seen.lock() {
                    *slot = Some(std::time::Instant::now());
                }
                // 顺便记下扩展自报的版本。
                //
                // 没带版本头也照记：记成空串。
                //
                // 这样"连上了但不报版本"和"上报了旧版本"是两种不同的
                // 旧法，界面提示的话也不一样。记成 None 的话，
                // 一次没带版本头的请求就会把之前记的版本抹掉。
                if let Ok(mut slot) = state.extension_version.lock() {
                    let reported = headers
                        .get(VERSION_HEADER)
                        .and_then(|v| v.to_str().ok())
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .unwrap_or("")
                        .to_string();
                    *slot = Some(reported);
                }
            }
            next.run(request).await
        }
        _ => {
            let err = ApiError::new("unauthorized", "token 不正确");
            (StatusCode::UNAUTHORIZED, Json(err)).into_response()
        }
    }
}

/// 这个请求是不是浏览器扩展发来的。
///
/// 只认显式声明：扩展在没有 Origin 头的场景（service worker 里
/// 某些请求）也要能被认出来，所以不能只靠 `chrome-extension://`。
/// 这个判断只用于显示状态，不参与鉴权——鉴权永远是 token。
fn is_extension_client(headers: &HeaderMap) -> bool {
    if headers
        .get(CLIENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("extension"))
        .unwrap_or(false)
    {
        return true;
    }
    headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(|o| o.starts_with("chrome-extension://") || o.starts_with("moz-extension://"))
        .unwrap_or(false)
}

/// 扩展是不是还在连着。
///
/// 扩展每分钟会拉一次角标数据，三分钟没动静基本就是退了、
/// 被禁用或者被卸载了。给一个宽裕的窗口，避免网络抖动误报。
pub fn extension_connected(
    seen: &std::sync::Mutex<Option<std::time::Instant>>,
) -> bool {
    seen.lock()
        .ok()
        .and_then(|slot| *slot)
        .map(|at| at.elapsed() < std::time::Duration::from_secs(180))
        .unwrap_or(false)
}

/// 扩展最近自报的版本号。
pub fn extension_version(version: &std::sync::Mutex<Option<String>>) -> Option<String> {
    version.lock().ok().and_then(|slot| slot.clone())
}

/// 扩展是不是跑着旧代码。
///
/// 返回 `None` 表示扩展没连上——那不是"旧版"，是"没在跑"，
/// 界面要提示的话得换个说法。
pub fn extension_is_stale(
    version: &std::sync::Mutex<Option<String>>,
    shipped: &str,
) -> Option<bool> {
    let reported = extension_version(version)?;
    if shipped.is_empty() {
        return None;
    }
    // 空版本号 = 连上了但不报版本 = 老版本扩展（它没这段代码）。
    // 那正是要提示用户去点「重新加载」的情况。
    Some(reported != shipped)
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
        .route("/api/v1/tasks/:id/confirm", post(confirm_task))
        .route("/api/v1/tasks/:id/cancel-confirmed", post(cancel_confirmed_task))
        .route("/api/v1/ui/show", post(show_ui))
        .route("/api/v1/settings", get(get_settings))
        .route("/api/v1/settings", post(update_settings))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware));

    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/bootstrap", get(bootstrap))
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
        // 浏览器扩展是不是还在连着。不涉及 token，健康检查
        // 本来就不需要鉴权，这里也不多暴露什么。
        "extension_connected": extension_connected(&state.extension_seen),
        // 扩展自报的版本，和随包发布的版本。
        //
        // 两个都有才说明是"跑着旧代码"；扩展没连上时都是空的。
        "extension_version": extension_version(&state.extension_version),
        "shipped_extension_version": state.shipped_extension_version.as_str(),
    }))
}

/// 引导接口：让扩展自己找到地址和 token。
///
/// 没有这个的话，用户得手动从应用设置里把 48 位 token 抄到
/// 扩展的设置页里——最容易卡住的一步。
///
/// 但 token 正是用来挡住"网站往本地塞下载任务"的，
/// 所以这里必须只让**浏览器扩展**读：
///
/// - 没有 Origin 头 —— 命令行工具（curl 之类），本地程序本来就有全权限
/// - `chrome-extension://` / `moz-extension://` —— 浏览器扩展
///
/// 网站的 Origin 是 `https://xxx`，`file://` 页面是 `null`，
/// 两者都拿不到 token。
async fn bootstrap(State(state): State<BridgeState>, headers: HeaderMap) -> Response {
    let origin = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !bootstrap_origin_allowed(origin) {
        let err = ApiError::new("forbidden", "只有浏览器扩展可以读取引导信息");
        return (StatusCode::FORBIDDEN, Json(err)).into_response();
    }

    Json(json!({
        "port": state.port,
        "token": state.token.as_str(),
    }))
    .into_response()
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
    let default_max = state.manager.engine().config().await.max_connections as u32;

    let list: Vec<TaskDto> = tasks
        .iter()
        .map(|t| TaskDto::from_task(t, live.get(&t.id), default_max))
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
    let default_max = state.manager.engine().config().await.max_connections as u32;
    let dto = TaskDto::from_task(&task, live.get(&id), default_max);
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
        task.max_connections = Some(n.clamp(1, 64));
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

/// 用户确认保存位置。
///
/// `path` 为空表示"直接下载"，用默认目录；
/// 给了路径就下载完成后转存到那里。
#[derive(Debug, Default, Deserialize)]
pub struct ConfirmBody {
    pub path: Option<String>,
    /// 只给了目录时，用它加上原文件名。
    pub dir: Option<String>,
}

async fn confirm_task(
    State(state): State<BridgeState>,
    Path(id): Path<String>,
    Json(body): Json<ConfirmBody>,
) -> Result<Json<Value>, ApiError> {
    let id = parse_id(&id)?;

    let path = match (body.path, body.dir) {
        (Some(p), _) if !p.trim().is_empty() => Some(std::path::PathBuf::from(p)),
        (None, Some(d)) if !d.trim().is_empty() => {
            let task = state
                .manager
                .get(id)
                .map_err(|e| ApiError::new("internal", e.to_string()))?
                .ok_or_else(|| ApiError::new("not_found", "任务不存在"))?;
            Some(std::path::PathBuf::from(d).join(&task.filename))
        }
        _ => None,
    };

    state
        .manager
        .confirm_path(id, path)
        .await
        .map_err(|e| ApiError::new("confirm_failed", e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

/// 用户在确认弹窗里点了取消。已下载的暂存文件会被删掉。
async fn cancel_confirmed_task(
    State(state): State<BridgeState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let id = parse_id(&id)?;
    state
        .manager
        .cancel_with_cleanup(id)
        .await
        .map_err(|e| ApiError::new("cancel_failed", e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

/// 把桌面应用的窗口叫到前面。
///
/// 扩展点「打开主界面」时调它。
///
/// 以前扩展是用 `chrome.tabs.create({url: 'idmshadow://'})` 唤起的——
/// 那会让 Edge 开一个标签页去加载协议链接，然后把「无法访问此页面」
/// 的错误页留在那儿。走接口就没这问题。
async fn show_ui(State(state): State<BridgeState>) -> Result<Json<Value>, ApiError> {
    if let Some(cb) = state.show_ui.as_ref() {
        cb();
    }
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
        "auto_start": is_auto_start_setting(&state.manager),
        // 扩展自报的版本。跟随包版本对不上就说明浏览器跑的是旧代码。
        "extension_version": extension_version(&state.extension_version),
        "shipped_extension_version": state.shipped_extension_version.as_str(),
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

/// 开机自启的开关存在数据库里，注册表只是按它写出来的结果。
///
/// 老版本没有这一项：那时"默认下载工具"约定连带打开自启，
/// 所以有 `default_download_tool=1` 就视为开启，下次启动会对账补写。
pub fn is_auto_start_setting(manager: &DownloadManager) -> bool {
    match manager.store().get_setting("auto_start") {
        Ok(Some(v)) => v == "1",
        _ => is_default_download_tool(manager),
    }
}

/// 按数据库里的选择，把注册表启动项对齐。
///
/// 每次启动都跑一遍：值改了要跟着改，exe 换目录要重写路径，
/// 注册表被安全软件清掉也要能自愈。
pub fn reconcile_auto_start(manager: &DownloadManager) {
    // 隔离实例（自动化测试）不去碰共享的注册表启动项。
    //
    // 测试实例自己的数据库里没有开机自启设置，一对账就会把用户
    // 真正的启动项删掉——跑一次测试，用户开机自启就没了。
    if system::is_isolated_instance() {
        tracing::debug!("隔离实例，跳过开机自启对账");
        return;
    }

    let Some(exe) = system::current_exe() else {
        tracing::warn!("拿不到程序自身路径，跳过开机自启对账");
        return;
    };

    let wanted = is_auto_start_setting(manager);
    let present = system::is_auto_start();

    if wanted && (!present || !system::auto_start_matches(&exe)) {
        match system::set_auto_start(true, &exe) {
            Ok(()) => tracing::info!(exe = %exe.display(), "已写入开机自启"),
            Err(err) => tracing::warn!("写入开机自启失败: {err}"),
        }
    } else if !wanted && present {
        if let Err(err) = system::set_auto_start(false, &exe) {
            tracing::warn!("删除开机自启失败: {err}");
        }
    }
}

/// 保存开机自启选择：先落库，再按结果写注册表。
///
/// 注册表写失败要回滚数据库里的值，否则界面显示"已开启"、
/// 实际重启不会启动，就是这个项目踩过的坑。
pub fn set_auto_start_setting(manager: &DownloadManager, enabled: bool) -> anyhow::Result<()> {
    // 同上：隔离实例不写共享的注册表。
    //
    // 而且这里写的是"真的"启动项——写进去会把用户装的那个指向
    // 临时目录里的测试 exe，开机时反而启动错了程序。
    if system::is_isolated_instance() {
        tracing::debug!("隔离实例，跳过开机自启设置");
        return Ok(());
    }

    let exe = system::current_exe()
        .ok_or_else(|| anyhow::anyhow!("找不到程序自身路径"))?;
    system::set_auto_start(enabled, &exe)?;
    manager
        .store()
        .set_setting("auto_start", if enabled { "1" } else { "0" })?;
    Ok(())
}

/// 打开"设为默认下载工具"。
/// 按约定：默认下载工具意味着开机就在后台待命，所以同时打开开机自启。
pub fn set_default_download_tool(manager: &DownloadManager, enabled: bool) -> anyhow::Result<()> {
    manager
        .store()
        .set_setting("default_download_tool", if enabled { "1" } else { "0" })?;

    if enabled {
        // 默认下载工具 ⇒ 开机自启。
        // 走 set_auto_start_setting 是为了把选择一并落库：
        // 只写注册表的话，下次启动对账会把它当成"用户没开"删掉。
        set_auto_start_setting(manager, true)?;
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
        // 光存数据库不够，还得让正在运行的管理器立刻换过来
        state.manager.set_default_dir(path.clone()).await;
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
        set_auto_start_setting(&state.manager, enabled)
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

/// 引导接口放行哪些来源。
///
/// token 是用来挡住"网站乱往本地塞下载任务"的，
/// 所以这个判断必须严：只有浏览器扩展能读到 token。
fn bootstrap_origin_allowed(origin: &str) -> bool {
    origin.is_empty()
        || origin.starts_with("chrome-extension://")
        || origin.starts_with("moz-extension://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_allows_extensions_and_cli_but_not_web_pages() {
        // 浏览器扩展
        assert!(bootstrap_origin_allowed("chrome-extension://abc123"));
        assert!(bootstrap_origin_allowed("moz-extension://abc123"));
        // 命令行工具（没有 Origin 头）
        assert!(bootstrap_origin_allowed(""));
        // 网站
        assert!(!bootstrap_origin_allowed("https://evil.example.com"));
        assert!(!bootstrap_origin_allowed("http://localhost"));
        // file:// 页面发的是 null
        assert!(!bootstrap_origin_allowed("null"));
    }

    #[test]
    fn extension_presence_is_tracked_from_marked_requests() {
        let seen = std::sync::Mutex::new(None);
        assert!(!extension_connected(&seen));

        let mut headers = HeaderMap::new();
        headers.insert(
            CLIENT_HEADER,
            axum::http::HeaderValue::from_static("extension"),
        );
        assert!(is_extension_client(&headers));

        *seen.lock().unwrap() = Some(std::time::Instant::now());
        assert!(extension_connected(&seen));
    }

    #[test]
    fn plain_web_requests_do_not_count_as_extension() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ORIGIN,
            axum::http::HeaderValue::from_static("https://example.com"),
        );
        assert!(!is_extension_client(&headers));

        headers.insert(
            axum::http::header::ORIGIN,
            axum::http::HeaderValue::from_static("chrome-extension://abc"),
        );
        assert!(is_extension_client(&headers));
    }

    #[test]
    fn extension_version_is_recorded_and_compared() {
        let version = std::sync::Mutex::new(None);

        // 没连上的时候没有版本——那是"没启用"，不是"旧版"。
        assert_eq!(extension_version(&version), None);
        assert_eq!(extension_is_stale(&version, "0.1.4"), None);

        // 连上了但没报版本号——那是老版本扩展，属于"旧版"。
        *version.lock().unwrap() = Some(String::new());
        assert_eq!(extension_is_stale(&version, "0.1.4"), Some(true));

        // 连上了，版本跟随包发布的对得上。
        *version.lock().unwrap() = Some("0.1.4".to_string());
        assert_eq!(extension_version(&version).as_deref(), Some("0.1.4"));
        assert_eq!(extension_is_stale(&version, "0.1.4"), Some(false));

        // 浏览器缓存了旧代码：报的版本比随包发布的旧。
        *version.lock().unwrap() = Some("0.1.0".to_string());
        assert_eq!(extension_is_stale(&version, "0.1.4"), Some(true));
    }
}
