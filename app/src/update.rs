//! 更新通道：检查、下载、安装。
//!
//! 唯一的更新入口是一条静态 HTTPS feed（Tauri updater 的 latest.json），
//! 安装包必须经过 minisign 签名校验。发布侧用 `tools/update/publish.mjs`
//! 生成 feed，客户端不需要知道它托管在哪里。
//!
//! 更新源优先级：环境变量 > 设置里的值 > 编译期默认值。
//! 环境变量主要是给自动化测试和临时换源用的，界面上不用展示。

use std::sync::{Arc, Mutex};

use idm_shadow_engine::manager::DownloadManager;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;
use url::Url;

const ENDPOINT_KEY: &str = "update_endpoint";
const CHECK_ON_START_KEY: &str = "update_check_on_start";
/// 上一次运行时的版本。用来判断"刚更新过"，好把本次更新说明弹给用户。
const LAST_RUN_VERSION_KEY: &str = "last_run_version";
/// 已展示过、用户还没关掉的更新说明。更新完重启后靠它把说明带出来。
const PENDING_NOTES_KEY: &str = "pending_release_notes";
/// 编译时可用 `IDM_SHADOW_UPDATE_URL` 写死一个默认源。
const DEFAULT_ENDPOINT: Option<&str> = option_env!("IDM_SHADOW_UPDATE_URL");
/// 默认更新源：GitHub Releases 的固定地址，永远指向最新版本的资产。
/// `--base-url` 发版时用的就是这个前缀。
pub const GITHUB_RELEASES_ENDPOINT: &str =
    "https://github.com/yefuchen1/idm-shadow/releases/latest/download/latest.json";

/// 检查结果。没更新时返回 None。
#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub current_version: String,
    pub notes: Option<String>,
    pub date: Option<String>,
}

/// 升级后第一次启动要展示的发布说明。
#[derive(Debug, Clone, Serialize)]
pub struct ReleaseNotes {
    pub version: String,
    pub notes: String,
}

/// 当前生效的更新源。
pub fn endpoint_setting(manager: &DownloadManager) -> String {
    if let Ok(value) = std::env::var("IDM_SHADOW_UPDATE_URL") {
        let value = value.trim().to_string();
        if !value.is_empty() {
            return value;
        }
    }
    manager
        .store()
        .get_setting(ENDPOINT_KEY)
        .ok()
        .flatten()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| DEFAULT_ENDPOINT.map(str::to_string))
        .unwrap_or_else(|| GITHUB_RELEASES_ENDPOINT.to_string())
}

/// 保存更新源。空字符串表示清空。
///
/// 保存前先校验：不是合法 URL 直接拒绝，免得等到检查更新时
/// 才报一个看不懂的解析错误。
pub fn set_endpoint_setting(manager: &DownloadManager, endpoint: &str) -> anyhow::Result<()> {
    let endpoint = endpoint.trim();
    if !endpoint.is_empty() {
        Url::parse(endpoint)?;
    }
    manager.store().set_setting(ENDPOINT_KEY, endpoint)
}

/// 启动后是否静默检查一次更新。默认开。
pub fn check_on_start_setting(manager: &DownloadManager) -> bool {
    manager
        .store()
        .get_setting(CHECK_ON_START_KEY)
        .ok()
        .flatten()
        .map(|value| value != "0")
        .unwrap_or(true)
}

fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 取"本次更新内容"。
///
/// 安装前把 feed 里的说明存进数据库；重启后版本号变了，
/// 就把这份说明交给界面弹一次。用户点关闭后才算读过。
pub fn pending_release_notes(manager: &DownloadManager) -> Option<ReleaseNotes> {
    let current = current_version();
    let last = manager.store().get_setting(LAST_RUN_VERSION_KEY).ok().flatten();

    match last {
        Some(last) if last != current && !last.is_empty() => {
            let notes = manager
                .store()
                .get_setting(PENDING_NOTES_KEY)
                .ok()
                .flatten()
                .filter(|notes| !notes.trim().is_empty());
            match notes {
                Some(notes) => Some(ReleaseNotes {
                    version: current,
                    notes,
                }),
                None => {
                    // 没有说明（比如手工装的新包），别反复当成未读。
                    let _ = manager
                        .store()
                        .set_setting(LAST_RUN_VERSION_KEY, &current);
                    None
                }
            }
        }
        // 老库没有这个键：不把首次记录当成"刚更新过"。
        _ => {
            let _ = manager
                .store()
                .set_setting(LAST_RUN_VERSION_KEY, &current);
            None
        }
    }
}

/// 用户看过之后调用：清掉说明，并记下当前版本。
pub fn acknowledge_release_notes(manager: &DownloadManager) {
    let _ = manager.store().set_setting(PENDING_NOTES_KEY, "");
    let _ = manager
        .store()
        .set_setting(LAST_RUN_VERSION_KEY, &current_version());
}

fn parse_endpoint(
    manager: &DownloadManager,
    override_endpoint: Option<&str>,
) -> Result<Url, String> {
    let raw = match override_endpoint.map(str::trim) {
        Some(value) if !value.is_empty() => value.to_string(),
        _ => endpoint_setting(manager),
    };
    if raw.is_empty() {
        return Err("未配置更新源".to_string());
    }
    Url::parse(&raw).map_err(|err| format!("更新源地址不合法: {err}"))
}

/// 检查更新。没有新版本返回 None。
///
/// `override_endpoint` 用于"界面上刚改完还没保存就点检查"的场景。
pub async fn check(
    app: &AppHandle,
    manager: &DownloadManager,
    override_endpoint: Option<&str>,
) -> Result<Option<UpdateInfo>, String> {
    let endpoint = parse_endpoint(manager, override_endpoint)?;
    let update = app
        .updater_builder()
        .endpoints(vec![endpoint])
        .map_err(|err| err.to_string())?
        .build()
        .map_err(|err| err.to_string())?
        .check()
        .await
        .map_err(|err| err.to_string())?;

    Ok(update.map(|update| UpdateInfo {
        version: update.version.clone(),
        current_version: update.current_version.clone(),
        notes: update.body.clone(),
        date: update.date.map(|d| d.to_string()),
    }))
}

/// 下载并安装更新。
///
/// 进度通过 `update-progress` 事件推给界面。Windows 上安装时
/// 应用会被安装器结束，这是 Tauri 的限制，不是异常。
pub async fn install(
    app: &AppHandle,
    manager: &DownloadManager,
    override_endpoint: Option<&str>,
) -> Result<(), String> {
    // 和检查用同一个源，否则用户在界面上改了地址、还没保存就点更新，
    // 会拿着旧源里的包去装。
    let endpoint = parse_endpoint(manager, override_endpoint)?;
    let update = app
        .updater_builder()
        .endpoints(vec![endpoint])
        .map_err(|err| err.to_string())?
        .build()
        .map_err(|err| err.to_string())?
        .check()
        .await
        .map_err(|err| err.to_string())?
        .ok_or_else(|| "当前已是最新版本".to_string())?;

    // 版本号基线也要先记下来：安装器结束后，下次启动靠它判断"刚升级过"。
    // 没这一步的话，重启后拿不到说明。
    if let Err(err) = manager
        .store()
        .set_setting(LAST_RUN_VERSION_KEY, &current_version())
    {
        tracing::warn!("保存当前版本失败: {err}");
    }

    // 先把说明存下来：安装器会结束进程，之后再写就来不及了。
    let notes = update
        .body
        .as_deref()
        .map(str::trim)
        .filter(|notes| !notes.is_empty())
        .map(|notes| format!("{}\n\n{}", update.version, notes))
        .unwrap_or_else(|| update.version.clone());
    if let Err(err) = manager.store().set_setting(PENDING_NOTES_KEY, &notes) {
        tracing::warn!("保存更新说明失败: {err}");
    }

    let downloaded = Arc::new(Mutex::new(0u64));
    let handle = app.clone();
    let counter = downloaded.clone();

    let _ = handle.emit(
        "update-progress",
        serde_json::json!({ "phase": "download", "downloaded": 0, "total": null }),
    );

    update
        .download_and_install(
            move |chunk, total| {
                let mut current = counter.lock().unwrap_or_else(|err| err.into_inner());
                *current += chunk as u64;
                let _ = handle.emit(
                    "update-progress",
                    serde_json::json!({
                        "phase": "download",
                        "downloaded": *current,
                        "total": total,
                    }),
                );
            },
            || {},
        )
        .await
        .map_err(|err| err.to_string())?;

    let _ = app.emit("update-progress", serde_json::json!({ "phase": "installed" }));
    Ok(())
}

/// 启动后检查一次；有更新就发事件，界面会在列表顶部提示。
pub fn spawn_startup_check(app: AppHandle, manager: Arc<DownloadManager>) {
    tauri::async_runtime::spawn(async move {
        // 每次启动都查。用户可以在设置里关掉，但默认开，
        // 而且不再等 60 秒——更新提示应该在打开程序时就出现。
        if check_on_start_setting(&manager) {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            match check(&app, &manager, None).await {
                Ok(Some(info)) => {
                    tracing::info!(version = %info.version, "发现新版本");
                    let _ = app.emit("update-available", info);
                }
                Ok(None) => {}
                Err(err) => tracing::debug!("启动检查更新失败: {err}"),
            }
        }
    });
}
