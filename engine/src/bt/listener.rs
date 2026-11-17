//! 入站 peer 监听。
//!
//! `bittorrent-rs` 只做主动连接，没有入站 TCP 监听。
//! 对 NAT 后面的用户影响很实际：别人连不上我们，只能我们主动去连别人。
//! 这个模块补上入站这一半——自己起一个监听，接受别人连过来的连接。

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

/// 入站监听的配置。
#[derive(Debug, Clone)]
pub struct InboundConfig {
    /// 监听的端口。
    pub port: u16,
    /// 是否启用。
    pub enabled: bool,
}

impl Default for InboundConfig {
    fn default() -> Self {
        Self {
            port: 6881,
            enabled: true,
        }
    }
}

/// 起一个入站监听。
///
/// 收到的连接会被丢进 `handler`，由调用方决定怎么处理。
/// 这里只负责"接受连接"这一件事，协议细节交给上层。
pub fn spawn_inbound_listener<F>(
    config: &InboundConfig,
    handler: F,
) -> Result<Option<std::thread::JoinHandle<()>>>
where
    F: Fn(std::net::TcpStream, String) + Send + Sync + 'static,
{
    if !config.enabled {
        return Ok(None);
    }

    let listener = TcpListener::bind(("0.0.0.0", config.port))
        .with_context(|| format!("监听端口 {} 失败", config.port))?;

    let actual_port = listener.local_addr().map(|a| a.port()).unwrap_or(config.port);

    listener.set_nonblocking(true).context("设置非阻塞失败")?;

    let handler = Arc::new(handler);
    let handle = std::thread::Builder::new()
        .name("idm-shadow-bt-inbound".to_string())
        .spawn(move || {
            tracing::info!(port = actual_port, "BT 入站监听已启动");
            loop {
                match listener.accept() {
                    Ok((stream, addr)) => {
                        let handler = handler.clone();
                        std::thread::spawn(move || {
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                            let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
                            handler(stream, addr.ip().to_string());
                        });
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        // 非阻塞模式下没有连接，歇一下再来
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(err) => {
                        tracing::warn!("接受 BT 连接失败: {err}");
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            }
        })
        .context("启动入站监听线程失败")?;

    Ok(Some(handle))
}

/// 检查某个端口能不能监听。
pub fn port_available(port: u16) -> bool {
    TcpListener::bind(("0.0.0.0", port)).is_ok()
}

/// 找一个能用的端口，从 `start` 开始往后找。
pub fn find_port(start: u16) -> Result<u16> {
    for port in start..start.saturating_add(64) {
        if port_available(port) {
            return Ok(port);
        }
    }
    Err(anyhow!("{start} 之后的端口都被占用了"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_an_available_port() {
        let port = find_port(45000).unwrap();
        assert!((45000..45064).contains(&port));
    }

    #[test]
    fn disabled_listener_returns_none() {
        let cfg = InboundConfig {
            port: 6881,
            enabled: false,
        };
        assert!(spawn_inbound_listener(&cfg, |_, _| {}).unwrap().is_none());
    }
}
