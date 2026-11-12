//! 本地测试服务器：支持 Range 的静态文件服务器。
//!
//! 手动验证下载器时的好用工具：
//!
//!     cargo run -p idm-shadow-engine --example test_server -- <目录> [端口]

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let port: u16 = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(18999);

    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(err) => {
            eprintln!("端口 {port} 绑定失败: {err}");
            std::process::exit(1);
        }
    };

    println!("测试服务器: http://127.0.0.1:{port}/  (根目录: {})", dir.display());

    loop {
        let Ok((mut socket, _)) = listener.accept() else {
            continue;
        };
        let dir = dir.clone();
        std::thread::spawn(move || {
            let _ = handle(socket, &dir);
        });
    }
}

fn handle(mut socket: std::net::TcpStream, dir: &Path) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = socket.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
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

    let name = path.trim_start_matches('/');
    let file = dir.join(name);

    if !file.is_file() {
        socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
        return Ok(());
    }

    let total = std::fs::metadata(&file)?.len();
    let range = request
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("range:"))
        .and_then(|l| l.split_once('='))
        .map(|(_, v)| v.trim().to_string())
        .and_then(|v| {
            let mut parts = v.split('-');
            let start: u64 = parts.next()?.trim().parse().ok()?;
            let end: u64 = parts.next().unwrap_or("").trim().parse().ok()?;
            Some((start, end))
        });

    let mut handle = File::open(&file)?;
    let (start, end) = match range {
        Some((s, e)) => {
            let s = s.min(total.saturating_sub(1));
            let e = if e == 0 { total - 1 } else { e.min(total - 1) };
            (s, e)
        }
        None => (0, total.saturating_sub(1)),
    };

    let length = end - start + 1;
    let header = format!(
        "HTTP/1.1 206 Partial Content\r\n\
         Content-Length: {length}\r\n\
         Content-Range: bytes {start}-{end}/{total}\r\n\
         Accept-Ranges: bytes\r\n\
         ETag: \"test-server\"\r\n\
         Connection: close\r\n\r\n"
    );
    socket.write_all(header.as_bytes())?;

    handle.seek(SeekFrom::Start(start))?;
    let mut remaining = length;
    let mut buf = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let want = std::cmp::min(remaining as usize, buf.len());
        let read = handle.read(&mut buf[..want])?;
        if read == 0 {
            break;
        }
        socket.write_all(&buf[..read])?;
        remaining -= read as u64;
    }
    socket.flush()?;
    Ok(())
}
