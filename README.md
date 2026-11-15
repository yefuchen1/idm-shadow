# IDM Shadow

复刻 IDM（Internet Download Manager）核心能力的 Windows 下载工具，
并且能和油猴（Tampermonkey）脚本直接对接——油猴触发的下载会进入本工具，
而不是落到浏览器自带的下载器。

## 它能做什么

- **多连接分段下载**：一个文件拆成多段并行拉取，吃满带宽。
- **动态重分割**：慢的连接会被拆开、份额转交给快的连接；文件尾部（关键路径）
  会按速率细分给最快的几条连接。这是 IDM 比普通下载器快的主要原因。
- **断点续传**：已完成区间落盘，关窗口、重启电脑都能接着下。
- **限速**：全局与单任务限速，token 桶实现，对所有连接同时生效。
- **IDM 式确认流程**：点下载后弹出**独立的下载对话框小窗口**，
  问"立即下载 / 另存为"，**弹窗期间下载已经在跑**，选定后文件转存到目标位置；
  下完自动换成"立即打开 / 打开文件所在位置 / 关闭"三个按钮。
  勾了「下载完成后自动关闭对话框」就会下完自己关窗；取消则删掉已下载的部分。
- **下载列表看得见来龙去脉**：每条都显示保存位置，文件被手动删掉或挪走之后
  整条置灰，末尾换成「删除本条」。
- **HLS（m3u8）引擎**：解析播放列表、并发下载分片、AES-128 解密、按序合并成文件。
  URL 以 `.m3u8` 结尾会自动识别。系统装了 ffmpeg 就转封装成标准 mp4。
- **BT / 磁力链接引擎**：DHT、PEX、Tracker 都支持，磁力链接会自动从 DHT
  拿 metadata。只下载不做种（不回传数据给 peer）。
- **油猴双通道对接**：
  - Chrome/Edge 扩展：挂钩浏览器下载动作，**现有油猴脚本一个字都不用改**；
  - 油猴桥接脚本：不吃扩展权限的备选通道，也能覆盖 `GM_download`。

## 设置项

| 设置 | 说明 |
|---|---|
| Bridge 地址 | 桌面应用对外的本地接口地址（默认 `http://127.0.0.1:16888`），油猴脚本和扩展靠它投递任务 |
| Token | 访问接口的口令。本机任何网页都能发请求到 `127.0.0.1`，没 token 的话谁都能往你队列里塞任务 |
| 下载目录 | 新任务的默认保存位置，带文件夹选择按钮 |
| 全局限速 | 所有任务加起来的速度上限（KB/s），令牌桶实现，对所有连接同时生效 |
| 单任务连接数 | 一个文件拆成几条连接并行拉，默认 16，可调 1–64 |
| 同时下载任务数 | 超出上限的任务排队，前面的跑完自动接上（0 = 不限） |
| 默认下载工具 | 勾上后浏览器下载自动交给本应用；**会连带打开开机自启** |
| 开机自启 | 注册表 `HKCU\...\Run`，当前用户即可生效 |
| 更新源 | 默认指向 GitHub Releases；每个版本发布后客户端自动发现 |
| 启动时检查 | 每次打开应用检查一次新版本，默认开 |

## 安装与更新

正式版是 NSIS 安装包，装到 `%LOCALAPPDATA%\IDM Shadow`（当前用户，
不需要管理员权限）。安装包不带开机自启：由应用按设置里的开关写入
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，值为
`"<安装目录>\idm-shadow-app.exe" --autostart`；带这个参数启动只进托盘，不弹主窗口。
每次启动都会按设置对账，安装目录变了会重写路径，被安全软件清掉会补回。

**更新只有一个入口：GitHub Releases。**

- 固定 feed：<https://github.com/yefuchen1/idm-shadow/releases/latest/download/latest.json>
- 客户端每次启动检查（设置里可关），发现新版本在列表顶部提示；
- 安装包必须带 minisign 签名，客户端验签通过才会安装；
- 安装完成后重启，主界面展示本次更新内容，关闭后不再重复。

源码、安装包、更新 feed 都通过 GitHub 管理；大文件走 Release 资产，
不进 Git 历史。

### 发版流程

```powershell
# 1. 改版本号：Cargo.toml / app/tauri.conf.json / package.json / app/ui/package.json
# 2. 构建签名安装包
$env:TAURI_SIGNING_PRIVATE_KEY="$env:USERPROFILE\.tauri\idm-shadow.key"
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD=(Get-Content "$env:USERPROFILE\.tauri\idm-shadow.password" -Raw).Trim()
npm run app:installer

# 3. 写本版说明并提交（发版脚本要求工作区干净）
#    docs/releases/v<version>.md 会同时用作 Release 正文和客户端更新说明
git add -A
git commit -m "release: v0.2.2"
git push origin main

# 4. 发布：自动打 tag、建 Release、上传 exe / .sig / latest.json
$env:GITHUB_TOKEN="<repo 权限的 token>"
npm run release:github
```

发版脚本会校验 `Cargo.toml` 与 `tauri.conf.json` 版本一致、安装包与 `.sig`
都存在；`latest.json` 最后上传，避免客户端读到新 feed 却下不到包。
feed 里的下载地址取自 GitHub 返回的真实资产地址（GitHub 会把资产名里的空格
改写成点号），不要手写文件名拼 URL。

签名私钥在 `%USERPROFILE%\.tauri\idm-shadow.key`，**必须单独备份**。
丢了私钥就无法再给老用户推送可验证的更新，只能让所有人手工重装。
私钥、密码、`*.key.pub` 都不入库（见 `.gitignore`）。

### 关于确认流程的实现

下载先写到 `目录/.idmshadow/<任务id>.part` 暂存文件，用户选好位置后，
下载完成时用 rename 转存到最终位置。暂存目录和最终目录同卷，
所以 10GB 的文件转存也是瞬间完成，不用等。

对话框是独立的小窗口，不跟任务列表挤在一起——列表可能很长，
一个对话框盖在上面两边都别扭。它出现时**下载已经在跑了**
（IDM 就是这个行为），里面显示实时的已下载字节、速度和连接数，
不是一个卡住的对话框。

引擎内部把"用户确认"和"下载结束"这两件事用一把锁串起来：
只有两边都齐了才动暂存文件。否则会出现"文件正被下载引擎写，
同时又被转存走"的撕裂——那种情况文件会直接坏掉。

### 关于 HLS

分片之间互不依赖，所以可以并发下载——这是 HLS 比直链快的原因。
加密支持 AES-128（没给 IV 时用分片序号当 IV，符合规范）。
SAMPLE-AES 之类属于 DRM 范畴，会明确报错而不是产出乱码文件。

产物说明：

| 情况 | 产物 |
|---|---|
| 装了 ffmpeg | 转封装成标准 mp4，任意播放器都能播 |
| 没装 ffmpeg | 原始 TS 流拼接，部分播放器不认（界面会标注） |

### 关于 BT

实现用的是 [`bittorrent-rs`](https://crates.io/crates/bittorrent-rs)（纯 Rust，
DHT / PEX / Tracker / uTP 都有）。原计划的 `bip` crate 编译不了——它要 nightly，
`bip-rs` 那个 `bittorrent` crate 0.2.0 又是个空壳。

这个库本身只有主动连接，没有入站 TCP 监听——我们能主动连别人，别人连不上
我们。对 NAT 后面的用户影响比较实际：拿不到 Incoming 连接，速度会受影响。

我们在 [`engine/src/bt/listener.rs`](engine/src/bt/listener.rs) 里补上了入站
这一半：自己起 TCP 监听（默认 6881，端口被占用就往后找），接受别人连过来的
连接。

另外 BT 生态里不少做种者会拒绝与纯吸血端交换数据，实际可用性要打折。

关于连接数：实测吞吐随连接数近似线性增长，48 连接后触顶。
所以默认给 16，再往上由你自己顶——更多连接确实更快，
但超过服务器并发上限就只会被拒绝或排队。

### 关于分片校验可视化

BT 任务下面那条分片条，一格代表一段 piece：

| 颜色 | 意思 |
|---|---|
| 绿 | 已经拿到并通过 SHA1 校验 |
| 浅绿 | 一格里有的下好了有的没有 |
| 红 | 校验失败过（坏块），正在重传 |
| 灰 | 还没拿到 |

鼠标悬停能看到这一段的分片区间，以及其中最稀缺的那块有几个 peer 持有
（越少说明越难下，值得优先拿）。

大种子有两万多个分片，逐个往界面推太重，所以引擎那边先降采样成
最多 240 格，每格取区间里最"坏"的状态。精确的"已校验 / 坏块 / 待下载"
数字仍然是准确的。

坏块位置靠对比前后两次的本地 bitfield 定位（某块本来有、这轮没了，
就是刚被判为坏块），累计重传次数取库里的校验池统计。

## 已知限制

- **blob: URL 无法接管**：blob URL 是页面内生命周期的，引擎取不到，
  扩展遇到这种下载只能放行给浏览器。
- **服务器不支持 Range 时退化为单连接**：无法分段，也无法断点续传。
- **资源随时间变化**：ETag 变了就没法续传，任务转为错误。

## 目录结构

    idm-shadow/
    ├─ engine/       纯 Rust 下载引擎：任务模型、分段、动态重分割、SQLite 存储
    ├─ bridge/       本地 HTTP API（127.0.0.1:16888）+ idmshadow:// 协议
    ├─ app/          Tauri 2 桌面应用：托盘 + 主窗口 + 前端
    ├─ extension/    Chrome/Edge MV3 扩展
    ├─ userscript/   油猴桥接脚本
    └─ docs/         设计规格与实施计划

`engine` 不依赖 UI 框架和路由层。将来要拆成常驻 daemon，把 `bridge` 挪出去就行，
引擎和前端一行不用改。

## 构建与运行

前置：Rust（MSVC）、Node.js、Visual Studio Build Tools（C++ 工作负载 + Windows SDK）、
WebView2 运行时。

    # 一条命令搞定（前端 + 后端）
    npm run app:build

    # 跑起来
    npm run app

或者手动来（顺序不能反）：

    cd app/ui && npm install && npm run build && cd ../..
    cargo build --release -p idm-shadow-app
    ./target/release/idm-shadow-app.exe

**界面加载不出来的话，八成是忘了先构建前端。**

这个应用的界面是打包进 exe 的。如果直接 `cargo run -p idm-shadow-app`
而 `app/ui/dist` 不存在或者过期，你会看到窗口里是
「无法访问此页面 / localhost 拒绝连接」——那不是应用崩了，
是界面根本没加载出来，浏览器组件在报错。

开发时想要前后端热更新，用 `cargo tauri dev`（需要 tauri CLI），
它会自己起开发服务器并连上去。

## 油猴对接步骤

1. 启动 IDM Shadow，打开「设置」，记下 **bridge 地址** 和 **token**。
2. 装扩展：Chrome 打开 `chrome://extensions`，开启开发者模式，
   「加载已解压的扩展程序」，选 `extension/` 目录。
3. 打开扩展的设置页，填入 bridge 地址与 token。接管方式默认是
   **「直接接管」**：浏览器点下载就直接弹出 IDM Shadow 自己的下载对话框，
   不会再在浏览器右下角弹那个「用 IDM Shadow 下载？」的气泡。
   接管不了的下载（blob: 地址、已经是本地文件）直接交给浏览器，不再打扰。
   想让桌面应用里的「默认下载工具」开关说了算，就切到「跟随应用设置」。
4. 或者装油猴脚本：`userscript/idm-shadow-bridge.user.js`，
   在脚本菜单里点「IDM Shadow 设置」填入同样的地址与 token。

装好之后，油猴脚本的下载按钮就会走进 IDM Shadow，浏览器的下载列表不再有动静。

## 命令行接口

bridge 的 HTTP API（`X-IDM-Shadow-Token` 头或 `?token=` 参数鉴权）：

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/v1/health` | 应用是否在跑 |
| POST | `/api/v1/tasks` | 新建任务 |
| GET | `/api/v1/tasks` | 任务列表 |
| GET | `/api/v1/tasks/:id` | 任务详情 |
| POST | `/api/v1/tasks/:id/pause` \| `/resume` \| `/cancel` \| `/retry` | 控制 |
| DELETE | `/api/v1/tasks/:id?delete_file=true` | 删除 |

自定义协议：`idmshadow://dl?url=<编码后的地址>&name=<文件名>`。

## 测试

    cargo test --workspace

浏览器侧（扩展与油猴脚本）用 Node 跑：

    npm test

一条命令跑全部：

    npm run test:all

扩展的浏览器实机验证（需要系统装了 Edge/Chrome）：

    npm run test:e2e

它会真的起一个 Edge、加载扩展、触发一次浏览器下载，验证
"浏览器下载 → 扩展接管 → 桌面应用弹窗等确认 → 确认后转存完成"整条链路。

属性测试（proptest）会随机重放分段与续传序列，检查字节区间**永远不重叠、不留洞**。
这是整个项目正确性的地基——分段的 bug 往往不会立刻崩溃，而是表现为文件损坏。

## 当前进度

P1（引擎 + 桌面应用 + 对接）完成；P2 是 HLS/m3u8 引擎，P3 是 BT/磁力，
都已实现。IDM 式确认流程、分段可视化、BT 分片校验可视化都已落地。
详见 [设计规格](docs/superpowers/specs/2026-11-12-idm-shadow-design.md)。

