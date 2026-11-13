import { useCallback, useEffect, useMemo, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import {
  Ban,
  Copy,
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
  type TaskStatus
} from "./api";
import { SegmentBar } from "./SegmentBar";
import { PieceBar } from "./PieceBar";

const ACTIVE: TaskStatus[] = ["queued", "probing", "downloading", "retrying"];

/** 任务到了这些状态就不用再问用户保存位置了。 */
const TERMINAL: TaskStatus[] = ["completed", "error", "cancelled"];

/**
 * 确认弹窗里的下载状态。
 *
 * IDM 的行为是"弹窗出现时下载已经在跑了"，所以这里把
 * 已下载 / 总大小 / 速度 / 连接数摆出来，让用户知道
 * 这东西确实在动，不是一个卡住的对话框。
 */
function ConfirmProgress({ task }: { task: TaskDto }) {
  const running = task.status === "downloading" || task.status === "awaiting_confirm";
  if (!running) return null;

  return (
    <div className="confirm-progress">
      <div className="confirm-progress-bar">
        <div
          className="confirm-progress-fill"
          style={{
            width: task.total_size
              ? `${Math.min(100, (task.downloaded / task.total_size) * 100)}%`
              : "0%"
          }}
        />
      </div>
      <span className="confirm-progress-text">
        {task.total_size
          ? `${formatSize(task.downloaded)} / ${formatSize(task.total_size)}`
          : formatSize(task.downloaded)}
        {task.speed > 0 && ` · ${formatSpeed(task.speed)}`}
        {task.connections > 0 && ` · ${task.connections} 连接`}
      </span>
    </div>
  );
}

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
    autoStart: false
  });
  const [deleteTarget, setDeleteTarget] = useState<{
    id: string;
    filename: string;
  } | null>(null);
  const [deleteFile, setDeleteFile] = useState(true);
  const [confirmTarget, setConfirmTarget] = useState<TaskDto | null>(null);
  const [confirmDir, setConfirmDir] = useState("");
  // 已经确认过的任务，不再弹窗
  const [confirmedIds, setConfirmedIds] = useState<Set<string>>(new Set());
  const [detailId, setDetailId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const list = await api.listTasks();
      setTasks(list);
    } catch (err) {
      setError(String(err));
    }
  }, []);

  useEffect(() => {
    refresh();
    api
      .getSettings()
      .then((s) => {
        setSettings(s);
        setDraft({
          downloadDir: s.download_dir,
          globalLimit: "",
          maxConnections: String(s.max_connections),
          maxConcurrentTasks: String(s.max_concurrent_tasks),
          defaultDownloadTool: s.default_download_tool,
          autoStart: s.auto_start
        });
      })
      .catch(() => undefined);

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

  // 有任务在等确认，就弹窗。下载这时候已经在跑了。
  useEffect(() => {
    const pending = tasks.find(
      (t) =>
        t.needs_confirm &&
        !confirmedIds.has(t.id) &&
        // 失败/取消/完成的任务不再问。
        //
        // 之前这里只查 needs_confirm，可任务失败之后这个标记还留在库里，
        // 于是弹窗弹的是那个早就失败的老任务——文件名都是错的，
        // 点"直接下载"会确认错对象。
        !TERMINAL.includes(t.status)
    );
    if (pending && !confirmTarget) {
      setConfirmTarget(pending);
      setConfirmDir(settings?.download_dir ?? "");
    }
  }, [tasks, confirmTarget, settings, confirmedIds]);

  // 弹窗里的下载进度要跟着实时刷新，
  // 否则 confirmTarget 只是一份建弹窗那一刻的快照，数字不会动。
  useEffect(() => {
    if (!confirmTarget) return;
    const fresh = tasks.find((t) => t.id === confirmTarget.id);
    if (!fresh || TERMINAL.includes(fresh.status)) {
      // 弹窗开着的时候任务挂了（下载失败、被取消），
      // 留着弹窗只会让人点一个已经没意义的"直接下载"。
      setConfirmTarget(null);
    } else if (fresh !== confirmTarget) {
      setConfirmTarget(fresh);
    }
  }, [tasks, confirmTarget]);

  async function confirmDownload(useCustomDir: boolean) {
    if (!confirmTarget) return;
    const target = confirmTarget;
    const dir = confirmDir.trim();
    setConfirmTarget(null);
    setConfirmedIds((prev) => new Set(prev).add(target.id));
    await guard(async () => {
      await api.confirmTask(
        target.id,
        useCustomDir && dir ? dir : undefined
      );
    });
  }

  async function cancelDownload() {
    if (!confirmTarget) return;
    const target = confirmTarget;
    setConfirmTarget(null);
    setConfirmedIds((prev) => new Set(prev).add(target.id));
    await guard(() => api.cancelConfirmedTask(target.id));
  }

  async function pickConfirmDir() {
    const picked = await open({
      directory: true,
      multiple: false,
      defaultPath: confirmDir
    });
    if (typeof picked === "string") {
      setConfirmDir(picked);
    }
  }

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
    const picked = await open({
      directory: true,
      multiple: false,
      defaultPath: draft.downloadDir
    });
    if (typeof picked === "string") {
      setDraft((d) => ({ ...d, downloadDir: picked }));
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

    await guard(async () => {
      await api.updateSettings({
        downloadDir: draft.downloadDir,
        globalLimit: limit,
        maxConnections: conns,
        maxConcurrentTasks: tasks,
        defaultDownloadTool: draft.defaultDownloadTool,
        autoStart: draft.autoStart
      });
      const s = await api.getSettings();
      setSettings(s);
      setDraft((d) => ({
        ...d,
        globalLimit: "",
        defaultDownloadTool: s.default_download_tool,
        autoStart: s.auto_start
      }));
    });
  }

  async function confirmDelete() {
    if (!deleteTarget) return;
    const target = deleteTarget;
    setDeleteTarget(null);
    await guard(() => api.removeTask(target.id, deleteFile));
  }

  return (
    <div className="app">
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
        {visible.length === 0 && (
          <div className="empty">
            <p>还没有下载任务</p>
            <span>油猴脚本或浏览器扩展投过来的链接会出现在这里</span>
          </div>
        )}
        {visible.map((task) => {
          const remaining =
            task.total_size !== null ? task.total_size - task.downloaded : null;
          return (
            <div className={`row ${task.status}`} key={task.id}>
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

      {confirmTarget && (
        <div className="modal-backdrop">
          <div className="modal">
            <h2>下载此文件</h2>
            <p className="modal-text">{confirmTarget.filename}</p>
            <ConfirmProgress task={confirmTarget} />
            <div className="confirm-actions">
              <button
                className="btn primary"
                onClick={() => confirmDownload(false)}
              >
                直接下载
              </button>
              <button
                className="btn"
                onClick={() => confirmDownload(true)}
              >
                另存为…
              </button>
            </div>
            <div className="confirm-dir">
              <span className="confirm-dir-label">保存到</span>
              <input
                className="dir-input"
                placeholder={settings?.download_dir ?? "默认下载目录"}
                value={confirmDir}
                onChange={(e) => setConfirmDir(e.target.value)}
              />
              <button
                className="btn small"
                title="浏览文件夹"
                onClick={pickConfirmDir}
              >
                <FolderCog size={13} />
              </button>
            </div>
            <div className="modal-actions">
              <button className="btn" onClick={cancelDownload}>
                取消
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
