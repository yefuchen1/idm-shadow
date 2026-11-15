//! 浏览器扩展的落地与安装引导。
//!
//! Chrome/Edge 没有"默认下载器"这种注册表项，接管下载必须靠扩展。
//! 扩展在商店上架前没法静默装，所以这里做两件事：
//!
//! 1. 把扩展从 exe 里释放到一个固定目录，用户不必去仓库里找；
//! 2. 打开这个目录，并把扩展管理页地址交给界面复制。
//!
//! 浏览器会忽略外部进程传进来的 `chrome://extensions`，所以那一步
//! 只能用户自己粘贴一次地址，这个没法绕过。
//!
//! 文件用 `include_str!` / `include_bytes!` 编译进 exe，安装包不用额外
//! 处理资源，升级后也不会出现"扩展目录还是旧版本"的残留。

use std::path::PathBuf;

const MANIFEST: &str = include_str!("../../extension/manifest.json");
const BACKGROUND: &str = include_str!("../../extension/background.js");
const CONTENT: &str = include_str!("../../extension/content.js");
const OPTIONS_HTML: &str = include_str!("../../extension/options.html");
const OPTIONS_JS: &str = include_str!("../../extension/options.js");
const POPUP_HTML: &str = include_str!("../../extension/popup.html");
const POPUP_JS: &str = include_str!("../../extension/popup.js");
const ICON: &[u8] = include_bytes!("../../extension/icon.png");
const ICON32: &[u8] = include_bytes!("../../extension/icon32.png");

/// 扩展目录：`%APPDATA%\IDM Shadow\extension`。
///
/// 放在应用数据目录而不是安装目录：升级重装会动安装目录，
/// Chrome 里已加载的扩展一旦找不到文件就会被禁用，
/// 用户又得重新加载一遍。
pub fn extension_dir() -> Option<PathBuf> {
    let base = std::env::var("APPDATA").map(PathBuf::from).ok()?;
    Some(base.join("IDM Shadow").join("extension"))
}

/// 把扩展文件写到磁盘。已存在就覆盖，保证内容跟当前 exe 一致。
pub fn materialize() -> std::io::Result<PathBuf> {
    let dir = extension_dir().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "找不到 APPDATA")
    })?;
    std::fs::create_dir_all(&dir)?;

    let files: [(&str, &[u8]); 9] = [
        ("manifest.json", MANIFEST.as_bytes()),
        ("background.js", BACKGROUND.as_bytes()),
        ("content.js", CONTENT.as_bytes()),
        ("options.html", OPTIONS_HTML.as_bytes()),
        ("options.js", OPTIONS_JS.as_bytes()),
        ("popup.html", POPUP_HTML.as_bytes()),
        ("popup.js", POPUP_JS.as_bytes()),
        ("icon.png", ICON),
        ("icon32.png", ICON32),
    ];
    for (name, bytes) in files {
        let target = dir.join(name);
        // 内容没变就不写。重写会改 mtime，Chrome 会当成扩展变了
        // 而重新加载一遍——每次开机都来一次没意义的刷新。
        if std::fs::read(&target).map(|old| old == bytes).unwrap_or(false) {
            continue;
        }
        std::fs::write(&target, bytes)?;
    }
    Ok(dir)
}

/// 启动时把扩展同步到磁盘。
///
/// 每次都写：安装包本身不带扩展目录，用户装完第一次打开应用就得能
/// 在设置里看到它；升级时也靠这里把旧副本覆盖成新版本。
/// 文件只有几十 KB，代价可以忽略。
pub fn sync_on_startup() {
    match materialize() {
        Ok(dir) => tracing::debug!(dir = %dir.display(), "浏览器扩展已同步"),
        Err(err) => tracing::warn!("释放浏览器扩展失败: {err}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Browser {
    Chrome,
    Edge,
}

impl Browser {
    pub fn as_str(self) -> &'static str {
        match self {
            Browser::Chrome => "chrome",
            Browser::Edge => "edge",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Browser::Chrome => "Chrome",
            Browser::Edge => "Edge",
        }
    }

    fn exe(self) -> Option<PathBuf> {
        let candidates: Vec<PathBuf> = match self {
            Browser::Chrome => ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
                .into_iter()
                .filter_map(|var| std::env::var(var).ok())
                .map(|base| {
                    PathBuf::from(base)
                        .join("Google")
                        .join("Chrome")
                        .join("Application")
                        .join("chrome.exe")
                })
                .collect(),
            Browser::Edge => ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"]
                .into_iter()
                .filter_map(|var| std::env::var(var).ok())
                .map(|base| {
                    PathBuf::from(base)
                        .join("Microsoft")
                        .join("Edge")
                        .join("Application")
                        .join("msedge.exe")
                })
                .collect(),
        };
        candidates.into_iter().find(|p| p.is_file())
    }

    /// 扩展管理页地址。只能让用户自己输入：
    /// Chrome/Edge 会丢掉外部进程传进来的 `chrome://` 参数，打不开。
    pub fn extensions_page(self) -> &'static str {
        match self {
            Browser::Chrome => "chrome://extensions/",
            Browser::Edge => "edge://extensions/",
        }
    }
}

/// 这台机器上装了哪些受支持的浏览器。
pub fn installed_browsers() -> Vec<Browser> {
    [Browser::Chrome, Browser::Edge]
        .into_iter()
        .filter(|b| b.exe().is_some())
        .collect()
}

/// 在资源管理器里打开扩展目录。
///
/// 不能直接打开 `chrome://extensions`：Chrome/Edge 会忽略外部进程
/// 传进来的内部协议地址，最多给你开个新标签页。所以改成打开目录，
/// 让用户把文件夹拖进扩展页——这一步浏览器是支持的。
pub fn open_extension_folder() -> Result<(), String> {
    let dir = materialize().map_err(|e| format!("写入扩展文件失败: {e}"))?;
    std::process::Command::new("explorer")
        .arg(&dir)
        .spawn()
        .map_err(|e| format!("打开扩展目录失败: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifest_is_mv3_with_downloads() {
        assert!(MANIFEST.contains("\"manifest_version\": 3"));
        assert!(MANIFEST.contains("\"downloads\""));
    }

    #[test]
    fn browser_strings_are_stable_for_frontend() {
        assert_eq!(Browser::Chrome.as_str(), "chrome");
        assert_eq!(Browser::Edge.as_str(), "edge");
    }
}
