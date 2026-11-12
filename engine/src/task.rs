//! 任务模型：引擎与队列之间传递的核心类型。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type TaskId = Uuid;

/// 任务使用的引擎。`Auto` 由 probe 结果决定落到 Http 或 Hls。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    Auto,
    Http,
    Hls,
    Bt,
}

impl EngineKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EngineKind::Auto => "auto",
            EngineKind::Http => "http",
            EngineKind::Hls => "hls",
            EngineKind::Bt => "bt",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(EngineKind::Auto),
            "http" => Some(EngineKind::Http),
            "hls" => Some(EngineKind::Hls),
            "bt" => Some(EngineKind::Bt),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Probing,
    Downloading,
    Retrying,
    Paused,
    Completed,
    Error,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Probing => "probing",
            TaskStatus::Downloading => "downloading",
            TaskStatus::Retrying => "retrying",
            TaskStatus::Paused => "paused",
            TaskStatus::Completed => "completed",
            TaskStatus::Error => "error",
            TaskStatus::Cancelled => "cancelled",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(TaskStatus::Queued),
            "probing" => Some(TaskStatus::Probing),
            "downloading" => Some(TaskStatus::Downloading),
            "retrying" => Some(TaskStatus::Retrying),
            "paused" => Some(TaskStatus::Paused),
            "completed" => Some(TaskStatus::Completed),
            "error" => Some(TaskStatus::Error),
            "cancelled" => Some(TaskStatus::Cancelled),
            _ => None,
        }
    }

    /// 任务是否仍占用调度名额。
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            TaskStatus::Queued | TaskStatus::Probing | TaskStatus::Downloading | TaskStatus::Retrying
        )
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Completed | TaskStatus::Cancelled | TaskStatus::Error
        )
    }
}

/// 任务级错误。错误永远属于单个任务，不会中断整个调度器。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskError {
    pub code: String,
    pub message: String,
}

impl TaskError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// 已持久化的任务。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub engine: EngineKind,
    pub url: String,
    pub filename: String,
    pub dir: PathBuf,
    pub status: TaskStatus,
    pub total_size: Option<u64>,
    pub downloaded: u64,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    pub cookies: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub max_connections: u32,
    pub error: Option<TaskError>,
    /// 断点续传用的资源指纹。ETag 变化意味着资源已变，不能续传。
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub accepts_ranges: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Task {
    pub fn new(id: TaskId, req: TaskRequest, dir: PathBuf, filename: String) -> Self {
        let now = Utc::now();
        Self {
            id,
            engine: req.engine,
            url: req.url,
            filename,
            dir,
            status: TaskStatus::Queued,
            total_size: None,
            downloaded: 0,
            referer: req.referer,
            user_agent: req.user_agent,
            cookies: req.cookies,
            headers: req.headers,
            max_connections: req.max_connections,
            error: None,
            etag: None,
            last_modified: None,
            accepts_ranges: false,
            created_at: now,
            updated_at: now,
        }
    }

    /// 目标文件的完整路径。
    pub fn target_path(&self) -> PathBuf {
        self.dir.join(&self.filename)
    }

    /// 已下载比例（0.0 ~ 1.0）。总大小未知时返回 0。
    pub fn progress(&self) -> f64 {
        match self.total_size {
            Some(total) if total > 0 => (self.downloaded as f64 / total as f64).clamp(0.0, 1.0),
            _ => 0.0,
        }
    }

    /// 剩余字节数。
    pub fn remaining(&self) -> Option<u64> {
        self.total_size.map(|t| t.saturating_sub(self.downloaded))
    }
}

/// 新建任务的请求。字段全部来自外部输入（扩展 / 油猴 / UI），
/// 因此 filename 必须经过 sanitize，url 必须经过校验。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRequest {
    pub url: String,
    pub filename: Option<String>,
    pub dir: Option<String>,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    pub cookies: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub max_connections: u32,
    pub engine: EngineKind,
}

impl Default for TaskRequest {
    fn default() -> Self {
        Self {
            url: String::new(),
            filename: None,
            dir: None,
            referer: None,
            user_agent: None,
            cookies: None,
            headers: BTreeMap::new(),
            max_connections: 8,
            engine: EngineKind::Auto,
        }
    }
}

impl TaskRequest {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }
}

/// 文件名清洗：去掉路径分隔符与 Windows 非法字符，限制长度。
pub fn sanitize_filename(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();

    // 去掉首尾的空白与点（Windows 不允许以点或空格结尾）
    let trimmed = cleaned.trim().trim_matches('.').trim();

    if trimmed.is_empty() {
        return "download".to_string();
    }

    // 保留 UTF-8 边界的前提下截断到 180 字节
    truncate_utf8(trimmed, 180)
}

fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// 从 `Content-Disposition` 中解析文件名。解析不出来返回 None。
pub fn filename_from_content_disposition(value: &str) -> Option<String> {
    // filename*=UTF-8''... 优先
    let lower = value.to_ascii_lowercase();
    if let Some(idx) = lower.find("filename*") {
        let rest = &value[idx + "filename*".len()..];
        let rest = rest.trim_start().trim_start_matches('=').trim();
        // 形如 UTF-8''%E4%B8%AD%E6%96%87.txt
        let mut parts = rest.splitn(2, '\'');
        let charset = parts.next().unwrap_or("");
        let encoded = parts.next().unwrap_or("");
        let decoded = percent_decode(encoded);
        if let Some(name) = decoded {
            let name = name.trim();
            if !name.is_empty() {
                let _ = charset;
                return Some(sanitize_filename(name));
            }
        }
    }

    if let Some(idx) = lower.find("filename") {
        let rest = &value[idx + "filename".len()..];
        let rest = rest.trim_start().trim_start_matches('=').trim();
        let raw = if let Some(stripped) = rest.strip_prefix('"') {
            stripped.split('"').next().unwrap_or("")
        } else {
            rest.split(';').next().unwrap_or("").trim()
        };
        let raw = raw.trim();
        if !raw.is_empty() {
            return Some(sanitize_filename(raw));
        }
    }

    None
}

/// 百分号解码（UTF-8）。
fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            let v = u8::from_str_radix(hex, 16).ok()?;
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// 从 URL 里猜一个文件名，作为 Content-Disposition 缺失时的兜底。
pub fn filename_from_url(url: &str) -> Option<String> {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let last = without_query.rsplit('/').next()?;
    if last.is_empty() {
        return None;
    }
    let decoded = percent_decode(last)?;
    let name = sanitize_filename(&decoded);
    if name.is_empty() || name == "download" {
        None
    } else {
        Some(name)
    }
}

/// 目标文件已存在时，追加 `(1)`、`(2)` 之类的后缀。
pub fn unique_path(dir: &Path, filename: &str) -> PathBuf {
    let base = Path::new(filename);
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| filename.to_string());
    let ext = base
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();

    let candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }

    for n in 1..1000 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem} (999){ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_removes_illegal_chars() {
        assert_eq!(sanitize_filename("a/b\\c:d*e?f.txt"), "a_b_c_d_e_f.txt");
        assert_eq!(sanitize_filename("  ..name..  "), "name");
        assert_eq!(sanitize_filename(""), "download");
        assert_eq!(sanitize_filename("..."), "download");
    }

    #[test]
    fn sanitize_truncates_on_char_boundary() {
        let name = "中".repeat(100);
        let out = sanitize_filename(&name);
        assert!(out.len() <= 180);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn content_disposition_quoted() {
        assert_eq!(
            filename_from_content_disposition("attachment; filename=\"movie.mp4\""),
            Some("movie.mp4".to_string())
        );
    }

    #[test]
    fn content_disposition_rfc5987() {
        assert_eq!(
            filename_from_content_disposition("attachment; filename*=UTF-8''%E4%B8%AD%E6%96%87.txt"),
            Some("中文.txt".to_string())
        );
    }

    #[test]
    fn content_disposition_bare() {
        assert_eq!(
            filename_from_content_disposition("inline; filename=clip.webm"),
            Some("clip.webm".to_string())
        );
    }

    #[test]
    fn filename_from_url_uses_last_segment() {
        assert_eq!(
            filename_from_url("https://a.com/x/y/file.bin?token=1"),
            Some("file.bin".to_string())
        );
        assert_eq!(filename_from_url("https://a.com/"), None);
    }

    #[test]
    fn progress_and_remaining() {
        let mut task = Task::new(
            Uuid::new_v4(),
            TaskRequest::new("https://a.com/x"),
            PathBuf::from("C:/dl"),
            "x.bin".to_string(),
        );
        task.total_size = Some(100);
        task.downloaded = 25;
        assert_eq!(task.progress(), 0.25);
        assert_eq!(task.remaining(), Some(75));
    }
}
