//! 任务模型：引擎与队列之间传递的核心类型。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type TaskId = Uuid;

/// BT 种子里的一个文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BtFileInfo {
    /// 在种子文件列表里的下标。跟库的 `files_to_download` 下标一致。
    pub index: usize,
    /// 相对路径（含种子根目录名）。多文件种子是 `根目录/子目录/文件`。
    pub path: String,
    /// 文件大小（字节）。
    pub length: u64,
}

/// BT 任务抓到的文件列表。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BtFileList {
    /// 种子名。多文件种子是根目录名，单文件种子是文件名。
    pub name: String,
    /// 种子里的全部文件。
    pub files: Vec<BtFileInfo>,
    /// 是否还需要用户勾选。
    ///
    /// 断点续传时之前已经选过了，这时是 false——
    /// 不然每次恢复都弹一次窗。
    pub needs_selection: bool,
}

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

/// 任务是从哪儿来的。
///
/// 起因是 2026-11-19 的事故：浏览器重启时恢复了 7 个几天前没下完的
/// 下载，`downloads.onCreated` 把它们当成新下载，应用这边看不出区别，
/// 于是自动开跑、连弹确认框。
///
/// 现在来源是任务模型的一部分：
///  - `page` / `restored` / `legacy` 直接拒绝，不建任务；
///  - `extension` 建任务但不自动开始、不弹框，等用户自己处理；
///  - 只有 `user` / `userscript` 允许弹保存位置确认框。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskSource {
    /// 用户在页面里点击、右键，或者在主界面手动新建。
    #[default]
    User,
    /// 页面脚本自己发起的下载（没有用户手势）。
    Page,
    /// 浏览器启动时恢复的旧下载。
    Restored,
    /// 扩展接管了，但无法确认是用户手势（兼容/兜底）。
    Extension,
    /// 油猴脚本调用。
    Userscript,
    /// `idmshadow://` 协议唤起。
    Protocol,
    /// 主界面手动新建。
    Ui,
    /// 应用内部探测（检查更新等）。
    Probe,
    /// 老版本客户端没带来源。按不可信处理。
    Legacy,
}

impl TaskSource {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskSource::User => "user",
            TaskSource::Page => "page",
            TaskSource::Restored => "restored",
            TaskSource::Extension => "extension",
            TaskSource::Userscript => "userscript",
            TaskSource::Protocol => "protocol",
            TaskSource::Ui => "ui",
            TaskSource::Probe => "probe",
            TaskSource::Legacy => "legacy",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "user" => Some(TaskSource::User),
            "page" => Some(TaskSource::Page),
            "restored" => Some(TaskSource::Restored),
            "extension" => Some(TaskSource::Extension),
            "userscript" => Some(TaskSource::Userscript),
            "protocol" => Some(TaskSource::Protocol),
            "ui" => Some(TaskSource::Ui),
            "probe" => Some(TaskSource::Probe),
            "legacy" => Some(TaskSource::Legacy),
            _ => None,
        }
    }

    /// 这些来源不允许建任务。
    ///
    /// 页面自动下载和浏览器恢复的下载在用户当时都没有表达过"我要下"，
    /// 接管它们就是这次事故。未知来源也按不可信处理。
    pub fn is_blocked(self) -> bool {
        matches!(
            self,
            TaskSource::Page | TaskSource::Restored | TaskSource::Legacy
        )
    }

    /// 能不能弹保存位置确认框。
    ///
    /// 只有用户明确触发的下载才有资格打断用户。页面自动/恢复/不确定
    /// 手势的下载即使建了任务，也不弹框。
    pub fn may_confirm(self) -> bool {
        matches!(self, TaskSource::User | TaskSource::Userscript)
    }

    /// 建完任务后能不能立刻开始下载。
    pub fn may_autostart(self) -> bool {
        matches!(
            self,
            TaskSource::User
                | TaskSource::Userscript
                | TaskSource::Protocol
                | TaskSource::Ui
                | TaskSource::Probe
        )
    }

    /// 是不是"浏览器自己发起"的来源。这类来源要做图片预览过滤。
    pub fn is_auto_capture(self) -> bool {
        matches!(
            self,
            TaskSource::Page | TaskSource::Restored | TaskSource::Extension
        )
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
    /// 等用户确认保存位置。此时下载可能已经在进行中。
    AwaitingConfirm,
    /// BT 任务已经起来了，但一个 peer 都还没连上，在等资源。
    ///
    /// 这个状态跟 `Queued` 有本质区别：
    /// - `Queued` 是**排队等名额**——前面有别的任务占着位置，
    ///   轮到它的时候会自动开始。
    /// - `WaitingPeers` 是**资源不存在**——tracker 和 DHT 都问过了，
    ///   但没人做这个种子，或者能连上的 peer 手里没有我们缺的块。
    ///
    /// 后者不会自动好转，得等用户的动作（换源）或者卡死检测器出手。
    /// 混用 `Queued` 表示会让用户以为"马上就开始下"，实际是干等。
    WaitingPeers,
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
            TaskStatus::AwaitingConfirm => "awaiting_confirm",
            TaskStatus::WaitingPeers => "waiting_peers",
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
            "awaiting_confirm" => Some(TaskStatus::AwaitingConfirm),
            "waiting_peers" => Some(TaskStatus::WaitingPeers),
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
            TaskStatus::Queued
                | TaskStatus::Probing
                | TaskStatus::Downloading
                | TaskStatus::Retrying
                | TaskStatus::WaitingPeers
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
    /// 任务来源。界面上直接展示，排查误接管时不用再猜。
    pub source: TaskSource,
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
    /// 本任务的连接数覆盖。None = 跟随全局设置。
    pub max_connections: Option<u32>,
    pub error: Option<TaskError>,
    /// 断点续传用的资源指纹。ETag 变化意味着资源已变，不能续传。
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub accepts_ranges: bool,
    /// 下载先写到这个暂存文件，用户确认后才转存到 `dir/filename`。
    /// 与最终位置同卷，改名是原子操作。
    pub staging_path: Option<PathBuf>,
    /// 用户确认的最终保存位置。None = 用默认位置（不需要转存）。
    pub final_path: Option<PathBuf>,
    /// 是否需要用户确认保存位置（IDM 式弹窗流程）。
    pub needs_confirm: bool,
    /// 磁力/种子任务抓到的文件清单。None = 不是 BT 或还没抓。
    pub bt_files: Option<BtFileList>,
    /// 用户勾选要下载的文件下标。
    ///
    /// None = 还没选过（多文件种子会等用户勾选）；
    /// Some(空) 非法，交给上层校验。
    pub bt_selected: Option<Vec<usize>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Task {
    pub fn new(id: TaskId, req: TaskRequest, dir: PathBuf, filename: String) -> Self {
        let now = Utc::now();
        Self {
            id,
            engine: req.engine,
            source: req.source,
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
            needs_confirm: req.needs_confirm,
            bt_files: None,
            bt_selected: None,
            error: None,
            etag: None,
            last_modified: None,
            accepts_ranges: false,
            staging_path: None,
            final_path: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// 目标文件的完整路径。
    pub fn target_path(&self) -> PathBuf {
        self.dir.join(&self.filename)
    }

    /// 实际写入的路径：有暂存文件就写暂存文件，否则写最终位置。
    pub fn write_path(&self) -> PathBuf {
        self.staging_path
            .clone()
            .unwrap_or_else(|| self.target_path())
    }

    /// 暂存文件所在的目录。
    pub fn staging_dir(&self) -> PathBuf {
        self.dir.join(".idmshadow")
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
    /// 单任务连接数覆盖。None = 跟随全局设置。
    pub max_connections: Option<u32>,
    /// 是否需要用户确认保存位置（IDM 式弹窗流程）。
    pub needs_confirm: bool,
    pub engine: EngineKind,
    /// 任务来源。调用方必须如实填写；老客户端缺省按 `Legacy` 处理。
    pub source: TaskSource,
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
            max_connections: None,
            needs_confirm: false,
            engine: EngineKind::Auto,
            source: TaskSource::User,
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

/// URL 末段里那些"看着像文件名、其实不是"的占位词。
///
/// `/api/download`、`/file`、`/index` 这类接口路径的末段根本不是文件名。
/// 拿来当文件名，落盘就是一个光秃秃的 "download"。
const GENERIC_NAMES: &[&str] = &[
    "download", "downloads", "file", "files", "index", "default", "api", "get", "down", "dl",
];

/// 这个末段是占位词吗（不是真文件名）。
pub fn is_generic_name(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    GENERIC_NAMES.contains(&lower.as_str())
}

/// 浏览器还没定好文件名时给的临时名。
///
/// Chrome 在文件名确定之前会先用 "download"、"download (2)" 这种占位名，
/// 正在写的时候还会加 ".crdownload" 后缀。这些都不能当成用户要的文件名——
/// 传给引擎只会让落盘文件变成一个光秃秃的 "download"。
pub fn is_temporary_name(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return true;
    }
    let stem = lower.strip_suffix(".crdownload").unwrap_or(&lower);
    if stem == "download" {
        return true;
    }
    if let Some(rest) = stem.strip_prefix("download (") {
        if let Some(num) = rest.strip_suffix(')') {
            if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

/// 名字里有没有像样的扩展名。
pub fn has_extension(name: &str) -> bool {
    let Some((_, ext)) = name.rsplit_once('.') else {
        return false;
    };
    !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric())
}

/// 根据 Content-Type 猜扩展名。判不出来返回 None（不猜）。
///
/// Content-Type 是服务器给的"事实"，比从 URL 末段瞎猜可靠得多。
/// 但 `application/octet-stream` 这种万能类型不给任何提示，直接放弃。
pub fn extension_for_content_type(content_type: &str) -> Option<&'static str> {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let ext = match ct.as_str() {
        "video/mp4" | "video/mp4v" | "video/x-m4v" => ".mp4",
        "video/webm" => ".webm",
        "video/quicktime" => ".mov",
        "video/x-msvideo" | "video/avi" => ".avi",
        "video/x-matroska" | "video/matroska" => ".mkv",
        "video/mp2t" | "video/mpeg" | "application/mp2t" => ".ts",
        "video/x-ms-wmv" => ".wmv",
        "video/3gpp" => ".3gp",
        "audio/mpeg" | "audio/mp3" => ".mp3",
        "audio/mp4" | "audio/x-m4a" => ".m4a",
        "audio/aac" => ".aac",
        "audio/ogg" => ".ogg",
        "audio/wav" | "audio/x-wav" => ".wav",
        "audio/flac" | "audio/x-flac" => ".flac",
        "image/jpeg" | "image/jpg" | "image/pjpeg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/bmp" | "image/x-ms-bmp" => ".bmp",
        "image/svg+xml" => ".svg",
        "image/avif" => ".avif",
        "image/x-icon" | "image/vnd.microsoft.icon" => ".ico",
        "image/tiff" => ".tiff",
        "application/pdf" => ".pdf",
        "application/zip" | "application/x-zip-compressed" => ".zip",
        "application/x-rar-compressed" | "application/vnd.rar" => ".rar",
        "application/x-7z-compressed" => ".7z",
        "application/gzip" => ".gz",
        "application/vnd.ms-excel" => ".xls",
        "application/msword" => ".doc",
        _ => return None,
    };
    Some(ext)
}

/// URL 看起来是不是一张图片（带不带扩展名都算）。
///
/// 摄像头/NAS/图床的预览接口经常长成 `/api/capture_image`、`/snapshot`，
/// 路径和文件名都没有 `.jpg`，只看后缀会漏。扩展侧已经挡了一层，
/// 这里是引擎侧的兜底：老客户端、或者扩展漏判时还能拦住。
pub fn looks_like_image_url(url: &str) -> bool {
    if url.trim().is_empty() {
        return false;
    }
    let lower = url.to_ascii_lowercase();

    // 拆出路径和查询串（不关心主机名，避免 images.example.com/x.zip
    // 这类主机名把压缩包误判成图片）。
    let without_scheme = lower
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(&lower);
    let path_and_query = match without_scheme.find('/') {
        Some(idx) => &without_scheme[idx..],
        None => "",
    };
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, q),
        None => (path_and_query, ""),
    };

    const IMAGE_EXT: &[&str] = &[
        ".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".svg", ".avif", ".ico", ".tif",
        ".tiff",
    ];
    if IMAGE_EXT.iter().any(|ext| path.ends_with(ext)) {
        return true;
    }

    const IMAGE_SEGMENTS: &[&str] = &[
        "capture_image",
        "capture-image",
        "snapshot",
        "screenshot",
        "thumbnail",
        "thumb",
        "thumbs",
        "preview",
        "cover",
        "poster",
        "avatar",
        "favicon",
        "mjpeg",
        "stream",
        "photo",
        "picture",
        "image",
        "images",
        "img",
    ];
    if path
        .split('/')
        .filter(|seg| !seg.is_empty())
        .any(|seg| IMAGE_SEGMENTS.contains(&seg))
    {
        return true;
    }

    // `?format=jpg`、`?mime=image%2Fpng`
    query.split('&').any(|pair| {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("");
        let value = parts.next().unwrap_or("");
        if !matches!(key, "format" | "ext" | "type" | "mime" | "content-type") {
            return false;
        }
        value.contains("image%2f")
            || value.contains("image/")
            || matches!(
                value,
                "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "avif" | "svg"
            )
    })
}

/// 统一的文件名定夺。
///
/// 优先级：
///   1. Content-Disposition 里的名字——服务器最权威，它说叫什么就叫什么；
///   2. 调用方给的名字（用户自己起的，或浏览器定好的）；
///   3. URL 末段；
///   4. "download"。
///
/// 名字没有扩展名时，按 Content-Type 补一个。
/// 这一步是关键：TikTok 这类平台的地址末段是一串哈希，完全没有扩展名，
/// 只按 URL 猜的话落盘就是一个没有扩展名的文件，播放器不认。
/// Content-Type 是服务器给的"事实"，用它补扩展名最靠谱。
///
/// `provided` 是调用方给的名字。传 `None` 表示"我没名字，你定"。
pub fn resolve_download_filename(
    disposition: Option<&str>,
    provided: Option<&str>,
    url: &str,
    content_type: Option<&str>,
) -> String {
    let ext = content_type.and_then(extension_for_content_type);

    // 1. Content-Disposition
    if let Some(name) = disposition.and_then(filename_from_content_disposition) {
        if !name.is_empty() {
            return with_extension(name, ext);
        }
    }

    // 2. 调用方给的名字
    let provided = provided
        .map(sanitize_filename)
        .filter(|n| !n.is_empty() && !is_temporary_name(n));

    // 3. URL 末段
    let from_url = last_path_segment(url)
        .map(|seg| sanitize_filename(&seg))
        .filter(|n| !n.is_empty() && !is_generic_name(n));

    // 4. 兜底
    let stem = provided
        .or(from_url)
        .unwrap_or_else(|| "download".to_string());

    with_extension(stem, ext)
}

/// URL 路径的最后一个有效段。
///
/// 末尾的斜杠不算（`/video/tos/abc/` 的末段是空串），
/// 这种地址在 TikTok 之类的站点上很常见。
fn last_path_segment(url: &str) -> Option<String> {
    // 先把协议和主机名去掉，只留路径部分。
    //
    // 不去掉的话，"https://a.com/" 这种没有路径的地址
    // 会把主机名 "a.com" 当成文件名。
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let path = match without_query.find("://") {
        Some(idx) => {
            let rest = &without_query[idx + 3..];
            // 跳过主机名（到第一个斜杠为止）
            match rest.find('/') {
                Some(slash) => &rest[slash..],
                None => "",
            }
        }
        None => without_query,
    };
    let trimmed = path.trim_end_matches('/');
    let seg = trimmed.rsplit('/').next()?;
    if seg.is_empty() {
        return None;
    }
    percent_decode(seg)
}

/// 给没有扩展名的名字补一个扩展名。
fn with_extension(name: String, ext: Option<&str>) -> String {
    let Some(ext) = ext else {
        return name;
    };
    if has_extension(&name) {
        return name;
    }
    format!("{name}{ext}")
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
        // RFC 5987 的格式是 charset'language'value，两个单引号都是分隔符。
        let mut parts = rest.splitn(3, '\'');
        let charset = parts.next().unwrap_or("");
        let _language = parts.next().unwrap_or("");
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
    fn temporary_names_are_recognized() {
        assert!(is_temporary_name("download"));
        assert!(is_temporary_name("Download"));
        assert!(is_temporary_name("download (2)"));
        assert!(is_temporary_name("download (12).crdownload"));
        assert!(is_temporary_name(""));
        assert!(is_temporary_name("   "));
        // 真的文件名不该被误判
        assert!(!is_temporary_name("video.mp4"));
        assert!(!is_temporary_name("downloads.mp4"));
        assert!(!is_temporary_name("my-download.zip"));
    }

    #[test]
    fn content_type_gives_extension() {
        assert_eq!(extension_for_content_type("video/mp4"), Some(".mp4"));
        assert_eq!(extension_for_content_type("video/mp4; charset=utf-8"), Some(".mp4"));
        assert_eq!(extension_for_content_type("application/pdf"), Some(".pdf"));
        assert_eq!(extension_for_content_type("application/octet-stream"), None);
        assert_eq!(extension_for_content_type("text/html"), None);
        assert_eq!(extension_for_content_type(""), None);
    }

    #[test]
    fn disposition_wins_over_everything() {
        assert_eq!(
            resolve_download_filename(
                Some("attachment; filename=\"real name.mp4\""),
                Some("from-chrome.mp4"),
                "https://a.com/api/download?url=x",
                Some("video/mp4"),
            ),
            "real name.mp4"
        );
    }

    #[test]
    fn api_download_url_gets_extension_from_content_type() {
        // ClipKnight 的实际形状：末段是 "download"，没有扩展名。
        // 以前落盘就是一个光秃秃的 "download"。
        assert_eq!(
            resolve_download_filename(
                None,
                None,
                "https://clipknight.com/api/download?url=https%3A%2F%2Fx.com%2Fx%2Fstatus%2F1&rank=0",
                Some("video/mp4"),
            ),
            "download.mp4"
        );
    }

    #[test]
    fn hash_url_gets_extension_from_content_type() {
        // TikTok 这类平台的地址末段是一串哈希，完全没有扩展名。
        let url = "https://v16-webapp-prime.us.tiktok.com/video/tos/abc/o4oQxrDpIFABEOByoKEc5cExBj3VfROLqIHHgf/?a=1988";
        assert_eq!(
            resolve_download_filename(None, None, url, Some("video/mp4")),
            "o4oQxrDpIFABEOByoKEc5cExBj3VfROLqIHHgf.mp4"
        );
    }

    #[test]
    fn url_segment_with_extension_is_kept() {
        assert_eq!(
            resolve_download_filename(None, None, "https://a.com/x/clip.webm", Some("video/webm")),
            "clip.webm"
        );
    }

    #[test]
    fn provided_name_survives_when_server_says_nothing() {
        assert_eq!(
            resolve_download_filename(None, Some("myvideo.mp4"), "https://a.com/api/download", None),
            "myvideo.mp4"
        );
    }

    #[test]
    fn provided_name_without_extension_gets_one() {
        assert_eq!(
            resolve_download_filename(None, Some("myvideo"), "https://a.com/api/download", Some("video/mp4")),
            "myvideo.mp4"
        );
    }

    #[test]
    fn temporary_provided_name_is_ignored() {
        // 浏览器给的 "download" 不该压过服务器给的真名。
        assert_eq!(
            resolve_download_filename(
                Some("attachment; filename=\"server.mp4\""),
                Some("download"),
                "https://a.com/api/download",
                Some("video/mp4"),
            ),
            "server.mp4"
        );
    }

    #[test]
    fn nothing_known_falls_back_to_download() {
        assert_eq!(
            resolve_download_filename(None, None, "https://a.com/api/download", None),
            "download"
        );
        // 全空 URL 也不能崩。
        assert_eq!(resolve_download_filename(None, None, "", None), "download");
    }

    #[test]
    fn generic_url_segments_are_not_filenames() {
        assert!(is_generic_name("download"));
        assert!(is_generic_name("DOWNLOAD"));
        assert!(is_generic_name("index"));
        assert!(!is_generic_name("clip.mp4"));
    }

    #[test]
    fn trailing_slash_is_not_a_filename() {
        // TikTok 的地址末尾带斜杠，末段是空串。
        assert_eq!(last_path_segment("https://a.com/video/tos/abc/"), Some("abc".to_string()));
        assert_eq!(last_path_segment("https://a.com/"), None);
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
