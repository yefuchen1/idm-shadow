//! 系统集成：开机自启、设为默认下载工具。
//!
//! 开机自启走注册表 HKCU\Software\Microsoft\Windows\CurrentVersion\Run，
//! 只影响当前用户，不需要管理员权限。

use std::path::Path;

use anyhow::{Context, Result};

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "IDM Shadow";

/// 当前用户是否设置了开机自启。
pub fn is_auto_start() -> bool {
    run_reg(&["query", RUN_KEY, "/v", RUN_VALUE]).is_ok()
}

/// 打开/关闭开机自启。
pub fn set_auto_start(enabled: bool, exe: &Path) -> Result<()> {
    if enabled {
        let exe = exe.to_string_lossy().to_string();
        // 路径带空格，命令行里要加引号
        let quoted = format!("\"{}\"", exe);
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

/// 程序自身路径。拿不到就返回 None。
pub fn current_exe() -> Option<std::path::PathBuf> {
    std::env::current_exe().ok()
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
}
