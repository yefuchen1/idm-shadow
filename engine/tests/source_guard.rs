//! 「不是用户发起的下载不准跑」的回归测试。
//!
//! 起因是 2026-11-19 20:12 的事故：
//!
//!   Edge 重启时把 7 个几天前没下完的下载记录重新创建了一遍，
//!   `chrome.downloads.onCreated` 把它们当成新下载，扩展照单全收，
//!   应用自动开跑、连弹 7 个保存位置确认框。
//!
//! 根因是"接管"这条路不区分下载是谁发起的。这个文件盯住三层防线：
//!   1. 引擎拒绝非用户来源的任务（`source` 不可信就建不了任务）；
//!   2. 图片预览（NAS 封面之类）不建任务；
//!   3. 任务来源会落库，界面上能一眼看出来。

use std::sync::Arc;

use idm_shadow_engine::http::engine::DownloadConfig;
use idm_shadow_engine::manager::DownloadManager;
use idm_shadow_engine::store::Store;
use idm_shadow_engine::task::{looks_like_image_url, TaskRequest, TaskSource, TaskStatus};
use uuid::Uuid;

fn manager() -> Arc<DownloadManager> {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let config = DownloadConfig {
        max_connections: 4,
        ..Default::default()
    };
    Arc::new(DownloadManager::new(store, config).unwrap())
}

#[tokio::test]
async fn page_triggered_downloads_are_refused() {
    // 页面脚本自己发起的下载：用户当时没有说"我要下"。
    let mgr = manager();
    let err = mgr
        .add(TaskRequest {
            url: "https://ads.example.com/promo.exe".to_string(),
            source: TaskSource::Page,
            ..Default::default()
        })
        .await
        .expect_err("页面自动发起的下载不该建任务");
    assert!(
        err.to_string().contains("不是用户发起"),
        "错误信息要说明原因: {err}"
    );
    assert!(mgr.list().unwrap().is_empty());
}

#[tokio::test]
async fn restored_downloads_are_refused() {
    // 浏览器恢复的旧下载：startTime 是几天前，用户此刻没碰过。
    let mgr = manager();
    let err = mgr
        .add(TaskRequest {
            url: "https://dl.kookapp.cn/assets/release/html_pc/kook_3321/Kook_PC_Setup.exe"
                .to_string(),
            source: TaskSource::Restored,
            ..Default::default()
        })
        .await
        .expect_err("浏览器恢复的下载不该建任务");
    assert!(err.to_string().contains("不是用户发起"), "{err}");
    assert!(mgr.list().unwrap().is_empty());
}

#[tokio::test]
async fn requests_without_a_source_are_refused() {
    // 老客户端不带来源字段。不带来源 = 不知道是谁发起的 = 不可信。
    //
    // 这条是防"降级到旧客户端就能绕过"的：来源缺失时宁可不做。
    let mgr = manager();
    let err = mgr
        .add(TaskRequest {
            url: "https://example.com/file.bin".to_string(),
            source: TaskSource::Legacy,
            ..Default::default()
        })
        .await
        .expect_err("来源不明的下载不该建任务");
    assert!(err.to_string().contains("不是用户发起"), "{err}");
}

#[tokio::test]
async fn nas_preview_images_are_refused() {
    // 事故里漏掉的那一条：NAS 封面预览图。
    // URL 和文件名都没有后缀，只看后缀会漏。
    let mgr = manager();
    let err = mgr
        .add(TaskRequest {
            url: "http://192.168.2.39/api/capture_image?source=0".to_string(),
            filename: Some("capture_image".to_string()),
            source: TaskSource::Extension,
            ..Default::default()
        })
        .await
        .expect_err("图片预览不该建任务");
    assert!(err.to_string().contains("图片预览"), "{err}");
    assert!(mgr.list().unwrap().is_empty());
}

#[tokio::test]
async fn user_clicks_are_still_accepted() {
    // 最重要的回归：不能为了堵住误接管把正常下载也堵了。
    let mgr = manager();
    let task = mgr
        .add(TaskRequest {
            url: "https://example.com/setup.exe".to_string(),
            filename: Some("setup.exe".to_string()),
            source: TaskSource::User,
            ..Default::default()
        })
        .await
        .expect("用户点击的下载必须照常建任务");
    assert_eq!(task.source, TaskSource::User);
    assert_eq!(task.filename, "setup.exe");
}

#[tokio::test]
async fn only_user_triggered_tasks_get_a_confirm_dialog() {
    // 扩展接管的下载即使带了 needs_confirm，也不该弹框打断用户。
    let mgr = manager();
    let task = mgr
        .add(TaskRequest {
            url: "https://example.com/manual.bin".to_string(),
            filename: Some("manual.bin".to_string()),
            needs_confirm: true,
            source: TaskSource::Extension,
            ..Default::default()
        })
        .await
        .expect("扩展接管的下载可以建任务");
    assert!(
        !task.needs_confirm,
        "来源不确定的下载不该弹保存位置确认框"
    );
    assert_ne!(task.status, TaskStatus::AwaitingConfirm);
}

#[tokio::test]
async fn the_source_survives_a_round_trip_through_the_database() {
    // 来源要落库：界面和事后排查都靠它。
    let store = Arc::new(Store::open_in_memory().unwrap());
    let mgr = Arc::new(
        DownloadManager::new(store.clone(), DownloadConfig::default()).unwrap(),
    );
    let task = mgr
        .add(TaskRequest {
            url: "https://example.com/kept.bin".to_string(),
            filename: Some("kept.bin".to_string()),
            source: TaskSource::Userscript,
            ..Default::default()
        })
        .await
        .unwrap();

    let stored = store.get_task(task.id).unwrap().unwrap();
    assert_eq!(stored.source, TaskSource::Userscript);
    assert_eq!(stored.source.as_str(), "userscript");
}

#[test]
fn image_urls_are_recognized_without_a_file_extension() {
    // 事故里漏掉的那一条。
    assert!(looks_like_image_url(
        "http://192.168.2.39/api/capture_image?source=0"
    ));
    assert!(looks_like_image_url("https://x.com/snapshot"));
    assert!(looks_like_image_url("https://x.com/thumb/1"));
    assert!(looks_like_image_url("https://x.com/preview.png"));
    assert!(looks_like_image_url("https://x.com/raw?format=jpg"));
    assert!(looks_like_image_url("https://x.com/raw?mime=image%2Fpng"));

    // 不能误伤：主机名里带 images 的压缩包、正常的下载接口。
    assert!(!looks_like_image_url("https://images.example.com/file.zip"));
    assert!(!looks_like_image_url("https://x.com/api/download?url=https%3A%2F%2Fa"));
    assert!(!looks_like_image_url("https://x.com/setup.exe"));
    assert!(!looks_like_image_url("https://x.com/"));
    assert!(!looks_like_image_url(""));
}

#[test]
fn sources_decide_what_is_allowed() {
    assert!(TaskSource::Page.is_blocked());
    assert!(TaskSource::Restored.is_blocked());
    assert!(TaskSource::Legacy.is_blocked());
    assert!(!TaskSource::User.is_blocked());
    assert!(!TaskSource::Userscript.is_blocked());

    assert!(TaskSource::User.may_confirm());
    assert!(TaskSource::Userscript.may_confirm());
    assert!(!TaskSource::Extension.may_confirm());
    assert!(!TaskSource::Page.may_confirm());

    assert!(TaskSource::User.may_autostart());
    assert!(!TaskSource::Extension.may_autostart());
    assert!(!TaskSource::Page.may_autostart());
}
