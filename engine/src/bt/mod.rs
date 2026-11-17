//! BT / 磁力链接引擎。
//!
//! 用 bittorrent-rs 做协议实现（DHT、PEX、Tracker、uTP 都有）。
//! 我们只下载，不做种——seeding(false)，所以不往 peer 发数据。
//!
//! 磁力链接需要先从 DHT 拿到 metadata 才能知道文件列表，
//! 这段时间任务显示"获取磁力信息"。

use std::path::Path;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bittorrent_rs::core::metainfo::MetaInfoFile;
use bittorrent_rs::session::{SessionEvent, TorrentSession};
use bittorrent_rs::session::SessionConfig;
use bittorrent_rs::utils::util::get_bitfield_index_and_mask;
use bittorrent_rs::utils::util::LockExt;

use crate::http::engine::{EngineCtx, PieceCell, PieceState, PieceStatus, TaskProgress};
use crate::task::Task;

pub mod listener;
pub mod discovery;
pub mod pe;
pub mod peer_session;
pub mod tracker;

use listener::InboundConfig;

/// 下载磁力链接或 .torrent 文件。
pub fn download(
    task: &Task,
    ctx: &EngineCtx,
) -> Result<()> {
    let url = task.url.trim();
    let dir = task.write_path();
    let download_dir = dir.parent().unwrap_or(Path::new(".")).to_path_buf();
    std::fs::create_dir_all(&download_dir).ok();

    // 端口先定下来。
    //
    // 这里必须**先占端口再建会话**：库的 NAT-PMP 映射、LSD 宣告、
    // tracker 上报用的都是 `config.listen_port`，而我们自己的入站监听
    // 用的是另外找的端口。两边不一致的话，NAT 后别人连不进来，
    // DHT 里宣告的端口也指向一个没人监听的地址。
    //
    // 所以：挑一个没人用的端口 → 把它填进 config → 再起监听。
    // 三边用同一个端口，映射、宣告、监听才对得上。
    let listen_port = listener::find_port(6881).unwrap_or(6881);

    // 只下载不做种：peer 不会给我们数据，我们也不回传。
    //
    // 打开异步校验：开了它 piece 的哈希校验走后台线程池，
    // 不阻塞下载；而且校验结果（尤其是坏块）会被统计出来，
    // 界面上就能显示"坏块重传统计"。
    let mut session = if url.starts_with("magnet:") {
        let config = session_config_magnet(listen_port);
        // 磁力链接没有 tracker 会很难找到 peer，
        // 补一批实测可达的。
        let ctx = bittorrent_rs::TorrentSession::from_magnet(url, &download_dir)
            .seeding(false)
            .enable_pex(true)
            .enable_async_verification(true)
            .config(config.clone())
            .build()
            .context("创建磁力会话失败")?;
        discovery::inject_fallback_trackers(&ctx.context());
        ctx
    } else {
        // 普通的 .torrent 文件或者 http 直链
        let torrent_path = fetch_torrent_file(url, &download_dir)?;
        let config = session_config(&torrent_path, &download_dir, listen_port);
        TorrentSession::builder(&torrent_path, &download_dir)
            .seeding(false)
            .enable_pex(true)
            .enable_async_verification(true)
            .config(config)
            .build()
            .context("创建 BT 会话失败")?
    };

    // 坏块记录：piece 序号 → 校验失败次数。
    // 库把校验结果内部消化掉了（失败就丢掉数据重下），
    // 不对外暴露具体是哪一块坏了，所以我们自己盯 bitfield 的变化。
    let corrupted = Arc::new(Mutex::new(BTreeMap::<u32, u32>::new()));
    let previous_local = Arc::new(Mutex::new(Vec::<bool>::new()));

    session
        .start_download()
        .context("启动 BT 下载失败")?;

    // DHT announce。
    //
    // 库的 DHT 只回应别人的 announce_peer，自己从来不主动宣告——
    // 也就是「别人能查到我们，我们查不到别人」。
    // 这个线程补上主动宣告这一半，让别的 peer 能反过来找到我们。
    let dht_stats = Arc::new(discovery::DhtStats::default());
    let dht_running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let dht_info_hash = {
        let ctx_arc = session.context();
        let guard = ctx_arc.lock_safe();
        let mut ih = [0u8; 20];
        let n = guard.info_hash.len().min(20);
        ih[..n].copy_from_slice(&guard.info_hash[..n]);
        ih
    };
    let (dht_announcer, dht_thread) = discovery::DhtAnnouncer::spawn(
        dht_info_hash,
        listen_port,
        dht_stats.clone(),
        dht_running.clone(),
    );

    // 入站监听：bittorrent-rs 自己只做主动连接，
    // 我们补上接受别人连过来的那一半。
    //
    // 监听的端口跟 config.listen_port 是同一个，
    // 这样 NAT-PMP 映射出去的端口、DHT 里宣告的端口、
    // 实际监听的端口三者一致。
    let inbound_cfg = InboundConfig {
        port: listen_port,
        enabled: true,
    };
    let inbound_stats = Arc::new(peer_session::InboundStats::default());
    let session_ctx = session.context();
    let inbound_handle = listener::spawn_inbound_listener(&inbound_cfg, move |stream, addr| {
        // 入站连接：真的当成一个 peer 对待——
        // 握手、发 bitfield、主动要块、收块落盘。
        let info_hash = {
            let guard = session_ctx.lock_safe();
            let mut ih = [0u8; 20];
            let n = guard.info_hash.len().min(20);
            ih[..n].copy_from_slice(&guard.info_hash[..n]);
            ih
        };
        let port = stream
            .peer_addr()
            .map(|a| a.port())
            .unwrap_or(0);
        peer_session::handle_inbound_peer(
            stream,
            addr,
            port,
            &info_hash,
            session_ctx.clone(),
            inbound_stats.clone(),
        );
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
    // 上一轮读到的分片状态。读不到时沿用它。
    let mut last_pieces = PieceStatus::default();
    let mut reported_downloading = false;
    // tracker announce：自己来一份，用真实端口上报。
    let mut tracker_announcer = tracker::TrackerAnnouncer::new(listen_port);

    loop {
        if *ctx.cancel.borrow() {
            let _ = session.stop();
            break;
        }

        // 自己 announce 一次 tracker（用真实端口）。
        let found_peers = tracker_announcer.tick(&mut session);
        if found_peers > 0 {
            tracing::debug!("BT tracker 返回 {found_peers} 个 peer");
        }

        // 把库 DHT 路由表里的节点喂给自己的 announce 线程，
        // 这样 announce 能覆盖到库实际连上的那些节点。
        feed_dht_candidates(&session, &dht_announcer);

        if let Some(stats) = session.try_stats() {
            let downloaded = stats.bytes_downloaded;
            let total = stats.bytes_downloaded + stats.bytes_left;
            let speed = stats.download_speed.max(0.0) as u64;

            // 报一次阶段变化，任务状态从"探测中"变成"下载中"。
            // 不发的话界面上的状态会一直停在"探测中"。
            if !reported_downloading {
                reported_downloading = true;
                ctx.send_phase("downloading");
            }

            // 分片状态是非阻塞读的。
            //
            // 库自带的 piece_availability() 会阻塞在会话的 context 锁上，
            // 那个锁被下载线程占着的时候进度就整个卡住了——
            // 表现为任务永远停在"探测中"，界面上什么都不动。
            // 这里用 try_lock 自己读，拿不到就沿用上一轮的值，
            // 进度照常上报。
            if let Some((local, availability, failures)) = read_pieces(&session) {
                track_corruption(&corrupted, &previous_local, &local);
                last_pieces =
                    build_piece_status(&local, &availability, &corrupted, failures);
            }
            let pieces = last_pieces.clone();

            ctx.send_progress(TaskProgress {
                downloaded,
                total: Some(total),
                speed,
                connections: stats.connected_peers,
                segments: Vec::new(),
                pieces: Some(pieces.clone()),
                verified_pieces: Some(pieces.verified as u64),
                corrupted_pieces: Some(pieces.corrupted as u64),
                dht_nodes: Some(dht_node_count(&session, &dht_stats)),
                note: None,
            });

            if stats.progress >= 100.0 && stats.bytes_left == 0 {
                finished = true;
                break;
            }
        }

        std::thread::sleep(Duration::from_millis(300));
    }

    let _ = session.stop();
    dht_running.store(false, std::sync::atomic::Ordering::Relaxed);
    let _ = dht_thread.join();
    drop(event_thread);

    if finished {
        Ok(())
    } else {
        Err(anyhow!("BT 下载未完成"))
    }
}

/// 把库 DHT 路由表里的节点喂给 announce 线程。
///
/// 库的 DHT 负责维护路由表（哪些节点还活着），
/// 我们自己的 announce 线程负责宣告自己。
/// 两边合起来才有完整的 DHT 能力。
fn feed_dht_candidates(
    session: &TorrentSession,
    announcer: &Arc<discovery::DhtAnnouncer>,
) {
    let Some(dht) = session.dht.clone() else {
        return;
    };
    let nodes = {
        let table = dht.routing_table.lock_safe();
        table
            .buckets
            .iter()
            .flat_map(|b| b.iter())
            .map(|n| (n.ip.clone(), n.port))
            .collect::<Vec<_>>()
    };
    for (ip, port) in nodes {
        announcer.add_candidate(ip, port);
    }
}

/// DHT 里现在有多少个节点。
///
/// 两个来源加起来：库的路由表 + 我们自己的候选表。
fn dht_node_count(session: &TorrentSession, stats: &Arc<discovery::DhtStats>) -> u64 {
    let from_library = session
        .dht
        .as_ref()
        .map(|dht| {
            let table = dht.routing_table.lock_safe();
            table.buckets.iter().map(|b| b.len() as u64).sum::<u64>()
        })
        .unwrap_or(0);
    from_library.max(stats.nodes())
}

/// 记住哪些块校验失败过。
///
/// 库内部把校验失败的块丢掉重下，对外只留一个累计计数。
/// 我们靠对比前后两次的本地 bitfield 来定位：
/// 一块本来有、这轮没了，说明刚被判定为坏块。
fn track_corruption(
    corrupted: &Arc<Mutex<BTreeMap<u32, u32>>>,
    previous_local: &Arc<Mutex<Vec<bool>>>,
    local: &[bool],
) {
    let mut prev = previous_local.lock().unwrap_or_else(|e| e.into_inner());
    let mut map = corrupted.lock().unwrap_or_else(|e| e.into_inner());

    if !prev.is_empty() && prev.len() == local.len() {
        for (i, (&had, &has)) in prev.iter().zip(local.iter()).enumerate() {
            // 之前有、现在没了 —— 刚被判为坏块
            if had && !has {
                *map.entry(i as u32).or_insert(0) += 1;
            }
        }
    }
    *prev = local.to_vec();
}

/// 非阻塞地读出分片状态。
///
/// 返回 (本地已有的块, 每块在 swarm 里的持有者数量, 累计校验失败次数)。
/// 拿不到锁就返回 None，这一轮跳过。
///
/// 为什么不直接用库给的 `piece_availability()`：
/// 它内部 `lock_safe()` 会一直等会话的 context 锁，
/// 而下载线程持有那把锁的时间可能很长——
/// 一旦卡在这里，整个进度上报就停了，
/// 任务会永远停在"探测中"，界面上连进度条都不动。
fn read_pieces(session: &TorrentSession) -> Option<(Vec<bool>, Vec<u16>, u64)> {
    // 本地 bitfield 和校验统计都在 context 里。
    let context = session.context();
    let ctx = context.try_lock().ok()?;
    let bitfield = &ctx.bitfield;
    let total = ctx.number_of_pieces;
    let failures = ctx.verification_metrics().corrupted_pieces;

    let local: Vec<bool> = (0..total)
        .map(|i| {
            let (byte_idx, mask) = get_bitfield_index_and_mask(i as u32);
            bitfield
                .get(byte_idx)
                .map(|b| b & mask != 0)
                .unwrap_or(false)
        })
        .collect();
    drop(ctx);

    // 稀缺度：swarm 里有几个 peer 手上有这块。
    // peer_swarm 是 RwLock，用 try_read 拿不到就算了，不影响别的。
    let mut availability = vec![0u16; total];
    if let Ok(swarm) = session.peer_swarm.try_read() {
        for peer in swarm.values() {
            let Ok(peer) = peer.try_lock() else { continue };
            for i in 0..total {
                let (byte_idx, mask) = get_bitfield_index_and_mask(i as u32);
                if peer
                    .remote_piece_bitfield
                    .get(byte_idx)
                    .map(|b| b & mask != 0)
                    .unwrap_or(false)
                {
                    availability[i] = availability[i].saturating_add(1);
                }
            }
        }
    }

    Some((local, availability, failures))
}

/// 按"是不是全新下载"配置会话。
///
/// 关键在 `skip_hash_check`：
///
/// 库建会话时会扫盘，把已经存在的文件**逐块做 SHA1 校验**
/// （`create_torrent_bitfield`）。刚建会话时文件已经被预分配成
/// 完整大小，于是这 6.6GB 全零也要整份读一遍算哈希——
/// 几十秒起步，任务会一直卡在"探测中"，界面上进度条都不动。
///
/// 全新下载没什么可校验的（盘上什么都没有），直接跳过；
/// 断点续传才需要认真校验，知道哪些块已经下好了。
fn session_config(torrent_path: &Path, download_dir: &Path, listen_port: u16) -> SessionConfig {
    let mut config = SessionConfig::default();

    apply_common_config(&mut config, listen_port);

    // 看看目标文件在不在。
    let existing = match MetaInfoFile::new(torrent_path).and_then(|mut m| {
        m.parse()?;
        m.local_files_to_download_list(download_dir)
    }) {
        Ok((_, files)) => files.iter().any(|f| Path::new(&f.name).exists()),
        // 解析不了就保守一点：老老实实校验。
        Err(_) => true,
    };

    config.skip_hash_check = !existing;
    tracing::info!(
        "BT: 盘上已有数据={existing}，跳过整盘哈希校验={}",
        config.skip_hash_check
    );
    config
}

/// 磁力链接的会话配置。
fn session_config_magnet(listen_port: u16) -> SessionConfig {
    let mut config = SessionConfig::default();
    apply_common_config(&mut config, listen_port);
    config
}

/// 两个会话共用的配置。
fn apply_common_config(config: &mut SessionConfig, listen_port: u16) {
    // 监听端口必须跟入站监听是同一个，
    // 否则 NAT-PMP 映射出去的是另一个端口，别人连进来会连错地方。
    config.listen_port = listen_port;
    config.dht_port = listen_port;

    // 并发 peer 上限。
    //
    // 库默认 50，迅雷那种量级能到几百。
    // swarm 上限（`maximum_swarm_size`）是 100，
    // 超过之后 `add_peer` 会拒绝新连接，所以这里给得比它高，
    // 让库自己去按实际连接情况裁剪。
    config.max_connections = MAX_CONNECTIONS;
    config.max_peer_candidates = MAX_PEER_CANDIDATES;

    // 磁力链接常常是 private 种子，
    // 但 LSD 只在局域网有意义，这里放开一点更实用。
    config.allow_private_lsd = true;
}

/// 并发连接上限。
const MAX_CONNECTIONS: usize = 200;

/// peer 候选队列上限。
const MAX_PEER_CANDIDATES: usize = 2000;

/// 画图最多用多少格。
const MAX_PIECE_CELLS: usize = 240;

/// 组装分片状态快照。
fn build_piece_status(
    local: &[bool],
    availability: &[u16],
    corrupted: &Arc<Mutex<BTreeMap<u32, u32>>>,
    verification_failures: u64,
) -> PieceStatus {
    let map = corrupted.lock().unwrap_or_else(|e| e.into_inner());
    let total = local.len() as u32;

    let verified = local.iter().filter(|x| **x).count() as u32;

    // 坏块统计：当前还判定为坏的。
    let corrupted_count = map
        .keys()
        .filter(|i| {
            let idx = **i as usize;
            idx < local.len() && !local[idx]
        })
        .count() as u32;

    // 降采样：分片多的时候按桶合并，每格取区间里最"坏"的状态。
    // 两万多个分片逐个往界面推太重（每次进度更新一兆多）。
    let bucket_count = local.len().clamp(1, MAX_PIECE_CELLS);
    let per_bucket = local.len() as f64 / bucket_count as f64;
    let mut cells = Vec::with_capacity(bucket_count);
    for b in 0..bucket_count {
        let from = (b as f64 * per_bucket).floor() as usize;
        let to = if b + 1 == bucket_count {
            local.len()
        } else {
            (((b + 1) as f64 * per_bucket).floor() as usize).max(from + 1)
        };
        let to = to.min(local.len());
        let slice = &local[from..to];
        let verified_in = slice.iter().filter(|x| **x).count();
        let corrupted_in = (from..to).filter(|i| map.contains_key(&(*i as u32))).count();

        let state = if corrupted_in > 0 {
            PieceState::Corrupted
        } else if verified_in == slice.len() {
            PieceState::Verified
        } else if verified_in > 0 {
            // 一格里有的下了有的没下
            PieceState::Partial
        } else {
            PieceState::Missing
        };
        // 稀缺度取这一格里最稀的那块
        let availability = availability
            .get(from..to.min(availability.len()))
            .map(|s| s.iter().copied().max().unwrap_or(0))
            .unwrap_or(0);
        cells.push(PieceCell {
            from: from as u32,
            to: to.saturating_sub(1) as u32,
            state,
            availability,
        });
    }

    PieceStatus {
        total,
        verified,
        corrupted: corrupted_count,
        verification_failures,
        cells,
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

    #[test]
    fn piece_status_marks_verified_and_corrupted() {
        // 本地有第 0、1 块，第 2 块之前有、这轮没了（被判为坏块）
        let corrupted = Arc::new(Mutex::new(BTreeMap::<u32, u32>::new()));
        let previous = Arc::new(Mutex::new(vec![true, true, true, false]));

        let local = vec![true, true, false, false];
        let availability = vec![3u16, 1, 5, 0];

        track_corruption(&corrupted, &previous, &local);
        let status = build_piece_status(&local, &availability, &corrupted, 1);

        assert_eq!(status.total, 4);
        assert_eq!(status.verified, 2);
        assert_eq!(status.corrupted, 1, "第 2 块应当被记成坏块");

        // 分片少的时候一格一块
        assert_eq!(status.cells.len(), 4);
        assert_eq!(status.cells[0].state, PieceState::Verified);
        assert_eq!(status.cells[1].state, PieceState::Verified);
        assert_eq!(status.cells[2].state, PieceState::Corrupted);
        assert_eq!(status.cells[3].state, PieceState::Missing);

        // 稀缺度要带上
        assert_eq!(status.cells[0].availability, 3);
        assert_eq!(status.cells[3].availability, 0);
    }

    #[test]
    fn piece_status_first_round_has_no_false_corruption() {
        // 第一轮没有"上一轮"可比，不能把还没拿到的块误判成坏块。
        let corrupted = Arc::new(Mutex::new(BTreeMap::<u32, u32>::new()));
        let previous = Arc::new(Mutex::new(Vec::new()));

        let local = vec![false, false, false];
        let availability = vec![1u16, 1, 1];

        track_corruption(&corrupted, &previous, &local);
        let status = build_piece_status(&local, &availability, &corrupted, 0);

        assert_eq!(status.verified, 0);
        assert_eq!(status.corrupted, 0);
        assert!(status.cells.iter().all(|c| c.state == PieceState::Missing));
    }

    #[test]
    fn piece_status_counts_repeated_corruption() {
        // 坏块定位靠的是 bitfield 从 1 变 0。
        //
        // 同一个块反复坏时，要经历"坏掉 → 重下好 → 又坏掉"，
        // 每次 1→0 都能被抓到一次，所以位置和次数都数得出来。
        // 而"累计校验失败次数"以库给的数为准（那才是权威）。
        let corrupted = Arc::new(Mutex::new(BTreeMap::<u32, u32>::new()));
        let previous = Arc::new(Mutex::new(vec![true, true]));

        // 第 0 块坏了
        let local = vec![false, true];
        track_corruption(&corrupted, &previous, &local);

        // 第 0 块重新下好了
        let local2 = vec![true, true];
        track_corruption(&corrupted, &previous, &local2);

        // 第 0 块又坏了
        let local3 = vec![false, true];
        track_corruption(&corrupted, &previous, &local3);

        // 库报的是两次失败
        let status = build_piece_status(&local3, &[1, 1], &corrupted, 2);
        assert_eq!(status.verification_failures, 2, "累计坏块次数应当是 2");

        // 坏块位置靠 bitfield 对比定位
        let map = corrupted.lock().unwrap();
        assert_eq!(map.get(&0).copied().unwrap_or(0), 2, "第 0 块坏了两次");
        assert!(map.get(&1).is_none(), "第 1 块是好的");
    }

    #[test]
    fn piece_cells_are_downsampled_for_big_torrents() {
        // 大种子有两万多个分片，不能一格一块地往界面推。
        let corrupted = Arc::new(Mutex::new(BTreeMap::<u32, u32>::new()));
        let previous = Arc::new(Mutex::new(Vec::new()));

        let total = 25_390;
        let local: Vec<bool> = (0..total).map(|i| i < 100).collect();
        let availability = vec![2u16; total];

        track_corruption(&corrupted, &previous, &local);
        let status = build_piece_status(&local, &availability, &corrupted, 0);

        assert_eq!(status.total, total as u32);
        assert_eq!(status.verified, 100);
        assert!(
            status.cells.len() <= MAX_PIECE_CELLS,
            "格子数应当被压下来，实际 {}",
            status.cells.len()
        );
        assert!(status.cells.len() > 1);

        // 格子要覆盖全部分片，不能重不漏
        assert_eq!(status.cells[0].from, 0);
        assert_eq!(status.cells[status.cells.len() - 1].to, total as u32 - 1);
        for pair in status.cells.windows(2) {
            assert!(
                pair[0].to + 1 == pair[1].from,
                "格子之间不能有空隙或重叠"
            );
        }

        // 前 100 块下好了 -> 第一格是"部分下载"（一格里有的有的是不是）
        assert_eq!(status.cells[0].state, PieceState::Partial);
    }
}
