//! 自定义协议 `idmshadow://`。
//!
//! `idmshadow://dl?url=<enc>&name=<enc>` 新建下载任务，
//! `idmshadow://` 唤起应用。协议注册写在 `HKCU` 下，不需要管理员权限。

use std::path::Path;

use anyhow::{Context, Result};

/// 协议要传达的动作。
#[derive(Debug, Clone, PartialEq)]
pub enum ProtocolAction {
    Download { url: String, name: Option<String> },
    Show,
}

/// 解析协议链接。
pub fn parse(raw: &str) -> Option<ProtocolAction> {
    let rest = raw
        .strip_prefix("idmshadow://")
        .or_else(|| raw.strip_prefix("idmshadow:"))?;

    let (action, query) = match rest.split_once('?') {
        Some((a, q)) => (a, q),
        None => (rest, ""),
    };

    let action = action.trim_matches('/').to_ascii_lowercase();
    match action.as_str() {
        "" | "show" | "index" => Some(ProtocolAction::Show),
        "dl" | "download" => {
            let params = parse_query(query);
            let url = params.get("url")?.clone();
            if url.is_empty() {
                return None;
            }
            Some(ProtocolAction::Download {
                url,
                name: params.get("name").cloned(),
            })
        }
        _ => None,
    }
}

/// 解析 `a=1&b=2` 形式的查询串，值做百分号解码。
pub fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        out.insert(percent_decode(k), percent_decode(&v.replace('+', " ")));
    }
    out
}

/// 百分号解码。
pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&input[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 百分号编码。
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 注册协议（当前用户）。
pub fn register(exe: &Path) -> Result<()> {
    // 隔离实例（自动化测试）不注册协议。
    //
    // 协议是全局的：写进去就把 idmshadow:// 指向测试用的临时 exe，
    // 用户点油猴脚本里的链接会启动到错的程序上去。
    if crate::system::is_isolated_instance() {
        return Ok(());
    }

    let exe = exe.to_string_lossy().to_string();
    let key = r"HKCU\Software\Classes\idmshadow";

    run_reg(&[
        "add",
        key,
        "/ve",
        "/t",
        "REG_SZ",
        "/d",
        "URL:IDM Shadow Protocol",
        "/f",
    ])?;
    run_reg(&["add", key, "/v", "URL Protocol", "/t", "REG_SZ", "/d", "", "/f"])?;

    let command = format!("\"{}\" \"%1\"", exe);
    run_reg(&[
        "add",
        &format!("{key}\\shell\\open\\command"),
        "/ve",
        "/t",
        "REG_SZ",
        "/d",
        &command,
        "/f",
    ])?;

    Ok(())
}

/// 注销协议。
pub fn unregister() -> Result<()> {
    run_reg(&["delete", r"HKCU\Software\Classes\idmshadow", "/f"])?;
    Ok(())
}

fn run_reg(args: &[&str]) -> Result<()> {
    let output = std::process::Command::new("reg")
        .args(args)
        .output()
        .context("调用 reg 失败")?;
    if !output.status.success() {
        anyhow::bail!(
            "reg 执行失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_download_link() {
        let url = "https://a.com/x%20y.mp4";
        let raw = format!(
            "idmshadow://dl?url={}&name=my%20file.mp4",
            percent_encode(url)
        );
        match parse(&raw) {
            Some(ProtocolAction::Download { url: u, name }) => {
                assert_eq!(u, url);
                assert_eq!(name.as_deref(), Some("my file.mp4"));
            }
            other => panic!("解析失败: {other:?}"),
        }
    }

    #[test]
    fn parses_show_link() {
        assert_eq!(parse("idmshadow://"), Some(ProtocolAction::Show));
        assert_eq!(parse("idmshadow://show"), Some(ProtocolAction::Show));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse("https://a.com/x"), None);
        assert_eq!(parse("idmshadow://dl"), None);
    }

    #[test]
    fn query_parsing() {
        let q = parse_query("a=1&b=hello%20world&c");
        assert_eq!(q.get("a").map(String::as_str), Some("1"));
        assert_eq!(q.get("b").map(String::as_str), Some("hello world"));
        assert_eq!(q.get("c").map(String::as_str), Some(""));
    }
}
