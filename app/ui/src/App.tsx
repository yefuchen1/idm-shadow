import { useCallback, useEffect, useMemo, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  Ban,
  Copy,
  FolderOpen,
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
  type TaskDto,
  type TaskStatus
} from "./api";

const ACTIVE: TaskStatus[] = ["queued", "probing", "downloading", "retrying"];

export default function App() {
  const [tasks, setTasks] = useState<TaskDto[]>([]);
  const [settings, setSettings] = useState<SettingsDto | null>(null);
  const [url, setUrl] = useState("");
  const [query, setQuery] = useState("");
  const [showSettings, setShowSettings] = useState(false);
  const [limit, setLimit] = useState("");
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
    api.getSettings().then(setSettings).catch(() => undefined);

    const unlistenProgress = listen<{
      id: string;
      downloaded: number;
      total: number | null;
      speed: number;
      connections: number;
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
                connections: p.connections
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

    const timer = window.setInterval(refresh, 1500);
    return () => {
      window.clearInterval(timer);
      unlistenProgress.then((fn) => fn());
      unlistenStatus.then((fn) => fn());
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

  async function applyLimit() {
    const parsed = limit.trim() === "" ? null : Number(limit.trim());
    if (parsed !== null && (Number.isNaN(parsed) || parsed < 0)) {
      setError("限速值不对");
      return;
    }
    await guard(() => api.setGlobalLimit(parsed));
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
            <label>全局限速</label>
            <input
              className="limit-input"
              placeholder="不限速"
              value={limit}
              onChange={(e) => setLimit(e.target.value)}
            />
            <span className="unit">KB/s</span>
            <button className="btn small" onClick={applyLimit}>
              应用
            </button>
          </div>
          <div className="settings-row">
            <label>下载目录</label>
            <code>{settings.download_dir}</code>
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
          const percent =
            task.total_size && task.total_size > 0
              ? Math.min(100, (task.downloaded / task.total_size) * 100)
              : 0;
          const remaining =
            task.total_size !== null ? task.total_size - task.downloaded : null;
          return (
            <div className={`row ${task.status}`} key={task.id}>
              <div className="col-name">
                <span className="filename" title={task.path}>
                  {task.filename}
                </span>
                <span className="suburl" title={task.url}>
                  {task.url}
                </span>
              </div>
              <div className="col-size">
                <div className="bar">
                  <div className="bar-fill" style={{ width: `${percent}%` }} />
                </div>
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
                  onClick={() => guard(() => api.removeTask(task.id, false))}
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
    </div>
  );
}
