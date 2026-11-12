//! 下载吞吐基准：量不同连接数下的实际速度。
//! cargo run --release -p idm-shadow-engine --example bench
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use idm_shadow_engine::http::engine::{DownloadConfig, EngineCtx, HttpEngine};
use idm_shadow_engine::store::Store;
use idm_shadow_engine::task::{Task, TaskRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio::net::TcpListener;
use uuid::Uuid;
/// 服务器给每条连接固定带宽，模拟单连接限速的服务器。
async fn serve(listener: TcpListener, total: u64, per_conn_kbps: u64) {
    let per_conn = per_conn_kbps * 1024;
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
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
                Some((s, e)) => (s.min(total.saturating_sub(1)), e.min(total - 1)),
                None => (0, total - 1),
            };
            let length = end - start + 1;
            let header = format!(
                "HTTP/1.1 206 Partial Content\r\n\
                 Content-Length: {length}\r\n\
                 Content-Range: bytes {start}-{end}/{total}\r\n\
                 Accept-Ranges: bytes\r\n\
                 ETag: \"bench\"\r\n\
                 Connection: close\r\n\r\n"
            );
            if socket.write_all(header.as_bytes()).await.is_err() {
                return;
            }
            let chunk = 32 * 1024u64;
            let sleep_ms = if per_conn > 0 {
                (chunk * 1000 / per_conn).max(1)
            } else {
                0
            };
            let mut sent = 0u64;
            let buf = vec![0u8; chunk as usize];
            while sent < length {
                let n = std::cmp::min(chunk, length - sent) as usize;
                if socket.write_all(&buf[..n]).await.is_err() {
                    return;
                }
                sent += n as u64;
                if sleep_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                }
            }
        });
    }
}
#[tokio::main]
async fn main() {
    let file_size: u64 = 64 * 1024 * 1024;
    let per_conn_kbps: u64 = 2000;
    let dir: PathBuf =
        std::env::temp_dir().join(format!("idm-bench-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src.bin");
    {
        let mut f = std::fs::File::create(&src).unwrap();
        let block: Vec<u8> = (0..1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        for _ in 0..(file_size / 1024 / 1024) {
            f.write_all(&block).unwrap();
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(listener, file_size, per_conn_kbps));
    println!("源文件 64 MB，服务器单连接限速 {} KB/s", per_conn_kbps);
    println!("{:>8}  {:>10}  {:>14}", "连接数", "耗时", "平均速度");
    println!("{}", "-".repeat(38));
    for connections in [1usize, 2, 4, 8, 16, 32, 48, 64] {
        let out_dir = dir.join(format!("out-{connections}"));
        std::fs::create_dir_all(&out_dir).unwrap();
        let task = Task::new(
            Uuid::new_v4(),
            TaskRequest::new(format!("http://{addr}/src.bin")),
            out_dir.clone(),
            "out.bin".to_string(),
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
            while progress_rx.recv().await.is_some() {}
        });
        let ctx = EngineCtx::new(cancel_rx, cancel_tx, progress_tx, phase_tx);
        let start = Instant::now();
        engine.download(&task, ctx).await.expect("下载失败");
        let elapsed = start.elapsed();
        let mbps = file_size as f64 / 1024.0 / 1024.0 / elapsed.as_secs_f64();
        println!(
            "{:>8}  {:>8.2}s  {:>10.2} MB/s",
            connections,
            elapsed.as_secs_f64(),
            mbps
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}
