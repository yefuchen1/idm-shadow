//! 只取元数据（info dict）的磁力客户端。
//!
//! ## 为什么不能直接用库的磁力会话
//!
//! `bittorrent-rs` 拿到 info dict 之后会立刻
//! `transition_from_metadata()` → `create_local_torrent_structure()`，
//! 也就是给种子里**所有**文件做 `File::create` + `set_len`。
//! 用户还没勾选文件，整个种子的体积就已经在磁盘上占满了。
//! 那两步之间没有任何钩子。
//!
//! 所以这里自己实现 metadata-only 阶段：
//!
//! 1. tracker（HTTP / UDP）+ DHT `get_peers` 找 peer
//! 2. 明文 TCP 握手（带 BEP 10 扩展位）
//! 3. `ut_metadata` 请求 info dict 的全部分片
//! 4. SHA1(info dict) 必须等于磁力链接里的 info_hash
//!
//! 拿到 info dict 之后，调用方负责：
//!
//! - 写成 `<info_hash>.torrent` 缓存（库看到缓存就不再自己抓 metadata）
//! - 用稀疏文件占位（`super::sparse`）
//! - 再交给库建会话——库只给**不存在**的文件分配，
//!   已经存在就跳过，全程零真实占用。
//!
//! ## 协议细节
//!
//! - 元数据分片固定 16 KiB（BEP 9）
//! - 扩展握手是 `ext_id = 0` 的 extended 消息，内容是一个 bencode 字典
//! - `m.ut_metadata` 给出对端使用的扩展 ID；我们这边固定宣告 1
//! - 请求：`d8:msg_typei0e5:piecei<index>ee`
//! - 数据：bencode 头 + 原始分片字节，最后一片可能不足 16 KiB
//! - 拒绝：`msg_type = 2`

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use bittorrent_rs::magnet::MagnetLink;
use bittorrent_rs::utils::bencode::{BNode, Bencode};
use sha1::{Digest, Sha1};

use crate::task::BtFileInfo;

use super::{discovery, pe};

/// BEP 9 的元数据分片大小。
pub const METADATA_PIECE_LEN: usize = 16 * 1024;

/// info dict 的大小上限。50 MiB 是 BEP 9 参考实现给的保护值。
pub const MAX_METADATA_LEN: usize = 50 * 1024 * 1024;

/// 一次 metadata 抓取最多尝试多少个 peer。
///
/// 大部分 peer 会拒绝 `ut_metadata`，或者根本不回话。
/// 实测前几个通常就能成功，给到 40 个足够覆盖冷门种子。
const MAX_PEER_ATTEMPTS: usize = 200;

/// 单个 peer 的会话超时。
const PEER_TIMEOUT: Duration = Duration::from_secs(12);

/// 连接超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

/// tracker 请求超时。
const TRACKER_TIMEOUT: Duration = Duration::from_secs(6);

/// 抓取结果。
#[derive(Debug, Clone)]
pub struct FetchedMetadata {
    /// 20 字节的 v1 info hash。
    pub info_hash: [u8; 20],
    /// 种子名（单文件是文件名，多文件是根目录名）。
    pub name: String,
    /// 原始 info dict 字节。写 `.torrent` 缓存时必须原样嵌入。
    pub info: Vec<u8>,
    /// 磁力链接里带的 tracker（不含兜底 tracker）。
    pub trackers: Vec<String>,
    /// 种子里的文件清单。
    pub files: Vec<BtFileInfo>,
    /// 全部文件的总字节数。
    pub total_size: u64,
    /// 单文件种子。
    pub single_file: bool,
}

impl FetchedMetadata {
    /// 这个种子要不要让用户勾选文件。
    ///
    /// 单文件种子没什么可选的；多文件但只有一个文件时也没必要弹窗。
    pub fn needs_selection(&self) -> bool {
        !self.single_file && self.files.len() > 1
    }
}

/// 解析磁力链接的 info hash（支持 hex / base32 / base64）。
pub fn info_hash_of_magnet(magnet: &str) -> Result<[u8; 20]> {
    let normalized = super::normalize_magnet(magnet);
    let link = MagnetLink::parse(&normalized).context("解析磁力链接失败")?;
    if link.info_hash.len() != 20 {
        return Err(anyhow!(
            "不支持的 info hash 长度: {}（只支持 v1 20 字节）",
            link.info_hash.len()
        ));
    }
    let mut info_hash = [0u8; 20];
    info_hash.copy_from_slice(&link.info_hash);
    Ok(info_hash)
}

/// 从磁盘上的 `.torrent` 读出文件清单。
pub fn read_torrent_file(path: &std::path::Path) -> Result<FetchedMetadata> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("读取 .torrent 失败: {}", path.display()))?;
    read_torrent_bytes(&bytes)
}

/// 从 `.torrent` 字节里读出文件清单。
pub fn read_torrent_bytes(bytes: &[u8]) -> Result<FetchedMetadata> {
    let (start, end) = top_level_dict_value_span(bytes, b"info")
        .ok_or_else(|| anyhow!(".torrent 里没有 info 字典"))?;
    let info = bytes[start..end].to_vec();
    let digest = Sha1::digest(&info);
    let mut info_hash = [0u8; 20];
    info_hash.copy_from_slice(digest.as_slice());
    let (name, files, total_size, single_file) = parse_info_dict(&info)?;
    Ok(FetchedMetadata {
        info_hash,
        name,
        info,
        trackers: extract_trackers(bytes),
        files,
        total_size,
        single_file,
    })
}

fn extract_trackers(bytes: &[u8]) -> Vec<String> {
    let Ok(node) = Bencode::decode(bytes) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(BNode::String(url)) = node.dict_get(b"announce") {
        out.push(String::from_utf8_lossy(url).to_string());
    }
    if let Some(BNode::List(tiers)) = node.dict_get(b"announce-list") {
        for tier in tiers {
            if let BNode::List(items) = tier {
                for item in items {
                    if let Some(url) = item.as_string() {
                        let url = String::from_utf8_lossy(url).to_string();
                        if !out.contains(&url) {
                            out.push(url);
                        }
                    }
                }
            }
        }
    }
    out
}

/// 抓取进度：给界面显示"正在通过 tracker / DHT 找 peer"这类阶段文字。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchStage {
    /// 解析磁力链接。
    Parsing,
    /// 通过 tracker 找 peer。
    Trackers,
    /// 通过 DHT 找 peer。
    Dht,
    /// 向 peer 要 metadata。
    Peers,
    /// 校验 info dict。
    Verifying,
}

impl FetchStage {
    /// 面向用户的中文描述。
    pub fn text(&self) -> &'static str {
        match self {
            FetchStage::Parsing => "解析磁力链接",
            FetchStage::Trackers => "通过 tracker 寻找 peer",
            FetchStage::Dht => "通过 DHT 寻找 peer",
            FetchStage::Peers => "正在从 peer 获取文件列表",
            FetchStage::Verifying => "校验种子信息",
        }
    }
}

/// 从磁力链接抓取 info dict。
///
/// `timeout` 是总时限；到点还没拿到就报错，让用户看到明确的失败，
/// 而不是任务永远停在"获取磁力信息"。
pub fn fetch(
    magnet: &str,
    timeout: Duration,
    should_cancel: Arc<dyn Fn() -> bool + Send + Sync>,
    stage: &dyn Fn(FetchStage),
) -> Result<FetchedMetadata> {
    stage(FetchStage::Parsing);
    let normalized = super::normalize_magnet(magnet);
    let link = MagnetLink::parse(&normalized).context("解析磁力链接失败")?;
    if link.info_hash.len() != 20 {
        return Err(anyhow!(
            "不支持的 info hash 长度: {}（只支持 v1 20 字节）",
            link.info_hash.len()
        ));
    }
    let mut info_hash = [0u8; 20];
    info_hash.copy_from_slice(&link.info_hash);

    // `xs=` 给的是 .torrent 直链，有的话最省事——一次 HTTP 就够。
    if let Some(meta) = try_exact_source(&link, &info_hash, &*should_cancel, stage)? {
        return Ok(meta);
    }

    let deadline = Instant::now() + timeout;

    stage(FetchStage::Trackers);
    let mut peers = discover_peers(&link, &info_hash, deadline, &*should_cancel, stage)?;
    if peers.is_empty() {
        return Err(anyhow!(
            "没有找到任何 peer。tracker 和 DHT 都没有返回可用地址，可能是种子已无做种者"
        ));
    }
    peers.truncate(200);
    tracing::info!(
        info_hash = %hex(&info_hash),
        peers = peers.len(),
        "BT metadata: 开始向 peer 请求 info dict"
    );

    stage(FetchStage::Peers);

    // peer 串行试太慢：冷门种子前十几个地址都是超时或主动断开，
    // 每个 4 秒，用户要等一分钟。这里开一组并发的探测线程，
    // 谁先拿到合法的 info dict 就算谁赢。
    peers.truncate(MAX_PEER_ATTEMPTS);
    let peers = Arc::new(peers);
    let next = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Result<(SocketAddr, FetchedMetadata), (SocketAddr, anyhow::Error)>>();
    let workers = 12.min(peers.len().max(1));

    for _ in 0..workers {
        let tx = tx.clone();
        let peers = peers.clone();
        let next = next.clone();
        let done = done.clone();
        let cancel = should_cancel.clone();
        let link = link.clone();
        std::thread::Builder::new()
            .name("idm-shadow-bt-meta-peer".to_string())
            .spawn(move || loop {
                if done.load(Ordering::Relaxed) || cancel() || Instant::now() >= deadline {
                    break;
                }
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= peers.len() {
                    break;
                }
                let addr = peers[index];
                let per_peer =
                    PEER_TIMEOUT.min(deadline.saturating_duration_since(Instant::now()));
                if per_peer.is_zero() {
                    break;
                }
                let stop = {
                    let done = done.clone();
                    let cancel = cancel.clone();
                    move || done.load(Ordering::Relaxed) || cancel()
                };
                match fetch_from_peer(addr, &info_hash, per_peer, &stop) {
                    Ok(info) => match parse_and_verify(info, info_hash, &link) {
                        Ok(meta) => {
                            done.store(true, Ordering::Relaxed);
                            let _ = tx.send(Ok((addr, meta)));
                            break;
                        }
                        Err(err) => {
                            tracing::debug!(peer = %addr, "BT metadata: 校验失败: {err:#}");
                            let _ = tx.send(Err((addr, err)));
                        }
                    },
                    Err(err) => {
                        tracing::debug!(peer = %addr, "BT metadata: {err:#}");
                        let _ = tx.send(Err((addr, err)));
                    }
                }
            })
            .ok();
    }
    drop(tx);

    let mut last_err: Option<anyhow::Error> = None;
    loop {
        if should_cancel() {
            return Err(anyhow!("已取消"));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(500))) {
            Ok(Ok((addr, meta))) => {
                tracing::info!(
                    info_hash = %hex(&info_hash),
                    peer = %addr,
                    files = meta.files.len(),
                    "BT metadata: 获取成功"
                );
                return Ok(meta);
            }
            Ok(Err((_, err))) => last_err = Some(err),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    Err(match last_err {
        Some(err) => anyhow!("从 peer 获取 metadata 失败: {err:#}"),
        None => anyhow!("没有可用的 peer 来获取 metadata"),
    })
}

/// 如果有 `xs=`（.torrent 直链），直接下载并校验。
fn try_exact_source(
    link: &MagnetLink,
    info_hash: &[u8; 20],
    should_cancel: &dyn Fn() -> bool,
    stage: &dyn Fn(FetchStage),
) -> Result<Option<FetchedMetadata>> {
    let Some(url) = link
        .exact_sources
        .iter()
        .find(|u| u.starts_with("http://") || u.starts_with("https://"))
    else {
        return Ok(None);
    };

    stage(FetchStage::Verifying);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("创建 HTTP 客户端失败")?;
    let resp = match client.get(url).send() {
        Ok(resp) if resp.status().is_success() => resp,
        Ok(resp) => {
            tracing::debug!("BT metadata: xs 返回 HTTP {}", resp.status());
            return Ok(None);
        }
        Err(err) => {
            tracing::debug!("BT metadata: xs 下载失败: {err}");
            return Ok(None);
        }
    };
    if should_cancel() {
        return Err(anyhow!("已取消"));
    }
    let bytes = resp.bytes().context("读取 .torrent 内容失败")?;
    let Some((start, end)) = top_level_dict_value_span(&bytes, b"info") else {
        return Ok(None);
    };
    let info = bytes[start..end].to_vec();
    let digest = Sha1::digest(&info);
    if digest.as_slice() != info_hash {
        tracing::warn!("BT metadata: xs 的 info hash 对不上，忽略");
        return Ok(None);
    }
    Ok(Some(parse_and_verify(info, *info_hash, link)?))
}

/// 解析 info dict、校验 SHA1，并整理文件清单。
fn parse_and_verify(
    info: Vec<u8>,
    info_hash: [u8; 20],
    link: &MagnetLink,
) -> Result<FetchedMetadata> {
    let digest = Sha1::digest(&info);
    if digest.as_slice() != info_hash {
        return Err(anyhow!(
            "metadata 校验失败：SHA1 是 {}，磁力链接是 {}",
            hex(digest.as_slice()),
            hex(&info_hash)
        ));
    }
    let (name, files, total_size, single_file) = parse_info_dict(&info)?;

    // 磁力链接里的 `dn` 可能比 info dict 里的名字更友好，
    // 但落盘路径必须跟库解析出来的一致，所以这里坚持用 info dict 的 name。

    Ok(FetchedMetadata {
        info_hash,
        name,
        info,
        trackers: link.trackers.clone(),
        files,
        total_size,
        single_file,
    })
}

/// 从 info dict 里取出名字、文件清单和总大小。
pub fn parse_info_dict(info: &[u8]) -> Result<(String, Vec<BtFileInfo>, u64, bool)> {
    let node = Bencode::decode(info).context("info dict 不是合法的 bencode")?;
    let name = node
        .dict_get(b"name")
        .and_then(|n| n.as_string())
        .map(|b| String::from_utf8_lossy(b).to_string())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("info dict 缺少 name"))?;
    let name = validate_component(&name).ok_or_else(|| anyhow!("种子名非法: {name}"))?;

    // 单文件种子：顶层有 length，没有 files。
    if let Some(length) = node
        .dict_get(b"length")
        .and_then(|n| n.as_number_bytes())
        .and_then(parse_u64)
    {
        let files = vec![BtFileInfo {
            index: 0,
            path: name.clone(),
            length,
        }];
        return Ok((name, files, length, true));
    }

    let Some(BNode::List(list)) = node.dict_get(b"files") else {
        return Err(anyhow!("info dict 既没有 length 也没有 files"));
    };

    let mut files = Vec::with_capacity(list.len());
    let mut total = 0u64;
    for (index, entry) in list.iter().enumerate() {
        let length = entry
            .dict_get(b"length")
            .and_then(|n| n.as_number_bytes())
            .and_then(parse_u64)
            .ok_or_else(|| anyhow!("第 {index} 个文件缺少 length"))?;
        let BNode::List(parts) = entry
            .dict_get(b"path")
            .ok_or_else(|| anyhow!("第 {index} 个文件缺少 path"))?
        else {
            return Err(anyhow!("第 {index} 个文件的 path 不是列表"));
        };
        let mut rel = Vec::with_capacity(parts.len());
        for part in parts {
            let raw = part
                .as_string()
                .ok_or_else(|| anyhow!("第 {index} 个文件的 path 分量非法"))?;
            let text = String::from_utf8_lossy(raw).to_string();
            let clean = validate_component(&text)
                .ok_or_else(|| anyhow!("第 {index} 个文件的路径分量非法: {text}"))?;
            rel.push(clean);
        }
        if rel.is_empty() {
            return Err(anyhow!("第 {index} 个文件的 path 是空的"));
        }
        let path = format!("{}/{}", name, rel.join("/"));
        files.push(BtFileInfo {
            index,
            path,
            length,
        });
        total = total.saturating_add(length);
    }

    Ok((name, files, total, false))
}

/// 把 info dict 写成 `.torrent`，返回文件路径。
///
/// 缓存文件名必须是 `<info_hash>.torrent`——库的磁力会话按这个名字找缓存，
/// 找到就不再自己抓 metadata，也不会触发那套真实预分配。
pub fn write_torrent_cache(
    dir: &std::path::Path,
    meta: &FetchedMetadata,
) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("创建目录失败: {}", dir.display()))?;
    let path = dir.join(format!("{}.torrent", hex(&meta.info_hash)));
    let bytes = build_torrent_bytes(&meta.info, &meta.trackers);
    std::fs::write(&path, &bytes)
        .with_context(|| format!("写入 .torrent 缓存失败: {}", path.display()))?;
    Ok(path)
}

/// 把 info dict 包装成完整的 `.torrent` 字节。
pub fn build_torrent_bytes(info: &[u8], trackers: &[String]) -> Vec<u8> {
    let list: Vec<&str> = trackers.iter().map(|s| s.as_str()).collect();
    let list = if list.is_empty() {
        discovery::FALLBACK_TRACKERS.to_vec()
    } else {
        list
    };

    let mut out = Vec::with_capacity(info.len() + 256);
    out.push(b'd');
    out.extend_from_slice(b"8:announce");
    out.extend_from_slice(&bencode_str(list[0].as_bytes()));
    out.extend_from_slice(b"13:announce-list");
    out.push(b'l');
    for tracker in &list {
        out.push(b'l');
        out.extend_from_slice(&bencode_str(tracker.as_bytes()));
        out.push(b'e');
    }
    out.push(b'e');
    out.extend_from_slice(b"4:info");
    out.extend_from_slice(info);
    out.push(b'e');
    out
}

/// 给所有文件做稀疏占位。
///
/// 返回实际创建的文件数。单文件种子占位在 `dir/name`，
/// 多文件种子占位在 `dir/name/相对路径`——跟库的
/// `local_files_to_download_list()` 完全一致。
pub fn create_placeholders(
    dir: &std::path::Path,
    meta: &FetchedMetadata,
) -> Result<usize> {
    let mut created = 0;
    for file in &meta.files {
        let path = dir.join(file.path.replace('/', std::path::MAIN_SEPARATOR_STR));
        super::sparse::create_sparse_file(&path, file.length)
            .with_context(|| format!("创建占位文件失败: {}", path.display()))?;
        created += 1;
    }
    Ok(created)
}

// ── peer 发现 ─────────────────────────────────────────────────────────────

fn discover_peers(
    link: &MagnetLink,
    info_hash: &[u8; 20],
    deadline: Instant,
    should_cancel: &dyn Fn() -> bool,
    stage: &dyn Fn(FetchStage),
) -> Result<Vec<SocketAddr>> {
    let mut seen = HashSet::new();
    let mut peers = Vec::new();

    // tracker 和 DHT 并行跑，先到的先用。
    let (tx, rx) = mpsc::channel::<Vec<SocketAddr>>();
    let mut trackers = link.trackers.clone();
    for fallback in discovery::FALLBACK_TRACKERS {
        if !trackers.iter().any(|t| t == fallback) {
            trackers.push((*fallback).to_string());
        }
    }

    let mut tracker_threads = 0;
    for tracker in trackers {
        let tx = tx.clone();
        let ih = *info_hash;
        std::thread::Builder::new()
            .name("idm-shadow-bt-tracker-meta".to_string())
            .spawn(move || {
                let found = announce_tracker(&tracker, &ih);
                let _ = tx.send(found);
            })
            .ok();
        tracker_threads += 1;
    }
    if tracker_threads > 0 {
        stage(FetchStage::Trackers);
    }

    let dht_info_hash = *info_hash;
    let dht_tx = tx.clone();
    std::thread::Builder::new()
        .name("idm-shadow-bt-dht-meta".to_string())
        .spawn(move || {
            let found = dht_lookup(&dht_info_hash, deadline);
            let _ = dht_tx.send(found);
        })
        .ok();
    drop(tx);

    // 给 tracker 一点时间；有 20 个 peer 就够开始了。
    let discovery_deadline = deadline.min(Instant::now() + Duration::from_secs(18));
    while Instant::now() < discovery_deadline {
        if should_cancel() {
            return Err(anyhow!("已取消"));
        }
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(found) => {
                for addr in found {
                    if seen.insert(addr) {
                        peers.push(addr);
                    }
                }
                if peers.len() >= 20 {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // 再把已经排队的应答收一遍，不做额外等待。
    while let Ok(found) = rx.try_recv() {
        for addr in found {
            if seen.insert(addr) {
                peers.push(addr);
            }
        }
    }

    Ok(peers)
}

fn announce_tracker(tracker: &str, info_hash: &[u8; 20]) -> Vec<SocketAddr> {
    if tracker.starts_with("udp://") {
        announce_udp_tracker(tracker, info_hash).unwrap_or_default()
    } else if tracker.starts_with("http://") || tracker.starts_with("https://") {
        announce_http_tracker(tracker, info_hash).unwrap_or_default()
    } else {
        Vec::new()
    }
}

fn announce_http_tracker(tracker: &str, info_hash: &[u8; 20]) -> Result<Vec<SocketAddr>> {
    let sep = if tracker.contains('?') { '&' } else { '?' };
    let peer_id = local_peer_id();
    let url = format!(
        "{tracker}{sep}info_hash={}&peer_id={}&port=6881&uploaded=0&downloaded=0&left=1&compact=1&numwant=80&event=started",
        percent_encode(info_hash),
        percent_encode(&peer_id),
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(TRACKER_TIMEOUT)
        .build()
        .context("创建 tracker HTTP 客户端失败")?;
    let resp = client.get(&url).send().context("tracker 请求失败")?;
    if !resp.status().is_success() {
        return Err(anyhow!("tracker 返回 HTTP {}", resp.status()));
    }
    let body = resp.bytes().context("读取 tracker 响应失败")?;
    Ok(parse_tracker_response(&body))
}

fn parse_tracker_response(body: &[u8]) -> Vec<SocketAddr> {
    let Ok(node) = Bencode::decode(body) else {
        return Vec::new();
    };
    if node.dict_get(b"failure reason").is_some() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(peers) = node.dict_get(b"peers") {
        match peers {
            BNode::String(compact) => out.extend(parse_compact_peers(compact)),
            BNode::List(list) => {
                for entry in list {
                    let ip = entry
                        .dict_get(b"ip")
                        .and_then(|n| n.as_string())
                        .map(|b| String::from_utf8_lossy(b).to_string());
                    let port = entry
                        .dict_get(b"port")
                        .and_then(|n| n.as_number_bytes())
                        .and_then(parse_u64);
                    if let (Some(ip), Some(port)) = (ip, port) {
                        if let Ok(mut addrs) = (ip.as_str(), port as u16).to_socket_addrs() {
                            out.extend(addrs.next());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(BNode::String(compact6)) = node.dict_get(b"peers6") {
        out.extend(parse_compact_peers_v6(compact6));
    }
    out
}

fn announce_udp_tracker(tracker: &str, info_hash: &[u8; 20]) -> Result<Vec<SocketAddr>> {
    let host = tracker
        .trim_start_matches("udp://")
        .split('/')
        .next()
        .ok_or_else(|| anyhow!("UDP tracker 地址非法"))?;
    let addr = host
        .to_socket_addrs()
        .context("解析 UDP tracker 地址失败")?
        .next()
        .ok_or_else(|| anyhow!("UDP tracker 地址为空"))?;

    let socket = UdpSocket::bind("0.0.0.0:0").context("创建 UDP socket 失败")?;
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .ok();

    // connect 请求
    let txid = random_u32();
    let mut req = Vec::with_capacity(16);
    req.extend_from_slice(&0x41727101980u64.to_be_bytes());
    req.extend_from_slice(&0u32.to_be_bytes());
    req.extend_from_slice(&txid.to_be_bytes());
    socket.send_to(&req, addr)?;

    let mut buf = [0u8; 1500];
    let (n, _) = socket.recv_from(&mut buf).context("UDP tracker 无响应")?;
    if n < 16 {
        return Err(anyhow!("UDP tracker connect 响应过短"));
    }
    let action = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let resp_txid = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if action != 0 || resp_txid != txid {
        return Err(anyhow!("UDP tracker connect 响应不匹配"));
    }
    let conn_id = u64::from_be_bytes(buf[8..16].try_into().unwrap());

    // announce 请求
    let txid = random_u32();
    let peer_id = local_peer_id();
    let mut req = Vec::with_capacity(98);
    req.extend_from_slice(&conn_id.to_be_bytes());
    req.extend_from_slice(&1u32.to_be_bytes());
    req.extend_from_slice(&txid.to_be_bytes());
    req.extend_from_slice(info_hash);
    req.extend_from_slice(&peer_id);
    req.extend_from_slice(&0u64.to_be_bytes()); // downloaded
    req.extend_from_slice(&1u64.to_be_bytes()); // left
    req.extend_from_slice(&0u64.to_be_bytes()); // uploaded
    req.extend_from_slice(&2u32.to_be_bytes()); // event = started
    req.extend_from_slice(&0u32.to_be_bytes()); // ip
    req.extend_from_slice(&random_u32().to_be_bytes()); // key
    req.extend_from_slice(&(-1i32).to_be_bytes()); // numwant
    req.extend_from_slice(&6881u16.to_be_bytes()); // port
    socket.send_to(&req, addr)?;

    let (n, _) = socket.recv_from(&mut buf).context("UDP tracker announce 无响应")?;
    if n < 20 {
        return Err(anyhow!("UDP tracker announce 响应过短"));
    }
    let action = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let resp_txid = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if action != 1 || resp_txid != txid {
        return Err(anyhow!("UDP tracker announce 响应不匹配"));
    }
    Ok(parse_compact_peers(&buf[20..n]))
}

/// 简易 DHT `get_peers` 搜索。
///
/// 库的 DHT 在磁力会话建起来之后才可用，而我们必须在建会话之前拿到 peer。
/// 所以这里独立起一个 UDP socket，从引导节点开始迭代查询。
fn dht_lookup(info_hash: &[u8; 20], deadline: Instant) -> Vec<SocketAddr> {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(err) => {
            tracing::debug!("BT metadata: DHT socket 创建失败: {err}");
            return Vec::new();
        }
    };
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .ok();

    let node_id = random_node_id();
    let mut queue: Vec<SocketAddr> = discovery::DHT_BOOTSTRAP
        .iter()
        .filter_map(|n| n.to_socket_addrs().ok().and_then(|mut a| a.next()))
        .collect();
    let mut queried: HashSet<SocketAddr> = HashSet::new();
    let mut seen_nodes: HashSet<SocketAddr> = queue.iter().copied().collect();
    let mut peers: Vec<SocketAddr> = Vec::new();
    let mut seen_peers: HashSet<SocketAddr> = HashSet::new();
    let mut next_tid = 0u16;
    let mut buf = [0u8; 2048];

    // 最多查这么多节点，避免在冷门种子上无限跑。
    const MAX_QUERIES: usize = 160;
    let mut queries = 0usize;

    while Instant::now() < deadline && queries < MAX_QUERIES {
        // 发一轮查询。
        while let Some(addr) = queue.pop() {
            if queried.contains(&addr) {
                continue;
            }
            if queries >= MAX_QUERIES {
                break;
            }
            queried.insert(addr);
            queries += 1;
            next_tid = next_tid.wrapping_add(1);
            let tid = next_tid.to_be_bytes().to_vec();
            if send_dht_get_peers(&socket, addr, &node_id, info_hash, &tid).is_err() {
                continue;
            }
        }

        // 收一轮应答。
        loop {
            let Ok((n, _from)) = socket.recv_from(&mut buf) else {
                break;
            };
            let Ok(msg) = Bencode::decode(&buf[..n]) else {
                continue;
            };
            if msg.dict_get(b"y").and_then(|y| y.as_string()) != Some(b"r") {
                continue;
            }
            let Some(r) = msg.dict_get(b"r") else {
                continue;
            };
            if let Some(BNode::String(nodes)) = r.dict_get(b"nodes") {
                for addr in parse_compact_nodes(nodes) {
                    if seen_nodes.insert(addr) {
                        queue.push(addr);
                    }
                }
            }
            if let Some(BNode::List(values)) = r.dict_get(b"values") {
                for value in values {
                    if let BNode::String(compact) = value {
                        for addr in parse_compact_peers(compact) {
                            if seen_peers.insert(addr) {
                                peers.push(addr);
                            }
                        }
                    }
                }
            }
            if peers.len() >= 60 {
                return peers;
            }
        }

        std::thread::sleep(Duration::from_millis(20));
    }

    tracing::debug!(
        "BT metadata: DHT 查询 {queries} 个节点，拿到 {} 个 peer",
        peers.len()
    );
    peers
}

fn send_dht_get_peers(
    socket: &UdpSocket,
    addr: SocketAddr,
    node_id: &[u8; 20],
    info_hash: &[u8; 20],
    tid: &[u8],
) -> std::io::Result<()> {
    let msg = Bencode::encode(&BNode::Dictionary(vec![
        (
            b"a".as_slice(),
            BNode::Dictionary(vec![
                (b"id".as_slice(), BNode::String(node_id)),
                (b"info_hash".as_slice(), BNode::String(info_hash)),
            ]),
        ),
        (b"q".as_slice(), BNode::String(b"get_peers")),
        (b"t".as_slice(), BNode::String(tid)),
        (b"y".as_slice(), BNode::String(b"q")),
        (b"v".as_slice(), BNode::String(b"\x01")),
    ]));
    socket.send_to(&msg, addr)?;
    Ok(())
}

// ── 跟单个 peer 要 metadata ───────────────────────────────────────────────

fn fetch_from_peer(
    addr: SocketAddr,
    info_hash: &[u8; 20],
    timeout: Duration,
    should_cancel: &dyn Fn() -> bool,
) -> Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .with_context(|| format!("连接 {addr} 失败"))?;
    stream.set_read_timeout(Some(Duration::from_millis(500))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(4))).ok();
    stream.set_nodelay(true).ok();

    // 明文握手。reserved 里置 BEP 10 扩展协议位。
    let mut handshake = Vec::with_capacity(68);
    handshake.push(19);
    handshake.extend_from_slice(b"BitTorrent protocol");
    handshake.extend_from_slice(&[0, 0, 0, 0, 0, 0x10, 0, 0]);
    handshake.extend_from_slice(info_hash);
    handshake.extend_from_slice(&local_peer_id());
    stream.write_all(&handshake)?;

    let mut reply = [0u8; 68];
    read_exact_until(&mut stream, &mut reply, deadline, should_cancel)
        .context("读取 peer 握手失败")?;
    let Some(parsed) = pe::Handshake::parse(&reply) else {
        return Err(anyhow!("对端的 BT 握手不合法"));
    };
    if parsed.info_hash != *info_hash {
        return Err(anyhow!("对端返回了别的种子的 info hash"));
    }
    if parsed.reserved[5] & 0x10 == 0 {
        return Err(anyhow!("对端不支持扩展协议"));
    }

    // 我们的扩展握手：只宣告 ut_metadata = 1。
    send_extended(&mut stream, 0, b"d1:md11:ut_metadatai1eee")?;

    let mut peer_ext_id: Option<u8> = None;
    let mut metadata_size: Option<usize> = None;

    // 先等对端的扩展握手。
    while Instant::now() < deadline {
        if should_cancel() {
            return Err(anyhow!("已取消"));
        }
        let Some((msg_id, payload)) = read_message(&mut stream, deadline, should_cancel)? else {
            continue;
        };
        if msg_id != 20 || payload.is_empty() {
            continue;
        }
        let ext_id = payload[0];
        let body = &payload[1..];
        if ext_id == 0 {
            if let Ok((dict, _)) = Bencode::decode_partial(body) {
                if let Some(BNode::Dictionary(map)) = dict.dict_get(b"m") {
                    for (key, value) in map {
                        if key == b"ut_metadata" {
                            if let Some(id) = value
                                .as_number_bytes()
                                .and_then(parse_u64)
                                .filter(|v| *v > 0 && *v < 256)
                            {
                                peer_ext_id = Some(id as u8);
                            }
                        }
                    }
                }
                metadata_size = dict
                    .dict_get(b"metadata_size")
                    .and_then(|n| n.as_number_bytes())
                    .and_then(parse_u64)
                    .map(|v| v as usize);
            }
            if peer_ext_id.is_some() && metadata_size.is_some() {
                break;
            }
        } else if ext_id == 1 {
            // 对端可能抢在我们前面发数据；先记下来，下面统一处理。
            // 这里不消费，简单起见要求对端按顺序先回握手。
        }
    }

    let peer_ext_id = peer_ext_id.ok_or_else(|| anyhow!("对端没有宣告 ut_metadata"))?;
    let Some(size) = metadata_size else {
        return Err(anyhow!("对端没有给出 metadata_size"));
    };
    if size == 0 || size > MAX_METADATA_LEN {
        return Err(anyhow!("metadata 大小异常: {size}"));
    }

    let pieces = size.div_ceil(METADATA_PIECE_LEN);
    for piece in 0..pieces {
        let req = format!("d8:msg_typei0e5:piecei{piece}ee");
        send_extended(&mut stream, peer_ext_id, req.as_bytes())?;
    }

    let mut got: HashMap<usize, Vec<u8>> = HashMap::new();
    while got.len() < pieces && Instant::now() < deadline {
        if should_cancel() {
            return Err(anyhow!("已取消"));
        }
        let Some((msg_id, payload)) = read_message(&mut stream, deadline, should_cancel)? else {
            continue;
        };
        if msg_id != 20 || payload.is_empty() {
            continue;
        }
        let ext_id = payload[0];
        // 数据消息用我们宣告的 ID（1）；有的实现对端会用自己的 ID 回，
        // 两种都接受。
        if ext_id != 1 && ext_id != peer_ext_id {
            continue;
        }
        let body = &payload[1..];
        let Ok((dict, consumed)) = Bencode::decode_partial(body) else {
            continue;
        };
        let msg_type = dict
            .dict_get(b"msg_type")
            .and_then(|n| n.as_number_bytes())
            .and_then(parse_u64)
            .unwrap_or(255);
        let piece = dict
            .dict_get(b"piece")
            .and_then(|n| n.as_number_bytes())
            .and_then(parse_u64)
            .map(|v| v as usize);

        match msg_type {
            1 => {
                let Some(piece) = piece else { continue };
                if piece >= pieces {
                    continue;
                }
                let offset = piece * METADATA_PIECE_LEN;
                let expected = (size - offset).min(METADATA_PIECE_LEN);
                let data = &body[consumed..];
                if data.len() < expected {
                    continue;
                }
                got.insert(piece, data[..expected].to_vec());
            }
            2 => {
                return Err(anyhow!(
                    "对端拒绝了 metadata 请求（分片 {}）",
                    piece.unwrap_or(0)
                ));
            }
            _ => {}
        }
    }

    if got.len() != pieces {
        return Err(anyhow!(
            "metadata 不完整：收到 {}/{} 片",
            got.len(),
            pieces
        ));
    }
    let mut info = Vec::with_capacity(size);
    for piece in 0..pieces {
        info.extend_from_slice(&got[&piece]);
    }
    info.truncate(size);
    Ok(info)
}

fn send_extended(stream: &mut TcpStream, ext_id: u8, payload: &[u8]) -> Result<()> {
    let mut msg = Vec::with_capacity(payload.len() + 6);
    msg.extend_from_slice(&((payload.len() + 2) as u32).to_be_bytes());
    msg.push(20);
    msg.push(ext_id);
    msg.extend_from_slice(payload);
    stream.write_all(&msg)?;
    stream.flush()?;
    Ok(())
}

/// 读一条完整消息：`(message_id, payload)`。
///
/// 返回 `Ok(None)` 表示 keep-alive。长度 0 的消息没有 message id。
fn read_message(
    stream: &mut TcpStream,
    deadline: Instant,
    should_cancel: &dyn Fn() -> bool,
) -> Result<Option<(u8, Vec<u8>)>> {
    let mut len_buf = [0u8; 4];
    read_exact_until(stream, &mut len_buf, deadline, should_cancel)
        .context("读取 peer 消息长度失败")?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return Ok(None);
    }
    if len > 2 * 1024 * 1024 {
        return Err(anyhow!("peer 消息过长: {len}"));
    }
    let mut buf = vec![0u8; len];
    read_exact_until(stream, &mut buf, deadline, should_cancel)
        .context("读取 peer 消息体失败")?;
    Ok(Some((buf[0], buf[1..].to_vec())))
}

/// 带总时限和取消检查的 `read_exact`。
///
/// socket 的读超时设得比较短（500ms），这里靠循环把"暂时没有数据"
/// 和"真的读不到"区分开。
fn read_exact_until(
    stream: &mut TcpStream,
    buf: &mut [u8],
    deadline: Instant,
    should_cancel: &dyn Fn() -> bool,
) -> Result<()> {
    let mut read = 0;
    while read < buf.len() {
        if should_cancel() {
            return Err(anyhow!("已取消"));
        }
        if Instant::now() >= deadline {
            return Err(anyhow!("等待 peer 超时"));
        }
        match stream.read(&mut buf[read..]) {
            Ok(0) => return Err(anyhow!("peer 提前断开")),
            Ok(n) => read += n,
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

// ── 工具函数 ─────────────────────────────────────────────────────────────

fn parse_compact_peers(data: &[u8]) -> Vec<SocketAddr> {
    data.chunks_exact(6)
        .filter_map(|c| {
            let ip = std::net::Ipv4Addr::new(c[0], c[1], c[2], c[3]);
            let port = u16::from_be_bytes([c[4], c[5]]);
            (port > 0 && !ip.is_unspecified()).then(|| SocketAddr::from((ip, port)))
        })
        .collect()
}

fn parse_compact_peers_v6(data: &[u8]) -> Vec<SocketAddr> {
    data.chunks_exact(18)
        .filter_map(|c| {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&c[..16]);
            let ip = std::net::Ipv6Addr::from(octets);
            let port = u16::from_be_bytes([c[16], c[17]]);
            (port > 0 && !ip.is_unspecified()).then(|| SocketAddr::from((ip, port)))
        })
        .collect()
}

fn parse_compact_nodes(data: &[u8]) -> Vec<SocketAddr> {
    data.chunks_exact(26)
        .filter_map(|c| {
            let ip = std::net::Ipv4Addr::new(c[20], c[21], c[22], c[23]);
            let port = u16::from_be_bytes([c[24], c[25]]);
            (port > 0 && !ip.is_unspecified()).then(|| SocketAddr::from((ip, port)))
        })
        .collect()
}

/// 校验种子里的一个路径分量。
///
/// **不能替换字符**：库按 info dict 里的原始路径落盘，
/// 我们把 `:` 换成 `_` 的话，占位文件和库要写的文件就不是同一个，
/// 库会重新 `File::create + set_len`，又把磁盘占满。
/// 所以这里只做拒绝——碰到 Windows 不允许的名字，在下载开始前就报错。
fn validate_component(raw: &str) -> Option<String> {
    let trimmed = raw.trim_end_matches([' ', '.']);
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return None;
    }
    if trimmed.contains(['/', '\\']) {
        return None;
    }
    if trimmed
        .chars()
        .any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || (c as u32) < 0x20)
    {
        return None;
    }
    Some(trimmed.to_string())
}

fn parse_u64(bytes: &[u8]) -> Option<u64> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for b in bytes {
        if b.is_ascii_alphanumeric() || matches!(*b, b'-' | b'.' | b'_' | b'~') {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn bencode_str(bytes: &[u8]) -> Vec<u8> {
    let mut out = format!("{}:", bytes.len()).into_bytes();
    out.extend_from_slice(bytes);
    out
}

/// 找到顶层字典里某个 key 对应 value 的字节区间。
///
/// 用来从 `.torrent` 里原样取出 info dict——重新编码会改变字节序，
/// info hash 就对不上了。
fn top_level_dict_value_span(buf: &[u8], key: &[u8]) -> Option<(usize, usize)> {
    if buf.first() != Some(&b'd') {
        return None;
    }
    let mut pos = 1;
    while pos < buf.len() && buf[pos] != b'e' {
        let (k, next) = parse_bencode_string(buf, pos)?;
        pos = next;
        let start = pos;
        pos = skip_bencode_value(buf, pos)?;
        if k == key {
            return Some((start, pos));
        }
    }
    None
}

fn parse_bencode_string(buf: &[u8], pos: usize) -> Option<(&[u8], usize)> {
    let colon = buf[pos..].iter().position(|b| *b == b':')? + pos;
    let len: usize = std::str::from_utf8(&buf[pos..colon]).ok()?.parse().ok()?;
    let start = colon + 1;
    let end = start.checked_add(len)?;
    (end <= buf.len()).then(|| (&buf[start..end], end))
}

fn skip_bencode_value(buf: &[u8], pos: usize) -> Option<usize> {
    match *buf.get(pos)? {
        b'i' => {
            let end = buf[pos..].iter().position(|b| *b == b'e')? + pos;
            Some(end + 1)
        }
        b'l' | b'd' => {
            let mut p = pos + 1;
            while p < buf.len() && buf[p] != b'e' {
                if buf[pos] == b'd' {
                    let (_, next) = parse_bencode_string(buf, p)?;
                    p = next;
                }
                p = skip_bencode_value(buf, p)?;
            }
            (p < buf.len()).then_some(p + 1)
        }
        b'0'..=b'9' => {
            let (_, end) = parse_bencode_string(buf, pos)?;
            Some(end)
        }
        _ => None,
    }
}

fn random_u32() -> u32 {
    use rand::Rng;
    rand::thread_rng().gen()
}

fn random_node_id() -> [u8; 20] {
    use rand::RngCore;
    let mut id = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut id);
    id
}

/// 我们自己的 peer id。前缀 `-IDMS-`，后面填版本和随机数。
fn local_peer_id() -> [u8; 20] {
    let mut id = *b"-IDMS-0001-000000000";
    id[16..].copy_from_slice(&random_u32().to_be_bytes()[..4]);
    id
}

/// 小写十六进制。缓存文件名和日志都用它。
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_info_multi() -> Vec<u8> {
        let pieces = [0u8; 20];
        Bencode::encode(&BNode::Dictionary(vec![
            (
                b"files".as_slice(),
                BNode::List(vec![
                    BNode::Dictionary(vec![
                        (b"length".as_slice(), BNode::Number(b"1024")),
                        (
                            b"path".as_slice(),
                            BNode::List(vec![BNode::String(b"a.mkv")]),
                        ),
                    ]),
                    BNode::Dictionary(vec![
                        (b"length".as_slice(), BNode::Number(b"2048")),
                        (
                            b"path".as_slice(),
                            BNode::List(vec![
                                BNode::String(b"sub"),
                                BNode::String(b"b.mkv"),
                            ]),
                        ),
                    ]),
                ]),
            ),
            (b"name".as_slice(), BNode::String(b"test")),
            (b"piece length".as_slice(), BNode::Number(b"16384")),
            (b"pieces".as_slice(), BNode::String(&pieces)),
        ]))
    }

    fn sample_info_single() -> Vec<u8> {
        let pieces = [0u8; 20];
        Bencode::encode(&BNode::Dictionary(vec![
            (b"length".as_slice(), BNode::Number(b"4096")),
            (b"name".as_slice(), BNode::String(b"file.bin")),
            (b"piece length".as_slice(), BNode::Number(b"16384")),
            (b"pieces".as_slice(), BNode::String(&pieces)),
        ]))
    }

    #[test]
    fn parses_multi_file_info() {
        let info = sample_info_multi();
        let (name, files, total, single) = parse_info_dict(&info).unwrap();
        assert_eq!(name, "test");
        assert!(!single);
        assert_eq!(total, 3072);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "test/a.mkv");
        assert_eq!(files[1].path, "test/sub/b.mkv");
        assert_eq!(files[0].index, 0);
    }

    #[test]
    fn parses_single_file_info() {
        let info = sample_info_single();
        let (name, files, total, single) = parse_info_dict(&info).unwrap();
        assert_eq!(name, "file.bin");
        assert!(single);
        assert_eq!(total, 4096);
        assert_eq!(files[0].path, "file.bin");
    }

    #[test]
    fn rejects_traversal_paths() {
        let info = Bencode::encode(&BNode::Dictionary(vec![
            (
                b"files".as_slice(),
                BNode::List(vec![BNode::Dictionary(vec![
                    (b"length".as_slice(), BNode::Number(b"1")),
                    (
                        b"path".as_slice(),
                        BNode::List(vec![BNode::String(b".."), BNode::String(b"evil")]),
                    ),
                ])]),
            ),
            (b"name".as_slice(), BNode::String(b"test")),
            (b"piece length".as_slice(), BNode::Number(b"16384")),
            (b"pieces".as_slice(), BNode::String(b"")),
        ]));
        assert!(parse_info_dict(&info).is_err());
    }

    #[test]
    fn torrent_cache_roundtrips_through_library_parser() {
        let info = sample_info_multi();
        let hash = Sha1::digest(&info);
        let mut info_hash = [0u8; 20];
        info_hash.copy_from_slice(&hash);
        let meta = FetchedMetadata {
            info_hash,
            name: "test".to_string(),
            info,
            trackers: vec!["http://tracker.example/announce".to_string()],
            files: vec![
                BtFileInfo {
                    index: 0,
                    path: "test/a.mkv".to_string(),
                    length: 1024,
                },
                BtFileInfo {
                    index: 1,
                    path: "test/sub/b.mkv".to_string(),
                    length: 2048,
                },
            ],
            total_size: 3072,
            single_file: false,
        };

        let dir = std::env::temp_dir().join(format!("idm-bt-meta-{}", uuid::Uuid::new_v4()));
        let path = write_torrent_cache(&dir, &meta).unwrap();
        assert!(path.file_name().unwrap().to_string_lossy().ends_with(".torrent"));

        let mut parsed = bittorrent_rs::core::metainfo::MetaInfoFile::new(&path).unwrap();
        parsed.parse().unwrap();
        assert_eq!(parsed.get_v1_info_hash().unwrap(), info_hash);
        let (total, files) = parsed.local_files_to_download_list(&dir).unwrap();
        assert_eq!(total, 3072);
        assert_eq!(files.len(), 2);
        assert!(files[1].name.replace('\\', "/").ends_with("test/sub/b.mkv"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn top_level_span_finds_raw_info_bytes() {
        let info = sample_info_single();
        let torrent = build_torrent_bytes(&info, &[]);
        let (start, end) = top_level_dict_value_span(&torrent, b"info").unwrap();
        assert_eq!(&torrent[start..end], info.as_slice());
    }

    #[test]
    fn percent_encodes_binary_hash() {
        let encoded = percent_encode(&[0x00, 0xff, b'a', b'-']);
        assert_eq!(encoded, "%00%FFa-");
    }

    /// 真实网络验证。默认忽略，按需运行：
    ///
    /// ```text
    /// $env:IDM_BT_LIVE_MAGNET="magnet:?xt=urn:btih:..."
    /// cargo test -p idm-shadow-engine --lib live_magnet_fetch -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn live_magnet_fetch() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();
        let magnet = std::env::var("IDM_BT_LIVE_MAGNET")
            .expect("需要设置 IDM_BT_LIVE_MAGNET");
        let started = Instant::now();
        let meta = fetch(
            &magnet,
            Duration::from_secs(90),
            Arc::new(|| false),
            &|stage| {
                println!("[{}] {}", started.elapsed().as_millis(), stage.text());
            },
        )
        .expect("抓取 metadata 失败");
        println!(
            "info_hash={} name={:?} files={} total={} 用时={}ms",
            hex(&meta.info_hash),
            meta.name,
            meta.files.len(),
            meta.total_size,
            started.elapsed().as_millis()
        );
        for file in meta.files.iter().take(10) {
            println!("  {} {}", file.length, file.path);
        }
        assert!(!meta.files.is_empty());
    }
}
