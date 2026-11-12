//! HLS 引擎：下载 m3u8 并合并成完整文件。
//!
//! 流程：
//! 1. 拉播放列表，解析出分片列表
//! 2. 并发下载分片（分片之间互不依赖）
//! 3. AES-128 加密的分片在内存里解密
//! 4. 按顺序合并成文件；系统里有 ffmpeg 就转封装成 mp4

pub mod crypto;
pub mod merge;
pub mod playlist;
pub mod segment;

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;

use crate::http::engine::EngineCtx;
use crate::http::{build_client, RequestContext};
use crate::task::Task;

use merge::merge;
use playlist::Playlist;
use segment::download_segments;

/// 下载 m3u8。
///
/// `progress` 报告 (已下载分片, 总分片, 已下载字节)。
pub async fn download_playlist(
    task: &Task,
    ctx: &EngineCtx,
) -> Result<()> {
    let client = build_client(task.user_agent.as_deref())?;
    let rctx = RequestContext::from_task(task);

    // 1. 拉播放列表
    let resp = rctx
        .apply(client.get(&task.url))
        .send()
        .await
        .with_context(|| format!("获取播放列表失败: {}", task.url))?;

    if !resp.status().is_success() {
        return Err(anyhow!("播放列表返回 HTTP {}", resp.status()));
    }

    let mut text = String::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        text.push_str(&String::from_utf8_lossy(&chunk?));
    }

    // 2. 解析
    let playlist = playlist::parse(&text, &task.url)
        .map_err(|e| anyhow!("解析播放列表失败: {e}"))?;

    if !playlist.unsupported_methods.is_empty() {
        return Err(anyhow!(
            "该流使用了不支持的加密方式：{}",
            playlist.unsupported_methods.join(", ")
        ));
    }

    tracing::info!(
        task = %task.id,
        segments = playlist.len(),
        live = playlist.is_live,
        duration = playlist.total_duration,
        "播放列表解析完成"
    );

    // 3. 下载分片
    let concurrency = task.max_connections.unwrap_or(8).clamp(1, 32) as usize;

    let segments = download_segments(
        &playlist,
        &client,
        &rctx,
        concurrency,
        &mut ctx.cancel.clone(),
        |done, _total, bytes| {
            ctx.send_progress(crate::http::engine::TaskProgress {
                downloaded: bytes,
                total: None,
                speed: 0,
                connections: done,
                segments: Vec::new(),
                note: None,
            });
        },
    )
    .await?;

    // 4. 合并
    let target = task.write_path();
    let result = merge(&segments, &target).await?;

    // 告诉用户产物是什么类型：转封装过是标准 mp4，
    // 没装 ffmpeg 就只是原始拼接的 TS，部分播放器不认。
    let note = if result.is_mp4 {
        "已转封装为 mp4".to_string()
    } else {
        "原始 TS 流（未安装 ffmpeg，未转封装）".to_string()
    };
    ctx.send_progress(crate::http::engine::TaskProgress {
        downloaded: result.bytes,
        total: Some(result.bytes),
        speed: 0,
        connections: 0,
        segments: Vec::new(),
        note: Some(note),
    });

    tracing::info!(
        task = %task.id,
        path = %result.path.display(),
        bytes = result.bytes,
        remuxed = result.remuxed,
        "HLS 合并完成"
    );

    Ok(())
}

/// 探测是不是 m3u8。
pub async fn probe_playlist(url: &str, referer: Option<&str>) -> Result<Playlist> {
    let client = build_client(None)?;
    let rctx = playlist::request_context(url, referer);
    let resp = rctx.apply(client.get(url)).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!("播放列表返回 HTTP {}", resp.status()));
    }
    let text = resp.text().await?;
    playlist::parse(&text, url).map_err(|e| anyhow!("{e}"))
}

/// 直播列表当前窗口的分片数。
pub fn live_window_size(playlist: &Playlist) -> usize {
    playlist.len()
}
