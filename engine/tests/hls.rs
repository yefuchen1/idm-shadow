//! HLS 端到端测试：真的起一个 m3u8 服务器，下载并合并。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use aes::cipher::{BlockEncryptMut, KeyIvInit};
use idm_shadow_engine::http::engine::{DownloadConfig, EngineCtx, HttpEngine};
use idm_shadow_engine::hls::crypto::decrypt_aes128;
use idm_shadow_engine::store::Store;
use idm_shadow_engine::task::{EngineKind, Task, TaskRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
/// 一个 m3u8 服务器。路由：
///   /playlist.m3u8  播放列表
///   /seg0.ts ...   分片
///   /key.bin       密钥
async fn serve_m3u8(
    listener: TcpListener,
    segments: Arc<Vec<Vec<u8>>>,
    key: Option<Arc<Vec<u8>>>,
    encrypted: bool,
) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let segments = segments.clone();
        let key = key.clone();

        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let Ok(n) = socket.read(&mut tmp).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }

            let request = String::from_utf8_lossy(&buf).to_string();
            let path = request
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();

            if path.ends_with(".m3u8") {
                let mut playlist =
                    String::from("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PLAYLIST-TYPE:VOD\n");
                if encrypted {
                    playlist.push_str("#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n");
                }
                for i in 0..segments.len() {
                    playlist.push_str("#EXTINF:4.0,\n");
                    playlist.push_str(&format!("seg{i}.ts\n"));
                }
                playlist.push_str("#EXT-X-ENDLIST\n");
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    playlist.len(),
                    playlist
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                return;
            }

            if path.ends_with("key.bin") {
                let Some(k) = key else {
                    let _ = socket
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    k.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.write_all(&k).await;
                return;
            }

            let idx: usize = path
                .trim_start_matches("/seg")
                .trim_end_matches(".ts")
                .parse()
                .unwrap_or(0);
            let Some(data) = segments.get(idx) else {
                let _ = socket
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await;
                return;
            };

            let payload = if encrypted {
                let k = key.clone().unwrap();
                let iv = idm_shadow_engine::hls::crypto::iv_from_sequence(idx as u64);
                let mut buf = vec![0u8; data.len() + 16];
                buf[..data.len()].copy_from_slice(data);
                let buf_len = data.len();
                let enc = Aes128CbcEnc::new_from_slices(&k, &iv).unwrap();
                enc.encrypt_padded_mut::<aes::cipher::block_padding::Pkcs7>(&mut buf, buf_len)
                    .unwrap()
                    .to_vec()
            } else {
                data.clone()
            };

            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.write_all(&payload).await;
        });
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "idm-shadow-hls-{}-{}-{}",
        tag,
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn run_hls(task: &Task) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let engine = HttpEngine::new(store, DownloadConfig::default()).unwrap();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (progress_tx, mut progress_rx) =
        mpsc::unbounded_channel::<idm_shadow_engine::http::TaskProgress>();
    let (phase_tx, _phase_rx) = mpsc::unbounded_channel::<&'static str>();
    tokio::spawn(async move {
        while progress_rx.recv().await.is_some() {}
    });

    let ctx = EngineCtx::new(cancel_rx, cancel_tx, progress_tx, phase_tx);
    idm_shadow_engine::hls::download_playlist(task, &ctx)
        .await
        .expect("HLS 下载应当成功");
}

#[tokio::test]
async fn downloads_and_merges_plain_playlist() {
    let segments: Vec<Vec<u8>> = (0..3)
        .map(|i| (0..1000u32).map(|j| ((i * 31 + j) % 251) as u8).collect())
        .collect();
    let expected: Vec<u8> = segments.iter().flat_map(|s| s.iter().copied()).collect();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve_m3u8(listener, Arc::new(segments), None, false));

    let dir = temp_dir("plain");
    let task = Task::new(
        Uuid::new_v4(),
        TaskRequest {
            url: format!("http://{addr}/playlist.m3u8"),
            filename: Some("out.mp4".to_string()),
            dir: Some(dir.to_string_lossy().to_string()),
            engine: EngineKind::Hls,
            ..Default::default()
        },
        dir.clone(),
        "out.mp4".to_string(),
    );

    run_hls(&task).await;

    let content = std::fs::read(dir.join("out.mp4")).unwrap();
    assert_eq!(content.len(), expected.len(), "合并后的长度应当一致");
    assert!(content == expected, "合并后的内容必须与分片拼接一致");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn downloads_and_decrypts_aes128_playlist() {
    let plain: Vec<Vec<u8>> = (0..3)
        .map(|i| (0..1000u32).map(|j| ((i * 17 + j) % 251) as u8).collect())
        .collect();
    let expected: Vec<u8> = plain.iter().flat_map(|s| s.iter().copied()).collect();

    let key = Arc::new(vec![0x42u8; 16]);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(serve_m3u8(
        listener,
        Arc::new(plain),
        Some(key.clone()),
        true,
    ));

    let dir = temp_dir("aes");
    let task = Task::new(
        Uuid::new_v4(),
        TaskRequest {
            url: format!("http://{addr}/playlist.m3u8"),
            filename: Some("enc.mp4".to_string()),
            dir: Some(dir.to_string_lossy().to_string()),
            engine: EngineKind::Hls,
            ..Default::default()
        },
        dir.clone(),
        "enc.mp4".to_string(),
    );

    run_hls(&task).await;

    let content = std::fs::read(dir.join("enc.mp4")).unwrap();
    assert_eq!(content.len(), expected.len(), "解密后长度应当一致");
    assert!(content == expected, "解密后的内容必须与原始一致");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn decrypt_helper_matches_reference() {
    let key = [0x42u8; 16];
    let iv = [0u8; 16];
    let plain = b"hls payload for decryption check";
    let mut buf = vec![0u8; plain.len() + 16];
    buf[..plain.len()].copy_from_slice(plain);
    let buf_len = plain.len();
    let enc = Aes128CbcEnc::new_from_slices(&key, &iv).unwrap();
    let ciphered = enc
        .encrypt_padded_mut::<aes::cipher::block_padding::Pkcs7>(&mut buf, buf_len)
        .unwrap()
        .to_vec();
    assert_eq!(decrypt_aes128(&ciphered, &key, &iv).unwrap(), plain);
}

#[test]
fn detects_hls_urls() {
    assert!(idm_shadow_engine::manager::looks_like_hls(
        "http://a.com/v/index.m3u8"
    ));
    assert!(idm_shadow_engine::manager::looks_like_hls(
        "http://a.com/v/index.m3u8?token=1"
    ));
    assert!(!idm_shadow_engine::manager::looks_like_hls(
        "http://a.com/v/file.mp4"
    ));
    // 查询参数里带 m3u8 不算
    assert!(!idm_shadow_engine::manager::looks_like_hls(
        "http://a.com/play?src=index.m3u8"
    ));
}
