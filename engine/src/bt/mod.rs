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
