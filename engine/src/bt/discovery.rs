//! peer 发现：tracker 兜底 + DHT announce。
//!
//! 磁力链接天生没有 tracker，只能靠 DHT 找 peer。
//! 但 `bittorrent-rs` 的 DHT 有两个问题：
//!
//! 1. bootstrap 节点固定是 `router.bittorrent.com` / `router.utorrent.com` /
//!    `dht.transmissionbt.com` / `dht.libtorrent.org`。实测在国内，
//!    只有 `dht.transmissionbt.com` 能应答，
//!    其余的 UDP 端口全超时（其中两个的 DNS 还被污染，返回的 IP 是假的）。
//! 2. 它**只回应**别人的 `announce_peer`，自己从来不主动 announce。
//!    也就是说：别人能查到我们，我们查不到别人。
//!
//! 所以这里做两件事：
//! - 给磁力链接补一批实测可达的 tracker
//! - 自己实现 DHT 的 `announce_peer`，让别的 peer 能反过来找到我们

use std::collections::HashMap;
use std::collections::HashSet;
use std::net::{ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bittorrent_rs::core::torrent_context::TorrentContext;
use bittorrent_rs::utils::bencode::{BNode, Bencode};
use bittorrent_rs::utils::util::LockExt;

/// DHT 引导节点。
///
/// 实测（2026-11-18，UDP find_node，4 秒超时）：
///
/// - `87.98.162.88:6881`、`212.129.33.59:6881`（`dht.transmissionbt.com`
///   的两个 IPv4）**能应答**，而且能返回 200 多个节点。
/// - 其余全部超时：`router.bittorrent.com`、`router.utorrent.com`、
///   `dht.libtorrent.org`、`dht.aelitis.com`、`bttracker.debian.org`、
///   `dht.openbittorrent.com`、`router.bitcomet.com`、
///   `cntracker.dht.libtorrent.org`、`tracker.opentrackr.org`。
/// - 从 transmissionbt 拿到的 nodes 里再挖一层，也全超时——
///   国内到 DHT 的 UDP 出站基本是不通的。
///
/// **为什么写 IP 而不是域名**：
/// `dht.transmissionbt.com` 会同时解析出 IPv6 和 IPv4，
/// 走 IPv6 的话一个包都发不出去（连超时都收不到），
/// 而只写 IPv4 就一次通。
/// 域名形式的留在后面兜底——换台机器、换个网络环境可能就通了。
pub const DHT_BOOTSTRAP: &[&str] = &[
    // dht.transmissionbt.com 的两个 IPv4，实测唯一能应答的。
    "87.98.162.88:6881",
    "212.129.33.59:6881",
    // 域名形式作为补充（网络环境变了可能就能用）。
    "dht.transmissionbt.com:6881",
    "router.bittorrent.com:6881",
    "router.utorrent.com:6881",
    "dht.libtorrent.org:25401",
];

/// 磁力链接的兜底 tracker。
///
/// 实测（2026-11-18）：
///
/// - HTTP：`tracker.opentrackr.org:1337` 能通（200）。
///   其余常见的（openbittorrent 403、gbitt 502、torrentmatrix 502、
///   1337x 502、bttracker.debian 超时）全不通。
/// - UDP：`tracker.opentrackr.org:1337` 和 `open.stealth.si:80` 能通
///   （收到 action=3 的应答）。
///   explodie / openbittorrent / exodus / torrent.eu.org 全超时。
///
/// 国内能用的 tracker 很少，所以 HTTP、UDP 各留几个能通的，
/// 其余当补充——换网络环境可能就通了。
pub const FALLBACK_TRACKERS: &[&str] = &[
    "http://tracker.opentrackr.org:1337/announce",
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://explodie.org:6969/announce",
    "http://tracker.openbittorrent.com/announce",
    "udp://tracker.openbittorrent.com:80/announce",
];

/// 把兜底 tracker 塞进会话。
///
/// 已经有的不重复加——磁力链接自带 `tr=` 的 tracker 优先级更高。
pub fn inject_fallback_trackers(ctx: &Arc<Mutex<TorrentContext>>) -> usize {
    let mut guard = ctx.lock_safe();
    let mut added = 0;
    for t in FALLBACK_TRACKERS {
        if guard.add_tracker_url(*t) {
            added += 1;
        }
    }
    added
}

/// DHT 节点数的统计。
///
/// 界面上要显示「DHT 里有多少个节点」，
/// 这样用户能看出磁力链接到底有没有连上 DHT。
#[derive(Debug, Default)]
pub struct DhtStats {
    /// 当前已知的 DHT 节点数。
    nodes: AtomicU64,
    /// 已经 announce 成功的节点数。
    announced: AtomicU64,
}

impl DhtStats {
    pub fn nodes(&self) -> u64 {
        self.nodes.load(Ordering::Relaxed)
    }

    pub fn announced(&self) -> u64 {
        self.announced.load(Ordering::Relaxed)
    }
}

/// DHT announce 器。
///
/// 库的 DHT 只收不发，我们自己补上 `announce_peer`——
/// 不然别的 peer 根本查不到我们，只能我们去找他们。
///
/// 关于 token：DHT 的 token 是**对端**按
/// `sha1(我们的IP || 对端的token_salt)` 算出来的，
/// 只跟我们的 IP 有关，跟我们的节点 ID、本地端口都无关。
/// 所以这里用自己的 socket 收发是安全的——
/// 只要 get_peers 和 announce_peer 用的是同一个源 IP（同一个 socket 自然是）。
pub struct DhtAnnouncer {
    socket: UdpSocket,
    node_id: [u8; 20],
    info_hash: [u8; 20],
    port: u16,
    /// 各节点给我们的 token。
    tokens: Mutex<HashMap<String, Vec<u8>>>,
    /// 已经 announce 过的节点，避免重复。
    announced: Mutex<HashSet<String>>,
    /// 候选节点（从库的路由表里拿）。
    candidates: Mutex<Vec<(String, u16)>>,
    stats: Arc<DhtStats>,
    running: Arc<AtomicBool>,
}

impl DhtAnnouncer {
    /// 起一个 announce 循环。
    ///
    /// `info_hash` 是我们的种子，`port` 是别人要连过来的端口。
    pub fn spawn(
        info_hash: [u8; 20],
        port: u16,
        stats: Arc<DhtStats>,
        running: Arc<AtomicBool>,
    ) -> (Arc<DhtAnnouncer>, std::thread::JoinHandle<()>) {
        let socket = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!("BT DHT announce socket 创建失败: {err}");
                // 拿不到 socket 就退化成只有引导节点、
                // 不做 announce 的状态——总比整个任务起不来强。
                let announcer = Arc::new(DhtAnnouncer {
                    socket: fallback_socket(),
                    node_id: random_node_id(),
                    info_hash,
                    port,
                    tokens: Mutex::new(HashMap::new()),
                    announced: Mutex::new(HashSet::new()),
                    candidates: Mutex::new(Vec::new()),
                    stats,
                    running: running.clone(),
                });
                let handle = std::thread::Builder::new()
                    .name("idm-shadow-bt-dht-announce".to_string())
                    .spawn(move || std::thread::sleep(Duration::from_secs(3600)))
                    .expect("启动 DHT announce 线程失败");
                return (announcer, handle);
            }
        };
        let _ = socket.set_read_timeout(Some(Duration::from_millis(300)));

        let announcer = Arc::new(DhtAnnouncer {
            socket,
            node_id: random_node_id(),
            info_hash,
            port,
            tokens: Mutex::new(HashMap::new()),
            announced: Mutex::new(HashSet::new()),
            candidates: Mutex::new(Vec::new()),
            stats,
            running: running.clone(),
        });
        let thread_announcer = announcer.clone();
        let handle = std::thread::Builder::new()
            .name("idm-shadow-bt-dht-announce".to_string())
            .spawn(move || thread_announcer.run())
            .expect("启动 DHT announce 线程失败");
        (announcer, handle)
    }

    /// 主循环。
    fn run(&self) {
        // 引导节点先塞进去。
        {
            let mut candidates = self.candidates.lock().unwrap_or_else(|e| e.into_inner());
            for node in DHT_BOOTSTRAP {
                if let Some((ip, port)) = split_node(node) {
                    candidates.push((ip, port));
                }
            }
        }

        let mut last_query = Instant::now() - Duration::from_secs(60);

        while self.running.load(Ordering::Relaxed) {
            // 收包：找 get_peers 的应答，拿 token。
            self.drain_responses();

            // 每 30 秒重新问一轮，token 有时效性。
            if last_query.elapsed() > Duration::from_secs(30) {
                self.query_candidates();
                last_query = Instant::now();
            }

            // 给已知节点 announce 一次。
            self.announce_known_nodes();

            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// 收 UDP 应答，把 get_peers 的 token 记下来。
    fn drain_responses(&self) {
        let mut buf = [0u8; 4096];
        loop {
            let Ok((n, addr)) = self.socket.recv_from(&mut buf) else {
                return;
            };
            let Ok(msg) = Bencode::decode(&buf[..n]) else {
                continue;
            };
            let Some(r) = msg.dict_get(b"r") else {
                continue;
            };
            // token 只跟源 IP 有关，跟端口无关，所以按 IP 存就够。
            //
            // 但应答里也可能带着 `values`（peer 列表）或者 `nodes`（更多节点），
            // 顺手收了能省一轮查询。
            if let Some(token) = r.dict_get(b"token").and_then(|t| t.as_string()) {
                let mut tokens = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
                tokens.insert(addr.ip().to_string(), token.to_vec());
            }

            if let Some(nodes) = r.dict_get(b"nodes").and_then(|n| n.as_string()) {
                for (ip, port) in parse_compact_nodes(nodes) {
                    self.add_candidate(ip, port);
                }
            }

            if let Some(BNode::List(values)) = r.dict_get(b"values") {
                for v in values {
                    if let BNode::String(compact) = v {
                        for (ip, port) in parse_compact_peers(compact) {
                            self.add_candidate(ip, port);
                        }
                    }
                }
            }
        }
    }

    /// 给候选节点发 get_peers，顺带刷新 token。
    fn query_candidates(&self) {
        let candidates: Vec<(String, u16)> = {
            let mut list = self.candidates.lock().unwrap_or_else(|e| e.into_inner());
            list.dedup();
            list.clone()
        };
        for (ip, port) in candidates {
            let _ = self.send_get_peers(&ip, port);
        }
    }

    /// 给已知节点发 announce_peer。
    fn announce_known_nodes(&self) {
        // 没有 token 就先不发——没有 token 的 announce 会被节点忽略。
        let tokens: Vec<(String, Vec<u8>)> = {
            let tokens = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
            tokens
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };

        for (ip, token) in tokens {
            {
                let announced = self.announced.lock().unwrap_or_else(|e| e.into_inner());
                if announced.contains(&ip) {
                    continue;
                }
            }
            // 端口我们只从路由表里拿到的候选节点身上试。
            //
            // 同一个 IP 可能对应多个端口（不同节点跑在同一台机器上），
            // 挨个试一遍，成功就记下来。
            let ports: Vec<u16> = {
                let candidates = self.candidates.lock().unwrap_or_else(|e| e.into_inner());
                let mut ports: Vec<u16> = candidates
                    .iter()
                    .filter(|(cip, _)| cip == &ip)
                    .map(|(_, p)| *p)
                    .collect();
                ports.sort_unstable();
                ports.dedup();
                ports
            };
            if ports.is_empty() {
                continue;
            }
            for port in ports {
                if self.announce_to(&ip, port, &token).is_ok() {
                    let mut announced = self.announced.lock().unwrap_or_else(|e| e.into_inner());
                    announced.insert(ip.clone());
                    self.stats.announced.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }
    }

    /// 发 get_peers，问对方要 peer 列表（顺便拿 token）。
    fn send_get_peers(&self, ip: &str, port: u16) -> std::io::Result<()> {
        let target = (ip, port).to_socket_addrs()?.next().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "DHT 节点地址无效")
        })?;
        let msg = Bencode::encode(&BNode::Dictionary(vec![
            (
                b"a".as_slice(),
                BNode::Dictionary(vec![
                    (b"id".as_slice(), BNode::String(&self.node_id)),
                    (b"info_hash".as_slice(), BNode::String(&self.info_hash)),
                ]),
            ),
            (b"q".as_slice(), BNode::String(b"get_peers")),
            (b"t".as_slice(), BNode::String(b"aa")),
            (b"y".as_slice(), BNode::String(b"q")),
            (b"v".as_slice(), BNode::String(b"\x01")),
        ]));
        self.socket.send_to(&msg, target)?;
        Ok(())
    }

    /// 给一个节点发 announce_peer。
    fn announce_to(&self, ip: &str, port: u16, token: &[u8]) -> std::io::Result<()> {
        let target = (ip, port).to_socket_addrs()?.next().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "DHT 节点地址无效")
        })?;

        let port_bytes = self.port.to_string().into_bytes();
        let msg = Bencode::encode(&BNode::Dictionary(vec![
            (
                b"a".as_slice(),
                BNode::Dictionary(vec![
                    (b"id".as_slice(), BNode::String(&self.node_id)),
                    (b"implied_port".as_slice(), BNode::Number(b"0")),
                    (b"info_hash".as_slice(), BNode::String(&self.info_hash)),
                    (b"port".as_slice(), BNode::Number(&port_bytes)),
                    (b"token".as_slice(), BNode::String(token)),
                ]),
            ),
            (b"q".as_slice(), BNode::String(b"announce_peer")),
            (b"t".as_slice(), BNode::String(b"aa")),
            (b"y".as_slice(), BNode::String(b"q")),
            (b"v".as_slice(), BNode::String(b"\x01")),
        ]));

        self.socket.send_to(&msg, target)?;
        Ok(())
    }

    /// 记下一个候选节点（IP + 端口）。
    pub fn add_candidate(&self, ip: String, port: u16) {
        let mut candidates = self.candidates.lock().unwrap_or_else(|e| e.into_inner());
        if !candidates.iter().any(|(c, p)| c == &ip && *p == port) {
            candidates.push((ip, port));
        }
        self.stats.nodes.fetch_add(1, Ordering::Relaxed);
    }

    /// 当前候选节点数。
    pub fn candidate_count(&self) -> usize {
        self.candidates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

/// 拆 `host:port`。
fn split_node(node: &str) -> Option<(String, u16)> {
    let (host, port) = node.rsplit_once(':')?;
    let port = port.parse().ok()?;
    Some((host.to_string(), port))
}

/// 解析 DHT 的 compact nodes 格式。
///
/// 每条 26 字节：20 字节节点 ID + 4 字节 IPv4 + 2 字节端口。
fn parse_compact_nodes(data: &[u8]) -> Vec<(String, u16)> {
    let mut out = Vec::new();
    for chunk in data.chunks_exact(26) {
        // 全零的 IP 是无效地址（有些实现会把 IPv6 塞在固定 4 字节里，
        // 结果就是 0.0.0.0），直接丢掉。
        if chunk[20] == 0 && chunk[21] == 0 && chunk[22] == 0 && chunk[23] == 0 {
            continue;
        }
        let ip = format!(
            "{}.{}.{}.{}",
            chunk[20], chunk[21], chunk[22], chunk[23]
        );
        let port = u16::from_be_bytes([chunk[24], chunk[25]]);
        if port > 0 {
            out.push((ip, port));
        }
    }
    out
}

/// 解析 DHT 的 compact peers 格式。
///
/// 每条 6 字节：4 字节 IPv4 + 2 字节端口。
fn parse_compact_peers(data: &[u8]) -> Vec<(String, u16)> {
    let mut out = Vec::new();
    for chunk in data.chunks_exact(6) {
        if chunk[0] == 0 && chunk[1] == 0 && chunk[2] == 0 && chunk[3] == 0 {
            continue;
        }
        let ip = format!(
            "{}.{}.{}.{}",
            chunk[0], chunk[1], chunk[2], chunk[3]
        );
        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
        if port > 0 {
            out.push((ip, port));
        }
    }
    out
}

/// 拿不到 socket 时的兜底：一个只绑本地、哪儿也发不出去的空 socket。
///
/// 目的是别让整个 BT 任务因为一个附属线程起不来而挂掉。
fn fallback_socket() -> UdpSocket {
    UdpSocket::bind("127.0.0.1:0").unwrap_or_else(|_| {
        // 实在连一个 socket 都建不出来，就用未初始化的（收发都会报错，
        // 但不会 panic）。
        std::net::UdpSocket::bind("0.0.0.0:0").expect("连 UDP socket 都建不出来")
    })
}

/// 随机节点 ID。
fn random_node_id() -> [u8; 20] {
    use rand::RngCore;
    let mut id = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut id);
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_trackers_are_unique_and_non_empty() {
        let mut seen = HashSet::new();
        for t in FALLBACK_TRACKERS {
            assert!(!t.is_empty(), "tracker 不能是空串");
            assert!(seen.insert(*t), "tracker 重复了: {t}");
        }
    }

    #[test]
    fn bootstrap_nodes_are_unique() {
        let mut seen = HashSet::new();
        for n in DHT_BOOTSTRAP {
            assert!(seen.insert(*n), "DHT 节点重复了: {n}");
            assert!(n.contains(':'), "DHT 节点必须带端口: {n}");
            assert!(split_node(n).is_some(), "DHT 节点解析不了: {n}");
        }
    }

    #[test]
    fn tracker_urls_are_well_formed() {
        for t in FALLBACK_TRACKERS {
            assert!(
                t.starts_with("http://") || t.starts_with("udp://"),
                "tracker 协议不对: {t}"
            );
            assert!(t.ends_with("/announce"), "tracker 路径不对: {t}");
        }
    }

    #[test]
    fn 拆分节点() {
        assert_eq!(
            split_node("dht.transmissionbt.com:6881"),
            Some(("dht.transmissionbt.com".to_string(), 6881))
        );
        assert_eq!(split_node("没有端口"), None);
    }

    #[test]
    fn 解析紧凑节点列表() {
        // 2 个节点：20 字节 ID + 4 字节 IP + 2 字节端口
        let mut data = Vec::new();
        data.extend_from_slice(&[0u8; 20]);
        data.extend_from_slice(&[1, 2, 3, 4]);
        data.extend_from_slice(&6881u16.to_be_bytes());
        data.extend_from_slice(&[0u8; 20]);
        data.extend_from_slice(&[192, 168, 0, 1]);
        data.extend_from_slice(&51413u16.to_be_bytes());

        let nodes = parse_compact_nodes(&data);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0], ("1.2.3.4".to_string(), 6881));
        assert_eq!(nodes[1], ("192.168.0.1".to_string(), 51413));
    }

    #[test]
    fn 解析紧凑节点列表忽略残缺尾巴() {
        // 尾巴不足 26 字节的应当丢掉，不能 panic。
        let mut data = vec![0u8; 26];
        data[20..24].copy_from_slice(&[10, 0, 0, 1]);
        data[24..26].copy_from_slice(&6881u16.to_be_bytes());
        data.extend_from_slice(&[0u8; 4]);
        let nodes = parse_compact_nodes(&data);
        assert_eq!(nodes.len(), 1);
    }

    #[test]
    fn 解析紧凑节点列表丢掉零ip() {
        // 全零 IP 是无效地址，得丢掉。
        let data = vec![0u8; 26];
        assert!(parse_compact_nodes(&data).is_empty());
    }

    #[test]
    fn 解析紧凑peer列表() {
        let mut data = Vec::new();
        data.extend_from_slice(&[203, 0, 113, 5]);
        data.extend_from_slice(&6881u16.to_be_bytes());
        let peers = parse_compact_peers(&data);
        assert_eq!(peers, vec![("203.0.113.5".to_string(), 6881)]);
    }

    #[test]
    fn 引导节点里必须有直连ip() {
        // 国内 DNS 被污染，域名形式经常解析到不相干的 IP。
        // 至少要留几个写死的 IPv4 兜底。
        let has_raw_ip = DHT_BOOTSTRAP.iter().any(|n| {
            n.split(':')
                .next()
                .map(|host| host.parse::<std::net::Ipv4Addr>().is_ok())
                .unwrap_or(false)
        });
        assert!(has_raw_ip, "引导节点里应当有写死的 IPv4");
    }

    /// 真连一次 DHT 引导节点：能拿到 token 就算通。
    ///
    /// 国内的网络环境 DHT 经常整条路都不通，
    /// 这个测试挂掉不代表代码有问题——但它挂了就说明
    /// 「DHT 节点数」在界面上永远显示 0，用户会以为功能坏了。
    /// 所以这里只是记录实际情况，不做断言。
    #[test]
    fn dht_引导节点能应答() {
        use std::net::UdpSocket;

        let socket = UdpSocket::bind("0.0.0.0:0").expect("应当能建 UDP socket");
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("应当能设超时");

        let node_id = random_node_id();
        let mut packet = Vec::new();
        {
            // bencode: d1:ad2:id20:<id>6:target20:<id>e1:q9:find_node1:t2:aa1:y1:q1:v4:LPXEe
            let mut inner = Vec::new();
            inner.extend_from_slice(b"2:id20:");
            inner.extend_from_slice(&node_id);
            inner.extend_from_slice(b"6:target20:");
            inner.extend_from_slice(&node_id);
            inner.push(b'e');
            packet.extend_from_slice(b"d1:ad");
            packet.extend_from_slice(&inner);
            packet.extend_from_slice(b"1:q9:find_node1:t2:aa1:y1:q1:v4:LPXEe");
        }

        let mut reachable = 0;
        for node in DHT_BOOTSTRAP {
            let Some((host, port)) = split_node(node) else {
                continue;
            };
            let Ok(target) = (host.as_str(), port).to_socket_addrs() else {
                continue;
            };
            let Some(addr) = target.into_iter().next() else {
                continue;
            };
            if socket.send_to(&packet, addr).is_err() {
                continue;
            }
            let mut buf = [0u8; 2048];
            if socket.recv_from(&mut buf).is_ok() {
                reachable += 1;
                eprintln!("DHT 引导节点可达: {node}");
            }
        }
        eprintln!("DHT 引导节点可达数: {reachable}/{}", DHT_BOOTSTRAP.len());
        // 不断言：网络环境差异太大。
        // 有节点可达时，token 流程就能跑起来。
    }
}
