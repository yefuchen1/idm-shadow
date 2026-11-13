import React, { useCallback, useEffect, useMemo, useState } from "react";
import ReactDOM from "react-dom/client";
import { open } from "@tauri-apps/plugin-dialog";
import { FolderCog } from "lucide-react";
import { api, formatSize, formatSpeed, type TaskDto } from "./api";
import "./styles.css";

/**
 * 下载对话框。
 *
 * 独立的小窗口，不跟任务列表挤在一起。
 *
 * 流程：
 *   1. 刚打开：下载其实已经在跑了，问"立即下载"还是"另存为"
 *   2. 点了之后：这个窗口自己负责把文件转存到目标目录
 *   3. 下载完成：变成"立即打开 / 打开文件所在位置 / 关闭"三个按钮
 */
type Phase = "choose" | "downloading" | "done" | "failed";

function DownloadDialog() {
  const params = new URLSearchParams(window.location.search);
  const taskId = params.get("task") ?? "";

  const [task, setTask] = useState<TaskDto | null>(null);
  const [dir, setDir] = useState("");
  const [defaultDir, setDefaultDir] = useState("");
  const [phase, setPhase] = useState<Phase>("choose");
  const [autoClose, setAutoClose] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api
      .getSettings()
      .then((s) => setDefaultDir(s.download_dir ?? ""))
      .catch(() => {});
  }, []);

  const load = useCallback(async () => {
    if (!taskId) return;
    try {
      const list = await api.listTasks();
      const found = list.find((t) => t.id === taskId) ?? null;
      if (found) setTask(found);
    } catch (err) {
      console.warn("[IDM Shadow] 读任务失败", err);
    }
  }, [taskId]);

  useEffect(() => {
    load();
    const t = window.setInterval(load, 1000);
    return () => window.clearInterval(t);
  }, [load]);

  // 阶段跟着任务状态走。
  //
  // 注意：引擎里"等确认"的任务状态一直是 awaiting_confirm，
  // 哪怕字节早就下完了——因为还不知道该存到哪。所以 awaiting_confirm
  // 归到 choose（还在等用户选位置），剩下的按状态分。
  useEffect(() => {
    if (!task) return;
    let next: Phase;
    if (task.status === "completed") next = "done";
    else if (task.status === "error" || task.status === "cancelled")
      next = "failed";
    else if (task.status === "awaiting_confirm") next = "choose";
    else next = "downloading";
    setPhase((cur) => (cur === next ? cur : next));
  }, [task]);

  // 勾了"自动关闭"就自己关掉。
  //
  // 这个勾必须在下载完成之前就能点上，所以它一直显示在
  // choose / downloading 阶段，而不是完成之后才冒出来。
  useEffect(() => {
    if (phase !== "done" || !autoClose) return;
    const t = window.setTimeout(() => {
      api.closeDialog(taskId);
    }, 500);
    return () => window.clearTimeout(t);
  }, [phase, autoClose, taskId]);

  const speed = task?.speed ?? 0;
  const total = task?.total_size ?? null;
  const downloaded = task?.downloaded ?? 0;
  const percent = total ? Math.min(100, (downloaded / total) * 100) : 0;
  // 字节下完了但还没确认存哪——进度条满了，人还在等选择。
  const bytesDone = total !== null && downloaded >= total;
  // 这个任务实际会存到哪。任务自己带了目录的话（扩展投过来的、
  // 或者在别处新建的），占位符就该显示它自己的目录，
  // 而不是全局默认目录——不然按"立即下载"落地的位置跟占位符对不上。
  const targetDir = useMemo(() => {
    const p = task?.path ?? "";
    const cut = Math.max(p.lastIndexOf("\\"), p.lastIndexOf("/"));
    return cut > 0 ? p.slice(0, cut) : defaultDir;
  }, [task?.path, defaultDir]);

  async function start(useCustomDir: boolean) {
    if (!task) return;
    let target = dir.trim();
    // "另存为"没填目录就先弹选文件夹，省得按了没反应。
    if (useCustomDir && !target) {
      const picked = await pickDir();
      if (!picked) return;
      target = picked;
    }
    setBusy(true);
    try {
      await api.confirmTask(task.id, target || undefined);
      setPhase("downloading");
    } catch (err) {
      console.warn("[IDM Shadow] 确认失败", err);
    } finally {
      setBusy(false);
    }
  }

  async function pickDir(): Promise<string | null> {
    const picked = await open({
      directory: true,
      multiple: false,
      defaultPath: dir || targetDir || undefined
    });
    if (typeof picked === "string") {
      setDir(picked);
      return picked;
    }
    return null;
  }

  async function cancelAll() {
    if (!task) return;
    setBusy(true);
    try {
      await api.cancelConfirmedTask(task.id);
      // 取消成功才关窗。取消失败的话留着窗口，
      // 用户还能看到错误、换个方式处理。
      api.closeDialog(task.id);
    } catch (err) {
      console.warn("[IDM Shadow] 取消失败", err);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="dlg">
      <div className="dlg-head">
        <div className="dlg-title">
          <span className="dlg-filename">{task?.filename ?? "…"}</span>
          <span className="dlg-sub">
            {total
              ? `${formatSize(downloaded)} / ${formatSize(total)}`
              : formatSize(downloaded)}
            {bytesDone && phase === "choose"
              ? " · 已下载完，等待确认保存位置"
              : ""}
          </span>
        </div>
      </div>

      {phase !== "done" && phase !== "failed" && (
        <div className="dlg-progress">
          <div className="dlg-progress-bar">
            <div className="dlg-progress-fill" style={{ width: `${percent}%` }} />
          </div>
          <div className="dlg-progress-info">
            <span>
              {total
                ? `${formatSize(downloaded)} / ${formatSize(total)}`
                : formatSize(downloaded)}
            </span>
            <span className="dlg-progress-stats">
              {speed > 0 && <span>{formatSpeed(speed)}</span>}
              {(task?.connections ?? 0) > 0 && (
                <span>{task?.connections} 连接</span>
              )}
            </span>
          </div>
        </div>
      )}

      {phase === "choose" && (
        <>
          <div className="dlg-actions">
            <button
              className="btn primary"
              disabled={busy}
              onClick={() => start(false)}
            >
              立即下载
            </button>
            <button
              className="btn"
              disabled={busy}
              onClick={() => start(true)}
            >
              另存为…
            </button>
          </div>
          <div className="dlg-dir">
            <span className="dlg-dir-label">保存到</span>
            <input
              className="dir-input"
              placeholder={targetDir || "默认下载目录"}
              value={dir}
              onChange={(e) => setDir(e.target.value)}
            />
            <button
              className="btn small"
              title="浏览文件夹"
              onClick={() => pickDir()}
            >
              <FolderCog size={13} />
            </button>
          </div>
        </>
      )}

      {phase === "done" && task && (
        <>
          <div className="dlg-done">
            <span className="dlg-done-mark">✓</span>
            <span className="dlg-done-text">
              下载完成
              <span className="dlg-done-path" title={task.path}>
                {task.path}
              </span>
            </span>
          </div>
          <div className="dlg-actions">
            <button
              className="btn primary"
              onClick={() => api.openFile(task.path)}
            >
              立即打开
            </button>
            <button className="btn" onClick={() => api.showInFolder(task.path)}>
              打开文件所在位置
            </button>
            <button className="btn" onClick={() => api.closeDialog(task.id)}>
              关闭
            </button>
          </div>
        </>
      )}

      {phase === "failed" && task && (
        <>
          <div className="dlg-done">
            <span className="dlg-done-mark failed">✕</span>
            <span className="dlg-done-text">
              {task.status === "cancelled" ? "已取消" : "下载失败"}
              <span className="dlg-done-path" title={task.error ?? ""}>
                {task.error ?? task.filename}
              </span>
            </span>
          </div>
          <div className="dlg-actions">
            <button
              className="btn primary"
              onClick={() => api.closeDialog(task.id)}
            >
              关闭
            </button>
          </div>
        </>
      )}

      {phase !== "done" && phase !== "failed" && (
        <>
          <label className="dlg-auto">
            <input
              type="checkbox"
              checked={autoClose}
              onChange={(e) => setAutoClose(e.target.checked)}
            />
            <span>下载完成后自动关闭对话框</span>
          </label>
          <div className="dlg-foot">
            <button
              className="btn small ghost"
              disabled={busy}
              onClick={cancelAll}
            >
              取消
            </button>
          </div>
        </>
      )}
    </div>
  );
}

ReactDOM.createRoot(document.getElementById("dlg-root")!).render(
  <React.StrictMode>
    <DownloadDialog />
  </React.StrictMode>
);
