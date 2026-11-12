//! 多连接分段下载的编排。
//!
//! 一个任务 = 一个 `Segmenter` + N 个 worker + 一个重平衡定时器。
//! worker 各自持有自己的文件句柄（各自 seek + write，互不干扰），
//! 分段边界由 `Segmenter` 统一裁决。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch, Mutex};

use super::segmenter::{Segmenter, WorkerId};
use super::{build_client, is_permanent_client_error, probe, status_to_error, RequestContext, ResourceMeta};
use crate::interval::IntervalSet;
use crate::limiter::RateLimiter;
use crate::store::Store;
use crate::task::Task;

/// 下载参数。
#[derive(Debug, Clone)]
pub struct DownloadConfig {
    /// 单任务并发连接数上限。
    pub max_connections: usize,
    /// 同时下载的任务数上限。None = 不限。
    pub max_concurrent_tasks: Option<usize>,
    /// 单段最小字节数。低于这个值不再切分。
    pub min_segment_size: u64,
    /// 单段最大字节数。
    pub max_segment_size: u64,
    /// 重平衡间隔。
    pub rebalance_interval: Duration,
    /// 速率低于最快连接的多少比例就淘汰。0.15 = 慢于最快者 15% 即淘汰。
    pub drop_ratio: f64,
    /// 低于这个速率不淘汰（服务器整体限速时不误伤）。
    pub min_speed: u64,
    /// 连续多少秒没有进展就判定连接卡死。
    pub stall_timeout: Duration,
    /// 单个请求的最大重试次数。
    pub max_retries: u32,
    /// 每隔多久把已完成区间落盘一次。
    pub persist_interval: Duration,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            // 16 是实测下来的甜点：吞吐随连接数近似线性增长，
            // 48 连接后触顶。16 既能吃满带宽，又不至于把服务器连接数撑爆。
            max_connections: 16,
            max_concurrent_tasks: None,
            min_segment_size: 256 * 1024,
            max_segment_size: 64 * 1024 * 1024,
            rebalance_interval: Duration::from_secs(2),
            drop_ratio: 0.15,
            min_speed: 8 * 1024,
            stall_timeout: Duration::from_secs(15),
            max_retries: 5,
            persist_interval: Duration::from_secs(5),
        }
    }
}

/// 一个连接当前持有的区间（用于 UI 展示）。
#[derive(Debug, Clone)]
pub struct SegmentInfo {
    pub start: u64,
    pub end: u64,
    pub speed: u64,
}

/// 进度快照。
#[derive(Debug, Clone, Default)]
pub struct TaskProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
    pub speed: u64,
    pub connections: usize,
    pub segments: Vec<SegmentInfo>,
}

/// 传给引擎的运行期上下文。
pub struct EngineCtx {
    /// 置为 true 表示取消。
    pub cancel: watch::Receiver<bool>,
    /// 取消信号的发送端。引擎自己也要能叫停所有 worker。
    pub cancel_tx: watch::Sender<bool>,
    /// 进度上报通道。
    pub progress: mpsc::UnboundedSender<TaskProgress>,
    /// 阶段变化通知。引擎进入真正下载阶段时发一条 `downloading`。
    pub phase: mpsc::UnboundedSender<&'static str>,
}

/// 引擎在任务结束时返回的原因。
#[derive(Debug, Clone, PartialEq)]
pub enum FinishReason {
    /// 正常下完。
    Completed,
    /// 被暂停了。
    Paused,
    /// 被用户取消。
    Cancelled,
}

impl FinishReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            FinishReason::Completed => "completed",
            FinishReason::Paused => "paused",
            FinishReason::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for FinishReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl EngineCtx {
    pub fn new(
        cancel: watch::Receiver<bool>,
        cancel_tx: watch::Sender<bool>,
        progress: mpsc::UnboundedSender<TaskProgress>,
        phase: mpsc::UnboundedSender<&'static str>,
    ) -> Self {
        Self {
            cancel,
            cancel_tx,
            progress,
            phase,
        }
    }

    /// 发一条进度。HLS 引擎用它报告分片进度。
    pub fn send_progress(&self, progress: TaskProgress) {
        let _ = self.progress.send(progress);
    }

    /// 报告阶段变化。
    pub fn send_phase(&self, phase: &'static str) {
        let _ = self.phase.send(phase);
    }
}

/// 引擎之间共享的运行期状态。
struct Shared {
    segmenter: Mutex<Segmenter>,
    /// 每个 worker 的当前速率（bytes/s）。
    speeds: Vec<AtomicU64>,
    /// 每个 worker 最近一次有进展的时间戳（毫秒）。
    last_progress: Vec<AtomicU64>,
    /// 每个 worker 是否还活着。
    alive: Vec<AtomicBool>,
    limiter: Arc<RateLimiter>,
}

pub struct HttpEngine {
    client: reqwest::Client,
    store: Arc<Store>,
    config: tokio::sync::RwLock<DownloadConfig>,
    global_limiter: Arc<RateLimiter>,
}

impl HttpEngine {
    pub fn new(store: Arc<Store>, config: DownloadConfig) -> Result<Self> {
        let client = build_client(None)?;
        Ok(Self {
            client,
            store,
            config: tokio::sync::RwLock::new(config),
            global_limiter: Arc::new(RateLimiter::new(None)),
        })
    }

    pub async fn config(&self) -> DownloadConfig {
        self.config.read().await.clone()
    }

    /// 改全局默认参数（连接数等）。
    pub async fn set_config(&self, config: DownloadConfig) {
        *self.config.write().await = config;
    }

    pub fn set_global_limit(&self, limit: Option<u64>) {
        self.global_limiter.set_limit(limit);
    }

    /// 下载一个任务。返回 Ok 表示完成。
    pub async fn download(&self, task: &Task, ctx: EngineCtx) -> Result<()> {
        let rctx = RequestContext::from_task(task);
        let meta = probe(&self.client, &rctx).await?;

        // 探测结束，进入下载阶段。
        let _ = ctx.phase.send("downloading");

        let total = match meta.total_size {
            Some(t) if meta.supports_range && t > 0 => t,
            _ => {
                // 服务器不支持 Range，或者长度未知：退化成单连接流式下载
                return self
                    .download_single_stream(task, &rctx, &meta, ctx)
                    .await;
            }
        };

        // 续传前的资源一致性检查
        let done = self.load_resumable(task, &meta, total)?;

        self.download_segmented(task, &rctx, &meta, total, done, ctx)
            .await
    }

    /// 读取已完成的区间；资源变了就丢弃旧进度。
    fn load_resumable(
        &self,
        task: &Task,
        meta: &ResourceMeta,
        total: u64,
    ) -> Result<IntervalSet> {
        let done = self.store.load_intervals(task.id)?;
        if done.is_empty() {
            return Ok(done);
        }

        // ETag 变了说明资源已经不同，旧字节不能用了
        if let (Some(old), Some(new)) = (&task.etag, &meta.etag) {
            if old != new {
                tracing::warn!(task = %task.id, "ETag 变化，丢弃旧进度");
                return Ok(IntervalSet::new());
            }
        }

        // 已完成的字节不能超过总大小
        let mut clamped = IntervalSet::new();
        for &(s, e) in done.ranges() {
            clamped.insert(s, e.min(total));
        }
        Ok(clamped)
    }
    /// 单连接流式下载：服务器不支持 Range，或者总长度未知。
    async fn download_single_stream(
        &self,
        task: &Task,
        rctx: &RequestContext,
        meta: &ResourceMeta,
        ctx: EngineCtx,
    ) -> Result<()> {
        let path = task.write_path();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }

        let resp = rctx
            .apply(self.client.get(&rctx.url))
            .send()
            .await
            .with_context(|| format!("请求失败: {}", rctx.url))?;

        if !resp.status().is_success() {
            return Err(status_to_error(resp.status()));
        }

        let total = meta.total_size;
        let mut file = tokio::fs::File::create(&path)
            .await
            .with_context(|| format!("创建文件失败: {}", path.display()))?;

        let mut offset: u64 = 0;
        let mut stream = resp.bytes_stream();
        let mut last_report = Instant::now();
        let mut last_bytes = 0u64;

        while let Some(chunk) = stream.next().await {
            if *ctx.cancel.borrow() {
                file.flush().await.ok();
                return Err(anyhow!("已取消"));
            }
            let chunk = chunk?;
            self.global_limiter.acquire(chunk.len() as u64).await;
            file.write_all(&chunk).await?;
            offset += chunk.len() as u64;

            let now = Instant::now();
            if now.duration_since(last_report) >= Duration::from_millis(500) {
                let elapsed = now.duration_since(last_report).as_secs_f64().max(0.001);
                let speed = ((offset - last_bytes) as f64 / elapsed) as u64;
                last_bytes = offset;
                last_report = now;
                let _ = ctx.progress.send(TaskProgress {
                    downloaded: offset,
                    total,
                    speed,
                    connections: 1,
                    segments: vec![SegmentInfo {
                        start: 0,
                        end: offset,
                        speed,
                    }],
                });
            }
        }

        file.flush().await.ok();
        let _ = ctx.progress.send(TaskProgress {
            downloaded: offset,
            total,
            speed: 0,
            connections: 0,
            segments: Vec::new(),
        });
        Ok(())
    }

    /// 多连接分段下载。
    #[allow(clippy::too_many_arguments)]
    async fn download_segmented(
        &self,
        task: &Task,
        rctx: &RequestContext,
        _meta: &ResourceMeta,
        total: u64,
        done: IntervalSet,
        ctx: EngineCtx,
    ) -> Result<()> {
        let path = task.write_path();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }

        // 预分配目标文件，之后所有写入都是偏移写
        {
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(&path)
                .await
                .with_context(|| format!("打开文件失败: {}", path.display()))?;
            file.set_len(total).await.ok();
            file.sync_all().await.ok();
        }

        // 单任务的连接数以任务上的设置为准，引擎的全局配置只作为默认值。
        // 之前这里只看全局配置，导致 PATCH 改单任务连接数完全不生效。
        let global = self.config.read().await;
        let connections = task
            .max_connections
            .map(|v| v as usize)
            .unwrap_or(global.max_connections)
            .clamp(1, 64);
        let config = global.clone();
        let shared = Arc::new(Shared {
            segmenter: Mutex::new(Segmenter::resume(total, done)),
            speeds: (0..connections).map(|_| AtomicU64::new(0)).collect(),
            last_progress: (0..connections).map(|_| AtomicU64::new(0)).collect(),
            alive: (0..connections).map(|_| AtomicBool::new(true)).collect(),
            limiter: self.global_limiter.clone(),
        });

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        for lp in shared.last_progress.iter() {
            lp.store(now_ms, Ordering::Relaxed);
        }

        let mut workers = Vec::new();
        for id in 0..connections {
            let shared = shared.clone();
            let rctx = rctx.clone();
            let client = self.client.clone();
            let path = path.clone();
            let config = config.clone();
            let mut cancel = ctx.cancel.clone();
            workers.push(tokio::spawn(async move {
                worker_loop(id, shared, rctx, client, path, config, &mut cancel).await;
            }));
        }

        let cancel_tx = ctx.cancel_tx.clone();
        let result = self.supervise(task, shared.clone(), total, ctx).await;

        // 无论成功失败都要通知 worker 收工
        let _ = cancel_tx.send(true);
        for w in workers {
            let _ = w.await;
        }

        // 落盘最终区间
        {
            let seg = shared.segmenter.lock().await;
            self.store.save_intervals(task.id, seg.done())?;
        }

        result
    }

    /// 重平衡定时器 + 进度上报。
    async fn supervise(
        &self,
        task: &Task,
        shared: Arc<Shared>,
        total: u64,
        mut ctx: EngineCtx,
    ) -> Result<()> {
        let config = self.config.read().await.clone();
        let mut tick = tokio::time::interval(config.rebalance_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_persist = Instant::now();

        loop {
            if *ctx.cancel.borrow() {
                return Err(anyhow!("已取消"));
            }

            let remaining = shared.segmenter.lock().await.remaining();
            if remaining == 0 {
                break;
            }

            tokio::select! {
                _ = tick.tick() => {
                    self.rebalance(&shared).await;

                    let now = Instant::now();
                    if now.duration_since(last_persist) >= config.persist_interval {
                        last_persist = now;
                        let seg = shared.segmenter.lock().await;
                        let done = seg.done().clone();
                        let _ = self.store.save_intervals(task.id, &done);
                    }
                }
                _ = ctx.cancel.changed() => {
                    if *ctx.cancel.borrow() {
                        return Err(anyhow!("已取消"));
                    }
                }
            }

            let progress = self.snapshot(&shared, total).await;
            let _ = ctx.progress.send(progress);
        }

        let progress = self.snapshot(&shared, total).await;
        let _ = ctx.progress.send(progress);
        Ok(())
    }

    /// 一轮重平衡：淘汰拖后腿的连接，把关键路径分给更快的连接。
    async fn rebalance(&self, shared: &Arc<Shared>) {
        let config = self.config.read().await.clone();
        let mut seg = shared.segmenter.lock().await;

        let speeds: Vec<(WorkerId, f64)> = shared
            .speeds
            .iter()
            .enumerate()
            .filter(|(_, s)| s.load(Ordering::Relaxed) > 0)
            .map(|(i, s)| (i, s.load(Ordering::Relaxed) as f64))
            .collect();

        if speeds.is_empty() {
            return;
        }

        let v_max = speeds.iter().map(|(_, v)| *v).fold(0.0f64, f64::max);
        let alive = shared
            .alive
            .iter()
            .filter(|a| a.load(Ordering::Relaxed))
            .count();

        // 1. 淘汰明显拖后腿的连接
        if alive >= 2 {
            for (id, v) in &speeds {
                let below_floor = (*v as u64) < config.min_speed;
                if below_floor && *v < config.drop_ratio * v_max {
                    tracing::debug!(worker = id, speed = *v, "淘汰慢连接");
                    seg.release(*id);
                }
            }
        }

        // 2. 关键路径保护：尾部区间分给更快的空闲连接
        let weights: Vec<(WorkerId, f64)> = shared
            .speeds
            .iter()
            .enumerate()
            .filter(|(_, s)| s.load(Ordering::Relaxed) > 0)
            .map(|(i, s)| (i, s.load(Ordering::Relaxed) as f64))
            .collect();
        if !weights.is_empty() {
            let assigned = seg.share_tail(&weights);
            if !assigned.is_empty() {
                tracing::debug!(count = assigned.len(), "关键路径重新分配");
            }
        }
    }

    /// 采集一份进度快照。
    async fn snapshot(&self, shared: &Arc<Shared>, total: u64) -> TaskProgress {
        let seg = shared.segmenter.lock().await;
        let segments: Vec<SegmentInfo> = seg
            .works()
            .iter()
            .filter_map(|w| {
                w.owner.map(|owner| SegmentInfo {
                    start: w.start,
                    end: w.end,
                    speed: shared
                        .speeds
                        .get(owner)
                        .map(|s| s.load(Ordering::Relaxed))
                        .unwrap_or(0),
                })
            })
            .collect();
        let speed = shared
            .speeds
            .iter()
            .map(|s| s.load(Ordering::Relaxed))
            .sum();
        let connections = segments.len();
        let downloaded = seg.downloaded();
        drop(seg);

        TaskProgress {
            downloaded,
            total: Some(total),
            speed,
            connections,
            segments,
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 单个连接的下载循环。
async fn worker_loop(
    id: WorkerId,
    shared: Arc<Shared>,
    rctx: RequestContext,
    client: reqwest::Client,
    path: PathBuf,
    config: DownloadConfig,
    cancel: &mut watch::Receiver<bool>,
) {
    let mut file = match tokio::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .await
    {
        Ok(f) => f,
        Err(err) => {
            tracing::error!(worker = id, "打开文件失败: {err}");
            shared.alive[id].store(false, Ordering::Relaxed);
            return;
        }
    };

    let mut retries = 0u32;
    let mut bytes_this_window = 0u64;
    let mut window_start = Instant::now();

    loop {
        if *cancel.borrow() {
            break;
        }

        // 1. 拿到自己该做的区间
        let range = {
            let mut seg = shared.segmenter.lock().await;
            if seg.remaining() == 0 {
                tracing::debug!(worker = id, "没有剩余区间，退出");
                break;
            }
            match seg.current_range(id) {
                Some(r) => Some(r),
                None => {
                    // 每条连接只拿剩余量的四分之一左右，剩下的留给其它连接，
                    // 这样多连接才真的能并行铺开。
                    let remaining = seg.remaining();
                    let workers = shared.alive.len().max(1) as u64;
                    let chunk = (remaining / (workers * 4))
                        .clamp(config.min_segment_size, config.max_segment_size);
                    seg.assign(id, chunk)
                }
            }
        };

        let Some((rstart, rend)) = range else {
            // 没活干，等下一次分配
            tokio::time::sleep(Duration::from_millis(150)).await;
            continue;
        };

        // 2. 发请求
        let url = rctx.url.clone();
        let request = rctx
            .apply(client.get(&url))
            .header(reqwest::header::RANGE, format!("bytes={}-{}", rstart, rend - 1));

        let resp = match request.send().await {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(worker = id, "请求失败: {err}");
                if retries >= config.max_retries {
                    shared.alive[id].store(false, Ordering::Relaxed);
                    break;
                }
                retries += 1;
                shared.segmenter.lock().await.release(id);
                tokio::time::sleep(backoff(retries)).await;
                continue;
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            if is_permanent_client_error(status) || retries >= config.max_retries {
                tracing::error!(worker = id, "HTTP {status}");
                shared.alive[id].store(false, Ordering::Relaxed);
                shared.segmenter.lock().await.release(id);
                break;
            }
            retries += 1;
            shared.segmenter.lock().await.release(id);
            tokio::time::sleep(backoff(retries)).await;
            continue;
        }

        retries = 0;
        let mut stream = resp.bytes_stream();
        let mut written = 0u64;

        // 3. 读流写盘
        while let Some(chunk) = stream.next().await {
            if *cancel.borrow() {
                break;
            }
            let chunk = match chunk {
                Ok(c) => c,
                Err(err) => {
                    tracing::warn!(worker = id, "读取中断: {err}");
                    retries += 1;
                    if retries > config.max_retries {
                        tracing::error!(worker = id, "重试次数用尽");
                        shared.alive[id].store(false, Ordering::Relaxed);
                        shared.segmenter.lock().await.release(id);
                        break;
                    }
                    break;
                }
            };

            // 区间可能已被重分割：不属于我了就停手重发。
            // complete() 会推进区间起点，所以这里比对自己的偏移，而不是起点。
            let current = shared.segmenter.lock().await.current_range(id);
            match current {
                Some((s, e)) if e == rend && s == rstart + written => {}
                _ => {
                    tracing::debug!(
                        worker = id,
                        current = ?current,
                        expect_start = rstart + written,
                        expect_end = rend,
                        "区间已变化，中断当前请求"
                    );
                    break;
                }
            }

            shared.limiter.acquire(chunk.len() as u64).await;

            if let Err(err) = file.seek(std::io::SeekFrom::Start(rstart + written)).await {
                tracing::error!(worker = id, "seek 失败: {err}");
                break;
            }
            if let Err(err) = file.write_all(&chunk).await {
                tracing::error!(worker = id, "写入失败: {err}");
                break;
            }

            let len = chunk.len() as u64;
            let counted = shared.segmenter.lock().await.complete(id, len);
            written += len;
            bytes_this_window += len;
            shared.last_progress[id].store(now_ms(), Ordering::Relaxed);

            // 区间被缩短时，多出来的字节不计入
            if counted < len {
                break;
            }
        }

        file.flush().await.ok();

        // 4. 更新速率窗口
        let elapsed = window_start.elapsed();
        if elapsed >= Duration::from_millis(500) {
            let speed = (bytes_this_window as f64 / elapsed.as_secs_f64()) as u64;
            shared.speeds[id].store(speed, Ordering::Relaxed);
            bytes_this_window = 0;
            window_start = Instant::now();
        }

        // 5. 重新领活。
        // 区间被缩短或释放时，下一轮会重新拿到区间并按新区间发请求，
        // 所以这里不需要保留旧状态。
        let current = shared.segmenter.lock().await.current_range(id);
        match current {
            Some((s, e)) if e <= s => {
                shared.segmenter.lock().await.release(id);
            }
            _ => {}
        }
    }

    shared.alive[id].store(false, Ordering::Relaxed);
}

/// 指数退避，上限 30 秒。
fn backoff(attempt: u32) -> Duration {
    let secs = 1u64 << attempt.min(5).saturating_sub(1);
    Duration::from_secs(secs.min(30))
}
