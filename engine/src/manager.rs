//! 任务管理器：把存储、引擎、队列调度串起来。
//!
//! 桌面应用和本地 HTTP API 都通过它操作任务，保证两条入口看到同一份状态。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
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
}

struct Running {
    cancel: watch::Sender<bool>,
    /// 用户对当前任务的意图。引擎只知道"被叫停了"，
    /// 是暂停还是取消，得由这里记账。
    intent: Arc<Mutex<Intent>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Intent {
    Active,
    Paused,
    Cancelled,
}

pub struct DownloadManager {
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
}

impl DownloadManager {
    pub fn new(store: Arc<Store>, config: DownloadConfig) -> Result<Self> {
        let engine = Arc::new(HttpEngine::new(store.clone(), config)?);
        let (events, _) = broadcast::channel(256);
        let default_dir = default_download_dir();
        Ok(Self {
            store,
            engine,
            running: Arc::new(Mutex::new(HashMap::new())),
            finishing: Arc::new(Mutex::new(HashSet::new())),
            live: Arc::new(Mutex::new(HashMap::new())),
            events,
            limiter: Arc::new(RateLimiter::new(None)),
        default_dir: tokio::sync::RwLock::new(default_dir),
        })
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn engine(&self) -> &Arc<HttpEngine> {
        &self.engine
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TaskEvent> {
        self.events.subscribe()
    }

    /// 当前的实时进度快照。
    pub async fn live_progress(&self) -> HashMap<TaskId, TaskProgress> {
        self.live.lock().await.clone()
    }

    pub fn set_global_limit(&self, limit: Option<u64>) {
        self.limiter.set_limit(limit);
        self.engine.set_global_limit(limit);
    }

    pub async fn set_default_dir(&self, dir: PathBuf) {
        *self.default_dir.write().await = dir;
    }

    pub async fn default_dir(&self) -> PathBuf {
        self.default_dir.read().await.clone()
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
        let task = Task::new(id, req, dir, filename);
        self.store.insert_task(&task)?;
        self.events
            .send(TaskEvent::StatusChanged(id, TaskStatus::Queued))
            .ok();
        Ok(task)
    }

    /// 启动（或恢复）一个任务。
    pub async fn start(&self, id: TaskId) -> Result<()> {
        {
            let running = self.running.lock().await;
            if running.contains_key(&id) {
                return Ok(());
            }
        }

        // 收尾中的任务不接受重新启动
        if self.finishing.lock().await.contains(&id) {
            return Err(anyhow!("任务正在收尾，请稍候"));
        }

        let task = self
            .store
            .get_task(id)?
            .ok_or_else(|| anyhow!("任务不存在"))?;

        if task.status == TaskStatus::Completed {
            return Err(anyhow!("任务已完成"));
        }

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let intent = Arc::new(Mutex::new(Intent::Active));
        self.running
            .lock()
            .await
            .insert(id, Running {
                cancel: cancel_tx.clone(),
                intent: intent.clone(),
            });

        let store = self.store.clone();
        let engine = self.engine.clone();
        let events = self.events.clone();
        let live = self.live.clone();
        let task_intent = intent.clone();
        let running = self.running.clone();
        let finishing = self.finishing.clone();

        tokio::spawn(async move {
            let manager_finishing = finishing.clone();
            manager_finishing.lock().await.insert(id);

            let (progress_tx, mut progress_rx) =
                tokio::sync::mpsc::unbounded_channel::<TaskProgress>();

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

            let _ = store.set_status(id, TaskStatus::Probing, None);
            let _ = events.send(TaskEvent::StatusChanged(id, TaskStatus::Probing));

            let ctx =
                crate::http::engine::EngineCtx::new(cancel_rx, cancel_tx.clone(), progress_tx);
            let result = engine.download(&task, ctx).await;

            let intent = *task_intent.lock().await;

            match (result, intent) {
                (Ok(()), _) => {
                    let _ = store.set_status(id, TaskStatus::Completed, None);
                    let _ = events.send(TaskEvent::StatusChanged(id, TaskStatus::Completed));
                    tracing::info!(task = %id, "任务完成");
                }
                (Err(_), Intent::Paused) => {
                    // 暂停：保留已完成区间，等用户继续
                    let _ = store.set_status(id, TaskStatus::Paused, None);
                    let _ = events.send(TaskEvent::StatusChanged(id, TaskStatus::Paused));
                    tracing::info!(task = %id, "任务已暂停");
                }
                (Err(_), Intent::Cancelled) => {
                    let _ = store.set_status(id, TaskStatus::Cancelled, None);
                    let _ = events.send(TaskEvent::StatusChanged(id, TaskStatus::Cancelled));
                    tracing::info!(task = %id, "任务已取消");
                }
                (Err(err), Intent::Active) => {
                    let error = Some(TaskError::new("download_failed", err.to_string()));
                    let _ = store.set_status(id, TaskStatus::Error, error.as_ref());
                    let _ = events.send(TaskEvent::StatusChanged(id, TaskStatus::Error));
                    tracing::warn!(task = %id, "任务失败: {err}");
                }
            }

            // 通道已经关闭，这里 recv 会立刻返回 None，等它把队列里
            // 最后一条进度消息处理完再收工。
            let _ = progress_task.await;

            // 任务结束：从运行表里摘掉，这样"继续"才能重新启动它。
            running.lock().await.remove(&id);
            manager_finishing.lock().await.remove(&id);
        });

        Ok(())
    }

    /// 暂停任务。
    pub async fn pause(&self, id: TaskId) -> Result<()> {
        let running = self.running.lock().await;
        if let Some(r) = running.get(&id) {
            *r.intent.lock().await = Intent::Paused;
            let _ = r.cancel.send(true);
        }
        drop(running);
        self.store.set_status(id, TaskStatus::Paused, None)?;
        self.events
            .send(TaskEvent::StatusChanged(id, TaskStatus::Paused))
            .ok();
        Ok(())
    }

    /// 继续任务（从断点续传）。
    pub async fn resume(&self, id: TaskId) -> Result<()> {
        self.store.set_status(id, TaskStatus::Queued, None)?;
        self.start(id).await
    }

    /// 取消任务。
    pub async fn cancel(&self, id: TaskId) -> Result<()> {
        let running = self.running.lock().await;
        if let Some(r) = running.get(&id) {
            *r.intent.lock().await = Intent::Cancelled;
            let _ = r.cancel.send(true);
        }
        drop(running);
        self.store.set_status(id, TaskStatus::Cancelled, None)?;
        self.events
            .send(TaskEvent::StatusChanged(id, TaskStatus::Cancelled))
            .ok();
        Ok(())
    }

    /// 重试一个失败的任务。
    pub async fn retry(&self, id: TaskId) -> Result<()> {
        self.store.set_status(id, TaskStatus::Queued, None)?;
        self.start(id).await
    }

    /// 删除任务。可选同时删除已下载的文件。
    pub async fn remove(&self, id: TaskId, delete_file: bool) -> Result<()> {
        let task = self.store.get_task(id)?;
        if let Some(task) = &task {
            if delete_file {
                let path = task.target_path();
                if path.exists() {
                    tokio::fs::remove_file(&path).await.ok();
                }
            }
        }
        let running = self.running.lock().await;
        if let Some(r) = running.get(&id) {
            let _ = r.cancel.send(true);
        }
        drop(running);
        self.store.delete_task(id)?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<Task>> {
        self.store.list_tasks()
    }

    pub fn get(&self, id: TaskId) -> Result<Option<Task>> {
        self.store.get_task(id)
    }

    /// 应用启动时，把上次残留的运行中任务恢复成暂停。
    pub fn recover_interrupted(&self) -> Result<()> {
        for task in self.store.list_tasks()? {
            if matches!(
                task.status,
                TaskStatus::Downloading | TaskStatus::Probing | TaskStatus::Queued
            ) {
                self.store
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
