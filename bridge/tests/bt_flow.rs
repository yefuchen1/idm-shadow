//! 真实磁力任务的接口级回归。
//!
//! 覆盖：创建任务 → 抓 metadata → 返回文件清单 → 勾选文件 →
//! 确认后真正下载 → 只下载勾选的文件。
//!
//! 需要真实网络，默认忽略：
//!
//! ```text
//! $env:IDM_BT_LIVE_MAGNET="magnet:?xt=urn:btih:...&tr=..."
//! cargo test -p idm-shadow-bridge --test bt_flow -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use idm_shadow_bridge::{router, BridgeState};
use idm_shadow_engine::http::engine::DownloadConfig;
use idm_shadow_engine::manager::DownloadManager;
use idm_shadow_engine::store::Store;
use serde_json::{json, Value};

async fn get_task(client: &reqwest::Client, base: &str, token: &str, id: &str) -> Value {
    let resp = client
        .get(format!("{base}/api/v1/tasks/{id}"))
        .header(idm_shadow_bridge::TOKEN_HEADER, token)
        .send()
        .await
        .expect("GET task 失败");
    assert!(resp.status().is_success(), "GET task: {}", resp.status());
    let body: Value = resp.json().await.unwrap();
    body["task"].clone()
}

#[tokio::test]
#[ignore]
async fn selective_magnet_download_through_bridge() {
    let magnet = std::env::var("IDM_BT_LIVE_MAGNET")
        .expect("需要设置 IDM_BT_LIVE_MAGNET");

    let dir = std::env::temp_dir().join(format!("idm-bridge-bt-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();

    let store = Arc::new(Store::open_in_memory().unwrap());
    let manager = Arc::new(
        DownloadManager::new(store, DownloadConfig::default()).unwrap(),
    );
    manager.set_default_dir(dir.clone()).await;

    let token = "bt-flow-token".to_string();
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

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    // 1. 创建任务
    let resp = client
        .post(format!("{base}/api/v1/tasks"))
        .header(idm_shadow_bridge::TOKEN_HEADER, &token)
        .json(&json!({
            "url": magnet,
            "autostart": true,
            "needs_confirm": true,
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "create: {}", resp.status());
    let created: Value = resp.json().await.unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    // 2. 等文件清单
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    let mut task = Value::Null;
    while std::time::Instant::now() < deadline {
        task = get_task(&client, &base, &token, &id).await;
        if !task["bt_files"].is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(!task["bt_files"].is_null(), "没有拿到 bt_files: {task}");

    let files = task["bt_files"]["files"].as_array().unwrap().clone();
    assert!(files.len() > 1, "这个种子应当有多个文件");
    let selected: Vec<usize> = files
        .iter()
        .filter(|f| f["path"].as_str().unwrap_or("").ends_with(".srt"))
        .map(|f| f["index"].as_u64().unwrap() as usize)
        .collect();
    assert!(!selected.is_empty(), "应当至少有一个字幕文件");
    println!("选中 {} 个文件: {:?}", selected.len(), selected);

    // 3. 确认并开始下载
    let resp = client
        .post(format!("{base}/api/v1/tasks/{id}/confirm"))
        .header(idm_shadow_bridge::TOKEN_HEADER, &token)
        .json(&json!({
            "dir": dir.to_string_lossy(),
            "files": selected,
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "confirm: {}", resp.status());

    // 4. 等完成
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    loop {
        task = get_task(&client, &base, &token, &id).await;
        let status = task["status"].as_str().unwrap_or("");
        if status == "completed" {
            break;
        }
        assert!(
            status != "error" && status != "cancelled",
            "下载失败: {task}"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "下载超时: {task}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // 5. 勾选的文件完整，未勾选的大文件没被下载
    for index in &selected {
        let file = &files[*index];
        let length = file["length"].as_u64().unwrap();
        let path = dir.join(
            file["path"]
                .as_str()
                .unwrap()
                .replace('/', std::path::MAIN_SEPARATOR_STR),
        );
        assert_eq!(std::fs::metadata(&path).unwrap().len(), length);
    }

    let largest = files
        .iter()
        .max_by_key(|f| f["length"].as_u64().unwrap_or(0))
        .unwrap();
    let largest_index = largest["index"].as_u64().unwrap() as usize;
    if !selected.contains(&largest_index) {
        let path = dir.join(
            largest["path"]
                .as_str()
                .unwrap()
                .replace('/', std::path::MAIN_SEPARATOR_STR),
        );
        let logical = std::fs::metadata(&path).unwrap().len();
        assert_eq!(logical, largest["length"].as_u64().unwrap());
        #[cfg(windows)]
        assert!(
            idm_shadow_engine::bt::sparse::allocated_size(&path).unwrap() < 1024 * 1024,
            "没勾选的大文件不应该占盘"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}
