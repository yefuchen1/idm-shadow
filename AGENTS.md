# 本仓库的交付约定

适用于本目录下所有程序：桌面应用、下载引擎、bridge、浏览器扩展、
油猴脚本、安装器与更新工具。

完整流程见本机 skill：
`C:\Users\ROG\.codex\skills\idm-shadow-release\SKILL.md`

只要改动会进入用户手里，就必须按该 skill 执行，不得只改本地文件：

1. 先看现有代码与模式，改完跑对应测试/构建。
2. 递增四处版本号：根 `Cargo.toml`、`app/tauri.conf.json`、
   `package.json`、`app/ui/package.json`。
3. 写 `docs/releases/v<version>.md`，面向用户说明本次改动；正文不要以版本号开头。
4. 构建带 `.sig` 的 NSIS 安装包。
5. 提交并推送 `main`；工作区必须干净。
6. 用 `npm run release:github` 发布到 GitHub Releases，确认
   `releases/latest/download/latest.json` 的版本、说明、下载地址正确。
7. 在真实安装版上验证应用内更新、更新后说明展示、开机自启项。

不变量：

- 唯一更新通道是 GitHub Releases，客户端每次启动检查；
- 安装包必须经过 minisign 签名；
- 私钥、密码、数据库、构建产物和 Release 大文件不得入库；
- 安装包只在干净工作区构建一次；Tauri/NSIS 不保证逐字节可复现，
  发版后不要重建覆盖同名资产；
- `latest.json` 最后上传，且下载地址取自 GitHub 返回的真实资产 URL；
- 更新说明优先从 `docs/releases/v<version>.md` 读取，不要用多行 `--notes`。
