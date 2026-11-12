//! 分片下载。
//!
//! 分片之间互不依赖，所以可以并发下载——这是 HLS 比普通直链下载快的原因。
//! 下载完先在内存里解密，然后按顺序写进目标文件。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use reqwest::Client;
use tokio::sync::Mutex;

use super::crypto::{decrypt_aes128, iv_from_sequence};
use super::playlist::{EncryptionKey, Playlist, Segment};
use crate::http::RequestContext;

/// 分片下载的产物。
#[derive(Debug, Clone)]
pub struct DownloadedSegment {
    pub sequence: u64,
    /// 解密后的数据。
    pub data: Vec<u8>,
}

/// 下载所有分片。
///
/// `on_progress` 回调报告 (已完成分片数, 总分片数, 已下载字节)。
pub async fn download_segments(
    playlist: &Playlist,
    client: &Client,
    rctx: &RequestContext,
    concurrency: usize,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
    mut on_progress: impl FnMut(usize, usize, u64),
) -> Result<Vec<DownloadedSegment>> {
    if !playlist.unsupported_methods.is_empty() {
        return Err(anyhow!(
            "不支持的加密方式：{}",
            playlist.unsupported_methods.join(", ")
        ));
    }

    // 密钥缓存：同一个 key 文件可能用于很多分片
    let keys: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));

    let total = playlist.segments.len();
    let mut results: Vec<Option<DownloadedSegment>> = vec![None; total];
    let mut done_count = 0usize;
    let mut done_bytes = 0u64;

    // 用信号量限制并发
    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency.clamp(1, 32)));
    let mut tasks = Vec::new();

    for (idx, segment) in playlist.segments.iter().enumerate() {
        let sem = sem.clone();
        let keys = keys.clone();
        let rctx = rctx.clone();
        let client = client.clone();
        let segment = segment.clone();
        let mut cancel = cancel.clone();

        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.ok();
            if *cancel.borrow() {
                return Err(anyhow!("已取消"));
            }
            let data = fetch_segment(&client, &rctx, &segment, &keys, &mut cancel).await?;
            Ok((idx, data))
        }));
    }

    for task in tasks {
        let (idx, data) = task.await.map_err(|e| anyhow!("分片任务失败: {e}"))??;
        done_count += 1;
        done_bytes += data.len() as u64;
        results[idx] = Some(DownloadedSegment {
            sequence: playlist.segments[idx].sequence,
            data,
        });
        on_progress(done_count, total, done_bytes);
    }

    let mut out = Vec::with_capacity(total);
    for r in results {
        out.push(r.ok_or_else(|| anyhow!("分片下载结果缺失"))?);
    }
    Ok(out)
}

/// 下载并解密单个分片。
async fn fetch_segment(
    client: &Client,
    rctx: &RequestContext,
    segment: &Segment,
    keys: &Arc<Mutex<HashMap<String, Vec<u8>>>>,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<Vec<u8>> {
    let resp = rctx
        .apply(client.get(&segment.url))
        .send()
        .await
        .with_context(|| format!("下载分片失败: {}", segment.url))?;

    if !resp.status().is_success() {
        return Err(anyhow!("分片 {} 返回 HTTP {}", segment.url, resp.status()));
    }

    let mut data = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if *cancel.borrow() {
            return Err(anyhow!("已取消"));
        }
        data.extend_from_slice(&chunk?);
    }

    match &segment.key {
        None => Ok(data),
        Some(key) => {
            let key_bytes = load_key(client, key, keys).await?;
            let iv = match &key.iv {
                Some(iv) => iv.clone(),
                None => iv_from_sequence(segment.sequence).to_vec(),
            };
            decrypt_aes128(&data, &key_bytes, &iv)
                .with_context(|| format!("解密分片失败: {}", segment.url))
        }
    }
}

/// 取密钥文件（带缓存）。
async fn load_key(
    client: &Client,
    key: &EncryptionKey,
    cache: &Arc<Mutex<HashMap<String, Vec<u8>>>>,
) -> Result<Vec<u8>> {
    if let Some(cached) = cache.lock().await.get(&key.uri) {
        return Ok(cached.clone());
    }

    let resp = client
        .get(&key.uri)
        .send()
        .await
        .with_context(|| format!("下载密钥失败: {}", key.uri))?;
    if !resp.status().is_success() {
        return Err(anyhow!("密钥 {} 返回 HTTP {}", key.uri, resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("读取密钥失败: {}", key.uri))?;
    let bytes = bytes.to_vec();

    cache.lock().await.insert(key.uri.clone(), bytes.clone());
    Ok(bytes)
}

/// 估算下载所有分片需要的时间（按并发数摊）。
pub fn estimate_duration(playlist: &Playlist, concurrency: usize) -> Duration {
    let total = playlist.total_duration;
    let c = concurrency.clamp(1, 32) as f64;
    Duration::from_secs_f64((total / c).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_scales_with_concurrency() {
        let mut pl = Playlist::default();
        pl.total_duration = 100.0;
        assert_eq!(estimate_duration(&pl, 1).as_secs(), 100);
        assert_eq!(estimate_duration(&pl, 10).as_secs(), 10);
    }
}
