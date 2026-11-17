//! 文件名定夺的端到端验证。
//!
//! 这些用例照着真实的故障现场写：
//! 站点给一个 `/api/download` 的地址，服务器或者重定向到 CDN，
//! 最终响应里没有 Content-Disposition，只有 Content-Type。
//! 这种情况下落盘的名字曾经是一个光秃秃的 "download"。

use std::net::SocketAddr;
use std::sync::Arc;

use idm_shadow_engine::http::engine::DownloadConfig;
use idm_shadow_engine::manager::DownloadManager;
use idm_shadow_engine::store::Store;
use idm_shadow_engine::task::TaskRequest;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 一个极简 HTTP 服务器：按路径给出不同的响应。
async fn serve(listener: TcpListener, body: Arc<Vec<u8>>) {
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
            let path = request
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();

            let response = if path.starts_with("/api/download") {
                // 像 ClipKnight 那样：给一个地址，服务器回一个 302。
                // 目标是自己这个服务器上的 /cdn/x.mp4。
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: /cdn/x.mp4\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .into_bytes()
            } else if path.starts_with("/cdn/") {
                // CDN 的最终响应：只有 Content-Type，没有 Content-Disposition。
                // 浏览器对这种响应不会当成下载——这正是故障的根源。
                let mut head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                head.extend_from_slice(&body);
                head
            } else if path.starts_with("/hash/") {
                // 最终地址的末段是一串哈希，完全没有扩展名。
                // 这种只能靠 Content-Type 定名。
                let mut head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                head.extend_from_slice(&body);
                head
            } else {
                let mut head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                head.extend_from_slice(&body);
                head
            };

            let _ = socket.write_all(&response).await;
            let _ = socket.flush().await;
        });
    }
}

async fn start_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = Arc::new(vec![7u8; 4096]);
    tokio::spawn(serve(listener, body));
    addr
}

#[tokio::test]
async fn redirect_without_disposition_still_gets_a_proper_name() {
    let addr = start_server().await;
    let store = Arc::new(Store::open_in_memory().unwrap());
    let mgr = DownloadManager::new(store, DownloadConfig::default()).unwrap();

    // 没给文件名——模拟扩展只拿到 Chrome 的临时名、或者干脆没名字。
    let task = mgr
        .add(TaskRequest {
            url: format!("http://{addr}/api/download?url=https://x.com/s/1&rank=0"),
            filename: None,
            ..Default::default()
        })
        .await
        .unwrap();

    // 重定向之后的最终地址末段是 "x.mp4"，用它定名。
    // （比按原始地址定名更准——字节就是从那儿来的。）
    assert_eq!(
        task.filename, "x.mp4",
        "重定向到没有 disposition 的 CDN，也该定出像样的名字"
    );
}

#[tokio::test]
async fn a_bare_download_name_from_the_browser_is_not_used_as_is() {
    let addr = start_server().await;
    let store = Arc::new(Store::open_in_memory().unwrap());
    let mgr = DownloadManager::new(store, DownloadConfig::default()).unwrap();

    // 浏览器给的 "download" 是占位名，不该直接落盘。
    let task = mgr
        .add(TaskRequest {
            url: format!("http://{addr}/api/download"),
            filename: Some("download".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(task.filename, "x.mp4");
}

#[tokio::test]
async fn a_hash_url_falls_back_to_the_content_type() {
    let addr = start_server().await;
    let store = Arc::new(Store::open_in_memory().unwrap());
    let mgr = DownloadManager::new(store, DownloadConfig::default()).unwrap();

    // TikTok 那种地址：末段是一串哈希，没有扩展名。
    let task = mgr
        .add(TaskRequest {
            url: format!("http://{addr}/hash/o4oQxrDpIFABEOByoKEc5cExBj3VfROLqIHHgf/?a=1988"),
            filename: None,
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(
        task.filename,
        "o4oQxrDpIFABEOByoKEc5cExBj3VfROLqIHHgf.mp4",
        "没有扩展名时按 Content-Type 补一个"
    );
}

#[tokio::test]
async fn a_real_name_from_the_browser_is_kept() {
    let addr = start_server().await;
    let store = Arc::new(Store::open_in_memory().unwrap());
    let mgr = DownloadManager::new(store, DownloadConfig::default()).unwrap();

    // 用户自己起的名字要尊重。
    let task = mgr
        .add(TaskRequest {
            url: format!("http://{addr}/whatever.bin"),
            filename: Some("我的视频.mp4".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(task.filename, "我的视频.mp4");
}
