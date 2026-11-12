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
  created_at: string;
  updated_at: string;
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
  confirmTask: (id: string, path?: string) =>
    invoke<void>("confirm_task", { id, path }),
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
