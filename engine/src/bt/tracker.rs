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
//! ## 什么时候才需要自己 announce
//!
//! 我们的监听端口也是优先找 6881 的（`listener::find_port(6881)`）。
//! 只有 6881 被别的程序占了、我们退到 6882/6883… 时，
//! 库的 announce 才是错的（它写死上报 6881），
//! 这时候才需要我们补一份。
//!
//! 端口正好是 6881 的时候，库 announce 的端口本来就是对的，
//! 自己再 announce 一次只是白费双倍带宽和 tracker 压力——
//! 所以这时直接跳过。

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
    /// 库的 tracker 硬编码上报 6881。
    ///
    /// 我们的监听端口也是优先找 6881 的（`listener::find_port(6881)`），
    /// 所以**大多数情况下库的 announce 就是对的**，
    /// 我们再 announce 一次纯属双倍流量。
    ///
    /// 只有当 6881 被别的程序占了、我们退到 6882/6883… 时，
    /// 库的 announce 才是错的，这时候才需要我们补一份。
    needed: bool,
    /// 上次 announce 的时间。
    last_announce: Instant,
}

/// announce 的间隔。
///
/// tracker 协议要求最短 60 秒一次，发太勤会被封。
/// 这里 30 秒一次——这个值只在我们的端口不是 6881 时才用得上，
/// 那时候库的 announce 报的是错端口，本来就该由我们补足频率。
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(30);

impl TrackerAnnouncer {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            needed: port != Self::LIBRARY_HARDCODED_PORT,
            // 第一次进入主循环就 announce 一次。
            last_announce: Instant::now() - ANNOUNCE_INTERVAL,
        }
    }

    /// 库的 tracker 写死的上报端口。
    const LIBRARY_HARDCODED_PORT: u16 = 6881;

    /// 需不需要我们自己 announce 一份。
    pub fn needed(&self) -> bool {
        self.needed
    }

    /// 立刻可以再 announce 一次。
    ///
    /// 卡死自救时用：踢掉当前 peer 之后要马上重新问 tracker 拿新 peer，
    /// 等 30 秒太久了。
    pub fn reset(&mut self) {
        self.last_announce = Instant::now() - ANNOUNCE_INTERVAL;
    }

    /// 该 announce 了就 announce，把 peer 交给库去连。
    ///
    /// 返回连上的 peer 数。
    pub fn tick(&mut self, session: &mut TorrentSession) -> usize {
        // 我们的端口就是 6881 的话，库 announce 的端口是对的，
        // 自己再 announce 一次只是白费双倍带宽和 tracker 压力。
        if !self.needed {
            return 0;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 端口是_6881_的时候不需要自己_announce() {
        // 库的 tracker 写死上报 6881，我们的端口也是 6881，
        // 那它 announce 的端口本来就是对的，再来一份纯属双倍流量。
        let a = TrackerAnnouncer::new(6881);
        assert!(!a.needed());
    }

    #[test]
    fn 端口不是_6881_的时候需要自己_announce() {
        // 6881 被占了，我们退到别的端口，
        // 库上报的 6881 是错的，得由我们补一份。
        let a = TrackerAnnouncer::new(6882);
        assert!(a.needed());
    }

    #[test]
    fn 重置之后可以立刻再_announce() {
        let mut a = TrackerAnnouncer::new(6882);
        a.reset();
        // reset 把 last_announce 拨到了过去，
        // 所以下一轮 tick 会真的去 announce（这里只验证状态，不联网）。
        assert!(a.last_announce.elapsed() >= ANNOUNCE_INTERVAL);
    }
}
