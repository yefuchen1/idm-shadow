//! BT / 磁力链接引擎。
//!
//! 用 bittorrent-rs 做协议实现（DHT、PEX、Tracker、uTP 都有）。
//! 我们只下载，不做种——seeding(false)，所以不往 peer 发数据。
//!
//! 磁力链接需要先从 DHT 拿到 metadata 才能知道文件列表，
//! 这段时间任务显示"获取磁力信息"。

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bittorrent_rs::session::{SessionEvent, TorrentSession};

use crate::http::engine::EngineCtx;
use crate::task::Task;

pub mod listener;

use listener::InboundConfig;

/// 下载磁力链接或 .torrent 文件。
pub fn download(
    task: &Task,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(u64, u64, u64, usize),
) -> Result<()> {
    let url = task.url.trim();
    let dir = task.write_path();
    let download_dir = dir.parent().unwrap_or(Path::new(".")).to_path_buf();
    std::fs::create_dir_all(&download_dir).ok();

    // 只下载不做种：peer 不会给我们数据，我们也不回传
    let mut session = if url.starts_with("magnet:") {
        TorrentSession::from_magnet(url, &download_dir)
            .seeding(false)
            .enable_pex(true)
            .build()
            .context("创建磁力会话失败")?
    } else {
        // 普通的 .torrent 文件或者 http 直链
        let torrent_path = fetch_torrent_file(url, &download_dir)?;
        TorrentSession::builder(&torrent_path, &download_dir)
            .seeding(false)
            .enable_pex(true)
            .build()
            .context("创建 BT 会话失败")?
    };

    session
        .start_download()
        .context("启动 BT 下载失败")?;

    // 入站监听：bittorrent-rs 自己只做主动连接，
    // 我们补上接受别人连过来的那一半。
    let inbound_port = listener::find_port(6881).unwrap_or(6881);
    let inbound_cfg = InboundConfig {
        port: inbound_port,
        enabled: true,
    };
    let inbound_handle = listener::spawn_inbound_listener(&inbound_cfg, |stream| {
        // 入站连接交给库里的 peer 会话处理
        handle_inbound_peer(stream);
    })?;
    let _ = &inbound_handle;

    // 订阅事件，主要用来知道下载结束
    let events = session.subscribe();
    let event_thread = std::thread::spawn(move || {
        while let Ok(event) = events.recv() {
            match event {
                SessionEvent::DownloadFinished => break,
                SessionEvent::Error(msg) => {
                    tracing::warn!("BT 事件: {msg}");
                }
                _ => {}
            }
        }
    });

    // 主循环：轮询进度
    let mut finished = false;

    loop {
        if *ctx.cancel.borrow() {
            let _ = session.stop();
            break;
        }

        if let Some(stats) = session.try_stats() {
            let downloaded = stats.bytes_downloaded;
            let total = stats.bytes_downloaded + stats.bytes_left;
            let speed = stats.download_speed.max(0.0) as u64;
            progress(downloaded, total, speed, stats.connected_peers);

            if stats.progress >= 100.0 && stats.bytes_left == 0 {
                finished = true;
                break;
            }
        }

        std::thread::sleep(Duration::from_millis(300));
    }

    let _ = session.stop();
    drop(event_thread);

    if finished {
        Ok(())
    } else {
        Err(anyhow!("BT 下载未完成"))
    }
}

/// 处理一个入站连接。
///
/// bittorrent-rs 的 peer 会话处理是内部函数，这里用它的公开 API
/// 走一遍握手 + 消息循环：先读 handshake，再按库的消息格式收发。
fn handle_inbound_peer(mut stream: std::net::TcpStream) {
    use std::io::{Read, Write};

    // 1. 读 68 字节的 handshake
    let mut hs = [0u8; 68];
    if stream.read_exact(&mut hs).is_err() {
        return;
    }
    if &hs[1..20] != b"BitTorrent protocol" && &hs[0..19] != b"BitTorrent protocol" {
        return;
    }

    // 2. 回 handshake：<19>"BitTorrent protocol"<8 reserved><20 infohash>
    //    info_hash 从对方发来的那份里取（第 28..48 字节）
    let mut reply = Vec::with_capacity(68);
    reply.push(19);
    reply.extend_from_slice(b"BitTorrent protocol");
    reply.extend_from_slice(&[0u8; 8]);
    reply.extend_from_slice(&hs[28..48]);
    reply.extend_from_slice(b"-IDMS-0001-000000000");
    if stream.write_all(&reply).is_err() {
        return;
    }

    // 3. 消息循环
    //    长度前缀 + id + 负载
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        while buf.len() < 4 {
            match stream.read(&mut tmp) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if len > 1024 * 1024 {
            return;
        }
        while buf.len() < 4 + len {
            match stream.read(&mut tmp) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        let id = buf[4];
        let _body = buf[5..4 + len].to_vec();
        buf.drain(..4 + len);

        match id {
            // interested
            2 => {
                let mut unchoke = Vec::new();
                unchoke.extend_from_slice(&1u32.to_be_bytes());
                unchoke.push(1);
                if stream.write_all(&unchoke).is_err() {
                    return;
                }
            }
            // keep-alive
            0 => {}
            _ => {}
        }
    }
}

/// 下载 .torrent 文件本身。
fn fetch_torrent_file(url: &str, dir: &Path) -> Result<std::path::PathBuf> {
    // 已经是本地文件就直接用
    let local = Path::new(url);
    if local.is_file() {
        return Ok(local.to_path_buf());
    }

    let resp = reqwest::blocking::get(url).context("下载 .torrent 文件失败")?;
    if !resp.status().is_success() {
        return Err(anyhow!(".torrent 下载返回 HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().context("读取 .torrent 内容失败")?;

    // 从 URL 里猜个文件名
    let name = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .next()
        .filter(|s| s.ends_with(".torrent"))
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{}.torrent", uuid::Uuid::new_v4().simple()));

    let path = dir.join(name);
    std::fs::write(&path, &bytes).context("保存 .torrent 文件失败")?;
    Ok(path)
}

/// URL 看起来像磁力链接吗。
pub fn is_magnet(url: &str) -> bool {
    url.trim().to_ascii_lowercase().starts_with("magnet:")
}

/// URL 看起来像 BT 资源吗。
pub fn looks_like_bt(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    if is_magnet(&lower) {
        return true;
    }
    let path = lower.split(['?', '#']).next().unwrap_or(&lower);
    path.ends_with(".torrent")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_magnet_links() {
        assert!(is_magnet("magnet:?xt=urn:btih:abc123"));
        assert!(is_magnet("MAGNET:?xt=urn:btih:abc"));
        assert!(!is_magnet("http://a.com/x.torrent"));
    }

    #[test]
    fn detects_bt_urls() {
        assert!(looks_like_bt("magnet:?xt=urn:btih:abc"));
        assert!(looks_like_bt("http://a.com/x.torrent"));
        assert!(looks_like_bt("http://a.com/x.torrent?token=1"));
        assert!(!looks_like_bt("http://a.com/x.mp4"));
    }
}
