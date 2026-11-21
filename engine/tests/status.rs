//! 任务状态不能报错：状态是用户判断"该不该动"的唯一依据。
//!
//! 起因是 2026-11-21 的实际事故：
//!
//!   磁力链接任务明明在下载（速度、连接数都是实的），
//!   界面上的标签却一直写着"排队中"。
//!
//! 根因是 `resume()` 无条件写 `Queued`，然后调 `start()`——
//! 而 `start()` 开头有个短路：任务已经在 `running` 表里就直接返回。
//! BT 引擎是常驻线程的，任务跑起来就不退出，所以点了「继续」之后
//! `start()` 走短路返回，**没有任何代码再去把状态改回来**。
//!
//! 这个文件盯住三类状态错报：
//!   1. 任务在跑的时候被写成"排队中"；
//!   2. 磁力任务一个 peer 都没连上，却写着"下载中"；
//!   3. 刚点过暂停就点「继续」，点了等于没点。

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

/// 一个慢速服务器：吐得慢，方便在下载中途操作。
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
                .and_then(|(_, v)| {
                    let mut parts = v.trim().split('-');
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
        "idm-shadow-status-{}-{}-{}",
        tag,
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn manager() -> Arc<DownloadManager> {
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

/// 起一个慢速下载服务器，返回 (地址, 目录, 管理器)。
async fn slow_download(tag: &str) -> (SocketAddr, PathBuf, Arc<DownloadManager>) {
    let body: Vec<u8> = (0..2_000_000u32).map(|i| (i % 251) as u8).collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve_slow(listener, Arc::new(body)));
    let dir = temp_dir(tag);
    (addr, dir, manager())
}

/// 任务确实在跑的时候点「继续」，状态不能被写成"排队中"。
///
/// 这是这次修复的核心回归测试。旧代码在这里会把状态写成 `Queued`，
/// 而且永远改不回来——因为 `start()` 短路返回了。
#[tokio::test]
async fn resume_while_running_does_not_report_queued() {
    let (addr, dir, mgr) = slow_download("resume-running").await;
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

    // 等它真的跑起来
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

    // 任务在跑的时候点「继续」——状态不该变成"排队中"。
    mgr.resume(task.id).await.unwrap();

    let status = mgr.get(task.id).unwrap().unwrap().status;
    assert_ne!(
        status,
        TaskStatus::Queued,
        "任务正在跑的时候点继续，状态不该被写成「排队中」"
    );
    assert_eq!(
        status,
        TaskStatus::Downloading,
        "任务正在跑的时候点继续，状态应当是「下载中」"
    );

    // 下载不该被打断——进度还在涨。
    //
    // 等 3 秒：进度是按重平衡节奏（2 秒一次）写库的，
    // 600ms 那个窗口里可能根本没落过新值。
    let before = mgr.get(task.id).unwrap().unwrap().downloaded;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let now = mgr.get(task.id).unwrap().unwrap();
    let after = now.downloaded;
    assert!(
        after > before,
        "点继续不该打断正在进行的下载（before={before} after={after} status={:?}）",
        now.status
    );

    mgr.cancel(task.id).await.ok();
    std::fs::remove_dir_all(&dir).ok();
}

/// 任务在跑的时候点「重试」，同样不能被写成"排队中"。
#[tokio::test]
async fn retry_while_running_does_not_report_queued() {
    let (addr, dir, mgr) = slow_download("retry-running").await;
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

    mgr.retry(task.id).await.unwrap();

    let status = mgr.get(task.id).unwrap().unwrap().status;
    assert_ne!(
        status,
        TaskStatus::Queued,
        "任务正在跑的时候点重试，状态不该被写成「排队中」"
    );

    mgr.cancel(task.id).await.ok();
    std::fs::remove_dir_all(&dir).ok();
}

/// 刚点完暂停就点「继续」，点了要真能继续。
///
/// 旧代码这里有个竞态：`pause()` 只是给引擎发了取消信号，
/// 引擎线程还在收尾，`running` 表里还挂着这个任务。
/// 这时候点「继续」，`start()` 会撞上"已经在跑"的短路直接返回，
/// 随后引擎退出又把状态写回「已暂停」——用户点了继续等于没点。
#[tokio::test]
async fn resume_right_after_pause_actually_resumes() {
    let (addr, dir, mgr) = slow_download("resume-after-pause").await;
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

    // 暂停，紧接着就继续——不给引擎留收尾的时间，
    // 专门撞那个竞态窗口。
    mgr.pause(task.id).await.unwrap();
    mgr.resume(task.id).await.unwrap();

    // 状态得回到"在下载"，而不是停在「已暂停」。
    let resumed = wait_for(
        || {
            mgr.get(task.id)
                .unwrap()
                .map(|t| {
                    t.status == TaskStatus::Downloading || t.status == TaskStatus::Completed
                })
                .unwrap_or(false)
        },
        Duration::from_secs(15),
    )
    .await;
    assert!(
        resumed,
        "暂停后立刻点继续，任务应当真的重新开始下载，而不是停在「已暂停」"
    );

    mgr.cancel(task.id).await.ok();
    std::fs::remove_dir_all(&dir).ok();
}

/// 磁力任务一个 peer 都没连上时，状态是「等待连接」，
/// 不是「下载中」，也不是「排队中」。
#[tokio::test]
async fn bt_task_without_peers_reports_waiting_peers() {
    use bittorrent_rs::utils::bencode::{BNode, Bencode};
    use sha1::{Digest, Sha1};

    let dir = std::env::temp_dir().join(format!("idm-bt-status-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();

    // 造一个只有一个文件的种子。announce 指向一个不存在的 tracker，
    // 这样既没有 peer 来，引擎也不会一直重试。
    let content = b"hello idm shadow";
    let pieces: [u8; 20] = Sha1::digest(content).into();
    let len_bytes = content.len().to_string().into_bytes();
    let piece_len_bytes = "16384".to_string().into_bytes();

    let info = BNode::Dictionary(vec![
        (b"length".as_slice(), BNode::Number(&len_bytes)),
        (b"name".as_slice(), BNode::String(b"single.bin")),
        (b"piece length".as_slice(), BNode::Number(&piece_len_bytes)),
        (b"pieces".as_slice(), BNode::String(&pieces)),
    ]);
    let root = BNode::Dictionary(vec![
        (
            b"announce".as_slice(),
            BNode::String(b"http://127.0.0.1:1/announce"),
        ),
        (b"info".as_slice(), info),
    ]);
    let torrent_bytes = Bencode::encode(&root);
    let torrent_path = dir.join("single.torrent");
    std::fs::write(&torrent_path, &torrent_bytes).unwrap();

    let mgr = manager();
    let task = mgr
        .add(TaskRequest {
            url: torrent_path.to_string_lossy().to_string(),
            engine: idm_shadow_engine::task::EngineKind::Bt,
            dir: Some(dir.to_string_lossy().to_string()),
            ..Default::default()
        })
        .await
        .unwrap();

    mgr.start(task.id).await.unwrap();

    // 种子里没人做，也没有 peer 会来——任务会一直等。
    //
    // 先等引擎把真实阶段报上来。
    //
    // 这时候界面上的状态还盖着「待确认」——磁力任务要弹窗问用户
    // 选保存位置，确认之前状态一直是 `AwaitingConfirm`。
    // 真实阶段记在 manager 内部，等用户确认之后才露出来。
    let prepared = wait_for(
        || {
            mgr.get(task.id)
                .unwrap()
                .map(|t| t.bt_files.is_some())
                .unwrap_or(false)
        },
        Duration::from_secs(20),
    )
    .await;
    assert!(prepared, "prepare 应当读出本地种子的文件清单");

    // 用户确认保存位置。
    mgr.confirm_path(task.id, None, None).await.unwrap();

    // 确认之后，状态应当露出引擎报上来的真实阶段：
    // 一个 peer 都没连上 → 「等待连接」。
    let waiting = wait_for(
        || {
            mgr.get(task.id)
                .unwrap()
                .map(|t| t.status == TaskStatus::WaitingPeers)
                .unwrap_or(false)
        },
        Duration::from_secs(30),
    )
    .await;

    let status = mgr.get(task.id).unwrap().unwrap().status;
    assert_eq!(
        status,
        TaskStatus::WaitingPeers,
        "没有 peer 的磁力任务，确认之后状态应当是「等待连接」，实际是 {status:?}，错误：{:?}",
        mgr.get(task.id).unwrap().unwrap().error
    );
    assert!(
        waiting,
        "没有 peer 的磁力任务应当进入「等待连接」状态"
    );

    mgr.pause(task.id).await.ok();
    mgr.cancel(task.id).await.ok();
    std::fs::remove_dir_all(&dir).ok();
}

/// 「等待连接」也算进行中——它占着下载名额，用户也还在等它。
#[test]
fn waiting_peers_counts_as_active() {
    assert!(TaskStatus::WaitingPeers.is_active());
    assert!(!TaskStatus::WaitingPeers.is_terminal());
}

/// 状态字符串要能来回转，界面和扩展都靠它。
#[test]
fn waiting_peers_round_trips_through_its_string_form() {
    let parsed = TaskStatus::from_str_opt("waiting_peers");
    assert_eq!(parsed, Some(TaskStatus::WaitingPeers));
    assert_eq!(TaskStatus::WaitingPeers.as_str(), "waiting_peers");
}
