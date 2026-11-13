import { invoke } from "@tauri-apps/api/core";

export type TaskStatus =
  | "queued"
  | "probing"
  | "downloading"
  | "retrying"
  | "paused"
  | "awaiting_confirm"
  | "completed"
  | "error"
  | "cancelled";

export interface TaskDto {
  id: string;
  engine: string;
  url: string;
  filename: string;
  path: string;
  status: TaskStatus;
  total_size: number | null;
  downloaded: number;
  speed: number;
  connections: number;
  max_connections: number;
  error: string | null;
  needs_confirm: boolean;
  /** 产物说明，比如 HLS 合并后是不是标准 mp4。 */
  note: string | null;
  /** 各连接持有的区间，分段可视化用。 */
  segments: SegmentDto[];
  /** BT 任务的分片状态，piece 校验可视化用。 */
  pieces: PieceStatusDto | null;
  /** 文件现在还在不在保存位置。null = 不适用（还没下完）。 */
  file_exists: boolean | null;
  created_at: string;
  updated_at: string;
}

export interface SegmentDto {
  start: number;
  end: number;
  speed: number;
}

export type PieceState = "missing" | "partial" | "verified" | "corrupted";

export interface PieceCellDto {
  /** 这一格覆盖的分片区间（含端点） */
  from: number;
  to: number;
  state: PieceState;
  /** 这一格里最稀缺的那块，有几个 peer 持有 */
  availability: number;
}

export interface PieceStatusDto {
  total: number;
  verified: number;
  corrupted: number;
  /** 累计校验失败次数（重传统计） */
  verification_failures: number;
  /** 降采样后的格子，用来画图 */
  cells: PieceCellDto[];
}

export interface SettingsDto {
  port: number;
  token: string;
  download_dir: string;
  max_connections: number;
  max_concurrent_tasks: number;
  default_download_tool: boolean;
  auto_start: boolean;
}

export const api = {
  listTasks: () => invoke<TaskDto[]>("list_tasks"),
  addTask: (url: string, filename?: string, dir?: string) =>
    invoke<string>("add_task", { url, filename, dir }),
  /** 确认保存位置。dir 是目录（空表示用默认目录），最终路径 = 目录 + 原文件名。 */
  confirmTask: (id: string, dir?: string) =>
    invoke<void>("confirm_task", { id, dir }),
  openDialog: (id: string) => invoke<void>("open_dialog", { id }),
  closeDialog: (id: string) => invoke<void>("close_dialog", { id }),
  openFile: (path: string) => invoke<void>("open_file", { path }),
  showInFolder: (path: string) => invoke<void>("show_in_folder", { path }),
  cancelConfirmedTask: (id: string) =>
    invoke<void>("cancel_confirmed_task", { id }),
  pauseTask: (id: string) => invoke<void>("pause_task", { id }),
  resumeTask: (id: string) => invoke<void>("resume_task", { id }),
  cancelTask: (id: string) => invoke<void>("cancel_task", { id }),
  retryTask: (id: string) => invoke<void>("retry_task", { id }),
  removeTask: (id: string, deleteFile: boolean) =>
    invoke<void>("remove_task", { id, deleteFile }),
  getSettings: () => invoke<SettingsDto>("get_settings"),
  updateSettings: (patch: SettingsPatch) =>
    invoke<void>("update_settings", patch as unknown as Record<string, unknown>),
  setGlobalLimit: (bytesPerSecond: number | null) =>
    invoke<void>("set_global_limit", { bytesPerSecond }),
  revealFile: (path: string) => invoke<void>("reveal_file", { path })
};

export interface SettingsPatch {
  downloadDir?: string;
  globalLimit?: number;
  maxConnections?: number;
  maxConcurrentTasks?: number;
  defaultDownloadTool?: boolean;
  autoStart?: boolean;
}

export function formatSize(bytes: number | null | undefined): string {
  if (bytes === null || bytes === undefined) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let i = 0;
  while (value >= 1024 && i < units.length - 1) {
    value /= 1024;
    i += 1;
  }
  const digits = i === 0 || value >= 10 ? 0 : 1;
  return `${value.toFixed(digits)} ${units[i]}`;
}

export function formatSpeed(bytesPerSecond: number): string {
  if (!bytesPerSecond) return "—";
  return `${formatSize(bytesPerSecond)}/s`;
}

export function formatEta(
  remaining: number | null,
  speed: number
): string {
  if (remaining === null || !speed) return "—";
  const seconds = Math.round(remaining / speed);
  if (seconds < 60) return `${seconds} 秒`;
  if (seconds < 3600) return `${Math.round(seconds / 60)} 分`;
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.round((seconds % 3600) / 60);
  return `${hours} 时 ${minutes} 分`;
}

export const STATUS_LABEL: Record<TaskStatus, string> = {
  queued: "排队中",
  probing: "探测中",
  downloading: "下载中",
  retrying: "重试中",
  paused: "已暂停",
  awaiting_confirm: "待确认",
  completed: "已完成",
  error: "出错",
  cancelled: "已取消"
};
