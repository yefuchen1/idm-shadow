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

/// 磁力/种子任务：抓完文件清单后先停在确认框，勾选结果要落库，
/// 确认后能重新启动下载。
#[tokio::test]
async fn bt_task_waits_for_file_selection_then_restarts() {
    use bittorrent_rs::utils::bencode::{BNode, Bencode};

    let dir = std::env::temp_dir().join(format!("idm-bt-lifecycle-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();

    let pieces = [0u8; 20];
    let info = Bencode::encode(&BNode::Dictionary(vec![
        (
            b"files".as_slice(),
            BNode::List(vec![
                BNode::Dictionary(vec![
                    (b"length".as_slice(), BNode::Number(b"1024")),
                    (
                        b"path".as_slice(),
                        BNode::List(vec![BNode::String(b"a.bin")]),
                    ),
                ]),
                BNode::Dictionary(vec![
                    (b"length".as_slice(), BNode::Number(b"2048")),
                    (
                        b"path".as_slice(),
                        BNode::List(vec![BNode::String(b"b.bin")]),
                    ),
                ]),
            ]),
        ),
        (b"name".as_slice(), BNode::String(b"sample")),
        (b"piece length".as_slice(), BNode::Number(b"16384")),
        (b"pieces".as_slice(), BNode::String(&pieces)),
    ]));
    let torrent_path = dir.join("sample.torrent");
    std::fs::write(
        &torrent_path,
        idm_shadow_engine::bt::metadata::build_torrent_bytes(
            &info,
            &["http://127.0.0.1:1/announce".to_string()],
        ),
    )
    .unwrap();

    let mgr = manager(&dir);
    let task = mgr
        .add(TaskRequest {
            url: torrent_path.to_string_lossy().to_string(),
            dir: Some(dir.to_string_lossy().to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(task.engine, idm_shadow_engine::task::EngineKind::Bt);

    mgr.start(task.id).await.unwrap();
    let ready = wait_for(
        || {
            mgr.get(task.id)
                .ok()
                .flatten()
                .map(|t| t.bt_files.is_some())
                .unwrap_or(false)
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(ready, "prepare 应当很快读出本地种子的文件清单");

    let waiting = mgr.get(task.id).unwrap().unwrap();
    assert_eq!(waiting.status, TaskStatus::AwaitingConfirm);
    let files = waiting.bt_files.as_ref().unwrap();
    assert_eq!(files.files.len(), 2);
    assert!(files.needs_selection);
    assert_eq!(waiting.filename, "sample");
    assert!(dir.join("sample").join("a.bin").is_file());
    assert_eq!(
        std::fs::metadata(dir.join("sample").join("b.bin"))
            .unwrap()
            .len(),
        2048
    );

    mgr.confirm_path(task.id, None, Some(vec![1])).await.unwrap();
    let confirmed = mgr.get(task.id).unwrap().unwrap();
    assert_eq!(confirmed.bt_selected, Some(vec![1]));
    assert!(!confirmed.needs_confirm);

    // 这个种子没有真实 peer，下载会一直等；收尾别让测试挂着。
    mgr.pause(task.id).await.ok();
    mgr.cancel(task.id).await.ok();
    std::fs::remove_dir_all(&dir).ok();
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
    mgr.confirm_path(task.id, Some(custom.join("c.bin")), None)
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
    mgr.confirm_path(task.id, Some(custom.join("early.bin")), None)
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
#[tokio::test]
async fn confirm_after_completion_moves_the_file() {
    // 回归测试：下载很快时，用户点确认之前文件就下完了。
    //
    // IDM 式的流程是：文件先落在暂存位置，用户确认后才转存。
    // 所以下载完成时，文件应该在 .idmshadow/ 下的暂存文件里，
    // 默认目录不该有东西；确认之后才出现在用户选的位置。
    let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve_slow(listener, Arc::new(body.clone())));

    let dir = temp_dir("confirm-late");
    let mgr = manager(&dir);

    let task = mgr
        .add(TaskRequest {
            url: format!("http://{addr}/late.bin"),
            filename: Some("late.bin".to_string()),
            dir: Some(dir.to_string_lossy().to_string()),
            needs_confirm: true,
            ..Default::default()
        })
        .await
        .unwrap();

    mgr.start(task.id).await.unwrap();

    // 等下载完成。注意状态是 AwaitingConfirm（还在等用户点确认），
    // 不是 Completed——文件下完了但还没转存。
    let done = wait_for(
        || {
            mgr.get(task.id)
                .unwrap()
                .map(|t| t.downloaded == body.len() as u64)
                .unwrap_or(false)
        },
        Duration::from_secs(30),
    )
    .await;
    assert!(done, "下载应当完成");

    // 文件在暂存位置，默认目录是干净的
    assert!(
        !dir.join("late.bin").exists(),
        "确认之前默认目录不该有文件"
    );
    let staging = dir.join(".idmshadow").join(format!("{}.part", task.id));
    assert!(staging.exists(), "暂存文件应当存在");

    // 用户确认另一个位置
    let custom = dir.join("late-target");
    std::fs::create_dir_all(&custom).unwrap();
    mgr.confirm_path(task.id, Some(custom.join("late.bin")), None)
        .await
        .unwrap();

    let final_file = custom.join("late.bin");
    assert!(final_file.exists(), "确认后文件应当出现在目标位置");
    assert!(!staging.exists(), "暂存文件应当被转存掉");

    let content = std::fs::read(&final_file).unwrap();
    assert!(content == body, "转存后内容必须一致");

    // 状态变成已完成
    let after = mgr.get(task.id).unwrap().unwrap();
    assert_eq!(after.status, TaskStatus::Completed);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn confirm_long_after_completion_moves_the_file() {
    // 下载早就跑完了，用户过了好一会儿才点确认。
    //
    // 这是实际使用里最常见的顺序——文件几十兆，下完���用户
    // 过一会儿才回到对话框点"立即下载"。
    // 引擎这时候已经彻底收工，转存必须由确认这一侧完成。
    let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve_slow(listener, Arc::new(body.clone())));

    let dir = temp_dir("confirm-much-later");
    let mgr = manager(&dir);

    let task = mgr
        .add(TaskRequest {
            url: format!("http://{addr}/late.bin"),
            filename: Some("late.bin".to_string()),
            dir: Some(dir.to_string_lossy().to_string()),
            needs_confirm: true,
            ..Default::default()
        })
        .await
        .unwrap();

    mgr.start(task.id).await.unwrap();

    let done = wait_for(
        || {
            mgr.get(task.id)
                .unwrap()
                .map(|t| t.downloaded == body.len() as u64)
                .unwrap_or(false)
        },
        Duration::from_secs(30),
    )
    .await;
    assert!(done, "下载应当完成");

    // 让引擎彻底收工：引擎返回、阶段处理跑完、通道关闭。
    tokio::time::sleep(Duration::from_millis(800)).await;

    let staging = dir.join(".idmshadow").join(format!("{}.part", task.id));
    assert!(staging.exists(), "暂存文件应当还在等着转存");

    let custom = dir.join("late-target");
    std::fs::create_dir_all(&custom).unwrap();
    mgr.confirm_path(task.id, Some(custom.join("late.bin")), None)
        .await
        .unwrap();

    let final_file = custom.join("late.bin");
    assert!(
        final_file.exists(),
        "引擎早就跑完了，这时确认也必须把文件转存过去"
    );
    let content = std::fs::read(&final_file).unwrap();
    assert!(content == body, "转存后内容必须一致");
    assert!(!staging.exists(), "暂存文件应当被转存掉");

    let after = mgr.get(task.id).unwrap().unwrap();
    assert_eq!(after.status, TaskStatus::Completed);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn confirm_without_custom_path_lands_in_the_default_dir() {
    // 回归测试：用户点"直接下载"，没指定目录。
    //
    // 之前 confirm_path 把"没给路径"原样记成 None，
    // 下载完成那一步看到 final_path 是 None，就以为
    // "用户还没确认"，把文件永远留在 .idmshadow 暂存目录里，
    // 界面上还显示成"待确认"——下载完了却不见文件。
    //
    // 现在没给路径就解析成 默认目录 + 原文件名。
    let body: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8).collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve_slow(listener, Arc::new(body.clone())));

    let dir = temp_dir("confirm-default");
    let mgr = manager(&dir);

    let task = mgr
        .add(TaskRequest {
            url: format!("http://{addr}/default.bin"),
            filename: Some("default.bin".to_string()),
            dir: Some(dir.to_string_lossy().to_string()),
            needs_confirm: true,
            ..Default::default()
        })
        .await
        .unwrap();

    // 直接下载：不给路径
    mgr.confirm_path(task.id, None, None).await.unwrap();

    let confirmed = mgr.get(task.id).unwrap().unwrap();
    assert!(
        confirmed.final_path.is_some(),
        "确认后必须记下最终位置，否则文件会卡在暂存目录"
    );

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

    // 文件应当在默认目录里
    let final_file = dir.join("default.bin");
    assert!(
        final_file.exists(),
        "直接下载后文件应当出现在默认目录，而不是留在暂存目录"
    );
    let content = std::fs::read(&final_file).unwrap();
    assert!(content == body, "内容必须一致");

    // 暂存目录不该有残留
    let staging = dir.join(".idmshadow").join(format!("{}.part", task.id));
    assert!(!staging.exists(), "暂存文件应当被转存掉");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn confirm_starts_a_task_that_was_only_registered() {
    // 回归测试：bridge 的 autostart=false（扩展投递、网页「提交下载」）。
    //
    // 这种任务只登记、不启动，等用户在保存位置弹窗里点确认才开始。
    // 以前 confirm_path 只把状态改成「下载中」，没人真正拉起引擎，
    // 界面上的进度就永远停在 0 字节，看起来像网络卡死；
    // 手动点一次继续才突然跑完。
    let body: Vec<u8> = (0..120_000u32).map(|i| (i % 251) as u8).collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve_slow(listener, Arc::new(body.clone())));

    let dir = temp_dir("confirm-not-started");
    let mgr = manager(&dir);

    let task = mgr
        .add(TaskRequest {
            url: format!("http://{addr}/registered.bin"),
            filename: Some("registered.bin".to_string()),
            dir: Some(dir.to_string_lossy().to_string()),
            needs_confirm: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        mgr.get(task.id).unwrap().unwrap().status,
        TaskStatus::AwaitingConfirm
    );

    // 只确认，不手动 start——这一步必须自己把下载拉起来。
    mgr.confirm_path(task.id, Some(dir.join("registered.bin")), None)
        .await
        .unwrap();

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
    assert!(done, "确认之后任务应当自己开始并下载完成");

    let content = std::fs::read(dir.join("registered.bin")).unwrap();
    assert!(content == body, "内容必须一致");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn confirm_while_downloading_keeps_running_until_the_file_lands() {
    // 回归测试：下载进行中点确认。
    //
    // 确认只是记录最终位置，不该打断下载。
    // 下载结束后再转存到那个位置。
    let body: Vec<u8> = (0..800_000u32).map(|i| (i % 251) as u8).collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve_slow(listener, Arc::new(body.clone())));

    let dir = temp_dir("confirm-live");
    let mgr = manager(&dir);

    let task = mgr
        .add(TaskRequest {
            url: format!("http://{addr}/live.bin"),
            filename: Some("live.bin".to_string()),
            dir: Some(dir.to_string_lossy().to_string()),
            needs_confirm: true,
            ..Default::default()
        })
        .await
        .unwrap();

    mgr.start(task.id).await.unwrap();

    // 等下载真的动起来
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

    // 下载中确认到另一个目录
    let custom = dir.join("live-target");
    std::fs::create_dir_all(&custom).unwrap();
    mgr.confirm_path(task.id, Some(custom.join("live.bin")), None)
        .await
        .unwrap();

    // 确认不该把任务打断——状态回到下载中
    let mid = mgr.get(task.id).unwrap().unwrap();
    assert_eq!(mid.status, TaskStatus::Downloading);
    assert!(!mid.needs_confirm);

    // 等它下完
    let done = wait_for(
        || {
            mgr.get(task.id)
                .unwrap()
                .map(|t| t.status == TaskStatus::Completed)
                .unwrap_or(false)
        },
        Duration::from_secs(40),
    )
    .await;
    assert!(done, "确认之后下载应当继续并完成");

    let final_file = custom.join("live.bin");
    assert!(final_file.exists(), "文件应当转存到确认的位置");
    let content = std::fs::read(&final_file).unwrap();
    assert!(content == body, "内容必须一致");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn removing_a_task_deletes_the_file_it_actually_landed_on() {
    // 回归测试：用户在确认弹窗里选了别的目录，文件落在那儿。
    //
    // 之前删除任务只删默认目录里的路径，用户选的位置上
    // 那个真文件会变成删不掉的孤儿，白占磁盘。
    let body: Vec<u8> = (0..120_000u32).map(|i| (i % 251) as u8).collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve_slow(listener, Arc::new(body.clone())));

    let dir = temp_dir("remove-final");
    let mgr = manager(&dir);

    let task = mgr
        .add(TaskRequest {
            url: format!("http://{addr}/rm.bin"),
            filename: Some("rm.bin".to_string()),
            dir: Some(dir.to_string_lossy().to_string()),
            needs_confirm: true,
            ..Default::default()
        })
        .await
        .unwrap();

    let custom = dir.join("rm-target");
    std::fs::create_dir_all(&custom).unwrap();
    mgr.confirm_path(task.id, Some(custom.join("rm.bin")), None)
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

    let final_file = custom.join("rm.bin");
    assert!(final_file.exists(), "文件应当在用户选的位置");

    // 删除任务并勾上"删除文件"
    mgr.remove(task.id, true).await.unwrap();

    assert!(
        !final_file.exists(),
        "用户选的位置上的文件也应当被删掉，不能留孤儿"
    );
    assert!(
        !dir.join("rm.bin").exists(),
        "默认位置也不该残留文件"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_failed_task_stops_asking_for_a_save_location() {
    // 回归测试：任务在等确认的时候下载失败。
    //
    // 之前 needs_confirm 标记会一直留在库里，
    // 于是界面下次打开还会拿这个早就失败的任务弹确认框——
    // 而且因为任务列表按创建顺序排，弹出来的往往还是它，
    // 用户看到的是错的文件名，点"直接下载"会确认错对象。
    let dir = temp_dir("confirm-fail");
    let mgr = manager(&dir);

    // 指向一个不存在的地址，任务会 404
    let task = mgr
        .add(TaskRequest {
            url: "http://127.0.0.1:1/nope.bin".to_string(),
            filename: Some("nope.bin".to_string()),
            dir: Some(dir.to_string_lossy().to_string()),
            needs_confirm: true,
            ..Default::default()
        })
        .await
        .unwrap();

    mgr.start(task.id).await.unwrap();

    let failed = wait_for(
        || {
            mgr.get(task.id)
                .unwrap()
                .map(|t| t.status == TaskStatus::Error)
                .unwrap_or(false)
        },
        Duration::from_secs(30),
    )
    .await;
    assert!(failed, "任务应当失败");

    let after = mgr.get(task.id).unwrap().unwrap();
    assert!(
        !after.needs_confirm,
        "任务失败之后不该再弹保存位置确认框"
    );

    std::fs::remove_dir_all(&dir).ok();
}
