//! 入站 peer 会话。
//!
//! `bittorrent-rs` 只做主动连接：它主动去连别人，
//! 别人连过来的连接它直接丢掉。
//! 我们自己监听端口，那条连接进来之后要真的能干活——
//! 握手、发自己的 bitfield、主动向对方 request piece、收 piece 落到盘上。
//!
//! 为什么不能直接复用库里的 `session::worker::handle_peer_session`：
//! 它第一件事是 `socket_factory.connect()`，那是主动连接；
//! 入站连接已经有 socket 了。而且 `run_peer_loop` 是私有函数。
//! 所以这里自己写一份会话循环，用库公开的 `PeerMessage` / `Peer` /
//! `TorrentContext` 这些类型，保证线协议跟库那边一致。
//!
//! ## 一个关键的坑：别把活着的 socket 交给库
//!
//! 库的 `Peer` 里存着 socket。库在收到块校验通过后会广播 `Have`，
//! 用的就是这个 peer 的 socket——也就是我们正在读的那个 socket。
//!
//! 如果把真实的 `TcpStream` 塞进 `Peer`，那么：
//! 我们在读 peer 消息的同时，库往同一个 socket 写 `Have`，
//! 两边写进同一条字节流，对方在解析时就乱了。
//! （而且库的写是异步的，顺序完全不可控。）
//!
//! 所以这里的做法是：**peer 记录用库的类型，但 socket 给它一个
//! 空的 `MockSocket`**。我们自己的读写走原始 `TcpStream`，
//! 库那边只把它当成"有个 peer 在 swarm 里"的记录用。

use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bittorrent_rs::core::constants::BLOCK_SIZE;
use bittorrent_rs::core::torrent_context::TorrentContext;
use bittorrent_rs::network::peer::Peer;
use bittorrent_rs::network::peer_message::PeerMessage;
use bittorrent_rs::utils::util::LockExt;

use super::pe::{self, PeStream};

/// 一次会话里最多同时挂着的请求数。
///
/// 跟库里的出站连接保持一致：流水线深度 10 是个甜点，再多对端也未必给。
const MAX_OUTSTANDING_REQUESTS: usize = 10;

/// 单条消息的字节上限。
///
/// bitfield 最长是 (分片数 + 7) / 8 字节。
/// 几万片的大种子也就几 KB，给到 1 MiB 足够宽裕。
const MAX_MESSAGE_LEN: usize = 1024 * 1024;

/// 读超时。
///
/// peer 空闲时我们要靠这个超时发 keep-alive，
/// 所以不能太长（会白白挂着连接），也不能太短（会被误杀）。
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// 请求超时。
///
/// 跟库的 `check_request_timeouts` 一致：15 秒没回就撤单重发。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// 空闲多久就断开。
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// keep-alive 的发送间隔。
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(60);

/// 入站统计。
///
/// 界面上要显示「入站接了几个 peer」，所以得有个地方记着。
#[derive(Debug, Default)]
pub struct InboundStats {
    /// 累计接受的连接数。
    accepted: AtomicU64,
    /// 握手成功的连接数。
    handshaked: AtomicU64,
    /// 当前还活着的会话数。
    active: AtomicU64,
}

impl InboundStats {
    pub fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::Relaxed)
    }

    pub fn handshaked(&self) -> u64 {
        self.handshaked.load(Ordering::Relaxed)
    }

    pub fn active(&self) -> u64 {
        self.active.load(Ordering::Relaxed)
    }
}

/// 处理一条入站连接。
///
/// 整条流程：
/// 1. 读握手（明文或 PE 加密），校验 info_hash 对得上
/// 2. 发自己的 bitfield + interested
/// 3. 循环：收消息（bitfield/have/choke/unchoke/request/piece），
///    不停向对方 request 我们缺的块
pub fn handle_inbound_peer(
    stream: TcpStream,
    peer_addr: String,
    peer_port: u16,
    info_hash: &[u8; 20],
    ctx: Arc<Mutex<TorrentContext>>,
    stats: Arc<InboundStats>,
) {
    stats.accepted.fetch_add(1, Ordering::Relaxed);
    stats.active.fetch_add(1, Ordering::Relaxed);
    let _guard = ActiveGuard {
        stats: stats.clone(),
    };

    if let Err(err) = run_session(stream, peer_addr, peer_port, info_hash, ctx.clone(), &stats) {
        tracing::debug!("BT 入站会话结束: {err}");
    }
}

/// 会话结束时把 active 减掉。
///
/// 用 Drop 保证 panic 也不会漏减。
struct ActiveGuard {
    stats: Arc<InboundStats>,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.stats.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 一次完整的入站会话。
fn run_session(
    stream: TcpStream,
    peer_addr: String,
    peer_port: u16,
    info_hash: &[u8; 20],
    ctx: Arc<Mutex<TorrentContext>>,
    stats: &Arc<InboundStats>,
) -> anyhow::Result<()> {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_nodelay(true);

    // ── 1. 握手 ───────────────────────────────────────────────────────
    //
    // 对端可能是明文，也可能用 PE 加密握手。
    // 加密握手的第一个字节是随机的，没法靠"peek 一下"来判断，
    // 所以先按明文读 20 字节看看协议标识；
    // 对不上就退回从头读 PE。
    let mut reader = pe::Buffered::new(stream);
    let mut writer = reader.inner().try_clone()?;

    let mut peek = [0u8; 20];
    reader.take_exact(&mut peek)?;

    let (handshake, mut crypto) = if pe::is_plain_protocol_identifier(&peek) {
        // 明文：把剩下的 48 字节读完。
        let mut raw = [0u8; 68];
        raw[..20].copy_from_slice(&peek);
        raw[20..].copy_from_slice(&reader.take(48)?);
        let Some(hs) = pe::Handshake::parse(&raw) else {
            return Ok(());
        };
        (hs, None)
    } else {
        // PE 加密握手。前面那 20 字节是 DH 公钥的前缀，得还回去。
        reader.prepend(&peek);
        match pe::server_handshake(&mut reader, &mut writer, info_hash)? {
            Some((hs, s)) => (hs, Some(s)),
            None => return Ok(()),
        }
    };

    if handshake.info_hash != *info_hash {
        tracing::debug!("BT 入站 {peer_addr} info_hash 不匹配，丢弃");
        return Ok(());
    }

    // 明文握手要回一份 BT 握手。
    //
    // 加密握手**不能**回：对方的 IA 里已经带了 BT 握手，
    // 再发一份的话对方会把这 68 字节当成 peer 消息的长度前缀，
    // 整条流就乱了。
    if crypto.is_none() {
        let reply = pe::Handshake::build_reply(info_hash);
        writer.write_all(&reply)?;
        writer.flush()?;
    }

    // 明文握手时，对方的 reserved 位里有我们关心的能力标志。
    // 加密握手时拿不到（对方在 IA 里给的是标准握手，reserved 也是标准的），
    // 所以按明文处理就行——加密握手的协商结果在 IA 里。
    let supports_extensions = (handshake.reserved[5] & 0x10) != 0;
    let supports_fast = (handshake.reserved[7] & 0x04) != 0;

    stats.handshaked.fetch_add(1, Ordering::Relaxed);
    tracing::debug!("BT 入站 {peer_addr} 握手成功");

    // ── 2. 建一个库里的 peer 记录 ─────────────────────────────────────
    //
    // 用库的类型，好处是 bitfield、扩展协议这些字段的语义跟出站那边一致。
    //
    // peer 的 key 是 **ip:port**。
    // 库内部用 ip 当 key（`add_peer` / `remove_peer` 都按 ip），
    // 那样同一个 IP 的第二条连接会被直接拒绝——
    // 但 NAT 后面很多用户共用一个公网 IP，
    // 那样会把能用的连接挡掉。
    // 所以这里用 ip:port 区分，代价是跟库的出站连接可能各占一个 key，
    // 但那只是统计上分开，不影响协议。
    let peer_key = format!("{}:{}", peer_addr, peer_port);

    let (num_pieces, bitfield_len) = {
        let guard = ctx.lock_safe();
        (guard.number_of_pieces, guard.bitfield.len())
    };

    // socket 给它一个空的 MockSocket。
    //
    // 原因见文件头的说明：库会用 peer 的 socket 广播 Have，
    // 那会写进我们正在解析的同一条字节流里。
    // 我们自己的收发走原始 TcpStream，不经过库。
    let (mock, _tx, _rx) = bittorrent_rs::utils::io_traits::MockSocket::new();
    let peer = Arc::new(Mutex::new(Peer::new_with_socket(
        peer_key.clone(),
        peer_port,
        Arc::new(bittorrent_rs::utils::io_traits::Socket::Mock(mock)),
    )));
    {
        let mut guard = peer.lock_safe();
        guard.init_bitfield(num_pieces, bitfield_len);
        guard.supports_extensions = supports_extensions;
        guard.supports_fast_extension = supports_fast;
        guard.connected = true;
        guard.remote_peer_id = Some(handshake.peer_id.to_vec());
        guard.am_interested = true;
        guard.am_choking = false;
    }

    // 把 peer 放进 swarm，这样稀缺度统计和选块都能看见它。
    {
        let guard = ctx.lock_safe();
        if !guard.add_peer(peer.clone()) {
            tracing::debug!("BT 入站 {peer_addr} swarm 满了，丢弃");
            return Ok(());
        }
    }

    // ── 3. 发自己的状态 ──────────────────────────────────────────────
    //
    // 告诉对方我手上有什么（bitfield），以及我对它的种子感兴趣。
    // 不发这个，对方不知道该给我什么块。
    let (local_bitfield, is_complete) = {
        let guard = ctx.lock_safe();
        (guard.bitfield.clone(), guard.is_download_complete())
    };

    if num_pieces > 0 {
        if is_complete {
            write_msg(&mut writer, &mut crypto, &PeerMessage::HaveAll)?;
        } else if local_bitfield.is_empty() || local_bitfield.iter().all(|b| *b == 0) {
            write_msg(&mut writer, &mut crypto, &PeerMessage::HaveNone)?;
        } else {
            write_msg(&mut writer, &mut crypto, &PeerMessage::Bitfield(&local_bitfield))?;
        }
    }
    // 只发 interested。
    //
    // **不发 unchoke**：unchoke 的意思是「我允许你下载我的数据」，
    // 我们只下载、不做种，给了也拿不出东西。
    // 发了反而会让对方以为我们能供源，白等一场。
    // 对方发 interested 过来的时候再回 unchoke（见下面的分支），
    // 那是为了不让对方觉得我们完全不理它、把它踢掉。
    write_msg(&mut writer, &mut crypto, &PeerMessage::Interested)?;

    // ── 4. 主循环 ─────────────────────────────────────────────────────
    let mut buf = vec![0u8; 128 * 1024];
    let mut last_sent = Instant::now();
    let mut last_recv = Instant::now();

    let result = loop {
        // 把请求管道填满。
        //
        // 一次只发一个 request 的话，每个块都要等一个 RTT，
        // 带宽利用率会被 RTT 拖死。这里一次把流水线填到
        // MAX_OUTSTANDING_REQUESTS，跟库的出站连接保持一致。
        if fill_requests(&ctx, &peer, &mut writer, &mut crypto, &mut last_sent).is_err() {
            break Ok(());
        }

        // 读一条消息。超时就当对方还活着，发个 keep-alive 续着。
        match read_msg(&mut reader, &mut crypto, &mut buf) {
            Ok(Some(msg)) => {
                last_recv = Instant::now();
                match handle_inbound_message(
                    &mut writer,
                    &mut crypto,
                    &ctx,
                    &peer,
                    msg,
                    &mut last_sent,
                ) {
                    Ok(true) => break Ok(()),
                    Ok(false) => {}
                    Err(err) => break Err(err),
                }
            }
            Ok(None) => {
                // 对端关闭了连接。
                break Ok(());
            }
            Err(err) => {
                let timed_out = err
                    .downcast_ref::<std::io::Error>()
                    .map(|e| {
                        e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut
                    })
                    .unwrap_or(false);
                if !timed_out {
                    tracing::debug!("BT 入站 {peer_addr} 读取失败: {err}");
                    break Ok(());
                }
                // 超时：太久没动静就断开，否则发 keep-alive。
                if last_recv.elapsed() > IDLE_TIMEOUT {
                    break Ok(());
                }
                if last_sent.elapsed() > KEEPALIVE_INTERVAL {
                    if write_msg(&mut writer, &mut crypto, &PeerMessage::KeepAlive).is_err() {
                        break Ok(());
                    }
                    last_sent = Instant::now();
                }
            }
        }

        // 撤掉超时的请求，不然块会被一直占着不放。
        if check_request_timeouts(&ctx, &peer, &mut writer, &mut crypto, &mut last_sent).is_err() {
            break Ok(());
        }

        // 后台校验线程的结果要定期取出来，否则 bitfield 不会更新。
        {
            let mut guard = ctx.lock_safe();
            let storage = guard.storage.clone();
            guard.drain_verification_results(&*storage);
        }
    };

    // 会话结束，把 peer 从 swarm 里摘掉。
    {
        let mut guard = ctx.lock_safe();
        guard.remove_peer(&peer_key);
    }

    result
}

/// 处理一条收到的消息。
///
/// 返回 `true` 表示会话该结束了。
fn handle_inbound_message(
    writer: &mut TcpStream,
    crypto: &mut Option<PeStream>,
    ctx: &Arc<Mutex<TorrentContext>>,
    peer: &Arc<Mutex<Peer>>,
    msg: OwnedMessage,
    last_sent: &mut Instant,
) -> anyhow::Result<bool> {
    match msg {
        OwnedMessage::Bitfield(bf) => {
            // 先把本地 bitfield 的长度取出来。
            //
            // 注意**不要**在持有 peer 锁的时候去拿 ctx 锁——
            // `next_request` 是先 ctx 后 peer 的顺序，
            // 两个地方反着来就是死锁。
            let local_len = {
                let ctx = ctx.lock_safe();
                ctx.bitfield.len()
            };
            {
                let mut guard = peer.lock_safe();
                // 对端的 bitfield 长度可能跟本地的不一样
                // （有些实现会把最后一个字节多发几位，有些会少发）。
                // 统一补齐到本地的长度，缺的部分当 0（对方没有），
                // 这样 `merge_piece_bitfield` 按字节扫的时候才对得上。
                let mut bf = bf.clone();
                if bf.len() < local_len {
                    bf.resize(local_len, 0);
                } else if bf.len() > local_len {
                    bf.truncate(local_len);
                }
                guard.set_remote_bitfield(bf);
            }
            // 收到 bitfield 必须合并到稀缺度统计里，
            // 否则选块策略不知道哪些块稀缺。
            {
                let mut guard = ctx.lock_safe();
                let p = peer.lock_safe();
                guard.merge_piece_bitfield(&p);
            }
        }
        OwnedMessage::Have(index) => {
            let was_new = {
                let mut guard = peer.lock_safe();
                let was_new = !guard.is_piece_on_remote_peer(index);
                guard.set_piece_on_remote_peer(index);
                was_new
            };
            if was_new {
                let mut guard = ctx.lock_safe();
                guard.increment_peer_count(index);
            }
        }
        OwnedMessage::Unchoke => {
            // 对方放开了我们。`peer_choking` 为 true 表示"没被 choke"，
            // 库里的出站路径也是这么判断的（`peer_choking.wait_one(0)` 为真
            // 才去要块）。
            //
            // 只在「从 choke 变成 unchoke」时动计数，
            // 否则重复收到 Unchoke 会把计数加乱。
            let was_choked = {
                let guard = peer.lock_safe();
                let was_choked = !guard.peer_choking.wait_one(0);
                guard.peer_choking.set();
                was_choked
            };
            if was_choked {
                // 同步库里的计数。库的 `remove_peer` 在 peer 处于 unchoke
                // 状态时会减掉这个计数，所以这边必须对称地加。
                let guard = ctx.lock_safe();
                guard.unchoked_peers_count.fetch_add(1, Ordering::Relaxed);
            }
        }
        OwnedMessage::Choke => {
            let was_unchoked = {
                let guard = peer.lock_safe();
                let was_unchoked = guard.peer_choking.wait_one(0);
                guard.peer_choking.reset();
                was_unchoked
            };
            if was_unchoked {
                let guard = ctx.lock_safe();
                guard.unchoked_peers_count.fetch_sub(1, Ordering::Relaxed);
            }
        }
        OwnedMessage::Interested => {
            // 对方想要我们的数据。我们不做种，但协议上要回 unchoke，
            // 否则一部分客户端会直接断开。
            write_msg(writer, crypto, &PeerMessage::Unchoke)?;
            *last_sent = Instant::now();
        }
        OwnedMessage::NotInterested => {}
        OwnedMessage::Piece {
            index,
            begin,
            block,
        } => {
            // 收到块：交给库的 context 去写盘 + 校验。
            let ip = {
                let guard = peer.lock_safe();
                guard.ip.clone()
            };
            {
                let mut guard = ctx.lock_safe();
                let storage = guard.storage.clone();
                if let Err(err) =
                    guard.process_piece_block(&*storage, index, begin, &block, &ip)
                {
                    // 校验失败说明这块坏了，库内部会重下，这里不用管。
                    tracing::debug!("BT 入站块校验失败: {err}");
                }
            }
            // 撤掉占位。
            //
            // 库在 `process_piece_block` 里校验通过后会把整块 piece 的
            // 占位清掉（`clear_piece_requests`），但那是在「整个 piece 齐了」
            // 的时候才做的。单个 block 收完，这块占位必须由我们自己撤，
            // 否则这个块永远不会被别的 peer 要——下载会卡死。
            let block_index = begin / BLOCK_SIZE as u32;
            {
                let guard = ctx.lock_safe();
                guard.release_block_request(index, block_index);
            }
            {
                let mut guard = peer.lock_safe();
                guard.outstanding_requests_count =
                    guard.outstanding_requests_count.saturating_sub(1);
                guard
                    .reserved_blocks
                    .retain(|&(p, b, _)| !(p == index && b == block_index));
            }
        }
        OwnedMessage::Request {
            index,
            begin,
            length,
        } => {
            // 对方跟我们要数据。我们只下载不做种，
            // 但协议上必须给个回应，不然对方会一直重试。
            // 按 BEP 6 始终回 Reject。
            write_msg(
                writer,
                crypto,
                &PeerMessage::Reject {
                    index,
                    begin,
                    length,
                },
            )?;
            *last_sent = Instant::now();
        }
        OwnedMessage::Reject {
            index,
            begin,
            ..
        } => {
            // 对端拒了我们的请求：撤掉占位，让别的 peer 去要。
            let block_index = begin / BLOCK_SIZE as u32;
            {
                let guard = ctx.lock_safe();
                guard.release_block_request(index, block_index);
            }
            let mut guard = peer.lock_safe();
            guard.outstanding_requests_count = guard.outstanding_requests_count.saturating_sub(1);
            guard
                .reserved_blocks
                .retain(|&(p, b, _)| !(p == index && b == block_index));
        }
        OwnedMessage::Cancel { .. } => {}
        OwnedMessage::Port(_) => {}
        OwnedMessage::Extended { .. } => {
            // 扩展协议（ut_pex / ut_metadata）我们不主动用，
            // 但对方可能发过来。忽略即可。
        }
        OwnedMessage::KeepAlive => {}
        OwnedMessage::HaveAll => {
            // 对方说他有全部的分片。
            //
            // 先取本地 bitfield 的长度，再拿 peer 锁——
            // 不要在持有 peer 锁的时候去拿 ctx 锁。
            let local_len = {
                let ctx = ctx.lock_safe();
                ctx.bitfield.len()
            };
            {
                let mut guard = peer.lock_safe();
                guard.peer_is_seed = true;
                // bitfield 的长度得跟本地的对齐，否则 `merge_piece_bitfield`
                // 按字节扫的时候会对不上。
                guard.remote_piece_bitfield = vec![0xFF; local_len];
                guard.number_of_missing_pieces = 0;
            }
            {
                let mut guard = ctx.lock_safe();
                let p = peer.lock_safe();
                guard.merge_piece_bitfield(&p);
            }
        }
        OwnedMessage::HaveNone => {
            // 对方说他一个分片都没有——对下载没意义。
            let mut guard = peer.lock_safe();
            guard.peer_is_seed = false;
        }
        OwnedMessage::Suggest(_) => {}
        OwnedMessage::AllowedFast(_) => {}
    }
    Ok(false)
}

/// 挑一个块向这个 peer 要。
///
/// 复用库的 `next_block_request_for_peer`，
/// 这样稀缺度优先的选块策略跟出站连接保持一致。
///
/// 注意 `next_block_request_for_peer` **内部已经**做了
/// `reserve_block_request`（在 assembler 里登记占位）。
/// 所以外面只管往 `peer.reserved_blocks` 里再记一份，
/// 千万别重复 reserve——否则占位永远不释放。
fn next_request(
    ctx: &Arc<Mutex<TorrentContext>>,
    peer: &Arc<Mutex<Peer>>,
) -> Option<PeerMessage<'static>> {
    let mut guard = ctx.lock_safe();
    let mut p = peer.lock_safe();

    // 已经挂着的请求太多就先等等。
    if p.outstanding_requests_count >= MAX_OUTSTANDING_REQUESTS {
        return None;
    }

    // 被对方 choke 了就别要了。
    //
    // `peer_choking` 这个名字有点误导：它是 ManualResetEvent，
    // **置位**表示「当前没被 choke」，清零表示「被 choke 了」。
    // 库的出站路径也是这么判断的（`peer_choking.wait_one(0)` 为真才去要块）。
    if !p.peer_choking.wait_one(0) {
        return None;
    }

    let (index, begin, length) = guard.next_block_request_for_peer(&p)?;

    // 记一份占位。
    //
    // 注意这里**不能**再 `peer.lock_safe()` 一次——
    // 上面已经拿着 peer 的锁了，`std::sync::Mutex` 不可重入，
    // 再拿一次就是自己把自己锁死（表现是请求永远发不出去）。
    let block_index = begin / BLOCK_SIZE as u32;
    p.reserved_blocks
        .push((index, block_index, Instant::now()));
    p.outstanding_requests_count = p.outstanding_requests_count.saturating_add(1);

    Some(PeerMessage::Request {
        index,
        begin,
        length,
    })
}

/// 把请求流水线填满。
///
/// 一直要到块，直到：
/// - 对端手里没有我们需要的块了（`next_request` 返回 None）
/// - 或者挂着的请求数到了上限（`MAX_OUTSTANDING_REQUESTS`）
///
/// 一发一收的话，每个块都要等一个 RTT，吞吐会被 RTT 拖死。
fn fill_requests(
    ctx: &Arc<Mutex<TorrentContext>>,
    peer: &Arc<Mutex<Peer>>,
    writer: &mut TcpStream,
    crypto: &mut Option<PeStream>,
    last_sent: &mut Instant,
) -> std::io::Result<()> {
    loop {
        let Some(req) = next_request(ctx, peer) else {
            return Ok(());
        };
        write_msg(writer, crypto, &req)?;
        *last_sent = Instant::now();
    }
}

/// 撤掉超时的请求。
///
/// 不撤的话，块在 assembler 里会一直被占着，
/// 那个块就再也不会被别人要——下载会卡住。
fn check_request_timeouts(
    ctx: &Arc<Mutex<TorrentContext>>,
    peer: &Arc<Mutex<Peer>>,
    writer: &mut TcpStream,
    crypto: &mut Option<PeStream>,
    last_sent: &mut Instant,
) -> std::io::Result<()> {
    let timed_out: Vec<(u32, u32)> = {
        let mut guard = peer.lock_safe();
        let mut timed_out = Vec::new();
        guard.reserved_blocks.retain(|&(pn, bi, time)| {
            if time.elapsed() > REQUEST_TIMEOUT {
                timed_out.push((pn, bi));
                false
            } else {
                true
            }
        });
        timed_out
    };

    if timed_out.is_empty() {
        return Ok(());
    }

    for (pn, bi) in timed_out {
        let begin = bi * BLOCK_SIZE as u32;
        let length = {
            let guard = ctx.lock_safe();
            (BLOCK_SIZE as u32).min(guard.get_piece_length(pn).saturating_sub(begin))
        };
        {
            let guard = ctx.lock_safe();
            guard.release_block_request(pn, bi);
        }
        {
            let mut guard = peer.lock_safe();
            guard.outstanding_requests_count =
                guard.outstanding_requests_count.saturating_sub(1);
        }
        write_msg(
            writer,
            crypto,
            &PeerMessage::Cancel {
                index: pn,
                begin,
                length,
            },
        )?;
        *last_sent = Instant::now();
    }
    Ok(())
}

/// 写一条消息。
fn write_msg(
    writer: &mut TcpStream,
    crypto: &mut Option<PeStream>,
    msg: &PeerMessage<'_>,
) -> std::io::Result<()> {
    let mut buf = msg.encode();
    if let Some(s) = crypto.as_mut() {
        s.enc.apply(&mut buf);
    }
    writer.write_all(&buf)?;
    writer.flush()
}

/// 读一条消息。
///
/// 返回 `Ok(None)` 表示对端正常关闭。
fn read_msg(
    reader: &mut pe::Buffered<TcpStream>,
    crypto: &mut Option<PeStream>,
    buf: &mut [u8],
) -> anyhow::Result<Option<OwnedMessage>> {
    let mut len_buf = [0u8; 4];
    reader.take_exact(&mut len_buf)?;
    if let Some(s) = crypto.as_mut() {
        s.dec.apply(&mut len_buf);
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return Ok(Some(OwnedMessage::KeepAlive));
    }
    if len > MAX_MESSAGE_LEN {
        anyhow::bail!("BT 入站消息过长: {len}");
    }
    if len > buf.len() {
        anyhow::bail!("BT 入站消息超出缓冲区: {len}");
    }

    reader.take_exact(&mut buf[..len])?;
    if let Some(s) = crypto.as_mut() {
        s.dec.apply(&mut buf[..len]);
    }

    // 库的 `PeerMessage::decode` 返回借用 buffer 的消息，
    // 这里得把它变成拥有所有权的形式才能安全返回。
    let owned = match PeerMessage::decode(&buf[..len])? {
        PeerMessage::Bitfield(bf) => OwnedMessage::Bitfield(bf.to_vec()),
        PeerMessage::Piece {
            index,
            begin,
            block,
        } => OwnedMessage::Piece {
            index,
            begin,
            block: block.to_vec(),
        },
        PeerMessage::Request {
            index,
            begin,
            length,
        } => OwnedMessage::Request {
            index,
            begin,
            length,
        },
        PeerMessage::Cancel {
            index,
            begin,
            length,
        } => OwnedMessage::Cancel {
            index,
            begin,
            length,
        },
        PeerMessage::Extended { ext_id, payload } => OwnedMessage::Extended {
            ext_id,
            payload: payload.to_vec(),
        },
        PeerMessage::Have(i) => OwnedMessage::Have(i),
        PeerMessage::Suggest(i) => OwnedMessage::Suggest(i),
        PeerMessage::AllowedFast(i) => OwnedMessage::AllowedFast(i),
        PeerMessage::Reject {
            index,
            begin,
            length,
        } => OwnedMessage::Reject {
            index,
            begin,
            length,
        },
        PeerMessage::KeepAlive => OwnedMessage::KeepAlive,
        PeerMessage::Choke => OwnedMessage::Choke,
        PeerMessage::Unchoke => OwnedMessage::Unchoke,
        PeerMessage::Interested => OwnedMessage::Interested,
        PeerMessage::NotInterested => OwnedMessage::NotInterested,
        PeerMessage::Port(p) => OwnedMessage::Port(p),
        PeerMessage::HaveAll => OwnedMessage::HaveAll,
        PeerMessage::HaveNone => OwnedMessage::HaveNone,
        _ => OwnedMessage::KeepAlive,
    };
    Ok(Some(owned))
}

/// 自己定义一份拥有所有权的消息。
///
/// 库的 `PeerMessage<'a>` 里的 bitfield / piece / extended 都是借用类型，
/// 直接拿来当返回值是过不了生命周期的。
///
/// 有一批字段（`Cancel` 的 index/begin/length、`Port`、`Extended` 等）
/// 收了但不用——它们只需要"识别出来、然后忽略"，
/// 所以标上 `#[allow(dead_code)]`，免得编译器报一堆噪音。
#[derive(Debug)]
#[allow(dead_code)]
enum OwnedMessage {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    HaveAll,
    HaveNone,
    Bitfield(Vec<u8>),
    Request {
        index: u32,
        begin: u32,
        length: u32,
    },
    Piece {
        index: u32,
        begin: u32,
        block: Vec<u8>,
    },
    Cancel {
        index: u32,
        begin: u32,
        length: u32,
    },
    Reject {
        index: u32,
        begin: u32,
        length: u32,
    },
    Port(u16),
    Extended {
        ext_id: u8,
        payload: Vec<u8>,
    },
    Suggest(u32),
    AllowedFast(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_track_active_sessions() {
        let stats = Arc::new(InboundStats::default());
        {
            let guard = ActiveGuard {
                stats: stats.clone(),
            };
            stats.accepted.fetch_add(1, Ordering::Relaxed);
            stats.active.fetch_add(1, Ordering::Relaxed);
            assert_eq!(stats.accepted(), 1);
            assert_eq!(stats.active(), 1);
            drop(guard);
        }
        // Drop 守卫会把 active 减掉。
        assert_eq!(stats.active(), 0);
        assert_eq!(stats.accepted(), 1);
    }
}
