//! 自己接管 tracker 的 announce 循环。
//!
//! ## 为什么要自己来
//!
//! `bittorrent-rs` 的 tracker 有一个硬编码的坑：
//! `Tracker::new()` 里 `port: 6881` 是写死的，
//! `build_announce_context()` 直接用 `self.port` 上报。
//!
//! 也就是说：**不管我们实际监听的是哪个端口，
//! tracker 上报的都是 6881**。
//!
//! 对 NAT 后面的用户来说这是致命的：
//! tracker 告诉别人「这个种子在 6881」，但我们实际监听的是别的端口，
//! 别人连过来就是连接被拒。
//!
//! 端口被 6881 占用时我们会退到 6882、6883……
//! 这时候必须自己 announce，用真实端口上报。
//!
//! ## 怎么做
//!
//! `Tracker` 的 `port` 字段是 pub，直接改就行。
//! 但库在 `start_download()` 内部自己建 tracker 并起 reannounce 循环，
//! 我们改不到那个实例。
//!
//! 所以这里自己起一个循环：
//! 1. `session.tracker()` 拿一个 tracker 实例（新的、干净的）
//! 2. 把 `port` 改成我们实际监听的端口
//! 3. 定期 `announce_once()`，把拿到的 peer 交给 `download_from_peers`
//!
//! 库自己那个 announce 循环也还在跑——两边都在 announce，
//! 只是我们这边的端口是对的。重复 announce 对 tracker 无害，
//! 它只是覆盖一下 peer 列表。

use std::time::{Duration, Instant};

use bittorrent_rs::session::TorrentSession;

/// tracker announce 的节流器。
///
/// 库在 `start_download()` 内部自己建 tracker 并起 announce 循环，
/// 那个实例我们改不到——它上报的端口永远是 6881。
///
/// 所以这里另起一份 announce：端口是对的。
/// 代价是同一个 tracker 会被 announce 两次（库一次、我们一次），
/// 这对 tracker 无害，它只是覆盖一下 peer 列表。
pub struct TrackerAnnouncer {
    /// 实际监听的端口。上报给 tracker 用。
    port: u16,
    /// 上次 announce 的时间。
    last_announce: Instant,
}

/// announce 的间隔。
///
/// tracker 协议要求最短 60 秒一次，发太勤会被封。
/// 这里 30 秒一次——库自己那个循环也在 announce，
/// 两者加起来大致等于正常的 announce 频率。
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(30);

impl TrackerAnnouncer {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            // 第一次进入主循环就 announce 一次。
            last_announce: Instant::now() - ANNOUNCE_INTERVAL,
        }
    }

    /// 该 announce 了就 announce，把 peer 交给库去连。
    ///
    /// 返回连上的 peer 数。
    pub fn tick(&mut self, session: &mut TorrentSession) -> usize {
        if self.last_announce.elapsed() < ANNOUNCE_INTERVAL {
            return 0;
        }
        self.last_announce = Instant::now();

        let Ok(mut tracker) = session.tracker() else {
            return 0;
        };

        // 这一步是全部的意义所在：
        // 库默认上报 6881，这里换成我们实际监听的端口。
        tracker.port = self.port;

        let Ok(response) = tracker.announce_once() else {
            return 0;
        };
        let count = response.peer_list.len();
        if count == 0 {
            return 0;
        }

        // 把 peer 交给库去连。
        //
        // 走 `download_from_peers` 而不是自己起线程：
        // 它会复用库自己的 peer worker、连接数上限和死 peer 黑名单。
        let _ = session.download_from_peers(response.peer_list);
        count
    }
}
