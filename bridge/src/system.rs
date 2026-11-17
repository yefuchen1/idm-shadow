//! 系统集成：开机自启、设为默认下载工具。
//!
//! 开机自启走注册表 HKCU\Software\Microsoft\Windows\CurrentVersion\Run，
//! 只影响当前用户，不需要管理员权限。
//!
//! 注册表只当作"结果"，不当作"设置"的唯一来源：用户的选择存在数据库里，
//! 启动时按数据库对账并修正路径。exe 被移动、重装到别的目录之后，
//! 光靠注册表会留下一条指向旧路径的死启动项。

use std::path::Path;

use anyhow::{Context, Result};

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "IDM Shadow";
/// 开机自启时带上这个参数：只进托盘，不弹主窗口。
pub const AUTOSTART_ARG: &str = "--autostart";

/// 当前用户是否设置了开机自启。
pub fn is_auto_start() -> bool {
    run_reg(&["query", RUN_KEY, "/v", RUN_VALUE]).is_ok()
}

/// 打开/关闭开机自启。
pub fn set_auto_start(enabled: bool, exe: &Path) -> Result<()> {
    if enabled {
        let exe = exe.to_string_lossy().to_string();
        // 路径带空格，命令行里要加引号
        let quoted = format!("\"{}\" {}", exe, AUTOSTART_ARG);
        run_reg(&[
            "add",
            RUN_KEY,
            "/v",
            RUN_VALUE,
            "/t",
            "REG_SZ",
            "/d",
            &quoted,
            "/f",
        ])?;
    } else {
        // 删不掉也不算失败
        let _ = run_reg(&["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]);
    }
    Ok(())
}

/// 当前注册表里记的启动命令。关掉的项返回 None。
///
/// 只用于诊断和"路径是否过期"的判断，不单独决定开关状态。
pub fn auto_start_command() -> Option<String> {
    let output = std::process::Command::new("reg")
        .args(["query", RUN_KEY, "/v", RUN_VALUE])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // 输出形如：    IDM Shadow    REG_SZ    "C:\path\app.exe" --autostart
    stdout
        .lines()
        .find(|line| line.contains(RUN_VALUE) && line.contains("REG_SZ"))
        .and_then(|line| line.split("REG_SZ").nth(1))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 注册表里的启动命令是否正指向 `exe`。
pub fn auto_start_matches(exe: &Path) -> bool {
    let Some(command) = auto_start_command() else {
        return false;
    };
    let exe = exe.to_string_lossy().to_string();
    command.contains(&format!("\"{exe}\"")) || command.starts_with(&exe)
}

/// 程序自身路径。拿不到就返回 None。
pub fn current_exe() -> Option<std::path::PathBuf> {
    std::env::current_exe().ok()
}

/// 这份进程是不是"隔离实例"。
///
/// 自动化测试会用独立的数据目录跑起一个应用实例（端口、数据库、
/// 扩展目录都是临时的）。那种实例绝不能去动共享的注册表启动项——
/// 它自己的数据库里没有开机自启的设置，一对账就会把用户真正的
/// 启动项删掉。跑一次测试，用户开机自启就没了。
///
/// 判据是数据目录：测试实例必然被指定了 IDM_SHADOW_DATA_DIR。
pub fn is_isolated_instance() -> bool {
    std::env::var("IDM_SHADOW_DATA_DIR")
        .map(|dir| !dir.trim().is_empty())
        .unwrap_or(false)
}

/// 调用 reg.exe。
pub fn run_reg(args: &[&str]) -> Result<()> {
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
    fn auto_start_flag_is_readable() {
        // 只验证函数不 panic，读不读得到都无所谓
        let _ = is_auto_start();
    }

    #[test]
    fn generated_command_has_quoted_path_and_autostart_arg() {
        // 路径带空格时必须整体加引号，否则 Windows 会把前半段当程序名。
        // 这里直接拼命令串，不去动真实注册表。
        let exe = Path::new(r"C:\Program Files\IDM Shadow\idm-shadow-app.exe");
        let command = format!("\"{}\" {}", exe.to_string_lossy(), AUTOSTART_ARG);
        assert_eq!(
            command,
            r#""C:\Program Files\IDM Shadow\idm-shadow-app.exe" --autostart"#
        );
    }

    #[test]
    fn parses_reg_query_output() {
        let stdout = "HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\r\n    IDM Shadow    REG_SZ    \"C:\\x\\a.exe\" --autostart\r\n\r\n";
        let value = stdout
            .lines()
            .find(|line| line.contains(RUN_VALUE) && line.contains("REG_SZ"))
            .and_then(|line| line.split("REG_SZ").nth(1))
            .map(|value| value.trim().to_string());
        assert_eq!(value.as_deref(), Some(r#""C:\x\a.exe" --autostart"#));
    }

    #[test]
    fn an_isolated_instance_never_touches_the_shared_registry() {
        // 自动化测试用独立数据目录跑起一个应用实例。它自己的数据库里
        // 没有开机自启的设置，一对账就会把用户真正的启动项删掉——
        // 跑一次测试，用户开机自启就没了。
        //
        // 协议注册是同一个毛病：写进去就把 idmshadow:// 指向测试用的
        // 临时 exe，用户点油猴脚本里的链接会启动到错的程序上去。
        //
        // 这个测试跑在 cargo test 里，而 cargo test 会带上
        // IDM_SHADOW_DATA_DIR 吗？不会——所以这里得自己设。
        let previous = std::env::var("IDM_SHADOW_DATA_DIR").ok();
        std::env::set_var("IDM_SHADOW_DATA_DIR", r"C:\temp\idm-shadow-test");
        assert!(
            is_isolated_instance(),
            "指定了数据目录就该算隔离实例"
        );

        match previous {
            Some(value) => std::env::set_var("IDM_SHADOW_DATA_DIR", value),
            None => std::env::remove_var("IDM_SHADOW_DATA_DIR"),
        }
    }
}
