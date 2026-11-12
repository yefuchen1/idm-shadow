//! 任务生命周期测试：暂停、继续、取消、删除。
//!
//! 这些路径以前出过几个 bug（暂停被当成取消、运行表不释放导致继续无效），
//! 所以单独拿测试盯住。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use idm_shadow_engine::http::engine::DownloadConfig;
use idm_shadow_engine::manager::DownloadManager;
use idm_shadow_engine::store::Store;
use idm_shadow_engine::task::{TaskRequest, TaskStatus};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

/// 一个慢速服务器：每次只吐一点点，方便在下载中途操作。
async fn serve_slow(listener: TcpListener, body: Arc<Vec<u8>>) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let body = body.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 2048];
            loop {
                let Ok(n) = socket.read(&mut tmp).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }

            let request = String::from_utf8_lossy(&buf).to_string();
            let total = body.len() as u64;
            let range = request
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                .and_then(|l| l.split_once('='))
                .map(|(_, v)| v.trim().to_string())
                .and_then(|v| {
                    let mut parts = v.split('-');
                    let start: u64 = parts.next()?.trim().parse().ok()?;
                    let end: u64 = parts.next().unwrap_or("").trim().parse().ok()?;
                    Some((start, end))
                });

            let (start, end) = match range {
                Some((s, e)) => (s.min(total), e.min(total.saturating_sub(1))),
                None => (0, total.saturating_sub(1)),
            };
            let length = end.saturating_sub(start) + 1;
            let header = format!(
                "HTTP/1.1 206 Partial Content\r\n\
                 Content-Length: {}\r\n\
                 Content-Range: bytes {}-{}/{}\r\n\
                 Accept-Ranges: bytes\r\n\
                 ETag: \"slow\"\r\n\
                 Connection: close\r\n\r\n",
                length, start, end, total
            );
            if socket.write_all(header.as_bytes()).await.is_err() {
                return;
            }

            // 每 4KB 停 30ms，慢到足够在下载中途操作
            let mut offset = start as usize;
            let end_exclusive = end as usize + 1;
            while offset < end_exclusive {
                let hi = (offset + 4096).min(end_exclusive);
                if socket.write_all(&body[offset..hi]).await.is_err() {
                    return;
                }
                offset = hi;
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        });
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "idm-shadow-lifecycle-{}-{}-{}",
        tag,
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn manager(_dir: &PathBuf) -> Arc<DownloadManager> {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let config = DownloadConfig {
        max_connections: 4,
        ..Default::default()
    };
    Arc::new(DownloadManager::new(store, config).unwrap())
}

async fn wait_for<F>(mut check: F, timeout: Duration) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test]
async fn pause_keeps_task_paused_and_resume_restarts_it() {
    let body: Vec<u8> = (0..2_000_000u32).map(|i| (i % 251) as u8).collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve_slow(listener, Arc::new(body.clone())));

    let dir = temp_dir("pause");
    let mgr = manager(&dir);
    let task = mgr
        .add(TaskRequest {
            url: format!("http://{addr}/big.bin"),
            filename: Some("big.bin".to_string()),
            dir: Some(dir.to_string_lossy().to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    mgr.start(task.id).await.unwrap();

    // 等它真的开始下载
    let started = wait_for(
        || {
            mgr.get(task.id)
                .unwrap()
                .map(|t| t.downloaded > 0)
                .unwrap_or(false)
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(started, "任务应当开始下载");

    mgr.pause(task.id).await.unwrap();

    // 暂停后状态应当是 Paused，而不是 Cancelled
    let paused = wait_for(
        || {
            mgr.get(task.id)
                .unwrap()
                .map(|t| t.status == TaskStatus::Paused)
                .unwrap_or(false)
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(paused, "暂停后状态应当是 Paused");

    // 暂停期间进度不再增长
    let before = mgr.get(task.id).unwrap().unwrap().downloaded;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let after = mgr.get(task.id).unwrap().unwrap().downloaded;
    assert_eq!(before, after, "暂停期间进度不应该继续增长");

    // 继续：任务要能真的重新启动
    mgr.resume(task.id).await.unwrap();
    let resumed = wait_for(
        || {
            mgr.get(task.id)
                .unwrap()
                .map(|t| {
                    t.status == TaskStatus::Downloading || t.status == TaskStatus::Completed
                })
                .unwrap_or(false)
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(resumed, "继续后任务应当重新开始下载");

    // 最后要能下完
    let done = wait_for(
        || {
            mgr.get(task.id)
                .unwrap()
                .map(|t| t.status == TaskStatus::Completed)
                .unwrap_or(false)
        },
        Duration::from_secs(30),
    )
    .await;
    assert!(done, "任务应当下载完成");

    let content = std::fs::read(dir.join("big.bin")).unwrap();
    assert_eq!(content.len(), body.len());
    assert!(content == body, "暂停续传后的文件必须和原文件一致");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn cancel_marks_task_cancelled() {
    let body: Vec<u8> = (0..2_000_000u32).map(|i| (i % 251) as u8).collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve_slow(listener, Arc::new(body.clone())));

    let dir = temp_dir("cancel");
    let mgr = manager(&dir);
    let task = mgr
        .add(TaskRequest {
            url: format!("http://{addr}/big.bin"),
            filename: Some("c.bin".to_string()),
            dir: Some(dir.to_string_lossy().to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    mgr.start(task.id).await.unwrap();

    let started = wait_for(
        || {
            mgr.get(task.id)
                .unwrap()
                .map(|t| t.downloaded > 0)
                .unwrap_or(false)
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(started);

    mgr.cancel(task.id).await.unwrap();

    let cancelled = wait_for(
        || {
            mgr.get(task.id)
                .unwrap()
                .map(|t| t.status == TaskStatus::Cancelled)
                .unwrap_or(false)
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(cancelled, "取消后状态应当是 Cancelled");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn remove_deletes_task_and_optional_file() {
    let dir = temp_dir("remove");
    let mgr = manager(&dir);
    let task = mgr
        .add(TaskRequest {
            url: "http://127.0.0.1:1/x.bin".to_string(),
            filename: Some("x.bin".to_string()),
            dir: Some(dir.to_string_lossy().to_string()),
            ..Default::default()
        })
        .await
        .unwrap();

    std::fs::write(dir.join("x.bin"), b"dummy").unwrap();

    mgr.remove(task.id, false).await.unwrap();
    assert!(mgr.get(task.id).unwrap().is_none(), "任务应当被删掉");
    assert!(dir.join("x.bin").exists(), "不删文件时文件应当保留");

    let task2 = mgr
        .add(TaskRequest {
            url: "http://127.0.0.1:1/y.bin".to_string(),
            filename: Some("y.bin".to_string()),
            dir: Some(dir.to_string_lossy().to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    std::fs::write(dir.join("y.bin"), b"dummy").unwrap();

    mgr.remove(task2.id, true).await.unwrap();
    assert!(!dir.join("y.bin").exists(), "勾选删除文件时文件应当被删掉");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn concurrent_task_limit_queues_the_rest() {
    let body: Vec<u8> = (0..2_000_000u32).map(|i| (i % 251) as u8).collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve_slow(listener, Arc::new(body.clone())));

    let dir = temp_dir("concurrency");
    let mgr = manager(&dir);
    // 只允许同时跑 1 个任务
    mgr.set_max_concurrent_tasks(Some(1)).await;

    let mut ids = Vec::new();
    for i in 0..3 {
        let task = mgr
            .add(TaskRequest {
                url: format!("http://{addr}/f{i}.bin"),
                filename: Some(format!("f{i}.bin")),
                dir: Some(dir.to_string_lossy().to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        mgr.start(task.id).await.unwrap();
        ids.push(task.id);
    }

    // 只应该有一个任务真正在跑，其余排队
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let statuses: Vec<TaskStatus> = ids
        .iter()
        .map(|id| mgr.get(*id).unwrap().unwrap().status)
        .collect();
    let downloading = statuses
        .iter()
        .filter(|s| **s == TaskStatus::Downloading)
        .count();
    assert_eq!(downloading, 1, "同时只该有一个任务在下载，实际: {statuses:?}");

    // 排队的任务状态是 Queued
    let queued = statuses
        .iter()
        .filter(|s| **s == TaskStatus::Queued)
        .count();
    assert_eq!(queued, 2, "另外两个应当排队，实际: {statuses:?}");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn raising_the_limit_lets_queued_tasks_start() {
    let body: Vec<u8> = (0..500_000u32).map(|i| (i % 251) as u8).collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve_slow(listener, Arc::new(body.clone())));

    let dir = temp_dir("raise-limit");
    let mgr = manager(&dir);
    mgr.set_max_concurrent_tasks(Some(1)).await;

    let mut ids = Vec::new();
    for i in 0..3 {
        let task = mgr
            .add(TaskRequest {
                url: format!("http://{addr}/r{i}.bin"),
                filename: Some(format!("r{i}.bin")),
                dir: Some(dir.to_string_lossy().to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        mgr.start(task.id).await.unwrap();
        ids.push(task.id);
    }

    tokio::time::sleep(Duration::from_millis(500)).await;

    // 放开限制后，排队里的任务应该陆续跑起来
    mgr.set_max_concurrent_tasks(None).await;
    let pump = mgr.handle();
    pump.pump_queue_for_test().await;

    let started = wait_for(
        || {
            ids.iter()
                .filter(|id| {
                    matches!(
                        mgr.get(**id).unwrap().unwrap().status,
                        TaskStatus::Downloading | TaskStatus::Completed
                    )
                })
                .count()
                >= 2
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(started, "放开限制后更多任务应当开始下载");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn confirm_flow_stages_then_transfers_to_chosen_path() {
    let body: Vec<u8> = (0..500_000u32).map(|i| (i % 251) as u8).collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve_slow(listener, Arc::new(body.clone())));

    let dir = temp_dir("confirm");
    let mgr = manager(&dir);

    let task = mgr
        .add(TaskRequest {
            url: format!("http://{addr}/c.bin"),
            filename: Some("c.bin".to_string()),
            dir: Some(dir.to_string_lossy().to_string()),
            needs_confirm: true,
            ..Default::default()
        })
        .await
        .unwrap();

    // 需要确认时，暂存路径要准备好，状态是待确认
    let created = mgr.get(task.id).unwrap().unwrap();
    assert!(created.needs_confirm);
    assert_eq!(created.status, TaskStatus::AwaitingConfirm);
    assert!(created.staging_path.is_some(), "应当准备好暂存路径");

    // 确认到一个别的目录
    let custom = dir.join("custom");
    std::fs::create_dir_all(&custom).unwrap();
    mgr.confirm_path(task.id, Some(custom.join("c.bin")))
        .await
        .unwrap();

    let confirmed = mgr.get(task.id).unwrap().unwrap();
    assert!(!confirmed.needs_confirm);
    assert!(confirmed.final_path.is_some());

    // 开始下载，等它完成
    mgr.start(task.id).await.unwrap();
    let done = wait_for(
        || {
            mgr.get(task.id)
                .unwrap()
                .map(|t| t.status == TaskStatus::Completed)
                .unwrap_or(false)
        },
        Duration::from_secs(30),
    )
    .await;
    assert!(done, "任务应当下载完成");

    // 文件应当出现在用户选的位置
    let final_file = custom.join("c.bin");
    assert!(final_file.exists(), "文件应当转存到用户选择的位置");
    let content = std::fs::read(&final_file).unwrap();
    assert!(content == body, "转存后的文件内容必须一致");

    // 暂存文件应当已经不在了
    let staging = confirmed.staging_path.clone().unwrap();
    assert!(!staging.exists(), "暂存文件应当被转存掉");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn cancel_with_cleanup_removes_staged_file() {
    let body: Vec<u8> = (0..500_000u32).map(|i| (i % 251) as u8).collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve_slow(listener, Arc::new(body.clone())));

    let dir = temp_dir("confirm-cancel");
    let mgr = manager(&dir);

    let task = mgr
        .add(TaskRequest {
            url: format!("http://{addr}/x.bin"),
            filename: Some("x.bin".to_string()),
            dir: Some(dir.to_string_lossy().to_string()),
            needs_confirm: true,
            ..Default::default()
        })
        .await
        .unwrap();

    mgr.start(task.id).await.unwrap();
    // 等它写了一点到暂存文件
    let started = wait_for(
        || {
            mgr.get(task.id)
                .unwrap()
                .map(|t| t.downloaded > 0)
                .unwrap_or(false)
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(started, "下载应当已经开始");

    let staging = mgr.get(task.id).unwrap().unwrap().staging_path.clone().unwrap();
    assert!(staging.exists(), "暂存文件应该存在");

    mgr.cancel_with_cleanup(task.id).await.unwrap();

    assert!(!staging.exists(), "取消后暂存文件应当被删掉");
    let after = mgr.get(task.id).unwrap().unwrap();
    assert_eq!(after.status, TaskStatus::Cancelled);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn confirm_before_start_still_transfers_to_chosen_path() {
    // 回归测试：用户抢在下载启动之前就确认了保存位置。
    // 那时任务还没进 running 表，内存槽是空的，
    // 路径只能从数据库里读回来。之前这里会丢路径，文件落到默认目录。
    let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve_slow(listener, Arc::new(body.clone())));

    let dir = temp_dir("confirm-early");
    let mgr = manager(&dir);

    let task = mgr
        .add(TaskRequest {
            url: format!("http://{addr}/early.bin"),
            filename: Some("early.bin".to_string()),
            dir: Some(dir.to_string_lossy().to_string()),
            needs_confirm: true,
            ..Default::default()
        })
        .await
        .unwrap();

    // 先确认，再启动
    let custom = dir.join("early-target");
    std::fs::create_dir_all(&custom).unwrap();
    mgr.confirm_path(task.id, Some(custom.join("early.bin")))
        .await
        .unwrap();

    mgr.start(task.id).await.unwrap();
    let done = wait_for(
        || {
            mgr.get(task.id)
                .unwrap()
                .map(|t| t.status == TaskStatus::Completed)
                .unwrap_or(false)
        },
        Duration::from_secs(30),
    )
    .await;
    assert!(done, "任务应当下载完成");

    let final_file = custom.join("early.bin");
    assert!(final_file.exists(), "文件应当转存到用户选的位置");
    let content = std::fs::read(&final_file).unwrap();
    assert!(content == body, "转存后的内容必须一致");

    // 默认目录里不该有残留
    assert!(!dir.join("early.bin").exists(), "默认目录不该有文件");

    std::fs::remove_dir_all(&dir).ok();
}