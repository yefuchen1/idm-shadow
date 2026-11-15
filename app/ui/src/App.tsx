import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import {
  Ban,
  Copy,
  Download,
  FolderInput,
  FolderOpen,
  FolderCog,
  Pause,
  Play,
  Plus,
  RefreshCw,
  Settings as SettingsIcon,
  Trash2,
  X
} from "lucide-react";
import {
  api,
  formatEta,
  formatSize,
  formatSpeed,
  STATUS_LABEL,
  type SettingsDto,
  type SegmentDto,
  type PieceStatusDto,
  type TaskDto,
  type TaskStatus,
  type UpdateInfo,
  type UpdateProgress,
  type ReleaseNotes
} from "./api";
import { SegmentBar } from "./SegmentBar";
import { PieceBar } from "./PieceBar";

const ACTIVE: TaskStatus[] = ["queued", "probing", "downloading", "retrying"];

/** 任务到了这些状态就不用再问用户保存位置了。 */
const TERMINAL: TaskStatus[] = ["completed", "error", "cancelled"];

export default function App() {
  const [tasks, setTasks] = useState<TaskDto[]>([]);
  const [url, setUrl] = useState("");
  const [query, setQuery] = useState("");
  const [showSettings, setShowSettings] = useState(false);
  const [settings, setSettings] = useState<SettingsDto | null>(null);
  const [draft, setDraft] = useState({
    downloadDir: "",
    globalLimit: "",
    maxConnections: "16",
    maxConcurrentTasks: "0",
    defaultDownloadTool: false,
    autoStart: false,
    updateEndpoint: "",
    updateCheckOnStart: true
  });
  // 更新：检查结果、检查中、下载进度、错误都在这一块里。
  const [updateInfo, setUpdateInfo] = useState<UpdateInfo | null>(null);
  const [updateChecked, setUpdateChecked] = useState(false);
  const [checkingUpdate, setCheckingUpdate] = useState(false);
  const [installingUpdate, setInstallingUpdate] = useState(false);
  const [updateProgress, setUpdateProgress] = useState<UpdateProgress | null>(
    null
  );
  // 更新后第一次启动展示一次"本次更新内容"。
  const [releaseNotes, setReleaseNotes] = useState<ReleaseNotes | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<{
    id: string;
    filename: string;
  } | null>(null);
  const [deleteFile, setDeleteFile] = useState(true);
  // 已经开过下载对话框的任务，不重复开
  const [dialogOpenIds, setDialogOpenIds] = useState<Set<string>>(new Set());
  // 用户主动把对话框关掉、还没确认的任务。
  // 这些不再自动弹回来，想重选就去列表里点"选择保存位置"。
  const [dismissedIds, setDismissedIds] = useState<Set<string>>(new Set());
  // 待确认的下载攒了一堆时，不一个接一个地弹，
  // 改��列表顶上给一条提示，用户点了才逐个处理。
  const [manualQueue, setManualQueue] = useState(false);
  const [detailId, setDetailId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  // 事件监听器要读最新的任务表，但不想因为 tasks 变就重新订阅，
  // 所以用 ref 存一份。
  const tasksRef = useRef<TaskDto[]>([]);

  const refresh = useCallback(async () => {
    try {
      const list = await api.listTasks();
      setTasks(list);
    } catch (err) {
      setError(String(err));
    }
  }, []);

  // 事件监听器要读最新的任务表，但不想因为 tasks 变就重新订阅，
  // 所以用 ref 存一份。
  useEffect(() => {
    tasksRef.current = tasks;
  }, [tasks]);

  useEffect(() => {
    refresh();
    api
      .getSettings()
      .then((s) => {
        setSettings(s);
        if (s.release_notes) setReleaseNotes(s.release_notes);
        setDraft({
          downloadDir: s.download_dir,
          globalLimit: "",
          maxConnections: String(s.max_connections),
          maxConcurrentTasks: String(s.max_concurrent_tasks),
          defaultDownloadTool: s.default_download_tool,
          autoStart: s.auto_start,
          updateEndpoint: s.update_endpoint,
          updateCheckOnStart: s.update_check_on_start
        });
      })
      .catch(() => undefined);

    // 启动后静默检查发现的新版本，以及安装进度。
    const unlistenUpdate = listen<UpdateInfo>("update-available", (event) => {
      setUpdateInfo(event.payload);
      setUpdateChecked(true);
    });
    const unlistenUpdateProgress = listen<UpdateProgress>(
      "update-progress",
      (event) => {
        setUpdateProgress(event.payload);
        if (event.payload.phase === "installed") {
          setInstallingUpdate(false);
        }
      }
    );

    const unlistenProgress = listen<{
      id: string;
      downloaded: number;
      total: number | null;
      speed: number;
      connections: number;
      note: string | null;
      segments: SegmentDto[];
      pieces: PieceStatusDto | null;
    }>("task-progress", (event) => {
      const p = event.payload;
      setTasks((prev) =>
        prev.map((t) =>
          t.id === p.id
            ? {
                ...t,
                downloaded: p.downloaded,
                total_size: p.total,
                speed: p.speed,
                connections: p.connections,
                note: p.note ?? t.note,
                segments: p.segments ?? t.segments,
                pieces: p.pieces ?? t.pieces
              }
            : t
        )
      );
    });

    const unlistenStatus = listen<{ id: string; status: TaskStatus }>(
      "task-status",
      (event) => {
        setTasks((prev) =>
          prev.map((t) =>
            t.id === event.payload.id
              ? { ...t, status: event.payload.status }
              : t
          )
        );
      }
    );

    // 有任务在等确认：立刻拉一次任务列表。
    // 不等轮询（1.5 秒一次）——浏览器点了下载之后，
    // 用户盯着屏幕等弹窗，这个延迟是能感觉到的。
    const unlistenConfirm = listen<{ id: string }>(
      "task-confirm-needed",
      () => {
        refresh();
      }
    );

    const timer = window.setInterval(refresh, 1500);
    return () => {
      window.clearInterval(timer);
      unlistenProgress.then((fn) => fn());
      unlistenStatus.then((fn) => fn());
      unlistenConfirm.then((fn) => fn());
      unlistenUpdate.then((fn) => fn());
      unlistenUpdateProgress.then((fn) => fn());
    };
  }, [refresh]);

  const totals = useMemo(() => {
    let speed = 0;
    let connections = 0;
    let active = 0;
    for (const t of tasks) {
      if (ACTIVE.includes(t.status)) {
        active += 1;
        speed += t.speed;
        connections += t.connections;
      }
    }
    return { speed, connections, active };
  }, [tasks]);

  const visible = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return tasks;
    return tasks.filter(
      (t) =>
        t.filename.toLowerCase().includes(q) || t.url.toLowerCase().includes(q)
    );
  }, [tasks, query]);

  // 有任务在等确认，就给它开一个独立的下载对话框小窗口。
  //
  // 以前是在主窗口里弹模态框——主窗口是任务列表，可能很长，
  // 一个对话框盖在上面两边都别扭。现在分成两个窗口：
  // 列表归列表，对话框归对话框。
  //
  // 同时只开一个。以前这里写的是 find(还没开过窗口的待确认任务)，
  // 结果每轮轮询都跳过已经开着的那个、去找下一个，
  // 待确认任务一多就一个接一个地开窗口，而且谁都没被关掉——
  // 渲染进程能涨到一百多个。
  //
  // 待确认的队列（按创建顺序）。攒了一堆的时候不自动弹，
  // 列表顶上给一条提示，用户点了才逐个处理。
  const pendingQueue = useMemo(
    () =>
      tasks.filter(
        (t) =>
          t.needs_confirm &&
          // 用户已经把这个对话框关掉了，别再弹回来。
          // 想重新选保存位置，去列表里点那个按钮。
          !dismissedIds.has(t.id) &&
          // 失败/取消/完成的任务不再问。
          //
          // 之前这里只查 needs_confirm，可任务失败之后这个标记还留在库里，
          // 于是弹窗弹的是那个早就失败的老任务——文件名都是错的，
          // 点"直接下载"会确认错对象。
          !TERMINAL.includes(t.status)
      ),
    [tasks, dismissedIds]
  );

  useEffect(() => {
    const pending = pendingQueue[0];
    // 已经开着就别重复开。
    if (!pending || dialogOpenIds.has(pending.id)) return;
    // 攒了一堆的时候不自动弹——一个接一个地弹太烦。
    // 队列里只有一个（正常下载）就照常弹。
    if (pendingQueue.length > 1 && !manualQueue) return;
    setDialogOpenIds((prev) => new Set(prev).add(pending.id));
    api.openDialog(pending.id).catch((err) => {
      console.warn("[IDM Shadow] 打开下载对话框失败", err);
      setDialogOpenIds((prev) => {
        const next = new Set(prev);
        next.delete(pending.id);
        return next;
      });
    });
  }, [tasks, dialogOpenIds, dismissedIds, pendingQueue, manualQueue]);

  // 任务不再等确认了（确认了/失败了/取消了），把标记摘掉。
  useEffect(() => {
    for (const t of tasks) {
      if (!t.needs_confirm && dialogOpenIds.has(t.id)) {
        setDialogOpenIds((prev) => {
          const next = new Set(prev);
          next.delete(t.id);
          return next;
        });
      }
    }
  }, [tasks, dialogOpenIds]);

  // 对话框窗口被关掉了（用户点 X，或者代码主动关的）。
  //
  // 如果这个任务还没确认，说明是用户主动把它关掉的——
  // 那就记下来别再自动弹（否则关一张弹一张，没完没了），
  // 但队列里的下一个待确认任务可以接着弹。
  useEffect(() => {
    let unlisten: () => void;
    listen<{ id: string }>("dlg-closed", (event) => {
      const closedId = event.payload.id;
      setDialogOpenIds((prev) => {
        if (!prev.has(closedId)) return prev;
        const next = new Set(prev);
        next.delete(closedId);
        return next;
      });
      const stillPending = tasksRef.current.find((t) => t.id === closedId);
      if (stillPending?.needs_confirm) {
        setDismissedIds((prev) => new Set(prev).add(closedId));
      }
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  }, []);

  // 队列清空了就把手动模式关掉，下次又恢复自动弹。
  useEffect(() => {
    if (pendingQueue.length === 0) setManualQueue(false);
  }, [pendingQueue.length]);

  async function submit() {
    const value = url.trim();
    if (!value) return;
    setError(null);
    try {
      await api.addTask(value);
      setUrl("");
      await refresh();
    } catch (err) {
      setError(String(err));
    }
  }

  async function guard(fn: () => Promise<unknown>) {
    setError(null);
    try {
      await fn();
      await refresh();
    } catch (err) {
      setError(String(err));
    }
  }

  async function copy(text: string) {
    await navigator.clipboard.writeText(text);
  }

  async function pickFolder() {
    try {
      const picked = await open({
        directory: true,
        multiple: false,
        defaultPath: draft.downloadDir
      });
      if (typeof picked === "string") {
        setDraft((d) => ({ ...d, downloadDir: picked }));
      }
    } catch (err) {
      // 静默失败会让人以为按钮坏了，把原因摆出来。
      console.warn("[IDM Shadow] 选文件夹失败", err);
      setError(String(err));
    }
  }

  async function saveSettings() {
    const conns = Number(draft.maxConnections);
    if (!Number.isInteger(conns) || conns < 1 || conns > 64) {
      setError("连接数要在 1 到 64 之间");
      return;
    }
    const tasks = Number(draft.maxConcurrentTasks);
    if (!Number.isInteger(tasks) || tasks < 0) {
      setError("同时下载任务数不能为负数");
      return;
    }
    const limit =
      draft.globalLimit.trim() === "" ? 0 : Number(draft.globalLimit.trim());
    if (Number.isNaN(limit) || limit < 0) {
      setError("限速值不对");
      return;
    }
    const endpoint = draft.updateEndpoint.trim();
    if (endpoint && !/^https:\/\//i.test(endpoint)) {
      setError("正式版更新源必须是 https；http 仅用于本地自测");
      return;
    }

    await guard(async () => {
      await api.updateSettings({
        downloadDir: draft.downloadDir,
        globalLimit: limit,
        maxConnections: conns,
        maxConcurrentTasks: tasks,
        defaultDownloadTool: draft.defaultDownloadTool,
        autoStart: draft.autoStart,
        updateEndpoint: endpoint,
        updateCheckOnStart: draft.updateCheckOnStart
      });
      const s = await api.getSettings();
      setSettings(s);
      setDraft((d) => ({
        ...d,
        globalLimit: "",
        defaultDownloadTool: s.default_download_tool,
        autoStart: s.auto_start,
        updateEndpoint: s.update_endpoint,
        updateCheckOnStart: s.update_check_on_start
      }));
    });
  }

  async function checkUpdate() {
    setError(null);
    setCheckingUpdate(true);
    try {
      const info = await api.checkUpdate(draft.updateEndpoint.trim() || undefined);
      setUpdateInfo(info);
      setUpdateChecked(true);
    } catch (err) {
      setError(String(err));
    } finally {
      setCheckingUpdate(false);
    }
  }

  async function installUpdate() {
    setError(null);
    setInstallingUpdate(true);
    setUpdateProgress(null);
    try {
      await api.installUpdate(draft.updateEndpoint.trim() || undefined);
    } catch (err) {
      setInstallingUpdate(false);
      setError(String(err));
    }
  }

  async function confirmDelete() {
    if (!deleteTarget) return;
    const target = deleteTarget;
    setDeleteTarget(null);
    await guard(() => api.removeTask(target.id, deleteFile));
  }

  return (
    <div className="app">
      {releaseNotes && (
        <div className="release-notes">
          <div className="release-notes-head">
            <strong>已更新到 {releaseNotes.version}</strong>
            <button
              className="btn icon"
              title="关闭"
              onClick={() => {
                setReleaseNotes(null);
                api.dismissReleaseNotes().catch(() => undefined);
              }}
            >
              <X size={14} />
            </button>
          </div>
          <p>{releaseNotes.notes}</p>
        </div>
      )}
      <header className="titlebar">
        <div className="brand">
          <span className="logo" />
          <strong>IDM Shadow</strong>
        </div>
        <div className="titlebar-stats">
          <span className="stat">
            <span className="stat-label">总速度</span>
            <span className="stat-value">{formatSpeed(totals.speed)}</span>
          </span>
          <span className="stat">
            <span className="stat-label">连接</span>
            <span className="stat-value">{totals.connections}</span>
          </span>
          <span className="stat">
            <span className="stat-label">进行中</span>
            <span className="stat-value">{totals.active}</span>
          </span>
        </div>
      </header>

      <div className="toolbar">
        <div className="url-row">
          <input
            className="url-input"
            placeholder="粘贴直链地址，回车开始下载"
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") submit();
            }}
          />
          <button className="btn primary" onClick={submit}>
            <Plus size={15} />
            新建
          </button>
        </div>
        <div className="toolbar-actions">
          <button
            className="btn"
            title="暂停全部"
            onClick={() =>
              guard(async () => {
                for (const t of tasks.filter((x) => ACTIVE.includes(x.status))) {
                  await api.pauseTask(t.id);
                }
              })
            }
          >
            <Pause size={15} />
          </button>
          <button
            className="btn"
            title="继续全部"
            onClick={() =>
              guard(async () => {
                for (const t of tasks.filter((x) => x.status === "paused")) {
                  await api.resumeTask(t.id);
                }
              })
            }
          >
            <Play size={15} />
          </button>
          <button className="btn" title="刷新" onClick={refresh}>
            <RefreshCw size={15} />
          </button>
          <button
            className="btn"
            title="设置"
            onClick={() => setShowSettings((v) => !v)}
          >
            <SettingsIcon size={15} />
          </button>
        </div>
      </div>

      {showSettings && settings && (
        <div className="settings">
          <div className="settings-row">
            <label>Bridge 地址</label>
            <code>http://127.0.0.1:{settings.port}</code>
            <button className="btn small" onClick={() => copy(`http://127.0.0.1:${settings.port}`)}>
              <Copy size={13} />
            </button>
          </div>
          <div className="settings-row">
            <label>Token</label>
            <code className="token">{settings.token}</code>
            <button className="btn small" onClick={() => copy(settings.token)}>
              <Copy size={13} />
            </button>
          </div>
          <div className="settings-row">
            <label>下载目录</label>
            <input
              className="dir-input"
              placeholder="选择保存位置"
              value={draft.downloadDir}
              onChange={(e) =>
                setDraft((d) => ({ ...d, downloadDir: e.target.value }))
              }
            />
            <button
              className="btn small"
              title="浏览文件夹"
              onClick={pickFolder}
            >
              <FolderCog size={13} />
            </button>
          </div>
          <div className="settings-row">
            <label>全局限速</label>
            <input
              className="limit-input"
              placeholder="不限速"
              value={draft.globalLimit}
              onChange={(e) =>
                setDraft((d) => ({ ...d, globalLimit: e.target.value }))
              }
            />
            <span className="unit">KB/s</span>
          </div>
          <div className="settings-row">
            <label>单任务连接数</label>
            <input
              className="limit-input"
              type="number"
              min={1}
              max={64}
              value={draft.maxConnections}
              onChange={(e) =>
                setDraft((d) => ({ ...d, maxConnections: e.target.value }))
              }
            />
            <span className="unit">1–64</span>
          </div>
          <div className="settings-row">
            <label>同时下载任务数</label>
            <input
              className="limit-input"
              type="number"
              min={0}
              max={64}
              value={draft.maxConcurrentTasks}
              onChange={(e) =>
                setDraft((d) => ({ ...d, maxConcurrentTasks: e.target.value }))
              }
            />
            <span className="unit">0 = 不限</span>
          </div>
          <div className="settings-row">
            <label>默认下载工具</label>
            <label className="switch">
              <input
                type="checkbox"
                checked={draft.defaultDownloadTool}
                onChange={(e) =>
                  setDraft((d) => ({
                    ...d,
                    defaultDownloadTool: e.target.checked,
                    // 默认下载工具意味着开机就在后台待命
                    autoStart: e.target.checked ? true : d.autoStart
                  }))
                }
              />
              <span>浏览器下载自动交给 IDM Shadow</span>
            </label>
          </div>
          <div className="settings-row">
            <label>开机自启</label>
            <label className="switch">
              <input
                type="checkbox"
                checked={draft.autoStart}
                onChange={(e) =>
                  setDraft((d) => ({ ...d, autoStart: e.target.checked }))
                }
              />
              <span>开机后自动在后台运行</span>
            </label>
          </div>
          <div className="settings-row">
            <label>更新源</label>
            <input
              className="dir-input"
              placeholder="留空使用 GitHub Releases"
              value={draft.updateEndpoint}
              onChange={(e) =>
                setDraft((d) => ({ ...d, updateEndpoint: e.target.value }))
              }
            />
            <button
              className="btn small"
              disabled={checkingUpdate}
              onClick={checkUpdate}
            >
              <RefreshCw size={13} />
              {checkingUpdate ? "检查中" : "检查"}
            </button>
          </div>
          <div className="settings-row">
            <label>启动时检查</label>
            <label className="switch">
              <input
                type="checkbox"
                checked={draft.updateCheckOnStart}
                onChange={(e) =>
                  setDraft((d) => ({
                    ...d,
                    updateCheckOnStart: e.target.checked
                  }))
                }
              />
              <span>发现新版本只在界面提示，不自动安装</span>
            </label>
          </div>
          <div className="settings-row update-row">
            <label>版本</label>
            <span className="update-version">
              {settings.version}
              <span className="unit">
                {checkingUpdate
                  ? " · 正在检查更新"
                  : updateInfo
                    ? ` · 发现 ${updateInfo.version}`
                    : updateChecked
                      ? " · 已是最新"
                      : " · 启动时自动检查"}
              </span>
            </span>
            <button
              className="btn small primary"
              disabled={installingUpdate || checkingUpdate}
              title={
                updateInfo
                  ? `下载并安装 ${updateInfo.version}`
                  : "检查 GitHub Releases 上有没有新版本"
              }
              onClick={updateInfo ? installUpdate : checkUpdate}
            >
              {updateInfo ? <Download size={13} /> : <RefreshCw size={13} />}
              {installingUpdate
                ? "安装中"
                : updateInfo
                  ? "立即更新"
                  : checkingUpdate
                    ? "检查中"
                    : "检查更新"}
            </button>
          </div>
          {updateInfo?.notes && (
            <div className="settings-row update-row">
              <label>更新说明</label>
              <span className="update-notes">{updateInfo.notes}</span>
            </div>
          )}
          {updateProgress?.phase === "download" && (
            <div className="settings-row update-row">
              <label>下载更新</label>
              <span className="update-notes">
                {updateProgress.total
                  ? `${formatSize(updateProgress.downloaded ?? 0)} / ${formatSize(
                      updateProgress.total
                    )}`
                  : formatSize(updateProgress.downloaded ?? 0)}
              </span>
            </div>
          )}
          <div className="settings-row">
            <button className="btn primary" onClick={saveSettings}>
              保存设置
            </button>
          </div>
        </div>
      )}

      {error && (
        <div className="error-bar">
          <span>{error}</span>
          <button className="btn small" onClick={() => setError(null)}>
            <X size={13} />
          </button>
        </div>
      )}

      <div className="search-row">
        <input
          className="search-input"
          placeholder="搜索文件名或地址"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
      </div>

      <div className="list-head">
        <span className="col-name">文件</span>
        <span className="col-size">进度</span>
        <span className="col-speed">速度</span>
        <span className="col-eta">剩余</span>
        <span className="col-status">状态</span>
        <span className="col-actions" />
      </div>

      <div className="list">
        {updateInfo && (
          <div className="update-banner">
            <div className="update-banner-text">
              <strong>发现新版本 {updateInfo.version}</strong>
              {updateInfo.notes && <span>{updateInfo.notes}</span>}
            </div>
            <button
              className="btn small primary"
              disabled={installingUpdate}
              onClick={installUpdate}
            >
              <Download size={13} />
              {installingUpdate ? "安装中" : "立即更新"}
            </button>
            <button
              className="btn icon"
              title="稍后"
              onClick={() => setUpdateInfo(null)}
            >
              <X size={14} />
            </button>
          </div>
        )}
        {pendingQueue.length > 1 && (
          <div className="queue-banner">
            <span>
              有 {pendingQueue.length} 个下载等确认保存位置
            </span>
            <button
              className="btn small"
              onClick={() => setManualQueue(true)}
            >
              逐个处理
            </button>
          </div>
        )}
        {visible.length === 0 && (
          <div className="empty">
            <p>还没有下载任务</p>
            <span>油猴脚本或浏览器扩展投过来的链接会出现在这里</span>
          </div>
        )}
        {visible.map((task) => {
          const remaining =
            task.total_size !== null ? task.total_size - task.downloaded : null;
          // 文件不见了（被手动删了，或者换了机器）。
          // 这种条目置灰，末尾换成"删除本条"。
          const fileGone = task.file_exists === false;
          return (
            <div
              className={`row ${task.status}${fileGone ? " file-gone" : ""}`}
              key={task.id}
            >
              <div className="col-name">
                <span
                  className="filename"
                  title="点击查看各连接明细"
                  onClick={() =>
                    setDetailId((cur) => (cur === task.id ? null : task.id))
                  }
                >
                  {task.filename}
                </span>
                <span className="suburl" title={task.url}>
                  {task.url}
                </span>
                <span className="subpath" title={task.path}>
                  {task.path}
                </span>
                {fileGone && (
                  <span className="subpath gone-note">文件已不在保存位置</span>
                )}
                {detailId === task.id && task.segments.length > 0 && (
                  <div className="conn-list">
                    {task.segments.map((seg, i) => (
                      <span
                        key={i}
                        className="conn-chip"
                        title={`区间 ${formatSize(seg.start)} – ${formatSize(seg.end)}`}
                      >
                        {formatSpeed(seg.speed)}
                      </span>
                    ))}
                  </div>
                )}
              </div>
              <div className="col-size">
                <SegmentBar
                  segments={task.segments}
                  total={task.total_size}
                  downloaded={task.downloaded}
                />
                {task.engine === "bt" && task.pieces && (
                  <PieceBar pieces={task.pieces} />
                )}
                <span className="subtle">
                  {formatSize(task.downloaded)}
                  {task.total_size !== null
                    ? ` / ${formatSize(task.total_size)}`
                    : ""}
                </span>
              </div>
              <div className="col-speed">{formatSpeed(task.speed)}</div>
              <div className="col-eta">{formatEta(remaining, task.speed)}</div>
              <div className="col-status">
                <span className={`badge ${task.status}`}>
                  {STATUS_LABEL[task.status]}
                </span>
                {task.connections > 0 && (
                  <span className="subtle">{task.connections} 连接</span>
                )}
                {task.error && (
                  <span className="subtle error-text" title={task.error}>
                    {task.error}
                  </span>
                )}
                {task.note && (
                  <span className="subtle note-text" title={task.note}>
                    {task.note}
                  </span>
                )}
              </div>
              <div className="col-actions">
                {fileGone ? (
                  // 文件已经没了，"在文件夹中显示"和"删除文件"都没意义，
                  // 直接给一个"删除本条"。
                  <button
                    className="btn small"
                    title="删除本条记录"
                    onClick={() => guard(() => api.removeTask(task.id, false))}
                  >
                    删除本条
                  </button>
                ) : (
                  <>
                {task.needs_confirm && !TERMINAL.includes(task.status) && (
                  // 还在等用户选保存位置。
                  // 对话框窗口被用户用 X 关掉时，从这里能再叫回来。
                  <button
                    className="btn icon"
                    title="选择保存位置"
                    onClick={() => {
                      // 之前被用户关掉过的话，解除"不再自动弹"的标记，
                      // 不然这个按钮点了也不会有反应。
                      setDismissedIds((prev) => {
                        if (!prev.has(task.id)) return prev;
                        const next = new Set(prev);
                        next.delete(task.id);
                        return next;
                      });
                      setDialogOpenIds((prev) => new Set(prev).add(task.id));
                      guard(() => api.openDialog(task.id));
                    }}
                  >
                    <FolderInput size={14} />
                  </button>
                )}
                {task.status === "downloading" || task.status === "queued" ? (
                  <button
                    className="btn icon"
                    title="暂停"
                    onClick={() => guard(() => api.pauseTask(task.id))}
                  >
                    <Pause size={14} />
                  </button>
                ) : task.status === "paused" || task.status === "error" ? (
                  <button
                    className="btn icon"
                    title="继续"
                    onClick={() => guard(() => api.resumeTask(task.id))}
                  >
                    <Play size={14} />
                  </button>
                ) : null}
                <button
                  className="btn icon"
                  title="在文件夹中显示"
                  onClick={() => api.revealFile(task.path)}
                >
                  <FolderOpen size={14} />
                </button>
                <button
                  className="btn icon"
                  title="删除任务"
                  onClick={() => {
                    setDeleteTarget({ id: task.id, filename: task.filename });
                    setDeleteFile(true);
                  }}
                >
                  <Trash2 size={14} />
                </button>
                {task.status !== "cancelled" && task.status !== "completed" && (
                  <button
                    className="btn icon"
                    title="取消"
                    onClick={() => guard(() => api.cancelTask(task.id))}
                  >
                    <Ban size={14} />
                  </button>
                )}
                  </>
                )}
              </div>
            </div>
          );
        })}
      </div>

      <footer className="statusbar">
        <span>{tasks.length} 个任务</span>
        <span className="dot-sep">·</span>
        <span>bridge 127.0.0.1:{settings?.port ?? "-"}</span>
      </footer>

      {deleteTarget && (
        <div className="modal-backdrop" onClick={() => setDeleteTarget(null)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h2>确定删除吗？</h2>
            <p className="modal-text">{deleteTarget.filename}</p>
            <label className="switch modal-check">
              <input
                type="checkbox"
                checked={deleteFile}
                onChange={(e) => setDeleteFile(e.target.checked)}
              />
              <span>是否删除已下载的缓存文件</span>
            </label>
            <div className="modal-actions">
              <button className="btn" onClick={() => setDeleteTarget(null)}>
                取消
              </button>
              <button className="btn danger" onClick={confirmDelete}>
                删除
              </button>
            </div>
          </div>
        </div>
      )}

    </div>
  );
}
