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

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;

const MANIFEST: &str = include_str!("../../extension/manifest.json");
const BACKGROUND: &str = include_str!("../../extension/background.js");
const CONTENT: &str = include_str!("../../extension/content.js");
const OPTIONS_HTML: &str = include_str!("../../extension/options.html");
const OPTIONS_JS: &str = include_str!("../../extension/options.js");
const POPUP_HTML: &str = include_str!("../../extension/popup.html");
const POPUP_JS: &str = include_str!("../../extension/popup.js");
const ICON: &[u8] = include_bytes!("../../extension/icon.png");
const ICON32: &[u8] = include_bytes!("../../extension/icon32.png");

/// 扩展的名字。浏览器里就是靠这个名字认出"这是 IDM Shadow"。
const EXTENSION_NAME: &str = "IDM Shadow";

/// 应用随包发布的扩展文件。落地和比对都用这一份。
fn shipped_files() -> [(&'static str, &'static [u8]); 9] {
    [
        ("manifest.json", MANIFEST.as_bytes()),
        ("background.js", BACKGROUND.as_bytes()),
        ("content.js", CONTENT.as_bytes()),
        ("options.html", OPTIONS_HTML.as_bytes()),
        ("options.js", OPTIONS_JS.as_bytes()),
        ("popup.html", POPUP_HTML.as_bytes()),
        ("popup.js", POPUP_JS.as_bytes()),
        ("icon.png", ICON),
        ("icon32.png", ICON32),
    ]
}

/// 扩展目录：`%APPDATA%\IDM Shadow\extension`。
///
/// 放在应用数据目录而不是安装目录：升级重装会动安装目录，
/// Chrome 里已加载的扩展一旦找不到文件就会被禁用，
/// 用户又得重新加载一遍。
pub fn extension_dir() -> Option<PathBuf> {
    // 自动化测试要能指定别的目录。
    //
    // 不然测试用的构建会把扩展覆盖到用户真实的 %APPDATA% 里——
    // 用户浏览器里加载的就是那一份，等于用测试构建替换了用户的扩展。
    if let Ok(dir) = std::env::var("IDM_SHADOW_EXTENSION_DIR") {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    let base = std::env::var("APPDATA").map(PathBuf::from).ok()?;
    Some(base.join("IDM Shadow").join("extension"))
}

/// 把扩展文件写到磁盘。已存在就覆盖，保证内容跟当前 exe 一致。
pub fn materialize() -> std::io::Result<PathBuf> {
    let dir = extension_dir().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "找不到 APPDATA")
    })?;
    std::fs::create_dir_all(&dir)?;

    for (name, bytes) in shipped_files() {
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

/// 应用随包发布的扩展版本号。
pub fn shipped_version() -> String {
    serde_json::from_str::<serde_json::Value>(MANIFEST)
        .ok()
        .and_then(|v| {
            v.get("version")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default()
}

/// 浏览器里实际在跑的扩展副本。
///
/// 应用把扩展写到自己的目录，用户再把那个目录以「已解压的扩展程序」加载
/// 进浏览器。但很多人当初加载的是别处的一份拷贝——仓库目录、或者复制到
/// C 盘的那份（路径带中文时 Chromium 起不来，于是很多人这么干）。
///
/// 麻烦在于浏览器会**缓存**已解压扩展的注册信息：文件换了、版本号也换了，
/// 浏览器照样跑着旧代码，扩展的 service worker 甚至起不来。对用户来说
/// 就是"点了下载，一点反应都没有"，而应用这边一切正常——最难查的一种。
///
/// 所以这里把浏览器实际加载的目录扫出来，界面上直接告诉他该去哪里点
/// 「重新加载」。
#[derive(Debug, Clone, Serialize)]
pub struct LoadedCopy {
    /// 浏览器标识（"chrome" / "edge"）。
    pub browser: String,
    /// 浏览器名字，给界面用。
    pub browser_label: String,
    /// 浏览器配置档名（"Default"、"Profile 1"…）。
    pub profile: String,
    /// 扩展目录。
    pub path: String,
    /// 那份副本自己的 manifest 版本号。
    pub version: String,
    /// 目录里的文件和当前发布的对不上（版本旧了，或者被人改过）。
    pub stale: bool,
}

/// 浏览器用户数据目录。
fn user_data_dir(browser: Browser) -> Option<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").ok()?;
    Some(match browser {
        Browser::Chrome => PathBuf::from(local).join("Google").join("Chrome").join("User Data"),
        Browser::Edge => PathBuf::from(local).join("Microsoft").join("Edge").join("User Data"),
    })
}

/// 从浏览器的 "Secure Preferences" 里找出所有「已解压」扩展的目录。
///
/// `location == 4` 表示用户自己加载的已解压扩展；装在浏览器里的那些
/// （location 1/2/3）路径在程序目录里，不是用户能改的。
fn unpacked_extension_paths(prefs: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(prefs) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(settings) = value
        .get("extensions")
        .and_then(|e| e.get("settings"))
        .and_then(|s| s.as_object())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in settings.values() {
        if entry.get("location").and_then(|l| l.as_i64()) != Some(4) {
            continue;
        }
        if let Some(path) = entry.get("path").and_then(|p| p.as_str()) {
            let path = PathBuf::from(path);
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

/// 读一个目录里 manifest.json 的名字和版本。
fn read_manifest(dir: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(dir.join("manifest.json")).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&text).ok()?;
    let name = value.get("name")?.as_str()?.to_string();
    let version = value
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    Some((name, version))
}

/// 目录里的扩展文件和当前发布的是否一致。
fn matches_shipped(dir: &Path) -> bool {
    shipped_files().iter().all(|(name, bytes)| {
        std::fs::read(dir.join(name)).map(|old| old == *bytes).unwrap_or(false)
    })
}

/// 在一个浏览器的用户数据目录里找 IDM Shadow 的副本。
fn scan_user_data(browser: Browser, user_data: &Path) -> Vec<LoadedCopy> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(user_data) else {
        return out;
    };
    for profile in entries.flatten() {
        let dir = profile.path();
        let prefs = dir.join("Secure Preferences");
        if !prefs.is_file() {
            continue;
        }
        let profile_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        for path in unpacked_extension_paths(&prefs) {
            let Some((name, version)) = read_manifest(&path) else {
                continue;
            };
            if name != EXTENSION_NAME {
                continue;
            }
            out.push(LoadedCopy {
                browser: browser.as_str().to_string(),
                browser_label: browser.label().to_string(),
                profile: profile_name.clone(),
                path: path.to_string_lossy().to_string(),
                version,
                stale: !matches_shipped(&path),
            });
        }
    }
    out
}

/// 扫一遍所有受支持的浏览器，找出用户实际加载的 IDM Shadow 副本。
pub fn loaded_copies() -> Vec<LoadedCopy> {
    let mut out = Vec::new();
    for browser in [Browser::Chrome, Browser::Edge] {
        let Some(user_data) = user_data_dir(browser) else {
            continue;
        };
        out.extend(scan_user_data(browser, &user_data));
    }
    out
}

/// 副本扫描的缓存。
///
/// 界面每 4 秒问一次扩展状态，而 "Secure Preferences" 是个几 MB 的文件，
/// 每次都解析一遍太浪费。缓存半分钟，够用。
fn scan_cache() -> &'static Mutex<Option<(Instant, Vec<LoadedCopy>)>> {
    static CACHE: OnceLock<Mutex<Option<(Instant, Vec<LoadedCopy>)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 带缓存的副本列表。
pub fn loaded_copies_cached() -> Vec<LoadedCopy> {
    let mut guard = scan_cache().lock().unwrap_or_else(|e| e.into_inner());
    let fresh = guard
        .as_ref()
        .map(|(at, _)| at.elapsed() < Duration::from_secs(30))
        .unwrap_or(false);
    if !fresh {
        *guard = Some((Instant::now(), loaded_copies()));
    }
    guard
        .as_ref()
        .map(|(_, copies)| copies.clone())
        .unwrap_or_default()
}

/// 把当前发布的文件写进浏览器实际加载的那些副本目录。
///
/// 浏览器缓存了已解压扩展的注册信息，文件写对了还得让用户在扩展管理页
/// 点一次「重新加载」才生效——这一步浏览器不让外部程序代劳。
/// 但至少文件不会再是旧的，用户点一下就能拿到新版本。
pub fn sync_loaded_copies() -> Vec<PathBuf> {
    let own = extension_dir();
    let mut synced = Vec::new();
    for copy in loaded_copies() {
        let dir = PathBuf::from(&copy.path);
        if let Some(own) = own.as_ref() {
            if dir == *own {
                continue;
            }
        }
        if matches_shipped(&dir) {
            continue;
        }
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let mut ok = true;
        for (name, bytes) in shipped_files() {
            let target = dir.join(name);
            if std::fs::read(&target).map(|old| old == bytes).unwrap_or(false) {
                continue;
            }
            if std::fs::write(&target, bytes).is_err() {
                ok = false;
                break;
            }
        }
        if ok {
            synced.push(dir);
        }
    }
    synced
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
    // 浏览器实际加载的那份拷贝也一起更新。
    //
    // 用户常常加载的是别处的拷贝（路径带中文时 Chromium 起不来，
    // 很多人复制到 C 盘再加载）。浏览器不会自己重读已解压扩展的文件，
    // 所以这里先把文件补齐，用户在扩展管理页点一次「重新加载」就生效。
    let synced = sync_loaded_copies();
    if !synced.is_empty() {
        tracing::debug!(
            dirs = ?synced,
            "浏览器里加载的扩展副本已更新"
        );
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

    /// 造一个假的浏览器用户数据目录。
    fn fake_user_data(root: &Path, ext_dir: &Path, version: &str) -> PathBuf {
        let profile = root.join("Default");
        std::fs::create_dir_all(&profile).unwrap();
        let manifest = format!(
            r#"{{"manifest_version": 3, "name": "IDM Shadow", "version": "{version}"}}"#
        );
        std::fs::write(ext_dir.join("manifest.json"), manifest).unwrap();
        let prefs = format!(
            r#"{{"extensions": {{"settings": {{"abc": {{"location": 4, "path": "{}"}}}}}}}}"#,
            ext_dir.to_string_lossy().replace('\\', "\\\\")
        );
        std::fs::write(profile.join("Secure Preferences"), prefs).unwrap();
        root.to_path_buf()
    }

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

    #[test]
    fn shipped_version_matches_the_manifest() {
        assert_eq!(shipped_version(), "0.1.4");
    }

    #[test]
    fn finds_the_copy_the_browser_actually_loaded() {
        let tmp = std::env::temp_dir().join(format!("idm-shadow-scan-{}", uuid::Uuid::new_v4()));
        let root = tmp.join("User Data");
        let ext_dir = tmp.join("idm-shadow-ext");
        std::fs::create_dir_all(&ext_dir).unwrap();
        fake_user_data(&root, &ext_dir, "0.1.0");

        let found = scan_user_data(Browser::Edge, &root);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].browser, "edge");
        assert_eq!(found[0].profile, "Default");
        assert_eq!(found[0].version, "0.1.0");
        // 文件对不上——这正是"浏览器跑着旧代码"的情况。
        assert!(found[0].stale);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn a_current_copy_is_not_stale() {
        let tmp = std::env::temp_dir().join(format!("idm-shadow-scan-{}", uuid::Uuid::new_v4()));
        let root = tmp.join("User Data");
        let ext_dir = tmp.join("idm-shadow-ext");
        std::fs::create_dir_all(&ext_dir).unwrap();
        fake_user_data(&root, &ext_dir, "0.1.4");
        for (name, bytes) in shipped_files() {
            std::fs::write(ext_dir.join(name), bytes).unwrap();
        }

        let found = scan_user_data(Browser::Edge, &root);
        assert_eq!(found.len(), 1);
        assert!(!found[0].stale);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn other_extensions_are_ignored() {
        let tmp = std::env::temp_dir().join(format!("idm-shadow-scan-{}", uuid::Uuid::new_v4()));
        let root = tmp.join("User Data");
        let ext_dir = tmp.join("other-ext");
        std::fs::create_dir_all(&ext_dir).unwrap();
        let profile = root.join("Default");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(
            ext_dir.join("manifest.json"),
            r#"{"manifest_version": 3, "name": "Tampermonkey", "version": "5.5.1"}"#,
        )
        .unwrap();
        let prefs = format!(
            r#"{{"extensions": {{"settings": {{"abc": {{"location": 4, "path": "{}"}}}}}}}}"#,
            ext_dir.to_string_lossy().replace('\\', "\\\\")
        );
        std::fs::write(profile.join("Secure Preferences"), prefs).unwrap();

        assert!(scan_user_data(Browser::Edge, &root).is_empty());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn a_broken_preferences_file_does_not_panic() {
        let tmp = std::env::temp_dir().join(format!("idm-shadow-scan-{}", uuid::Uuid::new_v4()));
        let root = tmp.join("User Data");
        let profile = root.join("Default");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join("Secure Preferences"), "{ not json").unwrap();
        assert!(scan_user_data(Browser::Edge, &root).is_empty());
        std::fs::remove_dir_all(&tmp).ok();
    }

}
