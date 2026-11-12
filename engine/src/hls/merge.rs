//! 合并分片。
//!
//! 按播放列表里的顺序依次写入。MPEG-TS 分片直接拼起来就是一个 TS 文件，
//! 但不是所有播放器都认，所以如果系统里有 ffmpeg 就转封装成 mp4。

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

use super::segment::DownloadedSegment;

/// 合并产物。
#[derive(Debug, Clone)]
pub struct MergeResult {
    pub path: std::path::PathBuf,
    pub bytes: u64,
    /// 是否用 ffmpeg 转封装过。
    pub remuxed: bool,
    /// 产物是不是标准 mp4（转封装过才是）。
    pub is_mp4: bool,
}

/// 把分片按顺序写到目标文件。
pub async fn merge(segments: &[DownloadedSegment], target: &Path) -> Result<MergeResult> {
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }

    let mut file = tokio::fs::File::create(target)
        .await
        .with_context(|| format!("创建文件失败: {}", target.display()))?;

    let mut total = 0u64;
    for seg in segments {
        file.write_all(&seg.data)
            .await
            .with_context(|| format!("写入失败: {}", target.display()))?;
        total += seg.data.len() as u64;
    }
    file.flush().await.ok();
    file.sync_all().await.ok();
    drop(file);

    // 有 ffmpeg 就转封装成 mp4，产物更通用
    if has_ffmpeg() {
        match remux_to_mp4(target).await {
            Ok(()) => {
                return Ok(MergeResult {
                    path: target.to_path_buf(),
                    bytes: total,
                    remuxed: true,
                    is_mp4: true,
                })
            }
            Err(err) => {
                // 转封装失败就保留原样，至少文件还在
                tracing::warn!("ffmpeg 转封装失败，保留原始拼接文件: {err}");
            }
        }
    }

    Ok(MergeResult {
        path: target.to_path_buf(),
        bytes: total,
        remuxed: false,
        is_mp4: false,
    })
}

/// 系统里有没有 ffmpeg。
pub fn has_ffmpeg() -> bool {
    which("ffmpeg").is_some()
}

/// 找可执行文件。
fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            let exe = candidate.with_extension("exe");
            if exe.is_file() {
                return Some(exe);
            }
            return Some(candidate);
        }
    }
    None
}

/// 用 ffmpeg 把 TS 转封装成 mp4（不重新编码，只是换封装，速度很快）。
async fn remux_to_mp4(source: &Path) -> Result<()> {
    let temp = source.with_extension("remux.mp4");

    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.arg("-y")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(source)
        .arg("-c")
        .arg("copy")
        .arg("-movflags")
        .arg("+faststart")
        .arg(&temp)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let output = cmd.output().await.context("调用 ffmpeg 失败")?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let _ = tokio::fs::remove_file(&temp).await;
        anyhow::bail!("ffmpeg 转封装失败: {}", err.trim());
    }

    // 转封装成功后，把原文件换掉
    tokio::fs::rename(&temp, source)
        .await
        .context("替换转封装结果失败")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_nothing_for_bogus_binary() {
        assert!(which("definitely-not-a-real-binary-xyz").is_none());
    }

    #[test]
    fn ffmpeg_presence_check_does_not_panic() {
        let _ = has_ffmpeg();
    }
}
