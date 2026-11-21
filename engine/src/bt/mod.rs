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
use std::time::{Duration, Instant};

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
pub mod metadata;
pub mod pe;
pub mod peer_session;
pub mod sparse;
pub mod stall;
pub mod tracker;

use listener::InboundConfig;

/// 接管 BT 库的调试日志。
///
/// ## 为什么需要它
///
/// `bittorrent-rs` 把每一条调试信息都无条件写进**当前工作目录**下的
/// `debug.log`：
///
/// ```text
/// OpenOptions::new().create(true).append(true).open("debug.log")
/// ```
///
/// 没有开关、没有级别过滤、没有轮转。每写一条还 `flush()` 一次。
/// 实测一个卡住的 BT 任务半小时能写 9 万多行、7.4MB——
/// 全是 `[stats]`、`[peer ...]`、`[piece ...]` 这类高频噪音。
///
/// 这有两个后果：
///   1. 白白吃掉 CPU 和磁盘 IO（每次 flush 都是一次系统调用）；
///   2. 文件落在哪取决于进程启动时的工作目录——
///      从资源管理器启动就落在 exe 旁边，`cargo run` 就落在项目根目录。
///
/// ## 怎么绕过去
///
/// 库的文件句柄是 `OnceLock`，只在第一次写日志时打开一次，之后一直复用。
/// 而且 `open()` 失败的话它会存 `None`，之后**所有写入都变成空操作**。
///
/// 所以：
///   1. 临时切到一个受控的空目录；
///   2. 在那里建一个名叫 `debug.log` 的**目录**——
///      打开目录必然失败（Windows 上是访问被拒），句柄就是 `None`；
///   3. 触发一次写日志，让它把「打开失败」这件事固化下来；
///   4. 切回原来的工作目录。
///
/// 之后无论在哪个目录、什么时候，库都不会再写文件。
///
/// 同时装一个 `set_log_sender`，把库的消息引到我们自己的 `tracing` 里——
/// 这样默认级别是 `warn` 时它们被丢掉（不落盘、不格式化），
/// 用户想排查时设 `RUST_LOG=debug` 就能拿到。
pub fn init_logging() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        // 1. 找一个受控的空目录。
        let scratch = std::env::temp_dir().join("idm-shadow-bt-log");
        if std::fs::create_dir_all(&scratch).is_err() {
            return;
        }

        // 2. 在里面放一个叫 debug.log 的**目录**，让库开文件时失败。
        let blocker = scratch.join("debug.log");
        if !blocker.exists() {
            let _ = std::fs::create_dir_all(&blocker);
        }

        // 3. 临时切过去，触发一次打开，让 OnceLock 把「失败」固化下来。
        let original = std::env::current_dir().ok();
        if std::env::set_current_dir(&scratch).is_err() {
            return;
        }
        // 这一行会真正执行库的 log_debug，把文件句柄初始化成 None。
        bittorrent_rs::utils::util::log_debug("idm-shadow: 接管 BT 调试日志");

        // 4. 切回去。
        if let Some(dir) = original {
            let _ = std::env::set_current_dir(dir);
        }

        // 5. 库的消息引到我们的 tracing，默认级别下直接丢掉。
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        bittorrent_rs::utils::util::set_log_sender(tx);
        std::thread::Builder::new()
            .name("idm-shadow-bt-log".to_string())
            .spawn(move || {
                while let Ok(msg) = rx.recv() {
                    tracing::debug!("[bt] {msg}");
                }
            })
            .ok();
    });
}

/// 准备阶段：拿到文件清单、写好 `.torrent` 缓存、给所有文件做稀疏占位。
///
/// 这一步发生在建会话之前。关键点：
///
/// - 磁力链接只在这里抓 info dict，不经过库的 `transition_from_metadata`
/// - 占位用 `sparse::create_sparse_file`，逻辑大小等于文件大小，
///   实际占用接近 0
/// - 库在建会话时看到文件已经存在，会跳过 `File::create + set_len`
pub fn prepare(task: &Task, ctx: &EngineCtx) -> Result<metadata::FetchedMetadata> {
    let url = task.url.trim();
    let download_dir = task.dir.clone();
    std::fs::create_dir_all(&download_dir).ok();

    let meta = if is_magnet(url) {
        let info_hash = metadata::info_hash_of_magnet(url)?;
        let cache = download_dir.join(format!("{}.torrent", metadata::hex(&info_hash)));
        if cache.is_file() {
            metadata::read_torrent_file(&cache)?
        } else {
            send_note(ctx, "正在获取磁力信息…");
            let cancel = ctx.cancel.clone();
            let meta = metadata::fetch(
                url,
                Duration::from_secs(120),
                Arc::new(move || *cancel.borrow()),
                &|stage| send_note(ctx, stage.text()),
            )?;
            metadata::write_torrent_cache(&download_dir, &meta)?;
            meta
        }
    } else {
        let torrent_path = fetch_torrent_file(url, &download_dir)?;
        let meta = metadata::read_torrent_file(&torrent_path)?;
        metadata::write_torrent_cache(&download_dir, &meta)?;
        meta
    };

    let created = metadata::create_placeholders(&download_dir, &meta)?;
    tracing::info!(
        "BT: 文件清单 {} 个文件，共 {} 字节；稀疏占位 {created} 个",
        meta.files.len(),
        meta.total_size
    );
    Ok(meta)
}

/// 发一条只有说明文字的进度。界面靠它显示"正在获取磁力信息"。
pub(crate) fn send_note(ctx: &EngineCtx, note: &str) {
    ctx.send_progress(TaskProgress {
        downloaded: 0,
        total: None,
        speed: 0,
        connections: 0,
        segments: Vec::new(),
        pieces: None,
        verified_pieces: None,
        corrupted_pieces: None,
        dht_nodes: None,
        note: Some(note.to_string()),
    });
}

/// 下载已经准备好的种子。
///
/// `selected` 是用户勾选的文件下标；空表示全部。
pub fn download_prepared(
    task: &Task,
    ctx: &EngineCtx,
    meta: &metadata::FetchedMetadata,
    selected: &[usize],
) -> Result<()> {
    let download_dir = task.dir.clone();
    std::fs::create_dir_all(&download_dir).ok();

    let torrent_path =
        download_dir.join(format!("{}.torrent", metadata::hex(&meta.info_hash)));
    if !torrent_path.is_file() {
        metadata::write_torrent_cache(&download_dir, meta)?;
    }

    // 空选择 = 全部。库的 `select_files` 传空数组会把所有文件标成 Skip，
    // 所以这里必须先展开。
    let selection: Vec<usize> = if selected.is_empty() {
        (0..meta.files.len()).collect()
    } else {
        selected.to_vec()
    };
    let selected_total: u64 = selection
        .iter()
        .filter_map(|i| meta.files.get(*i))
        .map(|f| f.length)
        .sum();
    if selected_total == 0 {
        return Err(anyhow!("没有选中任何文件"));
    }

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
    // 全新任务跳过整盘哈希校验：占位文件全是 0，扫一遍纯属浪费时间。
    // 断点续传（downloaded > 0）才需要认真校验。
    let config = session_config(
        &torrent_path,
        &download_dir,
        listen_port,
        task.downloaded == 0,
    );
    let mut session = TorrentSession::builder(&torrent_path, &download_dir)
        .seeding(false)
        .enable_pex(true)
        .enable_async_verification(true)
        .select_files(&selection)
        .config(config)
        .build()
        .context("创建 BT 会话失败")?;

    // swarm 上限单独提。
    //
    // `config.max_connections` 管的是**出站**线程数，
    // 但 peer 真正能不能进 swarm 是 `add_peer()` 说了算，
    // 它拿 `ctx.maximum_swarm_size` 当准入条件（默认 100）。
    // 不改这个字段的话，连接上限调再多也卡在 100 个。
    raise_swarm_limit(&session, MAX_SWARM_SIZE);

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
    let dht_info_hash = meta.info_hash;
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
    // 稀缺度缓存。算一次 O(peer 数 × 分片数)，
    // 不用每轮都重算——它只影响界面显示。
    let mut availability: Vec<u16> = Vec::new();
    // 稀缺度是不是该刷新了。每隔几秒刷一次就够了。
    let mut availability_stale = true;
    let mut last_availability = Instant::now() - AVAILABILITY_INTERVAL;
    // 上一次报出去的状态是什么。
    //
    // 用 `Option` 而不是 bool：还没有报过任何状态的时候，
    // 跟"报过「等待连接」"是两回事——
    // 前者要报一次，后者不用再报。
    //
    // 一开始是 `None`，不是 `Some(false)`：否则第一个 peer 都没连上时
    // 会因为"状态没变"而永远不报，任务就一直停在「探测中」。
    let mut reported_phase: Option<bool> = None;
    // tracker announce：自己来一份，用真实端口上报。
    let mut tracker_announcer = tracker::TrackerAnnouncer::new(listen_port);
    // 卡死检测器：种子没人做的时候它会空转，这里负责把它捞出来。
    let mut stall_detector = stall::StallDetector::new(task.downloaded);
    // 上一轮的已下载字节数，用来判断「这一轮有没有进展」，
    // 以及决定轮询频率。
    let mut last_downloaded = task.downloaded;
    // 这一轮有没有进展。决定轮询频率：有进展就勤读一点，
    // 没进展就拉长间隔，别在零进度的任务上白烧 CPU。
    let mut progressed = false;

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
            // 只选了部分文件时，库的 `bytes_left` 是整个种子的剩余量，
            // 拿来当总大小的话进度条永远到不了 100%。这里用勾选文件的
            // 总字节数当分母。
            let downloaded = stats.bytes_downloaded.min(selected_total);
            let total = selected_total;
            let speed = stats.download_speed.max(0.0) as u64;
            // 有进展就刷新基准。轮询频率和卡死检测都靠它。
            progressed = downloaded != last_downloaded;
            if progressed {
                last_downloaded = downloaded;
            }

            // 报阶段变化，让界面上的状态跟实际情况对得上。
            //
            // 连上一个 peer 都没有的时候，状态是「等待连接」——
            // 不是「排队中」。排队中意味着"前面有任务占着名额，
            // 轮到我就开始下"，而实际上是我们自己在等 peer。
            // 混用会让用户以为马上就能开始下载。
            //
            // 这里报的是**当前**状态，不是"报过一次就算了"。
            //
            // 以前每种阶段只报一次，但确认保存位置那一步会把状态
            // 改成「下载中」——引擎这边不知道这件事，也就不再报了。
            // 结果任务明明一个 peer 都没连上，界面却写着「下载中」。
            //
            // 所以改成：只在阶段**真的变了**的时候报。
            // 连接数每轮都在变，但"连上了/没连上"这个状态本身很少变，
            // 所以实际产生的消息很少。
            let connected = stats.connected_peers > 0;
            if reported_phase != Some(connected) {
                reported_phase = Some(connected);
                ctx.send_phase(if connected { "downloading" } else { "waiting_peers" });
            }

            // 分片状态是非阻塞读的。
            //
            // 库自带的 piece_availability() 会阻塞在会话的 context 锁上，
            // 那个锁被下载线程占着的时候进度就整个卡住了——
            // 表现为任务永远停在"探测中"，界面上什么都不动。
            // 这里用 try_lock 自己读，拿不到就沿用上一轮的值，
            // 进度照常上报。
            if let Some((local, total_pieces, failures)) = read_pieces(&session) {
                track_corruption(&corrupted, &previous_local, &local);
                // 稀缺度单独算，而且不用每轮都算。
                //
                // 它是 O(peer 数 × 分片数)，4GB 的种子分片上万，
                // 每轮算一次光这一项就是十万次位运算。
                // 它只用来给界面显示「这块有多稀有」，慢一拍没关系。
                if availability_stale || last_availability.elapsed() >= AVAILABILITY_INTERVAL {
                    availability = read_availability(&session, total_pieces);
                    availability_stale = false;
                    last_availability = Instant::now();
                }
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

            // 完成判定必须用 wants-aware 的 `is_download_complete()`：
            // 未勾选的文件本来就不会有数据，`bytes_left` 不会归零。
            let complete = session
                .context()
                .try_lock()
                .map(|ctx| ctx.is_download_complete())
                .unwrap_or_else(|_| stats.bytes_left == 0);
            if complete {
                finished = true;
                break;
            }

            // 卡死检测。
            //
            // 没有进展但还连着 peer → 踢掉这批 peer，换一批。
            // 换过多次还是没进展 → 这个种子没救了，退出循环，
            // 让上层把任务标成失败。用户可以手动重试。
            match stall_detector.observe(downloaded) {
                stall::StallAction::None => {}
                stall::StallAction::Rescue => {
                    tracing::warn!(
                        "BT 卡住（第 {} 次），踢掉当前 peer 重新找",
                        stall_detector.rescues()
                    );
                    rescue_peers(&session);
                    // 重新问 tracker 和 DHT，拿新的 peer。
                    tracker_announcer.reset();
                    dht_announcer.reset_queries();
                    // peer 换了一批，稀缺度得重算。
                    availability_stale = true;
                    // 进度基准也重置，不然下一轮立刻又判卡住。
                    last_downloaded = downloaded;
                    stall_detector.reset_progress();
                    // 踢完之后多等一会儿再轮询，给连接留时间。
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
                stall::StallAction::GiveUp => {
                    tracing::warn!("BT 多次尝试仍无进展，判定种子已失效");
                    send_note(ctx, ctx_note_stalled());
                    break;
                }
            }
        }

        // 轮询频率自适应。
        //
        // 有进展时 300ms 一次，界面上的进度条才跟手；
        // 没进展时拉到 1.2 秒——反正速度是 0，
        // 读得再勤也变不出数据来，不如省下 CPU。
        let sleep = if progressed {
            Duration::from_millis(300)
        } else {
            Duration::from_millis(1200)
        };
        std::thread::sleep(sleep);
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

/// 踢掉当前所有 peer，让库重新去连别的。
///
/// 卡死的时候这些 peer 留着也没用——它们不给我们要的块，
/// 但库会一直 keep-alive 吊着它们，白占线程和 CPU。
/// 踢掉之后库会重新从 tracker / DHT 拿到的 peer 列表里挑人。
fn rescue_peers(session: &TorrentSession) {
    let ctx = session.context();
    let guard = ctx.lock_safe();
    guard.disconnect_all_peers();
}

/// 判定种子失效时给用户看的话。
fn ctx_note_stalled() -> &'static str {
    "种子已失效：多次尝试都无法获取数据，请换一个磁力链接或种子源"
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
/// 返回 (本地已有的块, 分片总数, 累计校验失败次数)。
/// 稀缺度不在这里算——它太贵，单独走 [`read_availability`]。
/// 拿不到锁就返回 None，这一轮跳过。
///
/// 为什么不直接用库给的 `piece_availability()`：
/// 它内部 `lock_safe()` 会一直等会话的 context 锁，
/// 而下载线程持有那把锁的时间可能很长——
/// 一旦卡在这里，整个进度上报就停了，
/// 任务会永远停在"探测中"，界面上连进度条都不动。
fn read_pieces(session: &TorrentSession) -> Option<(Vec<bool>, usize, u64)> {
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

    Some((local, total, failures))
}

/// 算稀缺度：swarm 里有几个 peer 手上有这块。
///
/// 这个是 O(peer 数 × 分片数) 的循环——4GB 的种子分片数上万，
/// 8 个 peer 就是十万多次位运算，每轮主循环做一次就够呛。
///
/// 所以它跟本地 bitfield 分开：本地 bitfield 每轮都要（进度要用），
/// 稀缺度只在需要的时候算（换 peer 的时候、下完一轮的时候），
/// 中间沿用上一轮的结果。
///
/// 稀缺度只用来给界面显示「这一块有多稀有」，
/// 慢一拍完全没关系。
fn read_availability(session: &TorrentSession, total: usize) -> Vec<u16> {
    // peer_swarm 是 RwLock，用 try_read 拿不到就算了，不影响别的。
    let mut availability = vec![0u16; total];
    let Ok(swarm) = session.peer_swarm.try_read() else {
        return availability;
    };
    for peer in swarm.values() {
        let Ok(peer) = peer.try_lock() else { continue };
        let bitfield = &peer.remote_piece_bitfield;
        for i in 0..total {
            let (byte_idx, mask) = get_bitfield_index_and_mask(i as u32);
            if bitfield
                .get(byte_idx)
                .map(|b| b & mask != 0)
                .unwrap_or(false)
            {
                availability[i] = availability[i].saturating_add(1);
            }
        }
    }
    availability
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
fn session_config(
    torrent_path: &Path,
    download_dir: &Path,
    listen_port: u16,
    fresh: bool,
) -> SessionConfig {
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

    // `fresh` 由任务本身决定：全新任务（downloaded == 0）跳过校验。
    // 光看文件在不在不行——我们刚给所有文件做了稀疏占位，
    // 文件一定存在，但里面全是 0。
    config.skip_hash_check = fresh || !existing;
    tracing::info!(
        "BT: 盘上已有数据={existing}，跳过整盘哈希校验={}",
        config.skip_hash_check
    );
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

/// swarm 里同时保留的 peer 数上限。
///
/// 库把这个数写死在 `constants::MAXIMUM_SWARM_SIZE = 100`，
/// 比 `config.max_connections` 更严格——后者管的是出站线程数，
/// 前者管的是 peer 能不能进 swarm。不改这个的话连接上限调再多也卡在 100。
const MAX_SWARM_SIZE: usize = 400;

/// 稀缺度（每个分片有几个 peer 持有）的刷新间隔。
///
/// 算一次是 O(peer 数 × 分片数)。4GB 的种子分片上万，
/// 8 个 peer 就是十万次位运算。
/// 它只用来给界面显示「这块有多稀有」，几秒刷一次完全够用。
const AVAILABILITY_INTERVAL: Duration = Duration::from_secs(5);

/// 把 swarm 的 peer 上限提上去。
///
/// `TorrentContext.maximum_swarm_size` 是 public 字段，
/// `add_peer()` 拿它当准入条件：
///
/// ```text
/// is_space_in_swarm(ip) => swarm.len() < self.maximum_swarm_size
/// ```
///
/// 顺带把 `swarm_topology` / `swarm_manager` 里那份副本也改了，
/// 免得 PEX 广播出去的时候还按 100 算。
pub fn raise_swarm_limit(session: &TorrentSession, limit: usize) {
    let ctx = session.context();
    let mut guard = ctx.lock_safe();
    guard.maximum_swarm_size = limit;
    guard.swarm_topology.maximum_swarm_size = limit;
    guard.swarm_manager.maximum_swarm_size = limit;
    tracing::info!("BT: swarm 上限提到 {limit}");
}

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
/// 把磁力链接里的 base64 info hash 换成 hex。
///
/// 库的 `magnet` 解析器只认两种 v1 hash：40 位 hex 和 32 位 base32。
/// 但磁力链接还有第三种常见写法——**base64**（27 个字符，
/// 结尾带一个 `=`）。这种链接在浏览器里点开是能下的，
/// 丢给我们却直接报「Invalid info hash format」。
///
/// 所以这里把 base64 解出来，改写成 hex 再交给库。
/// 顺手也把 base32 归一成 hex，省得库那边两套解析逻辑。
///
/// 认不出来就原样返回，让库自己报错——
/// 总比在这儿乱改要好。
fn normalize_magnet(url: &str) -> String {
    const PREFIX: &str = "urn:btih:";

    // 找到 xt=urn:btih: 的位置。磁力链接里可能有多个 xt，
    // 但 v1 的那个一定在最前面（v2 的是 urn:btmh:）。
    let Some(start) = url.find(PREFIX) else {
        return url.to_string();
    };
    let hash_start = start + PREFIX.len();
    let hash_end = url[hash_start..]
        .find('&')
        .map(|i| hash_start + i)
        .unwrap_or(url.len());
    let hash = &url[hash_start..hash_end];

    // 已经是 hex（40 位）就不用动了。
    if hash.len() == 40 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return url.to_string();
    }

    let bytes = if hash.len() == 27 || hash.len() == 28 {
        // base64：20 字节 → 28 个字符（尾部带一个 '=' 填充）。
        // 有的实现会省掉填充，就只剩 27 个——两种都认。
        decode_base64_20(hash)
    } else if hash.len() == 32 {
        decode_base32_20(hash)
    } else {
        None
    };

    let Some(bytes) = bytes else {
        return url.to_string();
    };
    if bytes.len() != 20 {
        return url.to_string();
    }

    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mut out = String::with_capacity(url.len() + 13);
    out.push_str(&url[..hash_start]);
    out.push_str(&hex);
    out.push_str(&url[hash_end..]);
    out
}

/// 解 base64 的 20 字节 info hash。
fn decode_base64_20(s: &str) -> Option<Vec<u8>> {
    // 磁力链接的 base64 用的字符集是标准的，
    // 但有的实现会写成 URL-safe（'-' 和 '_'）。
    let mut buf = 0u32;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(20);
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
            if out.len() == 20 {
                return Some(out);
            }
        }
    }
    if out.len() == 20 {
        Some(out)
    } else {
        None
    }
}

/// 解 base32 的 20 字节 info hash。
fn decode_base32_20(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(20);
    let mut buf = 0u64;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        } as u64;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
            if out.len() == 20 {
                return Some(out);
            }
        }
    }
    if out.len() == 20 {
        Some(out)
    } else {
        None
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

/// 新建 BT 任务时先用这个名字占位。
///
/// 真正的种子名要等拿到 info dict 才知道，这里只是让任务列表
/// 不显示一个光秃秃的 "download"。`prepare` 成功后会把
/// `Task.filename` 改成 info dict 里的名字。
pub fn suggested_filename(url: &str) -> Option<String> {
    if is_magnet(url) {
        let normalized = normalize_magnet(url);
        let link = bittorrent_rs::magnet::MagnetLink::parse(&normalized).ok()?;
        link.display_name
            .map(|name| crate::task::sanitize_filename(&name))
    } else {
        let path = url.split(['?', '#']).next().unwrap_or(url);
        let name = path.rsplit('/').next()?.trim();
        let stem = name.strip_suffix(".torrent").unwrap_or(name);
        (!stem.is_empty()).then(|| crate::task::sanitize_filename(stem))
    }
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
    fn magnet_base64_hash_is_rewritten_to_hex() {
        // 真实的一组：base64 的 20 字节 info hash。
        let m = "magnet:?xt=urn:btih:AcE3KH1vDtBaVnQtrnlPYyx5/z0=&dn=ubuntu";
        let out = normalize_magnet(m);
        assert_eq!(
            out,
            "magnet:?xt=urn:btih:01c137287d6f0ed05a56742dae794f632c79ff3d&dn=ubuntu"
        );
        // 改完库才认得。
        assert!(bittorrent_rs::core::magnet::MagnetLink::parse(&out).is_ok());
    }

    #[test]
    fn magnet_base32_hash_is_rewritten_to_hex() {
        let m = "magnet:?xt=urn:btih:AHATOKD5N4HNAWSWOQW246KPMMWHT7Z5&dn=ubuntu";
        let out = normalize_magnet(m);
        assert_eq!(
            out,
            "magnet:?xt=urn:btih:01c137287d6f0ed05a56742dae794f632c79ff3d&dn=ubuntu"
        );
        assert!(bittorrent_rs::core::magnet::MagnetLink::parse(&out).is_ok());
    }

    #[test]
    fn magnet_hex_hash_is_left_alone() {
        let m = "magnet:?xt=urn:btih:01c137287d6f0ed05a56742dae794f632c79ff3d&dn=ubuntu";
        assert_eq!(normalize_magnet(m), m);
    }

    #[test]
    fn magnet_that_cannot_be_decoded_is_left_alone() {
        // 长度不对、字符不认识——原样交给库，让它报原本的错。
        let m = "magnet:?xt=urn:btih:!!!!&dn=ubuntu";
        assert_eq!(normalize_magnet(m), m);

        let m2 = "magnet:?xt=urn:btih:abc&dn=ubuntu";
        assert_eq!(normalize_magnet(m2), m2);
    }

    #[test]
    fn magnet_keeps_other_parameters() {
        // 后面跟着 tracker 和别的参数时，只能换 hash 那一段。
        let m = "magnet:?xt=urn:btih:AcE3KH1vDtBaVnQtrnlPYyx5/z0=\
                 &dn=ubuntu&tr=udp%3A%2F%2Ftracker.example%3A80&ws=https%3A%2F%2Fmirror.example%2Ff";
        let out = normalize_magnet(m);
        assert!(out.starts_with("magnet:?xt=urn:btih:01c137287d6f0ed05a56742dae794f632c79ff3d"));
        assert!(out.contains("&dn=ubuntu"));
        assert!(out.contains("&tr=udp%3A%2F%2Ftracker.example%3A80"));
        assert!(out.contains("&ws=https%3A%2F%2Fmirror.example%2Ff"));
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

    #[test]
    fn prepare_creates_sparse_placeholders_and_file_list() {
        use bittorrent_rs::utils::bencode::{BNode, Bencode};

        let dir = std::env::temp_dir().join(format!("idm-bt-prepare-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let pieces = [0u8; 20];
        let info = Bencode::encode(&BNode::Dictionary(vec![
            (
                b"files".as_slice(),
                BNode::List(vec![
                    BNode::Dictionary(vec![
                        (b"length".as_slice(), BNode::Number(b"1024")),
                        (
                            b"path".as_slice(),
                            BNode::List(vec![BNode::String(b"a.bin")]),
                        ),
                    ]),
                    BNode::Dictionary(vec![
                        (b"length".as_slice(), BNode::Number(b"2048")),
                        (
                            b"path".as_slice(),
                            BNode::List(vec![BNode::String(b"b.bin")]),
                        ),
                    ]),
                ]),
            ),
            (b"name".as_slice(), BNode::String(b"sample")),
            (b"piece length".as_slice(), BNode::Number(b"16384")),
            (b"pieces".as_slice(), BNode::String(&pieces)),
        ]));
        let torrent_path = dir.join("sample.torrent");
        std::fs::write(
            &torrent_path,
            metadata::build_torrent_bytes(&info, &["http://127.0.0.1:1/announce".into()]),
        )
        .unwrap();

        let task = Task::new(
            uuid::Uuid::new_v4(),
            crate::task::TaskRequest {
                url: torrent_path.to_string_lossy().to_string(),
                engine: crate::task::EngineKind::Bt,
                ..Default::default()
            },
            dir.clone(),
            "sample".to_string(),
        );
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (progress_tx, _progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let (phase_tx, _phase_rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = EngineCtx::new(cancel_rx, cancel_tx, progress_tx, phase_tx);

        let meta = prepare(&task, &ctx).unwrap();
        assert_eq!(meta.files.len(), 2);
        assert_eq!(meta.total_size, 3072);
        assert_eq!(meta.name, "sample");

        let first = dir.join("sample").join("a.bin");
        let second = dir.join("sample").join("b.bin");
        assert_eq!(std::fs::metadata(&first).unwrap().len(), 1024);
        assert_eq!(std::fs::metadata(&second).unwrap().len(), 2048);
        #[cfg(windows)]
        {
            assert!(sparse::is_sparse(&first).unwrap());
            // 稀疏文件的真实占用应当远小于逻辑大小。
            assert!(sparse::allocated_size(&first).unwrap() < 1024);
        }

        // `.torrent` 缓存的名字必须是 info hash，库靠它认缓存。
        let cache = dir.join(format!("{}.torrent", metadata::hex(&meta.info_hash)));
        assert!(cache.is_file());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn suggested_name_comes_from_magnet_display_name() {
        let name = suggested_filename(
            "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=Sintel",
        );
        assert_eq!(name.as_deref(), Some("Sintel"));
    }

    /// 真实网络：只勾选字幕文件，验证 mp4 不被下载、字幕完整落地。
    ///
    /// ```text
    /// $env:IDM_BT_LIVE_MAGNET="magnet:?xt=urn:btih:08ada...&dn=Sintel&tr=..."
    /// cargo test -p idm-shadow-engine --lib live_download_only_selected_files -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn live_download_only_selected_files() {
        let magnet = std::env::var("IDM_BT_LIVE_MAGNET")
            .expect("需要设置 IDM_BT_LIVE_MAGNET");
        let dir = std::env::temp_dir().join(format!("idm-bt-live-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let task = Task::new(
            uuid::Uuid::new_v4(),
            crate::task::TaskRequest {
                url: magnet,
                engine: crate::task::EngineKind::Bt,
                ..Default::default()
            },
            dir.clone(),
            "Sintel".to_string(),
        );
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let (phase_tx, _phase_rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = EngineCtx::new(cancel_rx, cancel_tx, progress_tx, phase_tx);

        let meta = prepare(&task, &ctx).unwrap();
        let selected: Vec<usize> = meta
            .files
            .iter()
            .filter(|f| f.path.ends_with(".srt"))
            .map(|f| f.index)
            .collect();
        assert!(selected.len() >= 5, "这个种子应当有多个字幕文件");

        // 看门狗：网络卡住时别让测试永远挂着。
        let watchdog = ctx.cancel_tx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(180));
            let _ = watchdog.send(true);
        });

        // 后台把进度读掉，别让通道堆着。
        let drain = std::thread::spawn(move || {
            while progress_rx.blocking_recv().is_some() {}
        });

        download_prepared(&task, &ctx, &meta, &selected).unwrap();

        for file in meta.files.iter().filter(|f| selected.contains(&f.index)) {
            let path = dir.join(file.path.replace('/', std::path::MAIN_SEPARATOR_STR));
            let size = std::fs::metadata(&path).unwrap().len();
            assert_eq!(size, file.length, "{} 应当完整下载", file.path);
        }

        let mp4 = dir.join("Sintel").join("Sintel.mp4");
        assert_eq!(std::fs::metadata(&mp4).unwrap().len(), 129_241_752);
        #[cfg(windows)]
        assert!(
            sparse::allocated_size(&mp4).unwrap() < 1024 * 1024,
            "没勾选的 mp4 不应该被下载"
        );

        drop(ctx);
        drain.join().ok();
        std::fs::remove_dir_all(&dir).ok();
    }
}
