//! 卡死检测：BT 下载空转时怎么自救。
//!
//! ## 问题
//!
//! 磁力链接的种子可能早就没人做了。这种情况下会进入一个稳定的死循环：
//!
//! - tracker 返回一批 peer，连上了；
//! - peer 手里没有我们缺的块（`no blocks available`）；
//! - 库把 peer 留着不断 keep-alive，不主动踢掉；
//! - 主循环每 300ms 读一次进度，`speed` 永远是 0，`downloaded` 永远不变。
//!
//! 任务状态一直是「下载中」，但一个字节都下不来。
//! CPU 却实打实地烧着：主循环在转、peer 线程在 keep-alive、
//! DHT 在发 get_peers、库还在往 `debug.log` 里刷日志。
//!
//! 实测两个这样的任务能占掉大半个核心，而且**永远不会自己结束**。
//!
//! ## 策略：先换 peer，再判死
//!
//! 直接判死对用户没用——种子可能只是慢，或者我们连的这批 peer 不行，
//! 但别的 peer 是好的。所以分两步：
//!
//! 1. **换 peer**：一段时间没有任何进展，但 swarm 里还有 peer，
//!    就把当前这批全踢掉，让库重新去连别的。
//!    同时清掉「问过 get_peers」的记录，下一轮重新问 tracker 和 DHT，
//!    拿到新的 peer 列表。
//! 2. **判死**：换了 peer 还是零进展，而且已经没有任何 peer 可换，
//!    才认为这个任务真的没救了，把它标成失败。
//!
//! ## 为什么要留「冷却期」
//!
//! 刚启动的时候本来就可能一个 peer 都连不上（tracker 还没返回、
//! DHT 还在问）。这时候判死是误伤。
//! 所以刚启动的一段时间内只允许换 peer，不允许判死。

use std::time::{Duration, Instant};

/// 连续多久没有任何字节进展，就认为「卡住了」。
///
/// 不能太短：网络抖动、tracker 慢、磁盘偶尔卡一下都会造成短暂停顿。
/// 也不能太长：卡住的任务每多转一秒就多烧一秒 CPU。
/// 90 秒是个折中——比常见的 tracker 重试周期长，比用户失去耐心的阈值短。
pub const STALL_TIMEOUT: Duration = Duration::from_secs(90);

/// 刚启动多久之内不判死。
///
/// 启动初期本来就可能连不上 peer，
/// 这时候因为「没进展」就判死是误伤。
pub const GRACE_PERIOD: Duration = Duration::from_secs(60);

/// 一次「换 peer」之后，至少要隔多久才允许再换。
///
/// 换完立刻再换会变成疯狂重连：踢掉、连上、发现没块、再踢掉……
/// 既拿不到数据，又把 tracker 和 DHT 打爆。
///
/// 注意：换完 peer 之后停滞计时器会重置，所以实际间隔是
/// `STALL_TIMEOUT`（90 秒）——新连上的 peer 也需要时间送数据。
/// 这个值是**兜底下限**：万一以后把 `STALL_TIMEOUT` 调小了，
/// 也不会退化成疯狂重连。
pub const RESCUE_COOLDOWN: Duration = Duration::from_secs(30);

/// 一次判定周期里最多换几次 peer。
///
/// 换太多次说明这个种子确实没救了，继续换只是白烧 CPU。
pub const MAX_RESCUES: u32 = 3;

/// 卡死检测器。
///
/// 主循环每轮调一次 [`StallDetector::observe`]，
/// 拿到要不要动手的指示。
pub struct StallDetector {
    started: Instant,
    /// 上一次有字节进展的时刻。
    last_progress: Instant,
    /// 上一次已经下载的字节数。
    last_downloaded: u64,
    /// 上一次换 peer 的时刻。
    last_rescue: Option<Instant>,
    /// 已经换过几次 peer。
    rescues: u32,
}

impl StallDetector {
    /// 新建一个检测器。
    pub fn new(downloaded: u64) -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last_progress: now,
            last_downloaded: downloaded,
            last_rescue: None,
            rescues: 0,
        }
    }

    /// 这一轮观察到的下载情况。
    pub fn observe(&mut self, downloaded: u64) -> StallAction {
        observe_at(self, Instant::now(), downloaded)
    }

    /// 已经换过几次 peer。
    pub fn rescues(&self) -> u32 {
        self.rescues
    }

    /// 重置「最近一次有进展的时刻」。
    ///
    /// 换完 peer 之后调用：踢掉 peer 再重新连上需要时间，
    /// 这段时间本来就会零进展，不重置的话会立刻又判定一次卡住。
    pub fn reset_progress(&mut self) {
        self.last_progress = Instant::now();
    }
}

/// 检测器的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallAction {
    /// 正常，继续下。
    None,
    /// 卡住了，但还有救：踢掉现在的 peer，换一批。
    Rescue,
    /// 卡死了，而且已经换过多次 peer 没有用。
    GiveUp,
}

/// 判定逻辑。
///
/// 拆成自由函数是为了测试——`Instant::now()` 没法在测试里推进，
/// 把「现在」当参数传进来才能测时间相关的分支。
fn observe_at(detector: &mut StallDetector, now: Instant, downloaded: u64) -> StallAction {
    // 有进展：刷新基准。这是最常见的分支，所以放在最前面。
    if downloaded > detector.last_downloaded {
        detector.last_downloaded = downloaded;
        detector.last_progress = now;
        return StallAction::None;
    }

    // 没进展。看看卡了多久。
    let stalled_for = now.duration_since(detector.last_progress);
    if stalled_for < STALL_TIMEOUT {
        return StallAction::None;
    }

    // 启动初期不判死。
    if now.duration_since(detector.started) < GRACE_PERIOD {
        return StallAction::None;
    }

    // 换过太多次了，说明这个种子没救了。
    if detector.rescues >= MAX_RESCUES {
        return StallAction::GiveUp;
    }

    // 冷却期没过，先不动。
    if let Some(last) = detector.last_rescue {
        if now.duration_since(last) < RESCUE_COOLDOWN {
            return StallAction::None;
        }
    }

    // 准备换 peer。计数加一，重置进展基准。
    detector.rescues += 1;
    detector.last_rescue = Some(now);
    detector.last_progress = now;
    StallAction::Rescue
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在一个虚拟时刻上造一个检测器。
    fn detector_at(now: Instant, downloaded: u64) -> StallDetector {
        StallDetector {
            started: now,
            last_progress: now,
            last_downloaded: downloaded,
            last_rescue: None,
            rescues: 0,
        }
    }

    #[test]
    fn 有进展时不触发任何动作() {
        let base = Instant::now();
        let mut d = detector_at(base, 0);
        assert_eq!(observe_at(&mut d, base, 0), StallAction::None);
        assert_eq!(
            observe_at(&mut d, base + Duration::from_secs(10), 100),
            StallAction::None
        );
        assert_eq!(
            observe_at(&mut d, base + Duration::from_secs(20), 200),
            StallAction::None
        );
        assert_eq!(d.rescues(), 0);
    }

    #[test]
    fn 卡住但还在宽限期内不动() {
        let base = Instant::now();
        let mut d = detector_at(base, 0);
        // 60 秒：卡住了但还在宽限期内。
        assert_eq!(
            observe_at(&mut d, base + Duration::from_secs(60), 0),
            StallAction::None
        );
        // 80 秒：超过宽限期了，但要等 STALL_TIMEOUT。
        assert_eq!(
            observe_at(&mut d, base + Duration::from_secs(80), 0),
            StallAction::None
        );
    }

    #[test]
    fn 卡久了会换_peer() {
        let base = Instant::now();
        let mut d = detector_at(base, 0);
        // 超过宽限期且超过停滞阈值 → 换 peer。
        let action = observe_at(&mut d, base + Duration::from_secs(120), 0);
        assert_eq!(action, StallAction::Rescue);
        assert_eq!(d.rescues(), 1);
    }

    #[test]
    fn 换_peer_之后要重新等满停滞时间() {
        let base = Instant::now();
        let mut d = detector_at(base, 0);
        assert_eq!(
            observe_at(&mut d, base + Duration::from_secs(120), 0),
            StallAction::Rescue
        );
        // 换完之后停滞计时器重置，再观察不动。
        assert_eq!(
            observe_at(&mut d, base + Duration::from_secs(130), 0),
            StallAction::None
        );
        // 还没重新等满 90 秒，还是不动。
        assert_eq!(
            observe_at(&mut d, base + Duration::from_secs(200), 0),
            StallAction::None
        );
        // 等满 90 秒（120 + 90 = 210）之后才能再换。
        assert_eq!(
            observe_at(&mut d, base + Duration::from_secs(215), 0),
            StallAction::Rescue
        );
        assert_eq!(d.rescues(), 2);
    }

    #[test]
    fn 换太多次就放弃() {
        let base = Instant::now();
        let mut d = detector_at(base, 0);
        assert_eq!(
            observe_at(&mut d, base + Duration::from_secs(120), 0),
            StallAction::Rescue
        );
        assert_eq!(
            observe_at(&mut d, base + Duration::from_secs(215), 0),
            StallAction::Rescue
        );
        assert_eq!(
            observe_at(&mut d, base + Duration::from_secs(310), 0),
            StallAction::Rescue
        );
        // 第四次就该放弃了。
        assert_eq!(
            observe_at(&mut d, base + Duration::from_secs(405), 0),
            StallAction::GiveUp
        );
        assert_eq!(d.rescues(), MAX_RESCUES);
    }

    #[test]
    fn 有进展会重置停滞计时() {
        let base = Instant::now();
        let mut d = detector_at(base, 0);
        // 卡了 80 秒，然后突然下了一个字节。
        assert_eq!(
            observe_at(&mut d, base + Duration::from_secs(80), 0),
            StallAction::None
        );
        assert_eq!(
            observe_at(&mut d, base + Duration::from_secs(85), 1),
            StallAction::None
        );
        // 基准被刷新了，所以再过 80 秒也不该触发。
        assert_eq!(
            observe_at(&mut d, base + Duration::from_secs(160), 1),
            StallAction::None
        );
    }

    #[test]
    fn 断点续传时已有字节不算进展() {
        let base = Instant::now();
        // 已经下了 5GB，断点续传。
        let mut d = detector_at(base, 5_000_000_000);
        // 字节数没变 → 视为卡住。
        assert_eq!(
            observe_at(&mut d, base + Duration::from_secs(120), 5_000_000_000),
            StallAction::Rescue
        );
    }
}
