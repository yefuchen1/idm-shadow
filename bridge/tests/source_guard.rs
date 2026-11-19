//! 接口层的「不是用户发起的下载不准建任务」回归。
//!
//! 2026-11-19 的事故：浏览器恢复的旧下载被扩展接管，7 个任务一起弹了
//! 保存位置确认框。扩展是第一道防线，这里是第二道——即使扩展漏判，
//! 接口本身也不接受没有用户来源的请求。

use std::sync::Arc;
use std::time::Duration;

use idm_shadow_bridge::{router, BridgeState};
use idm_shadow_engine::http::engine::DownloadConfig;
use idm_shadow_engine::manager::DownloadManager;
use idm_shadow_engine::store::Store;
use serde_json::{json, Value};

struct Harness {
    base: String,
    token: String,
    manager: Arc<DownloadManager>,
    _dir: std::path::PathBuf,
}

async fn start() -> Harness {
    let dir = std::env::temp_dir().join(format!("idm-bridge-source-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();

    let store = Arc::new(Store::open_in_memory().unwrap());
    let manager = Arc::new(
        DownloadManager::new(store, DownloadConfig::default()).unwrap(),
    );
    manager.set_default_dir(dir.clone()).await;

    let token = "source-guard-token".to_string();
    let state = BridgeState {
        manager: manager.clone(),
        token: Arc::new(token.clone()),
        port: 0,
        show_ui: None,
        extension_seen: Arc::new(std::sync::Mutex::new(None)),
        extension_version: Arc::new(std::sync::Mutex::new(None)),
        shipped_extension_version: Arc::new(String::new()),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });

    Harness {
        base,
        token,
        manager,
        _dir: dir,
    }
}

async fn create(h: &Harness, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/api/v1/tasks", h.base))
        .header(idm_shadow_bridge::TOKEN_HEADER, &h.token)
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_page_triggered_download_is_refused_by_the_api() {
    let h = start().await;
    let resp = create(
        &h,
        json!({
            "url": "https://ads.example.com/promo.exe",
            "source": "page",
            "autostart": true,
            "needs_confirm": true,
        }),
    )
    .await;

    assert!(
        !resp.status().is_success(),
        "页面自动发起的下载不该被接受: {}",
        resp.status()
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "not_user_triggered");
    assert!(h.manager.list().unwrap().is_empty());
}

#[tokio::test]
async fn a_restored_download_is_refused_by_the_api() {
    let h = start().await;
    let resp = create(
        &h,
        json!({
            "url": "https://dl.kookapp.cn/assets/release/html_pc/kook_3321/Kook_PC_Setup.exe",
            "source": "restored",
        }),
    )
    .await;

    assert!(!resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "not_user_triggered");
    assert!(h.manager.list().unwrap().is_empty());
}

#[tokio::test]
async fn a_request_without_a_source_is_refused_by_the_api() {
    // 老客户端不带来源字段。不带来源就不知道是谁发起的，不能放行。
    let h = start().await;
    let resp = create(
        &h,
        json!({
            "url": "https://example.com/file.bin",
        }),
    )
    .await;

    assert!(
        !resp.status().is_success(),
        "来源不明的请求不该被接受: {}",
        resp.status()
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "not_user_triggered");
    assert!(h.manager.list().unwrap().is_empty());
}

#[tokio::test]
async fn a_user_triggered_download_is_accepted_and_keeps_its_source() {
    let h = start().await;
    let resp = create(
        &h,
        json!({
            "url": "https://example.com/setup.exe",
            "filename": "setup.exe",
            "source": "user",
            "autostart": false,
            "needs_confirm": true,
        }),
    )
    .await;

    assert!(resp.status().is_success(), "{}", resp.status());
    let body: Value = resp.json().await.unwrap();
    let id = body["id"].as_str().unwrap();

    let tasks = h.manager.list().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].source.as_str(), "user");
    assert!(tasks[0].needs_confirm, "用户点击的下载仍要走确认流程");
    assert!(!id.is_empty());
}

#[tokio::test]
async fn the_task_list_reports_the_source() {
    // 界面上要能一眼看出任务是谁发起的。
    let h = start().await;
    let resp = create(
        &h,
        json!({
            "url": "https://example.com/setup.exe",
            "filename": "setup.exe",
            "source": "user",
            "autostart": false,
        }),
    )
    .await;
    assert!(resp.status().is_success());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let resp = client
        .get(format!("{}/api/v1/tasks", h.base))
        .header(idm_shadow_bridge::TOKEN_HEADER, &h.token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["tasks"][0]["source"], "user");
}
