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
- **IDM 式确认流程**：点下载后弹窗问"直接下载 / 另存为"，
  **弹窗期间下载已经在跑**，选定后文件转存到目标位置；取消则删掉已下载的部分。
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

### 关于确认流程的实现

下载先写到 `目录/.idmshadow/<任务id>.part` 暂存文件，用户选好位置后，
下载完成时用 rename 转存到最终位置。暂存目录和最终目录同卷，
所以 10GB 的文件转存也是瞬间完成，不用等。

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

**已知限制：这个库只有主动连接，没有入站 TCP 监听。** 也就是说我们能主动连
别人，别人连不上我们。对 NAT 后面的用户影响比较实际：拿不到 Incoming 连接，
速度会受影响。真要解决得自己加监听，或者换 libtorrent-rasterbar。

另外 BT 生态里不少做种者会拒绝与纯吸血端交换数据，实际可用性要打折。

关于连接数：实测吞吐随连接数近似线性增长，48 连接后触顶。
所以默认给 16，再往上由你自己顶——更多连接确实更快，
但超过服务器并发上限就只会被拒绝或排队。

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

    # 编译桌面应用
    cargo build --release -p idm-shadow-app

    # 前端产物
    cd app/ui && npm install && npm run build && cd ../..

    # 跑起来
    cargo run -p idm-shadow-app

开发时用 `cargo tauri dev` 可以前后端热更新。

## 油猴对接步骤

1. 启动 IDM Shadow，打开「设置」，记下 **bridge 地址** 和 **token**。
2. 装扩展：Chrome 打开 `chrome://extensions`，开启开发者模式，
   「加载已解压的扩展程序」，选 `extension/` 目录。
3. 打开扩展的设置页，填入 bridge 地址与 token，接管方式选「提示后接管」。
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
"浏览器下载 → 扩展接管 → 桌面应用收到任务 → 文件下完"整条链路。

属性测试（proptest）会随机重放分段与续传序列，检查字节区间**永远不重叠、不留洞**。
这是整个项目正确性的地基——分段的 bug 往往不会立刻崩溃，而是表现为文件损坏。

## 当前进度

P1（引擎 + 桌面应用 + 对接）基本完成；P2 是 HLS/m3u8 引擎，P3 是 BT/磁力。
详见 [设计规格](docs/superpowers/specs/2026-11-12-idm-shadow-design.md)。

