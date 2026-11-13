//! 任务管理器：把存储、引擎、队列调度串起来。
//!
//! 桌面应用和本地 HTTP API 都通过它操作任务，保证两条入口看到同一份状态。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::sync::{broadcast, watch, Mutex};
use uuid::Uuid;

use crate::http::engine::{DownloadConfig, HttpEngine, TaskProgress};
use crate::http::{RequestContext, ResourceMeta};
use crate::limiter::RateLimiter;
use crate::store::Store;
use crate::task::{
    filename_from_content_disposition, filename_from_url, sanitize_filename, unique_path,
    EngineKind, Task, TaskError, TaskId, TaskRequest, TaskStatus,
};

/// 任务状态变化事件。UI 与扩展都靠它订阅。
#[derive(Clone, Debug)]
pub enum TaskEvent {
    Progress(TaskId, TaskProgress),
    StatusChanged(TaskId, TaskStatus),
    /// 有任务在等用户确认保存位置。
    ///
    /// 桌面应用靠这个事件把窗口弹出来——应用常驻托盘，
    /// 窗口是隐藏的，光有状态变化用户是看不到弹窗的。
    ConfirmNeeded(TaskId),
}

struct Running {
    cancel: watch::Sender<bool>,
    /// 用户对当前任务的意图。引擎只知道"被叫停了"，
    /// 是暂停还是取消，得由这里记账。
    intent: Arc<Mutex<Intent>>,
}

/// 一个任务在"等确认"这件事上的状态。
///
/// 它挂在 manager 上而不是 Running 上，因为下载引擎跑完之后
/// Running 条目就被摘掉了，但用户可能这时才点确认——
/// 状态得比引擎活得久。
#[derive(Debug, Default)]
struct ConfirmState {
    /// 用户确认的最终保存位置。
    final_path: Option<PathBuf>,
    /// 引擎是否已经跑完（下载结束）。
    engine_finished: bool,
    /// 暂存文件是否已经转存过。保证只转存一次。
    transferred: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Intent {
    Active,
    Paused,
    Cancelled,
}

#[derive(Clone)]
pub struct DownloadManager {
    inner: Arc<Inner>,
}

/// 共享的内部状态。
struct Inner {
    store: Arc<Store>,
    engine: Arc<HttpEngine>,
    running: Arc<Mutex<HashMap<TaskId, Running>>>,
    /// 正在收尾的任务。收尾期间不接受新的 pause/cancel 意图。
    finishing: Arc<Mutex<HashSet<TaskId>>>,
    /// 实时进度快照，UI 与桥接接口读它拿速度。
    live: Arc<Mutex<HashMap<TaskId, TaskProgress>>>,
    events: broadcast::Sender<TaskEvent>,
    limiter: Arc<RateLimiter>,
    default_dir: tokio::sync::RwLock<PathBuf>,
    /// 同时下载的任务数上限。None = 不限。
    max_concurrent_tasks: Arc<Mutex<Option<usize>>>,
    /// 排队等待的任务。超过并发上限时先进这里。
    pending: Arc<Mutex<VecDeque<TaskId>>>,
    /// 各任务"等确认"的状态。
    ///
    /// 下载结束和用户确认会同时发生，得靠这个把它们串起来，
    /// 否则会出现"文件正被下载引擎写，同时又被转存走"的撕裂。
    confirms: Arc<Mutex<HashMap<TaskId, ConfirmState>>>,
}

impl DownloadManager {
    pub fn new(store: Arc<Store>, config: DownloadConfig) -> Result<Self> {
        let engine = Arc::new(HttpEngine::new(store.clone(), config)?);
        let (events, _) = broadcast::channel(256);
        let default_dir = default_download_dir();
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                engine,
                running: Arc::new(Mutex::new(HashMap::new())),
                finishing: Arc::new(Mutex::new(HashSet::new())),
                live: Arc::new(Mutex::new(HashMap::new())),
                events,
                limiter: Arc::new(RateLimiter::new(None)),
                default_dir: tokio::sync::RwLock::new(default_dir),
                max_concurrent_tasks: Arc::new(Mutex::new(None)),
                pending: Arc::new(Mutex::new(VecDeque::new())),
                confirms: Arc::new(Mutex::new(HashMap::new())),
            }),
        })
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.inner.store
    }

    /// 复制一份管理器句柄，共享同一份内部状态。
    pub fn handle(&self) -> DownloadManager {
        DownloadManager {
            inner: self.inner.clone(),
        }
    }

    pub fn engine(&self) -> &Arc<HttpEngine> {
        &self.inner.engine
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TaskEvent> {
        self.inner.events.subscribe()
    }

    /// 当前的实时进度快照。
    pub async fn live_progress(&self) -> HashMap<TaskId, TaskProgress> {
        self.inner.live.lock().await.clone()
    }

    pub fn set_global_limit(&self, limit: Option<u64>) {
        self.inner.limiter.set_limit(limit);
        self.inner.engine.set_global_limit(limit);
    }

    pub async fn set_default_dir(&self, dir: PathBuf) {
        *self.inner.default_dir.write().await = dir;
    }

    pub async fn default_dir(&self) -> PathBuf {
        self.inner.default_dir.read().await.clone()
    }

    /// 设置同时下载的任务数上限。None = 不限。
    pub async fn set_max_concurrent_tasks(&self, limit: Option<usize>) {
        *self.inner.max_concurrent_tasks.lock().await = limit;
    }

    pub async fn max_concurrent_tasks(&self) -> Option<usize> {
        *self.inner.max_concurrent_tasks.lock().await
    }

    /// 有任务跑完时，把排队里的下一个捞起来。
    async fn pump_queue(&self) {
        let limit = self.max_concurrent_tasks().await;
        let Some(limit) = limit else {
            // 不限并发时，队列里所有任务都该跑起来
            let queued: Vec<TaskId> = self.inner.pending.lock().await.drain(..).collect();
            for id in queued {
                if let Err(err) = self.start(id).await {
                    tracing::warn!(task = %id, "排队任务启动失败: {err}");
                }
            }
            return;
        };

        loop {
            let active = self.inner.running.lock().await.len();
            if active >= limit {
                break;
            }
            let next = self.inner.pending.lock().await.pop_front();
            let Some(id) = next else {
                break;
            };
                if let Err(err) = self.start(id).await {
                tracing::warn!(task = %id, "排队任务启动失败: {err}");
            }
        }
    }

    /// 调度器循环：定期把排队里的任务捞起来。
    ///
    /// 单独一个长驻任务来做这件事，而不是在每个任务结束时再 spawn 一个——
    /// 后者会让 spawned 任务借用它自己持有的管理器，
    /// 那样得到的 future 不是 Send，spawn 不了。
    pub async fn run_scheduler(self: Arc<Self>) {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            self.pump_queue().await;
        }
    }

    /// 立刻把排队里的任务捞起来（测试用）。
    pub async fn pump_queue_for_test(&self) {
        self.pump_queue().await;
    }

    /// 新建任务。落到队列里等待启动。
    pub async fn add(&self, req: TaskRequest) -> Result<Task> {
        if req.url.trim().is_empty() {
            return Err(anyhow!("URL 不能为空"));
        }

        let default_dir = self.default_dir().await;
        let dir = req
            .dir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or(default_dir);

        let filename = req
            .filename
            .as_ref()
            .map(|f| sanitize_filename(f))
            .unwrap_or_else(|| "download".to_string());
        let path = unique_path(&dir, &filename);
        let filename = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or(filename);

        let id = Uuid::new_v4();
        let mut task = Task::new(id, req, dir, filename);

        // 自动判断引擎类型：URL 看着像 m3u8 就走 HLS。
        // 用户显式指定 engine 时不动。
        if task.engine == crate::task::EngineKind::Auto && looks_like_hls(&task.url) {
            task.engine = crate::task::EngineKind::Hls;
        }
        if task.engine == crate::task::EngineKind::Auto && crate::bt::looks_like_bt(&task.url) {
            task.engine = crate::task::EngineKind::Bt;
        }

        // 需要确认保存位置的：先写到暂存文件，用户选完再转存。
        // 暂存目录和最终目录同卷，转存时 rename 是原子操作。
        //
        // 关键：状态是 AwaitingConfirm，但下载**照样立刻开始**——
        // 这跟 IDM 一样（弹窗出现时下载已经在跑）。
        // 状态只是"在等用户点确认"，不代表没在下载。
        if task.needs_confirm {
            let staging_dir = task.staging_dir();
            std::fs::create_dir_all(&staging_dir).ok();
            task.staging_path = Some(staging_dir.join(format!("{id}.part")));
            task.status = TaskStatus::AwaitingConfirm;
        }
        self.inner.store.insert_task(&task)?;
        self.inner.events
            .send(TaskEvent::StatusChanged(id, task.status))
            .ok();
        if task.needs_confirm {
            // 通知应用：有任务在等确认。
            // 应用据此把窗口弹到前面——否则窗口藏在托盘里，
            // 用户根本不知道有个下载在等他点确认。
            self.inner.events.send(TaskEvent::ConfirmNeeded(id)).ok();
        }
        Ok(task)
    }

    /// 启动（或恢复）一个任务。
    pub async fn start(&self, id: TaskId) -> Result<()> {
        {
            let running = self.inner.running.lock().await;
            if running.contains_key(&id) {
                return Ok(());
            }
        }

        // 收尾中的任务不接受重新启动
        if self.inner.finishing.lock().await.contains(&id) {
            return Err(anyhow!("任务正在收尾，请稍候"));
        }

        let task = self
            .inner
            .store
            .get_task(id)?
            .ok_or_else(|| anyhow!("任务不存在"))?;

        if task.status == TaskStatus::Completed {
            return Err(anyhow!("任务已完成"));
        }

        // 超过同时下载任务数上限时，任务留在队列里等着。
        // 前面有任务跑完时，pump_queue 会把它捞起来。
        let limit = self.max_concurrent_tasks().await;
        if let Some(limit) = limit {
            let active = self.inner.running.lock().await.len();
            if active >= limit {
                self.inner.store.set_status(id, TaskStatus::Queued, None)?;
                self.inner.events
                    .send(TaskEvent::StatusChanged(id, TaskStatus::Queued))
                    .ok();
                self.inner.pending.lock().await.push_back(id);
                return Ok(());
            }
        }

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let intent = Arc::new(Mutex::new(Intent::Active));
        self.inner.running
            .lock()
            .await
            .insert(id, Running {
                cancel: cancel_tx.clone(),
                intent: intent.clone(),
            });

        // 确认状态挂在 manager 上，比引擎活得久：
        // 用户可能在下载结束之后才点确认。
        let confirms = self.inner.confirms.clone();
        {
            let mut map = confirms.lock().await;
            let entry = map.entry(id).or_default();
            entry.final_path = task.final_path.clone();
            entry.engine_finished = false;
            entry.transferred = false;
        }

        let store = self.inner.store.clone();
        let engine = self.inner.engine.clone();
        let events = self.inner.events.clone();
        let live = self.inner.live.clone();
        let task_intent = intent.clone();
        let task_confirms = confirms.clone();
        let running = self.inner.running.clone();
        let finishing = self.inner.finishing.clone();
        tokio::spawn(async move {
            let manager_finishing = finishing.clone();
            manager_finishing.lock().await.insert(id);

            let (progress_tx, mut progress_rx) =
                tokio::sync::mpsc::unbounded_channel::<TaskProgress>();
            let (phase_tx, mut phase_rx) = tokio::sync::mpsc::unbounded_channel::<&'static str>();

            // 阶段变化：引擎进入下载阶段时，把任务状态从 Probing 改成 Downloading。
            let phase_store = store.clone();
            let phase_events = events.clone();
            let phase_task = tokio::spawn(async move {
                while let Some(phase) = phase_rx.recv().await {
                    let status = match phase {
                        "downloading" => TaskStatus::Downloading,
                        _ => continue,
                    };
                    // 如果任务还在等用户确认，状态保持 AwaitingConfirm，
                    // 不然界面上的确认弹窗就没了。
                    let current = phase_store
                        .get_task(id)
                        .ok()
                        .flatten()
                        .map(|t| t.status)
                        .unwrap_or(status);
                    let final_status = if current == TaskStatus::AwaitingConfirm {
                        TaskStatus::AwaitingConfirm
                    } else {
                        status
                    };
                    let _ = phase_store.set_status(id, final_status, None);
                    let _ = phase_events.send(TaskEvent::StatusChanged(id, final_status));
                }
            });

            // 进度接收：更新数据库并广播事件
            let progress_store = store.clone();
            let progress_events = events.clone();
            let progress_live = live.clone();
            let progress_task = tokio::spawn(async move {
                while let Some(p) = progress_rx.recv().await {
                    let _ = progress_store.set_progress(id, p.downloaded, p.total);
                    progress_live.lock().await.insert(id, p.clone());
                    let _ = progress_events.send(TaskEvent::Progress(id, p));
                }
            });

            // 等确认的任务保持 AwaitingConfirm 状态，
            // 这样界面上的确认弹窗不会消失（下载其实已经在跑了）。
            let probing_status = if task.needs_confirm {
                TaskStatus::AwaitingConfirm
            } else {
                TaskStatus::Probing
            };
            let _ = store.set_status(id, probing_status, None);
            let _ = events.send(TaskEvent::StatusChanged(id, probing_status));

            let ctx =
                crate::http::engine::EngineCtx::new(
                    cancel_rx,
                    cancel_tx.clone(),
                    progress_tx,
                    phase_tx,
                );

            // 按引擎类型分流：HLS 走自己的流程，其余走 HTTP 引擎
            let result = match task.engine {
                crate::task::EngineKind::Hls => {
                    crate::hls::download_playlist(&task, &ctx).await
                }
                crate::task::EngineKind::Bt => {
                    // BT 库是阻塞式、线程模型的，放到专用线程上跑，
                    // 别占着 tokio 的工作线程。
                    let bt_task = task.clone();
                    let bt_ctx = ctx.clone();
                    // 进度直接用 ctx 里的 tokio 通道发——
                    // tokio 的 mpsc sender 是 Send + Sync，跨线程发没问题，
                    // 不需要中间再套一层转发线程。
                    //
                    // 之前那层转发用 std::sync::mpsc 的 recv()，
                    // 那是阻塞调用，会占死一个 tokio 工作线程。
                    let joined = tokio::task::spawn_blocking(move || {
                        crate::bt::download(&bt_task, &bt_ctx)
                    })
                    .await;

                    match joined {
                        Ok(result) => result,
                        Err(err) => Err(anyhow!("BT 线程异常: {err}")),
                    }
                }
                _ => engine.download(&task, ctx).await,
            };

            let intent = *task_intent.lock().await;

            // 下载结束了。转存要同时满足两个条件：
            //  1. 引擎跑完了（engine_finished）
            //  2. 用户确认过位置了（final_path 有值）
            //
            // 两者都齐了才动文件。这样就不会出现
            // "文件正被下载引擎写，同时被转存走"的撕裂。
            if matches!(result, Ok(())) {
                let ready = {
                    let mut map = task_confirms.lock().await;
                    let c = map.entry(id).or_default();
                    c.engine_finished = true;
                    // 用户抢在下载结束之前确认过，位置从数据库兜底读回来
                    if c.final_path.is_none() {
                        if let Ok(Some(fresh)) = store.get_task(id) {
                            c.final_path = fresh.final_path.clone();
                        }
                    }
                    c.final_path.is_some() && !c.transferred
                };

                if ready {
                    let target = {
                        let mut map = task_confirms.lock().await;
                        let c = map.entry(id).or_default();
                        if c.transferred {
                            None
                        } else {
                            c.transferred = true;
                            c.final_path.clone()
                        }
                    };
                    if let Some(target) = target {
                        if let Err(err) = finalize_download(&task, Some(&target)).await {
                            tracing::error!(task = %id, "转存失败: {err}");
                            let error = Some(TaskError::new("finalize_failed", err.to_string()));
                            let _ = store.set_status(id, TaskStatus::Error, error.as_ref());
                            let _ = events.send(TaskEvent::StatusChanged(id, TaskStatus::Error));
                            let _ = progress_task.await;
                            let _ = phase_task.await;
                            running.lock().await.remove(&id);
                            manager_finishing.lock().await.remove(&id);
                            return;
                        }
                    }
                }
            }

            match (result, intent) {
                (Ok(()), _) => {
                    // 还没确认位置的话，状态保持 AwaitingConfirm，
                    // 等用户点确认时再转存并置为完成。
                    let awaiting = {
                        let map = task_confirms.lock().await;
                        task.needs_confirm
                            && map
                                .get(&id)
                                .map(|c| c.final_path.is_none())
                                .unwrap_or(true)
                    };
                    let status = if awaiting {
                        TaskStatus::AwaitingConfirm
                    } else {
                        TaskStatus::Completed
                    };
                    let _ = store.set_status(id, status, None);
                    let _ = events.send(TaskEvent::StatusChanged(id, status));
                    tracing::info!(task = %id, status = ?status, "下载结束");
                }
                (Err(_), Intent::Paused) => {
                    // 暂停：保留已完成区间，等用户继续
                    let _ = store.set_status(id, TaskStatus::Paused, None);
                    let _ = events.send(TaskEvent::StatusChanged(id, TaskStatus::Paused));
                    tracing::info!(task = %id, "任务已暂停");
                }
                (Err(_), Intent::Cancelled) => {
                    // 取消了就没什么可确认的了，把标记清掉，
                    // 不然下次打开界面还会拿这个任务弹窗。
                    clear_needs_confirm(&store, id);
                    let _ = store.set_status(id, TaskStatus::Cancelled, None);
                    let _ = events.send(TaskEvent::StatusChanged(id, TaskStatus::Cancelled));
                    tracing::info!(task = %id, "任务已取消");
                }
                (Err(err), Intent::Active) => {
                    // 同上：失败的任务不该再问保存位置。
                    clear_needs_confirm(&store, id);
                    let error = Some(TaskError::new("download_failed", err.to_string()));
                    let _ = store.set_status(id, TaskStatus::Error, error.as_ref());
                    let _ = events.send(TaskEvent::StatusChanged(id, TaskStatus::Error));
                    tracing::warn!(task = %id, "任务失败: {err}");
                }
            }

            // 通道已经关闭，这里 recv 会立刻返回 None，等它把队列里
            // 最后一条进度消息处理完再收工。
            let _ = progress_task.await;
            let _ = phase_task.await;

            // 任务结束：从运行表里摘掉，这样"继续"才能重新启动它。
            running.lock().await.remove(&id);
            manager_finishing.lock().await.remove(&id);

        });

        Ok(())
    }

    /// 暂停任务。
    pub async fn pause(&self, id: TaskId) -> Result<()> {
        let running = self.inner.running.lock().await;
        if let Some(r) = running.get(&id) {
            *r.intent.lock().await = Intent::Paused;
            let _ = r.cancel.send(true);
        }
        drop(running);
        self.inner.store.set_status(id, TaskStatus::Paused, None)?;
        self.inner.events
            .send(TaskEvent::StatusChanged(id, TaskStatus::Paused))
            .ok();
        Ok(())
    }

    /// 继续任务（从断点续传）。
    pub async fn resume(&self, id: TaskId) -> Result<()> {
        self.inner.store.set_status(id, TaskStatus::Queued, None)?;
        self.start(id).await
    }

    /// 用户确认保存位置。
    ///
    /// `path` 为 None 表示"直接下载"，用默认目录。
    /// 转存在下载完成时执行。
    pub async fn confirm_path(&self, id: TaskId, path: Option<PathBuf>) -> Result<()> {
        let mut task = self
            .inner
            .store
            .get_task(id)?
            .ok_or_else(|| anyhow!("任务不存在"))?;

        // 解析最终落盘位置。
        //
        // 关键：没给路径时也要解析成默认位置（目录 + 原文件名），
        // 不能留成 None——留成 None 的话，下载完成那一步
        // 会以为"用户还没确认"，把文件永远留在暂存目录里。
        let resolved = match path {
            Some(p) => p,
            None => task.target_path(),
        };

        // 落库，重启后也知道最终位置
        task.final_path = Some(resolved.clone());
        task.needs_confirm = false;
        self.inner.store.update_task(&task)?;

        // 和下载结束那一步协调。
        //
        // 只有"引擎已经跑完"的时候才转存暂存文件。
        // 之前这里用的是 downloaded > 0 来判断"下载完了"，
        // 可下载中途 downloaded 也大于 0——于是会在下载引擎
        // 还在往暂存文件里写的时候把它挪走，文件直接损坏。
        //
        // 现在：确认只负责记录位置；转存由"引擎跑完"那一侧触发。
        let mut transfer_now = false;
        {
            let mut map = self.inner.confirms.lock().await;
            let c = map.entry(id).or_default();
            c.final_path = Some(resolved.clone());
            // 引擎已经跑完的话，现在就可以转存
            if c.engine_finished && !c.transferred {
                c.transferred = true;
                transfer_now = true;
            }
        }

        if transfer_now {
            if let Err(err) = finalize_download(&task, Some(&resolved)).await {
                let error = Some(TaskError::new("finalize_failed", err.to_string()));
                self.inner.store.set_status(id, TaskStatus::Error, error.as_ref())?;
                self.inner
                    .events
                    .send(TaskEvent::StatusChanged(id, TaskStatus::Error))
                    .ok();
                return Err(err);
            }
            self.inner.store.set_status(id, TaskStatus::Completed, None)?;
            self.inner
                .events
                .send(TaskEvent::StatusChanged(id, TaskStatus::Completed))
                .ok();
            return Ok(());
        }

        // 还在下载中：状态改回下载中，等下载结束那一步转存。
        if task.status == TaskStatus::AwaitingConfirm {
            self.inner
                .store
                .set_status(id, TaskStatus::Downloading, None)?;
            self.inner
                .events
                .send(TaskEvent::StatusChanged(id, TaskStatus::Downloading))
                .ok();
        }
        Ok(())
    }

    /// 用户取消（带确认弹窗的那种）。会删掉已下载的暂存文件。
    pub async fn cancel_with_cleanup(&self, id: TaskId) -> Result<()> {
        self.cancel(id).await?;

        // 删掉暂存文件
        if let Ok(Some(task)) = self.inner.store.get_task(id) {
            if let Some(staging) = task.staging_path {
                let _ = tokio::fs::remove_file(&staging).await;
                if let Some(dir) = staging.parent() {
                    let _ = tokio::fs::remove_dir(dir).await;
                }
            }
        }
        Ok(())
    }

    /// 取消任务。
    pub async fn cancel(&self, id: TaskId) -> Result<()> {
        let running = self.inner.running.lock().await;
        if let Some(r) = running.get(&id) {
            *r.intent.lock().await = Intent::Cancelled;
            let _ = r.cancel.send(true);
        }
        drop(running);
        self.inner.store.set_status(id, TaskStatus::Cancelled, None)?;
        self.inner.events
            .send(TaskEvent::StatusChanged(id, TaskStatus::Cancelled))
            .ok();
        Ok(())
    }

    /// 重试一个失败的任务。
    pub async fn retry(&self, id: TaskId) -> Result<()> {
        self.inner.store.set_status(id, TaskStatus::Queued, None)?;
        self.start(id).await
    }

    /// 删除任务。可选同时删除已下载的文件。
    pub async fn remove(&self, id: TaskId, delete_file: bool) -> Result<()> {
        let task = self.inner.store.get_task(id)?;
        if let Some(task) = &task {
            if delete_file {
                // 三个位置都得清：
                //  - final_path：用户确认后文件落的地方
                //  - target_path：默认位置（没确认过，或者选了默认目录）
                //  - staging_path：还在暂存目录里的半成品
                //
                // 之前只删 target_path，用户选了别的位置时
                // 真正的文件会变成删不掉的孤儿。
                let mut paths = vec![task.target_path()];
                if let Some(final_path) = task.final_path.clone() {
                    if !paths.contains(&final_path) {
                        paths.push(final_path);
                    }
                }
                if let Some(staging) = task.staging_path.clone() {
                    if !paths.contains(&staging) {
                        paths.push(staging);
                    }
                }

                for path in paths {
                    if path.exists() {
                        tokio::fs::remove_file(&path).await.ok();
                    }
                }
                // 暂存目录空了就删掉
                if let Some(staging) = task.staging_path.as_ref().and_then(|p| p.parent()) {
                    let _ = tokio::fs::remove_dir(staging).await;
                }
            }
        }
        let running = self.inner.running.lock().await;
        if let Some(r) = running.get(&id) {
            let _ = r.cancel.send(true);
        }
        drop(running);
        // 这个任务的确认状态也一并清掉
        self.inner.confirms.lock().await.remove(&id);
        self.inner.store.delete_task(id)?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<Task>> {
        self.inner.store.list_tasks()
    }

    pub fn get(&self, id: TaskId) -> Result<Option<Task>> {
        self.inner.store.get_task(id)
    }

    /// 应用启动时，把上次残留的运行中任务恢复成暂停。
    pub fn recover_interrupted(&self) -> Result<()> {
        for task in self.inner.store.list_tasks()? {
            if matches!(
                task.status,
                TaskStatus::Downloading | TaskStatus::Probing | TaskStatus::Queued
            ) {
                self.inner.store
                    .set_status(task.id, TaskStatus::Paused, None)?;
            }
        }
        Ok(())
    }

    /// 探测资源，供 UI 预览文件大小与文件名。
    pub async fn probe(&self, req: &TaskRequest) -> Result<ResourceMeta> {
        let client = crate::http::build_client(req.user_agent.as_deref())?;
        let ctx = RequestContext {
            url: req.url.clone(),
            referer: req.referer.clone(),
            user_agent: req.user_agent.clone(),
            cookies: req.cookies.clone(),
            headers: req.headers.clone(),
        };
        crate::http::probe(&client, &ctx).await
    }

    /// 根据探测结果补全文件名。
    pub async fn resolve_filename(&self, req: &TaskRequest) -> Result<String> {
        if let Some(name) = &req.filename {
            return Ok(sanitize_filename(name));
        }
        let meta = self.probe(req).await?;
        if let Some(name) = meta.suggested_filename {
            return Ok(name);
        }
        if let Some(name) = filename_from_content_disposition("") {
            return Ok(name);
        }
        filename_from_url(&req.url).ok_or_else(|| anyhow!("无法确定文件名"))
    }
}

/// 默认下载目录：用户目录下的 Downloads。
pub fn default_download_dir() -> PathBuf {
    if let Ok(profile) = std::env::var("USERPROFILE") {
        let dir = PathBuf::from(profile).join("Downloads");
        if dir.exists() {
            return dir;
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// URL 看起来像 HLS 播放列表吗。
pub fn looks_like_hls(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    // 去掉查询参数再判断，避免 ?url=xxx.m3u8 这类误伤
    let path = lower.split(['?', '#']).next().unwrap_or(&lower);
    path.ends_with(".m3u8") || path.ends_with(".m3u")
}

/// 下载完成后的收尾：把暂存文件挪到最终位置。
///
/// 没有暂存文件（不需要确认的流程）就直接返回。
/// 转存用 rename：同一卷内是原子操作，10GB 文件也是瞬间完成。
async fn finalize_download(task: &Task, target: Option<&PathBuf>) -> Result<()> {
    let Some(staging) = task.staging_path.as_ref() else {
        return Ok(());
    };

    let final_path = match target {
        Some(p) => p.clone(),
        None => task.target_path(),
    };

    if !staging.exists() {
        // 没有暂存文件，说明已经转存过了
        return Ok(());
    }

    if let Some(parent) = final_path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }

    // 同卷用 rename，跨卷 rename 会失败，这时退回"复制 + 删除"
    // rename 在 Windows 上会因为源文件还被下载引擎握着句柄而失败
    // （共享冲突）。引擎其实已经返回了，只是句柄还没释放干净，
    // 所以重试几次通常就能成功——rename 是原子的，比退回复制强得多：
    // 复制过程中目标位置会先出现一个不完整的文件。
    //
    // 以前这里一次就退回复制，删除暂存文件的错误还被 let _ = 吞掉，
    // 于是 .idmshadow/ 下的 .part 文件悄悄留在磁盘上，
    // 用户那边看到的却是"下载已完成"。
    let mut renamed = false;
    let mut last_err = None;
    for attempt in 0..5u64 {
        match tokio::fs::rename(staging, &final_path).await {
            Ok(()) => {
                renamed = true;
                break;
            }
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(50 * (attempt + 1))).await;
            }
        }
    }
    if !renamed {
        // 真的不行（比如跨卷），退回复制 + 删除
        tokio::fs::copy(staging, &final_path)
            .await
            .with_context(|| {
                format!(
                    "转存到 {} 失败: {:?}",
                    final_path.display(),
                    last_err
                )
            })?;
        if let Err(err) = tokio::fs::remove_file(staging).await {
            // 删不掉就记一笔，别让暂存文件悄悄留在磁盘上。
            tracing::warn!("暂存文件删除失败，磁盘上会留下残留: {err}");
        }
    }

    // 清掉暂存目录（空了就删）
    if let Some(staging_dir) = task.staging_path.as_ref().and_then(|p| p.parent()) {
        let _ = tokio::fs::remove_dir(staging_dir).await;
    }
    Ok(())
}

/// 任务不再需要用户确认保存位置时，清掉这个标记。
///
/// 失败或取消之后标记还留在库里的话，下次打开界面
/// 还会拿这个早就没意义的任务弹窗——之前就出过这个毛病。
fn clear_needs_confirm(store: &Store, id: TaskId) {
    if let Ok(Some(mut task)) = store.get_task(id) {
        if task.needs_confirm {
            task.needs_confirm = false;
            let _ = store.update_task(&task);
        }
    }
}

/// 引擎类型探测：URL 或响应看起来像 HLS 就走 HLS 引擎。
pub fn detect_engine(req: &TaskRequest, meta: &ResourceMeta) -> EngineKind {
    match req.engine {
        EngineKind::Auto => {
            if meta.looks_like_hls() {
                EngineKind::Hls
            } else {
                EngineKind::Http
            }
        }
        other => other,
    }
}
