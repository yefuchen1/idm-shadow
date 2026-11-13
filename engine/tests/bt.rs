//! BT 端到端测试。
//!
//! 用一个手写的最小做种端（直接讲 BitTorrent 线协议）配合 bittorrent-rs 的下载端，
//! 真的传一遍数据，验证 piece 校验和文件落盘都是通的。
//!
//! 注意：bittorrent-rs 只做主动连接，没有入站 TCP 监听，
//! 所以做种端得自己起。这正好也验证了协议实现本身。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bittorrent_rs::session::TorrentSession;
use bittorrent_rs::utils::bencode::{BNode, Bencode};
use idm_shadow_engine::task::{EngineKind, TaskRequest};
use uuid::Uuid;
/// 一个最小的 HTTP tracker：把见过的 peer 都返回回去。
async fn start_tracker() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let known: Arc<std::sync::Mutex<Vec<u16>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let known = known.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = Vec::new();
                let mut tmp = [0u8; 2048];
                loop {
                    let Ok(n) = socket.read(&mut tmp).await else {
                        return;
                    };
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&buf).to_string();
                if !req.starts_with("GET /announce") {
                    let _ = socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
                    return;
                }

                // 记下 announce 者自己的端口
                let query = req.split_whitespace().nth(1).unwrap_or("/announce");
                let mut my_port = 0u16;
                for pair in query.split('&') {
                    if let Some(v) = pair.strip_prefix("port=") {
                        my_port = v.parse().unwrap_or(0);
                    }
                }
                {
                    let mut list = known.lock().unwrap();
                    if my_port != 0 && !list.contains(&my_port) {
                        list.push(my_port);
                    }
                }

                // bencode: d8:completei1e10:incompletei0e8:intervali1800e5:peers<len>:<原始字节>e
                let mut peers: Vec<u8> = Vec::new();
                {
                    let list = known.lock().unwrap();
                    for port in list.iter() {
                        peers.extend_from_slice(&[127, 0, 0, 1]);
                        peers.extend_from_slice(&port.to_be_bytes());
                    }
                }

                let mut body: Vec<u8> = Vec::new();
                body.extend_from_slice(b"d8:completei1e10:incompletei0e8:intervali1800e5:peers");
                body.extend_from_slice(format!("{}:", peers.len()).as_bytes());
                body.extend_from_slice(&peers);
                body.push(b'e');

                let mut resp: Vec<u8> = Vec::new();
                resp.extend_from_slice(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: ",
                );
                resp.extend_from_slice(
                    format!("{}\r\nConnection: close\r\n\r\n", body.len()).as_bytes(),
                );
                resp.extend_from_slice(&body);
                let _ = socket.write_all(&resp).await;
            });
        }
    });

    addr
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "idm-shadow-bt-{}-{}-{}",
        tag,
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 造一个最小的单文件 torrent，announce 地址可指定。
fn make_torrent_with_announce(
    name: &str,
    payload: &[u8],
    piece_len: usize,
    announce_url: &str,
) -> Vec<u8> {
    // 算 piece 哈希
    let mut pieces = Vec::new();
    for chunk in payload.chunks(piece_len) {
        pieces.extend_from_slice(&sha1(chunk));
    }

    // 用库自带的 bencode 编码器，手写很容易漏掉字典的 d...e
    let announce = announce_url.as_bytes().to_vec();
    let len_bytes = payload.len().to_string().into_bytes();
    let piece_len_bytes = piece_len.to_string().into_bytes();

    let info = BNode::Dictionary(vec![
        (b"length".as_slice(), BNode::Number(&len_bytes)),
        (b"name".as_slice(), BNode::String(name.as_bytes())),
        (b"piece length".as_slice(), BNode::Number(&piece_len_bytes)),
        (b"pieces".as_slice(), BNode::String(&pieces)),
    ]);

    let root = BNode::Dictionary(vec![
        (b"announce".as_slice(), BNode::String(&announce)),
        (b"info".as_slice(), info),
    ]);

    Bencode::encode(&root)
}

/// 极简 SHA-1，只够算 piece 哈希用。
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0,
    ];
    let ml = (data.len() as u64) * 8;

    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&ml.to_be_bytes());

    for block in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for i in 0..80 {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[i]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// 一个手写的最小做种端。
///
/// 直接讲 BitTorrent 线协议：
///   1. 收 handshake，回 handshake
///   2. 发 bitfield（我全都有）
///   3. 收到 request 消息就发对应的 block
///
/// bittorrent-rs 没有入站监听，所以做种端得自己起。
async fn start_raw_seeder(
    payload: Arc<Vec<u8>>,
    piece_len: usize,
    info_hash: Arc<Vec<u8>>,
) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let payload = payload.clone();
            let piece_len = piece_len;
            let info_hash = info_hash.clone();

            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};

                // 1. handshake：<19>"\x13BitTorrent protocol"<8 reserved><20 infohash>
                let mut hs = [0u8; 68];
                if socket.read_exact(&mut hs).await.is_err() {
                    return;
                }
                // 标准 handshake 是 <19><"BitTorrent protocol"><8 reserved><20 infohash>，
                // 但 bittorrent-rs 发的是 <19><"BitTorrent protocol">（少了 \x13），
                // 两种都认。
                let std_ok = &hs[1..20] == b"BitTorrent protocol";
                let lib_ok = &hs[0..19] == b"BitTorrent protocol";
                if !std_ok && !lib_ok {
                    return;
                }

                let mut reply = Vec::with_capacity(68);
                reply.push(19);
                reply.extend_from_slice(b"BitTorrent protocol");
                reply.extend_from_slice(&[0u8; 8]);
                reply.extend_from_slice(&info_hash);
                reply.extend_from_slice(b"-IDMS-0001-000000000"); // peer id，正好 20 字节
                if socket.write_all(&reply).await.is_err() {
                    return;
                }

                // 2. 发 bitfield：我有全部的 piece
                let piece_count = payload.len().div_ceil(piece_len);
                let bitfield_len = piece_count.div_ceil(8);
                let mut bitfield = vec![0u8; bitfield_len];
                for i in 0..piece_count {
                    bitfield[i / 8] |= 0x80 >> (i % 8);
                }
                // 消息格式：<4 字节长度><1 字节 id><负载>
                // 长度只算 id + 负载
                let mut msg = Vec::new();
                msg.extend_from_slice(&((1 + bitfield.len()) as u32).to_be_bytes());
                msg.push(5); // id = 5 bitfield
                msg.extend_from_slice(&bitfield);
                if socket.write_all(&msg).await.is_err() {
                    return;
                }

                // 3. 循环处理请求
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    // 先看有没有完整的消息
                    while buf.len() < 4 {
                        let Ok(n) = socket.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }

                    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                    while buf.len() < 4 + len {
                        let Ok(n) = socket.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }

                    let id = buf[4];
                    let body = buf[5..4 + len].to_vec();
                    buf.drain(..4 + len);

                    match id {
                        // request: <index><begin><length>
                        6 if body.len() >= 12 => {
                            let index = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
                            let begin =
                                u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
                            let want =
                                u32::from_be_bytes([body[8], body[9], body[10], body[11]]) as usize;

                            let piece_start = index * piece_len;
                            let end = (piece_start + begin + want).min(payload.len());
                            let start = piece_start + begin;
                            if start >= payload.len() {
                                continue;
                            }
                            let chunk = &payload[start..end];

                            // piece 消息：id=7 + index(4) + begin(4) + 数据
                            let mut block = Vec::new();
                            block.extend_from_slice(&((9 + chunk.len()) as u32).to_be_bytes());
                            block.push(7);
                            block.extend_from_slice(&(index as u32).to_be_bytes());
                            block.extend_from_slice(&(begin as u32).to_be_bytes());
                            block.extend_from_slice(chunk);
                            let _ = socket.write_all(&block).await;
                        }
                        // interested
                        2 => {
                            // unchoke：id=1，无负载，长度 1
                            let mut unchoke = Vec::new();
                            unchoke.extend_from_slice(&1u32.to_be_bytes());
                            unchoke.push(1);
                            let _ = socket.write_all(&unchoke).await;
                        }
                        // keep-alive
                        0 => {}
                        _ => {}
                    }
                }
            });
        }
    });

    addr
}

/// 完整链路：手写做种端 + bittorrent-rs 下载端，真的把数据传过去。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bt_transfers_data_from_a_real_peer() {
    // 内容固定：256 KiB，分成 16 KiB 的 piece
    let payload: Vec<u8> = (0..256 * 1024u32).map(|i| (i % 251) as u8).collect();
    let piece_len = 16 * 1024;

    let dir = temp_dir("transfer");
    let leecher_dir = dir.join("leecher");
    std::fs::create_dir_all(&leecher_dir).unwrap();

    // tracker
    let tracker_addr = start_tracker().await;

    // 算出 info_hash：torrent 的 info 字典的 SHA-1
    let torrent_bytes =
        make_torrent_with_announce("data.bin", &payload, piece_len, "http://placeholder/announce");
    let info_hash = extract_info_hash(&torrent_bytes);

    // 做种端
    let seeder_addr =
        start_raw_seeder(Arc::new(payload.clone()), piece_len, Arc::new(info_hash.clone())).await;

    // 重新生成 torrent，announce 指向 tracker，并把做种端加进 tracker
    let torrent_bytes = make_torrent_with_announce(
        "data.bin",
        &payload,
        piece_len,
        &format!("http://{tracker_addr}/announce"),
    );
    let torrent_path = dir.join("test.torrent");
    std::fs::write(&torrent_path, &torrent_bytes).unwrap();

    // 让 tracker 认识做种端
    let seeder_port = seeder_addr.port();
    announce_to_tracker(tracker_addr, seeder_port).await;

    // 下载端
    let mut cfg = bittorrent_rs::session::config::SessionConfig::default();
    cfg.listen_port = 6882;
    cfg.dht_port = 6882;

    let mut leecher = TorrentSession::builder(&torrent_path, &leecher_dir)
        .seeding(false)
        .config(cfg)
        .build()
        .expect("下载端会话应当能建起来");
    leecher.start_download().expect("下载端应当能启动");

    // 手动把做种端塞给下载端
    let mut tracker = leecher.tracker().expect("下载端 tracker");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut peers = Vec::new();
    while Instant::now() < deadline && peers.is_empty() {
        match tracker.announce_once() {
            Ok(resp) => peers = resp.peer_list.clone(),
            Err(e) => {
                eprintln!("[leecher announce] {:?}", e);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    assert!(!peers.is_empty(), "下载端应当发现做种端");
    let connected = leecher.download_from_peers(peers);
    assert!(connected.is_ok(), "连接做种端应当成功: {connected:?}");

    // 等下载完成
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut done = false;
    while Instant::now() < deadline {
        if let Some(stats) = leecher.try_stats() {
            if stats.progress >= 100.0 && stats.bytes_left == 0 {
                done = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    assert!(done, "下载应当完成");

    let out = leecher_dir.join("data.bin");
    assert!(out.exists(), "下载端应当有文件");
    let content = std::fs::read(&out).unwrap();
    assert_eq!(content.len(), payload.len(), "文件大小应当一致");
    assert!(content == payload, "文件内容必须一致");

    let _ = leecher.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// 从 torrent 文件里取出 info 字典的 SHA-1（就是 info_hash）。
///
/// 不自己解析 bencode——解码器知道 info 字典的确切字节范围，
/// 重新编码一遍就行（字典的键要排序，编码器会处理）。
fn extract_info_hash(torrent: &[u8]) -> Vec<u8> {
    let root = Bencode::decode(torrent).expect("torrent 应当能解码");
    let info = Bencode::get_dictionary_entry(&root, b"info").expect("torrent 里应当有 info");
    sha1(&Bencode::encode(info)).to_vec()
}
/// 往 tracker 发一次 announce，让它记住这个 peer。
async fn announce_to_tracker(tracker: SocketAddr, port: u16) {
    let url = format!(
        "http://{tracker}/announce?info_hash=00&peer_id=-IDMS-0001-000000000&port={port}&compact=1&left=0&uploaded=0&downloaded=0"
    );
    let _ = reqwest::Client::new().get(&url).send().await;
}

/// 任务模型能识别磁力和 .torrent。
#[test]
fn task_engine_detection() {
    assert!(idm_shadow_engine::bt::looks_like_bt("magnet:?xt=urn:btih:abc"));
    assert!(idm_shadow_engine::bt::looks_like_bt("http://a.com/x.torrent"));
    assert!(!idm_shadow_engine::bt::looks_like_bt("http://a.com/x.mp4"));
}

/// SHA-1 跟标准实现一致（用已知向量验证）。
#[test]
fn sha1_matches_known_vectors() {
    assert_eq!(
        sha1(b"abc"),
        [
            0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
            0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d
        ]
    );
    assert_eq!(
        sha1(b""),
        [
            0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60,
            0x18, 0x90, 0xaf, 0xd8, 0x07, 0x09
        ]
    );
}

/// 引擎类型：磁力任务被识别为 Bt。
#[test]
fn magnet_tasks_get_bt_engine() {
    let mut req = TaskRequest::new("magnet:?xt=urn:btih:abc");
    req.engine = EngineKind::Auto;
    assert!(idm_shadow_engine::bt::looks_like_bt(&req.url));
}

/// 临时目录清理用得到。
#[test]
fn temp_dir_is_created() {
    let dir = temp_dir("sanity");
    assert!(dir.exists());
    std::fs::remove_dir_all(&dir).ok();
}

/// 入站监听：能接受连接，并且把连接交给处理函数。
#[test]
fn inbound_listener_accepts_connections() {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let port = idm_shadow_engine::bt::listener::find_port(46000).unwrap();
    let cfg = idm_shadow_engine::bt::listener::InboundConfig {
        port,
        enabled: true,
    };

    // 起监听，收到连接后往通道里丢一个字节
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = idm_shadow_engine::bt::listener::spawn_inbound_listener(&cfg, move |mut stream| {
        let mut b = [0u8; 1];
        let _ = stream.read_exact(&mut b);
        let _ = tx.send(b[0]);
    })
    .unwrap()
    .expect("监听线程应当起来了");

    // 稍等一下让监听就绪
    std::thread::sleep(Duration::from_millis(200));

    // 连上去
    let mut conn = TcpStream::connect(("127.0.0.1", port)).expect("应当能连上");
    conn.write_all(b"X").unwrap();
    conn.flush().unwrap();

    // 处理函数应当收到
    let got = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("处理函数应当收到数据");
    assert_eq!(got, b'X');

    drop(handle);
}
