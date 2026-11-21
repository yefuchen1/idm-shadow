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
    is_temporary_name, looks_like_image_url, resolve_download_filename, sanitize_filename,
    unique_path, EngineKind, Task, TaskError, TaskId, TaskRequest, TaskStatus,
};

/// 定文件名时顺手带回来的探测结果。
///
/// Content-Type 用来兜住"没有后缀的图片预览"：扩展侧按 URL 拦一层，
/// 引擎侧拿到响应头再确认一次。
struct ResolvedFilename {
    name: String,
    meta: Option<ResourceMeta>,
}

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
    /// 各任务当前**真实的**下载阶段。
    ///
    /// 界面上的状态会被"等用户确认"盖住（`AwaitingConfirm`），
    /// 所以引擎报上来的阶段得单独记一份。用户确认完之后，
    /// 用它把状态恢复成真实情况，而不是一律写成「下载中」——
    /// 磁力任务一个 peer 都没连上的时候，真实情况是「等待连接」。
    phases: Arc<Mutex<HashMap<TaskId, TaskStatus>>>,
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
    /// BT 选文件确认的唤醒信号。
    ///
    /// BT 引擎在等用户勾选要下载的文件。原来这里是**每 200 毫秒查一次
    /// 数据库**，用户可能几分钟都不点选择——那期间就是每 200ms 一次
    /// SQLite 查询，纯属白烧。
    ///
    /// 改成：用户选好之后发一个通知，等着的线程立刻醒过来检查一次。
    /// 没通知的时候就睡着，不用反复查库。
    bt_selection: Arc<BtSelectionSignal>,
}

/// BT 选文件的唤醒信号。
///
/// 用 `Condvar` 而不是 `tokio::sync::Notify`：
/// 等待方在 `spawn_blocking` 里——那是阻塞线程，没法 await。
/// `Condvar` 就是给这种场景用的。
#[derive(Default)]
struct BtSelectionSignal {
    /// 有没有待处理的通知。
    pending: std::sync::Mutex<bool>,
    /// 通知时唤醒等待者。
    cvar: std::sync::Condvar,
}

impl BtSelectionSignal {
    /// 发出一次通知。
    fn notify(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        *pending = true;
        self.cvar.notify_all();
    }

    /// 等一次通知，最多等 `timeout`。
    ///
    /// 返回 true 表示收到了通知，false 表示等超时了。
    /// 超时也返回，让调用方去做兜底检查——不能因为通知丢了就永远卡住。
    fn wait(&self, timeout: std::time::Duration) -> bool {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        // 通知可能已经在等着了，别漏掉。
        if *pending {
            *pending = false;
            return true;
        }
        let (mut guard, _timeout) = self
            .cvar
            .wait_timeout(pending, timeout)
            .unwrap_or_else(|e| e.into_inner());
        let notified = *guard;
        *guard = false;
        notified
    }
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
                phases: Arc::new(Mutex::new(HashMap::new())),
                finishing: Arc::new(Mutex::new(HashSet::new())),
                live: Arc::new(Mutex::new(HashMap::new())),
                events,
                limiter: Arc::new(RateLimiter::new(None)),
                default_dir: tokio::sync::RwLock::new(default_dir),
                max_concurrent_tasks: Arc::new(Mutex::new(None)),
                pending: Arc::new(Mutex::new(VecDeque::new())),
                confirms: Arc::new(Mutex::new(HashMap::new())),
                bt_selection: Arc::new(BtSelectionSignal::default()),
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
    pub async fn add(&self, mut req: TaskRequest) -> Result<Task> {
        if req.url.trim().is_empty() {
            return Err(anyhow!("URL 不能为空"));
        }

        // 页面自动发起 / 浏览器恢复的下载，用户当时没有说"我要下"。
        // 这类请求在应用这一层也必须拒绝——扩展可能漏判，老客户端
        // 甚至不会带来源。宁可让浏览器自己下，也不能再自动弹框。
        if req.source.is_blocked() {
            return Err(anyhow!("这个下载不是用户发起的，已跳过"));
        }

        // 浏览器侧的图片预览（NAS 封面、图床缩略图）常常没有后缀，
        // 但路径上写得很清楚。扩展会自动生成任务的话这里再拦一次。
        if req.source.is_auto_capture() && looks_like_image_url(&req.url) {
            return Err(anyhow!("图片预览不创建下载任务"));
        }

        // 确认框只给用户明确触发的下载。扩展/内部来源即使误带了
        // needs_confirm，也不能打断用户。
        req.needs_confirm = req.needs_confirm && req.source.may_confirm();

        let default_dir = self.default_dir().await;
        let dir = req
            .dir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or(default_dir);

        // 文件名在这里定，不是在下载阶段再改名。
        //
        // 原因有两个：
        //  1. 落盘名必须是弹窗里给用户看的那个名字。用户手快点「立即下载」，
        //     下载阶段才回填的话，弹窗显示 "download.mp4"、磁盘上却是 "download"，
        //     对不上；真要回填还得连暂存文件一起改名。
        //  2. 探测已经在下载前做过一次（engine/src/http/engine.rs），
        //     那次的响应头里已经有 Content-Disposition 和 Content-Type，
        //     这里再走一遍是同一次网络往返，不算额外成本。
        // 先定引擎，再定文件名：BT 任务不走 HTTP 探测，
        // 也不能被 unique_path 改名——种子里的文件路径是固定的。
        let mut engine = req.engine;
        if engine == crate::task::EngineKind::Auto && looks_like_hls(&req.url) {
            engine = crate::task::EngineKind::Hls;
        }
        if engine == crate::task::EngineKind::Auto && crate::bt::looks_like_bt(&req.url) {
            engine = crate::task::EngineKind::Bt;
        }

        let filename = if engine == crate::task::EngineKind::Bt {
            crate::bt::suggested_filename(&req.url)
                .unwrap_or_else(|| "magnet".to_string())
        } else {
            let resolved = self.resolve_task_filename(&req).await;
            // Content-Type 是比 URL 更可靠的证据：`/api/capture_image`
            // 没有后缀，服务器返回 image/jpeg。
            if req.source.is_auto_capture()
                && resolved.meta.as_ref().map(|m| m.is_image()).unwrap_or(false)
            {
                return Err(anyhow!("图片预览不创建下载任务"));
            }
            let filename = resolved.name;
            let path = unique_path(&dir, &filename);
            path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or(filename)
        };

        let id = Uuid::new_v4();
        let mut task = Task::new(id, req, dir, filename);
        task.engine = engine;

        // 磁力任务要弹窗让用户确认保存位置、勾选文件。
        // 文件清单要等 prepare 阶段抓完 metadata 才有，弹窗先出现，
        // 清单到了再补上。
        if task.engine == crate::task::EngineKind::Bt && task.source.may_confirm() {
            task.needs_confirm = true;
        }

        // 需要确认保存位置的：先写到暂存文件，用户选完再转存。
        // 暂存目录和最终目录同卷，转存时 rename 是原子操作。
        //
        // 关键：状态是 AwaitingConfirm，但下载**照样立刻开始**——
        // 这跟 IDM 一样（弹窗出现时下载已经在跑）。
        // 状态只是"在等用户点确认"，不代表没在下载。
        if task.needs_confirm {
            if task.engine != crate::task::EngineKind::Bt {
                let staging_dir = task.staging_dir();
                std::fs::create_dir_all(&staging_dir).ok();
                task.staging_path = Some(staging_dir.join(format!("{id}.part")));
            }
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

    /// 定出任务该用的文件名。
    ///
    /// 调用方给的名字如果像"真的名字"，就直接用；只给了一堆占位词
    /// （`download`、`download (2)`、空），就当没给，探测服务器定名。
    async fn resolve_task_filename(&self, req: &TaskRequest) -> ResolvedFilename {
        let provided = req.filename.as_deref().filter(|name| {
            let cleaned = sanitize_filename(name);
            !cleaned.is_empty() && !is_temporary_name(&cleaned)
        });

        // 名字又完整又有扩展名，不用白跑一趟网络。
        //
        // 但只有名字没有扩展名时还得探一次：
        // 用户自己起的名字也可能是 "video" 这种光秃秃的，
        // 补上扩展名它才能被播放器认出来。
        if let Some(name) = provided {
            if crate::task::has_extension(&name) {
                return ResolvedFilename {
                    name: name.to_string(),
                    meta: None,
                };
            }
        }

        match self.probe_filename(req, provided).await {
            Some((name, meta)) => ResolvedFilename {
                name,
                meta: Some(meta),
            },
            None => ResolvedFilename {
                name: match provided {
                    Some(name) => sanitize_filename(name),
                    None => "download".to_string(),
                },
                meta: None,
            },
        }
    }

    /// 探测服务器拿文件名。失败返回 None。
    ///
    /// 超时给得很短（5 秒）：这一步挡在用户点击和弹窗之间，
    /// 站点不响应的话不能让人干等。
    async fn probe_filename(
        &self,
        req: &TaskRequest,
        provided: Option<&str>,
    ) -> Option<(String, ResourceMeta)> {
        let client = crate::http::build_client(req.user_agent.as_deref()).ok()?;
        let ctx = RequestContext {
            url: req.url.clone(),
            referer: req.referer.clone(),
            user_agent: req.user_agent.clone(),
            cookies: req.cookies.clone(),
            headers: req.headers.clone(),
        };
        let meta = crate::http::probe_with_timeout(&client, &ctx, Duration::from_secs(5))
            .await
            .ok()?;
        let name = resolve_download_filename(
            meta.content_disposition.as_deref(),
            provided,
            &meta.final_url,
            meta.content_type.as_deref(),
        );
        if name.is_empty() {
            None
        } else {
            Some((name, meta))
        }
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
        let phases = self.inner.phases.clone();
        let task_intent = intent.clone();
        let task_confirms = confirms.clone();
        let running = self.inner.running.clone();
        let finishing = self.inner.finishing.clone();
        // BT 选文件的唤醒信号，等着的那边会用到。
        let bt_selection_signal = self.inner.bt_selection.clone();
        tokio::spawn(async move {
            let manager_finishing = finishing.clone();
            manager_finishing.lock().await.insert(id);

            let (progress_tx, mut progress_rx) =
                tokio::sync::mpsc::unbounded_channel::<TaskProgress>();
            let (phase_tx, mut phase_rx) = tokio::sync::mpsc::unbounded_channel::<&'static str>();

            // 阶段变化：引擎进入下载阶段时，把任务状态从 Probing 改成 Downloading。
            let phase_store = store.clone();
            let phase_events = events.clone();
            let phase_map = phases.clone();
            let phase_task = tokio::spawn(async move {
                while let Some(phase) = phase_rx.recv().await {
                    let status = match phase {
                        "downloading" => TaskStatus::Downloading,
                        "waiting_peers" => TaskStatus::WaitingPeers,
                        _ => continue,
                    };
                    // 真实阶段单独记一份。用户确认之后要用它恢复状态。
                    phase_map.lock().await.insert(id, status);
                    // 任务已经被删掉了，别再往回写状态。
                    let Some(current) = phase_store.get_task(id).ok().flatten() else {
                        continue;
                    };
                    // 如果任务还在等用户确认，状态保持 AwaitingConfirm，
                    // 不然界面上的确认弹窗就没了。
                    let final_status = if current.status == TaskStatus::AwaitingConfirm {
                        TaskStatus::AwaitingConfirm
                    } else {
                        status
                    };
                    // 状态没变就别写库。引擎那边会反复报同一个阶段，
                    // 每次都落库 + 广播是白做的功。
                    //
                    // 更重要的是这里成了**自愈**的地方：状态要是被别处
                    // 写歪了（比如用户点了"继续"，旧代码无条件写 `Queued`），
                    // 引擎下一轮报的阶段会把它纠正回来。
                    if final_status != current.status {
                        let _ = phase_store.set_status(id, final_status, None);
                        let _ = phase_events.send(TaskEvent::StatusChanged(id, final_status));
                    }
                }
            });

            // 进度接收：更新数据库并广播事件
            let progress_store = store.clone();
            let progress_events = events.clone();
            let progress_live = live.clone();
            let progress_task = tokio::spawn(async move {
                // 上一次发出去的进度。
                //
                // 引擎那边没有进展时也会照样发进度（速度是 0），
                // 一个卡住的任务每 500ms 发一次，内容一模一样。
                // 每条都要写一次数据库、克隆一次、广播给前端——
                // 全部是白做的功。
                //
                // 这里比一下，内容没变就不往下发。
                let mut last_sent: Option<TaskProgress> = None;
                while let Some(p) = progress_rx.recv().await {
                    if let Some(prev) = last_sent.as_ref() {
                        if prev.downloaded == p.downloaded
                            && prev.total == p.total
                            && prev.speed == p.speed
                            && prev.connections == p.connections
                            && prev.note == p.note
                        {
                            // 没变化，跳过。
                            continue;
                        }
                    }
                    let _ = progress_store.set_progress(id, p.downloaded, p.total);
                    progress_live.lock().await.insert(id, p.clone());
                    last_sent = Some(p.clone());
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
                    //
                    // 分两段：
                    //  1. prepare：抓 info dict、写 .torrent 缓存、稀疏占位
                    //  2. 需要用户勾选文件时先停下；否则直接下载
                    let bt_task = task.clone();
                    let bt_ctx = ctx.clone();
                    let bt_store = store.clone();
                    let bt_events = events.clone();
                    // 进度直接用 ctx 里的 tokio 通道发——
                    // tokio 的 mpsc sender 是 Send + Sync，跨线程发没问题，
                    // 不需要中间再套一层转发线程。
                    //
                    // 之前那层转发用 std::sync::mpsc 的 recv()，
                    // 那是阻塞调用，会占死一个 tokio 工作线程。
                    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                        let meta = crate::bt::prepare(&bt_task, &bt_ctx)?;

                        // 拿到 info dict 之后，任务名和总大小才有准信。
                        let mut fresh = bt_store
                            .get_task(id)?
                            .ok_or_else(|| anyhow!("任务不存在"))?;
                        let name = crate::task::sanitize_filename(&meta.name);
                        fresh.filename = name;
                        fresh.total_size = Some(meta.total_size);
                        fresh.bt_files = Some(crate::task::BtFileList {
                            name: meta.name.clone(),
                            files: meta.files.clone(),
                            needs_selection: meta.needs_selection()
                                && fresh.bt_selected.is_none(),
                        });
                        bt_store.update_task(&fresh)?;

                        // 多文件种子、还没勾选、也还没确认保存位置：停下来等。
                        //
                        // 关键：这个线程**不能退出**。确认弹窗里的勾选结果
                        // 只写数据库，由这里拿到再继续下载。
                        // 如果这里返回、让 confirm_path 重新 start，
                        // 会跟本线程的收尾撞在一起——running 表里还挂着 id，
                        // start 以为任务在跑，结果谁也没真正下载。
                        if fresh.bt_selected.is_none()
                            && meta.needs_selection()
                            && fresh.needs_confirm
                        {
                            let _ = bt_events
                                .send(TaskEvent::StatusChanged(id, TaskStatus::AwaitingConfirm));
                            let _ = bt_events.send(TaskEvent::ConfirmNeeded(id));
                            crate::bt::send_note(&bt_ctx, "等待选择要下载的文件…");

                            // 等用户勾选。
                            //
                            // 这里**不能退出去轮询数据库**：用户可能几分钟都不点，
                            // 每 200ms 查一次 SQLite 纯属白烧 CPU 和磁盘 IO。
                            //
                            // 改成等信号：用户选好之后 `confirm_path` 唤醒这边，
                            // 这边立刻醒过来查一次数据库。
                            // 等待期间线程是睡着的，不占 CPU。
                            //
                            // 保险起见留一个 5 秒的兜底超时——
                            // 万一信号丢了（进程重启、状态被别的路径改了），
                            // 任务也不会永远卡住。
                            loop {
                                if *bt_ctx.cancel.borrow() {
                                    return Err(anyhow!("已取消"));
                                }
                                bt_selection_signal.wait(std::time::Duration::from_secs(5));
                                if let Ok(current) = bt_store.get_task(id) {
                                    if let Some(current) = current {
                                        fresh = current;
                                        if fresh.bt_selected.is_some() || !fresh.needs_confirm {
                                            break;
                                        }
                                    }
                                }
                            }
                        }

                        if let Some(files) = fresh.bt_files.as_mut() {
                            files.needs_selection = false;
                        }
                        bt_store.update_task(&fresh)?;
                        let selection = fresh.bt_selected.clone().unwrap_or_default();
                        crate::bt::download_prepared(&fresh, &bt_ctx, &meta, &selection)
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

                // BT 的文件本来就写在最终目录里，没有暂存文件可转存。
                if ready && task.engine != crate::task::EngineKind::Bt {
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
                        // 用库里的最新任务，别用 start() 那一刻的快照——
                        // staging_path 之类的字段可能已经变了。
                        let fresh = store
                            .get_task(id)
                            .ok()
                            .flatten()
                            .unwrap_or_else(|| task.clone());
                        if let Err(err) = finalize_download(&fresh, Some(&target)).await {
                            tracing::error!(task = %id, "转存失败: {err}");
                            let error = Some(TaskError::new("finalize_failed", err.to_string()));
                            let _ = store.set_status(id, TaskStatus::Error, error.as_ref());
                            let _ = events.send(TaskEvent::StatusChanged(id, TaskStatus::Error));
                            let _ = progress_task.await;
                            let _ = phase_task.await;
                            running.lock().await.remove(&id);
                            phases.lock().await.remove(&id);
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
                        // 读库里的 needs_confirm，不能用闭包捕获的 task——
                        // 那是 start() 那一刻的快照。用户在这之后确认过的话，
                        // 快照里还是 true，就会把状态错判成"还在等确认"，
                        // 任务永远卡在 downloading。
                        let needs = store
                            .get_task(id)
                            .ok()
                            .flatten()
                            .map(|t| t.needs_confirm)
                            .unwrap_or(task.needs_confirm);
                        needs
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
            phases.lock().await.remove(&id);
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
        self.continue_task(id).await
    }

    /// 用户确认保存位置。
    ///
    /// `path` 为 None 表示"直接下载"，用默认目录。
    /// 转存在下载完成时执行。
    pub async fn confirm_path(
        &self,
        id: TaskId,
        path: Option<PathBuf>,
        files: Option<Vec<usize>>,
    ) -> Result<()> {
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

        // BT 任务：用户选的是目录，文件按种子里的相对路径落在那里。
        // 勾选的文件下标也一并落库，下一次 start 直接用。
        if task.engine == crate::task::EngineKind::Bt {
            if let Some(dir) = resolved.parent() {
                if !dir.as_os_str().is_empty() {
                    task.dir = dir.to_path_buf();
                }
            }
            if let Some(files) = files {
                let total = task.bt_files.as_ref().map(|f| f.files.len()).unwrap_or(0);
                if files.is_empty() {
                    return Err(anyhow!("至少要勾选一个文件"));
                }
                if let Some(bad) = files.iter().find(|i| **i >= total) {
                    return Err(anyhow!("文件下标超出范围: {bad}"));
                }
                task.bt_selected = Some(files);
            }
        }
        self.inner.store.update_task(&task)?;

        // 唤醒正在等选文件的 BT 线程。
        //
        // 它在 `spawn_blocking` 里睡着等这个信号，
        // 收到之后立刻去读数据库、继续下载。
        // 不发的话它只能等兜底超时（5 秒）。
        self.inner.bt_selection.notify();

        // BT 任务没有暂存文件，不需要转存；确认后直接启动/继续。
        if task.engine == crate::task::EngineKind::Bt {
            let never_started = !self.inner.running.lock().await.contains_key(&id);
            if never_started {
                self.start(id).await?;
            } else if task.status == TaskStatus::AwaitingConfirm {
                // 恢复成引擎报上来的**真实**阶段。
                //
                // 以前这里一律写成「下载中」。但磁力任务一个 peer
                // 都没连上的时候，真实情况是「等待连接」——
                // 写成「下载中」就是又一次误报，用户以为在下，
                // 实际上在干等。
                let status = self
                    .inner
                    .phases
                    .lock()
                    .await
                    .get(&id)
                    .copied()
                    .unwrap_or(TaskStatus::Downloading);
                self.inner.store.set_status(id, status, None)?;
                self.inner
                    .events
                    .send(TaskEvent::StatusChanged(id, status))
                    .ok();
            }
            return Ok(());
        }

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

        // 有些调用方建任务时只登记、不启动（bridge 的 autostart=false）。
        // 这种任务从没跑过引擎，确认时若只把状态改成「下载中」，
        // 就再也没人把它真正拉起来——界面上的进度会永远停在 0 字节。
        let never_started = task.status == TaskStatus::AwaitingConfirm
            && !self.inner.running.lock().await.contains_key(&id);
        if never_started {
            self.start(id).await?;
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
        self.continue_task(id).await
    }

    /// "继续"/"重试"的共同实现。
    ///
    /// 这里要分清三种情况——分错了就是用户点了「继续」没反应：
    ///
    /// 1. **任务正在跑**（引擎活着）：把被写歪的状态摆正，
    ///    不要重启。重启会打断正在进行的传输。
    /// 2. **任务正在收尾**（刚点过暂停，引擎还没退出）：等它退干净再重启。
    ///    这时候直接 `start()` 会被"已经在跑"的短路吃掉，什么都不做，
    ///    随后引擎退出又把状态写回「已暂停」——点了继续等于没点。
    /// 3. **任务没在跑**：正常重启。
    ///
    /// 起因：以前 `resume()` 无条件写 `Queued`，然后调 `start()`。
    /// 但 `start()` 开头有个短路：任务已经在 `running` 表里就直接返回。
    ///
    /// BT 引擎是常驻线程模型的——任务跑起来就不退出，
    /// 所以点了"继续"之后引擎往往还活着，`start()` 走短路返回，
    /// **没有任何代码再去把状态从 `Queued` 改成 `Downloading`**。
    /// 结果就是：任务明明在下载（速度、连接数都是实的），
    /// 标签却一直写着"排队中"。
    async fn continue_task(&self, id: TaskId) -> Result<()> {
        // 先把意图取出来，**取完立刻放锁**。
        //
        // 下面要 await（等收尾、重启任务），而 `start()` 里还会
        // 再去锁 `running`。tokio 的 Mutex 不可重入——
        // 攥着 `running` 去 await 一个会锁 `running` 的操作就是死锁。
        let intent = {
            let map = self.inner.running.lock().await;
            match map.get(&id) {
                Some(running) => Some(*running.intent.lock().await),
                // 情况 3：没在跑。
                None => None,
            }
        };

        match intent {
            // 情况 3：没在跑，正常重启。
            None => self.restart_task(id).await,
            // 情况 1：引擎还活着，任务在正常跑。
            Some(Intent::Active) => {
                self.realign_status(id).await?;
                Ok(())
            }
            // 情况 2：引擎正在收尾。等它退干净。
            Some(Intent::Paused) => {
                if !self.wait_for_teardown(id).await {
                    return Err(anyhow!("任务正在收尾，请稍候"));
                }
                self.restart_task(id).await
            }
            // 情况 2 变体：用户已经取消了。取消是不可逆的，
            // "继续"不该把它复活——等收尾完就完事。
            Some(Intent::Cancelled) => {
                self.wait_for_teardown(id).await;
                Ok(())
            }
        }
    }

    /// 把被写歪的状态摆正（任务确实还在跑的时候）。
    async fn realign_status(&self, id: TaskId) -> Result<()> {
        let current = self
            .inner
            .store
            .get_task(id)?
            .map(|t| t.status)
            .unwrap_or(TaskStatus::Downloading);
        // 只在状态确实不对的时候才改，避免无谓的写库。
        //
        // 恢复成引擎报上来的**真实**阶段。
        //
        // 不能一律写成「下载中」：磁力任务一个 peer 都没连上的时候，
        // 真实情况是「等待连接」。写成「下载中」就是骗用户。
        let real = self.inner.phases.lock().await.get(&id).copied();
        let fixed = match current {
            TaskStatus::Queued | TaskStatus::Probing => {
                real.unwrap_or(TaskStatus::Downloading)
            }
            other => other,
        };
        if fixed != current {
            self.inner.store.set_status(id, fixed, None)?;
            self.inner
                .events
                .send(TaskEvent::StatusChanged(id, fixed))
                .ok();
        }
        Ok(())
    }

    /// 正常重启一个没在跑的任务。
    async fn restart_task(&self, id: TaskId) -> Result<()> {
        self.inner.store.set_status(id, TaskStatus::Queued, None)?;
        self.start(id).await
    }

    /// 等任务从运行表和收尾表里都退出去。
    ///
    /// 引擎线程退出时是先把 `running` 摘掉、再摘 `finishing`，
    /// 两个表都空了才算真的退干净——只等 `running` 的话，
    /// 有个窗口 `start()` 会撞上"任务正在收尾"的检查而失败。
    async fn wait_for_teardown(&self, id: TaskId) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            // 两把锁分开拿，别同时攥着。
            //
            // 引擎线程收尾时是先摘 `running`、再摘 `finishing`。
            // 这里要是反过来（先 running 后 finishing）并且中途 await，
            // 两边就可能互相等。
            let running = { self.inner.running.lock().await.contains_key(&id) };
            let finishing = { self.inner.finishing.lock().await.contains(&id) };
            if !running && !finishing {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
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
                        // BT 的多文件种子落地的是一个目录，
                        // remove_file 对目录会失败，得整棵删。
                        if path.is_dir() {
                            tokio::fs::remove_dir_all(&path).await.ok();
                        } else {
                            tokio::fs::remove_file(&path).await.ok();
                        }
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
                TaskStatus::Downloading
                    | TaskStatus::Probing
                    | TaskStatus::Queued
                    | TaskStatus::WaitingPeers
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
