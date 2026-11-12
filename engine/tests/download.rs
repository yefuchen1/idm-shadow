//! 端到端下载测试：对着一个真实的本地 HTTP 服务器跑完整流程。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use idm_shadow_engine::http::engine::{DownloadConfig, EngineCtx, HttpEngine};
use idm_shadow_engine::store::Store;
use idm_shadow_engine::task::{Task, TaskRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

/// 一个只会说 HTTP/1.1 的极简服务器，支持 Range。
async fn serve(listener: TcpListener, body: Arc<Vec<u8>>, delay_ms: u64) {
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
                    break;
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

            if total == 0 {
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                return;
            }

            let length = end.saturating_sub(start) + 1;
            let header = format!(
                "HTTP/1.1 206 Partial Content\r\n\
                 Content-Length: {}\r\n\
                 Content-Range: bytes {}-{}/{}\r\n\
                 Accept-Ranges: bytes\r\n\
                 ETag: \"test-etag\"\r\n\
                 Connection: close\r\n\r\n",
                length, start, end, total
            );
            if socket.write_all(header.as_bytes()).await.is_err() {
                return;
            }

            // 分块写，慢一点，好让多个连接真的并发
            let chunk = 16 * 1024;
            let mut offset = start as usize;
            let end_exclusive = end as usize + 1;
            while offset < end_exclusive {
                let hi = (offset + chunk).min(end_exclusive);
                if socket.write_all(&body[offset..hi]).await.is_err() {
                    return;
                }
                offset = hi;
                if delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
            }
            let _ = socket.flush().await;
        });
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "idm-shadow-test-{}-{}-{}",
        tag,
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

async fn run_download(url: String, dir: PathBuf, filename: &str, connections: usize) -> PathBuf {
    let task = Task::new(
        Uuid::new_v4(),
        TaskRequest::new(url),
        dir.clone(),
        filename.to_string(),
    );

    let store = Arc::new(Store::open_in_memory().unwrap());
    let config = DownloadConfig {
        max_connections: connections,
        ..Default::default()
    };
    let engine = HttpEngine::new(store, config).unwrap();

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (progress_tx, mut progress_rx) =
        mpsc::unbounded_channel::<idm_shadow_engine::http::TaskProgress>();
    let (phase_tx, _phase_rx) = mpsc::unbounded_channel::<&'static str>();
    tokio::spawn(async move {
        while let Some(p) = progress_rx.recv().await {
            assert!(p.downloaded <= p.total.unwrap_or(u64::MAX) + 1024);
        }
    });

    let ctx = EngineCtx::new(cancel_rx, cancel_tx, progress_tx, phase_tx);
    engine.download(&task, ctx).await.expect("下载应当成功");

    dir.join(filename)
}

#[tokio::test]
async fn downloads_whole_file_byte_for_byte() {
    let body = payload(1_000_000);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve(listener, Arc::new(body.clone()), 1));

    let dir = temp_dir("full");
    let path = run_download(
        format!("http://{addr}/file.bin"),
        dir.clone(),
        "out.bin",
        4,
    )
    .await;

    let content = std::fs::read(&path).unwrap();
    assert_eq!(content.len(), body.len(), "文件长度应当一致");
    assert!(content == body, "文件内容应当逐字节一致");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn single_connection_also_works() {
    let body = payload(200_000);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve(listener, Arc::new(body.clone()), 0));

    let dir = temp_dir("single");
    let path = run_download(
        format!("http://{addr}/file.bin"),
        dir.clone(),
        "one.bin",
        1,
    )
    .await;

    let content = std::fs::read(&path).unwrap();
    assert!(content == body, "单连接下载的内容应当一致");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn resumes_from_completed_intervals() {
    let body = payload(500_000);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve(listener, Arc::new(body.clone()), 0));

    let dir = temp_dir("resume");
    let filename = "resume.bin";

    // 假装上次已经下完了前 200000 字节：区间记在数据库里，
    // 文件里也真的得有这些字节，否则续传就会留下空洞。
    let mut done = idm_shadow_engine::IntervalSet::new();
    done.insert(0, 200_000);
    let store = Arc::new(Store::open_in_memory().unwrap());
    let task = Task::new(
        Uuid::new_v4(),
        TaskRequest::new(format!("http://{addr}/file.bin")),
        dir.clone(),
        filename.to_string(),
    );
    store.insert_task(&task).unwrap();
    store.save_intervals(task.id, &done).unwrap();
    std::fs::write(dir.join(filename), &body[..200_000]).unwrap();

    let config = DownloadConfig {
        max_connections: 4,
        ..Default::default()
    };
    let engine = HttpEngine::new(store.clone(), config).unwrap();

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (progress_tx, _rx) =
        mpsc::unbounded_channel::<idm_shadow_engine::http::TaskProgress>();
    let (phase_tx, _phase_rx) = mpsc::unbounded_channel::<&'static str>();
    let ctx = EngineCtx::new(cancel_rx, cancel_tx, progress_tx, phase_tx);
    engine.download(&task, ctx).await.expect("续传应当成功");

    let content = std::fs::read(dir.join(filename)).unwrap();
    assert_eq!(content.len(), body.len());
    assert!(content == body, "续传后的文件必须和原文件完全一致");

    let saved = store.load_intervals(task.id).unwrap();
    assert_eq!(saved.total(), body.len() as u64);

    std::fs::remove_dir_all(&dir).ok();
}
