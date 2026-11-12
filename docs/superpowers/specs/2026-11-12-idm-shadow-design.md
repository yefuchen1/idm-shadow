# IDM Shadow 设计规格

- 日期：2026-11-12
- 状态：设计已确认，进入实施规划
- 目标读者：本项目的实现者

## 1. 目标

复刻 IDM（Internet Download Manager）的核心能力，做成一个 Windows 原生下载工具，
并支持与油猴（Tampermonkey）脚本直接对接：油猴脚本触发的下载能进入本工具，
而不是落到浏览器自带的下载器。

三条硬性要求：

1. 下载器本体是多连接分段、动态重分割、断点续传的 HTTP 下载引擎。
2. 桌面应用是独立 exe（托盘 + 窗口），不是浏览器扩展、不是命令行。
3. 油猴对接有两条通道：Chrome/Edge 扩展（零改动接管）和油猴桥接脚本（可控备选）。

## 2. 非目标（明确不做）

- 视频嗅探（抓取页面媒体流）——后续单独迭代。
- 网盘私有协议（阿里、百度、夸克等）——需要各家授权与加密协议，属另一个产品。
- BT 做种（上传）——P3 只做下载。
- macOS / Linux —— v1 只做 Windows，代码保持可移植。
- 自动更新与安装包分发流水线 —— 自用优先，但结构留好扩展点。

## 3. 范围分期

三个引擎（HTTP / HLS / BT）共享任务抽象、队列调度、存储与 UI，各自实现
`DownloadEngine` trait。

| 阶段 | 内容 | 出口标准 |
|---|---|---|
| P1 | 骨架 + HTTP 多连接引擎 + 桌面应用 + 扩展与桥接脚本对接 | 油猴脚本的直链能真实跑起来，全程可控速、可暂停、可断点续传 |
| P2 | HLS/m3u8 引擎：切片并发拉取、AES-128 解密、合并 mp4 | 一个 m3u8 能下载成一个完整可播放的 mp4 |
| P3 | BT / 磁力：DHT、PEX、Tracker，只下载不做种 | 一个磁力链接能拉起 peer 连接并完成下载 |

本规格聚焦 P1，P2/P3 给出接口约束，不展开实现细节。

## 4. 架构

### 4.1 进程模型

单进程、单 exe。关闭主窗口后托盘常驻，下载继续。

    +---------------------------------------------------------+
    | idmshadow.exe                                            |
    |   +-----------+  tauri::command  +-------------------+   |
    |   | 前端 UI    | ----------------> | app / Tauri 壳    |   |
    |   | React + TS | <-- Tauri Event --| 托盘 · 窗口 · 设置 |   |
    |   +-----------+                    +---------+---------+   |
    |                                             | 直接调用      |
    |   +-----------------------------------------v---------+   |
    |   | bridge/  本地 HTTP API  127.0.0.1:16888           |   |
    |   | 任务 CRUD · 暂停/继续/取消 · 设置 · 健康检查        |   |
    |   +-----------------------------------------+---------+   |
    |                                             |             |
    |   +-----------------------------------------v---------+   |
    |   | engine/  纯 Rust，无 UI 依赖                        |   |
    |   | 任务模型 · 调度器 · 分段引擎 · 动态重分割 · 存储     |   |
    |   +---------------------------------------------------+   |
    +---------------------------------------------------------+
            ^                    ^
            | HTTP API           | HTTP API
       +----+-----+        +-----+------+
       | Chrome扩展|        | 油猴桥接脚本 |
       | 接管下载  |        |  GM_xmlhttp |
       +----------+        +------------+

### 4.2 crate 边界

    idm-shadow/
    +- engine/      纯 Rust。reqwest + tokio + rusqlite。不依赖 UI 与 axum
    +- bridge/      axum HTTP API + 鉴权 + idmshadow:// 协议。只依赖 engine
    +- app/         Tauri 2 壳：托盘、窗口、前端。依赖两者
    +- extension/   Chrome/Edge MV3 扩展（JS）
    +- userscript/  油猴桥接脚本（JS）
    +- docs/

`engine` 不得引用 `bridge` / `app`。将来若要把引擎拆成常驻 daemon，
只需把 `bridge` 移入新二进制，`engine` 与前端不动。

### 4.3 技术栈

- Rust 2021 edition，stable toolchain，MSVC target。
- `tokio`（多线程 runtime）、`reqwest`（rustls，不依赖 OpenSSL/VC++ 运行库）。
- `rusqlite`（bundled SQLite，运行时无外部依赖）。
- `axum`（bridge）、`serde` / `serde_json`。
- `tauri` 2.x + React + TypeScript + Vite（前端）。
- `anyhow` / `thiserror`（错误类型）。

### 4.4 存储

SQLite 文件位于 `%APPDATA%\IDM Shadow\idm-shadow.db`。

| 表 | 内容 |
|---|---|
| `tasks` | 任务元数据：id、url、文件名、目录、引擎类型、总大小、已完成字节、状态、创建/更新时间、错误信息、HTTP 元信息 |
| `intervals` | 已完成的字节区间（task_id, start, end）。断点续传的事实来源 |
| `segments` | 引擎内部的分段状态：task_id、归属 worker、起止位置、状态 |
| `settings` | 端口、token、默认目录、并发上限、限速、引擎参数 |

任务状态机：

    Queued --> Probing --> Downloading --> Completed
                          |  ^   |
                          |  |   +--> Paused
                          v  |   +--> Error(可 retry)
                       Retrying+
                          |
                          +--> Cancelled

## 5. 任务模型

```rust
struct Task {
    id: TaskId,              // uuid v4
    engine: EngineKind,      // Http | Hls | Bt
    url: String,
    filename: String,        // 已 sanitize
    dir: PathBuf,
    status: TaskStatus,
    total_size: Option<u64>,
    downloaded: u64,
    referer: Option<String>,
    user_agent: Option<String>,
    cookies: Option<String>,
    headers: BTreeMap<String, String>,
    max_connections: u32,    // 单任务上限
    error: Option<TaskError>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}
```

`EngineKind::Auto` 时由 probe 结果决定落到 Http 还是 Hls
（URL 以 `.m3u8` 结尾或响应 `Content-Type: application/vnd.apple.mpegurl` 走 Hls）。

### 5.1 引擎 trait

```rust
#[async_trait]
pub trait DownloadEngine: Send + Sync {
    fn kind(&self) -> EngineKind;
    async fn probe(&self, req: &TaskRequest) -> Result<ResourceMeta>;
    async fn download(&self, task: TaskId, meta: ResourceMeta, ctx: EngineCtx)
        -> Result<DownloadOutcome>;
    async fn pause(&self, task: TaskId) -> Result<()>;
    async fn resume(&self, task: TaskId) -> Result<()>;
    async fn cancel(&self, task: TaskId) -> Result<()>;
}
```

`EngineCtx` 提供：进度上报回调、取消信号（`tokio::sync::watch`）、限速器句柄、
storage 句柄。

## 6. HTTP 引擎：多连接与动态重分割

这一节是整个项目的核心。

### 6.1 探测

1. `GET` 带 `Range: bytes=0-0`。
2. 若响应 `206` 且含 `Content-Range: bytes 0-0/<total>`，得到总大小，
   且服务器支持 Range。
3. 记录 `ETag` / `Last-Modified`，续传时用 `If-Range` 校验；
   若续传时 ETag 变了，判定资源已变，任务置为错误并提示重新开始。
4. 服务器忽略 Range（返回 `200` 且完整 body）时，退化为单连接流式下载，
   不支持分段与续传（UI 上标注"服务器不支持分段"）。
5. `Content-Length` 缺失（chunked）时，同样退化为单连接流式下载。

### 6.2 落盘模型

**预分配单文件 + 偏移写**。任务开始时把目标文件长度设为 `total_size`，
每条连接用 `write_at`（seek + write）在自己的偏移写入。

好处：

- 无需"分片文件再合并"这一步；
- 乱序写天然支持，多连接互不干扰；
- 断点续传只需知道"哪些字节已完成"；
- 磁盘占用与最终文件大小一致，不会出现"一半文件占双倍空间"。

已完成的字节用 `IntervalSet`（有序、不相交、可合并的区间集合）表示，
持久化到 `intervals` 表。续传时把未完成区间重新交给引擎。

### 6.3 动态重分割（核心算法）

与 aria2 的"固定分片 + 多 worker 抢"不同，IDM 的分段本身是**动态**的：
慢的连接会被拆开、其份额转交给快的连接；文件尾部（决定完成时刻的关键路径）
会被细分给最快的几个连接。规格如下。

数据结构：

```rust
struct Segment {
    start: u64,
    end: u64,             // 不含
    owner: Option<WorkerId>,
    state: SegmentState,  // Pending | Active | Done
}
```

不变量（必须由 property test 保证）：

- 所有 segment 区间两两不相交；
- 它们的并集恰好等于 `[0, total_size)`（不重叠、不留洞）；
- `Done` segment 的并集等于 `intervals` 表中持久化的内容。

重平衡 tick（默认 2 秒）：

1. 统计每个 worker 的瞬时速率 `v_i`（EMA，衰减系数 alpha = 0.3）。
2. **淘汰**：若 `v_i < 0.15 * v_max` 且存活 worker >= 2，判定该 worker 拖后腿，
   取消它当前的 HTTP 请求，把它未完成的 segment 尾部 `[pos_i, end_i)` 退回 Pending。
3. **关键路径保护**：文件最后一个未完成 segment 若被单个 worker 独占，
   按当前速率比例把它切成最多 4 段，分给速率最高的几个 worker。
4. **拆分规则**：拆分点只能落在该 segment 内，且必须大于持有者当前进度，
   否则会产生空洞或重复下载。
5. **补充**：任一 worker 空闲时，从 Pending 区间中取一段，
   优先靠近文件尾部，长度取「剩余总量 / (worker 数 * 4)」，
   并被限制在 `[min_segment_size, max_segment_size]` 内（默认 1 MiB / 64 MiB）。

每次拆分产生的新 segment 由接收方发起新的 HTTP Range 请求
（`Range: bytes=start-end`）。这就是"动态重分割"的字面含义：
分段边界在下载过程中不断移动。

**无进展检测**：某 worker 连续 15 秒读到 0 字节，按淘汰规则处理。

### 6.4 续传

- 进程退出前，`intervals` 落盘。
- 重启后，对未完成区间重新 probe，带 `If-Range: <etag>`。
- ETag 一致则继续；不一致则任务置 Error，提示"资源已变化，无法续传"。
- 服务器不支持 Range 时无法续传，任务从头开始（UI 需提前告知）。

### 6.5 限速与调度

- 全局限速与单任务限速，各自一个 token bucket（单位 bytes/sec，按补充速率发放令牌）。
- 全局并发 worker 上限（默认 16），单任务连接数上限（默认 8）。
- 调度器按任务 FIFO 取任务，任务内再分配 worker。

### 6.6 重试

| 情况 | 处理 |
|---|---|
| 连接重置 / 超时 | 指数退避重连（1s -> 2s -> 4s -> 8s，上限 30s），最多 5 次 |
| 403 | 带完整 Referer + Cookie 重试一次；仍失败则任务 Error |
| 404 / 410 | 不重试，直接 Error |
| 429 / 503 | 读 `Retry-After`，无则退避重试 |
| 磁盘写入失败 | 暂停全部任务并告警 |

## 7. 对接契约

### 7.1 本地 HTTP API

监听 `127.0.0.1`，默认端口 `16888`（被占用则自动 +1，写入 settings）。
鉴权：请求头 `X-IDM-Shadow-Token`，或 URL 参数 `?token=`（供协议链接使用）。
CORS：允许任意 Origin，方法 GET/POST/PATCH/DELETE/OPTIONS，
允许头 `Content-Type, X-IDM-Shadow-Token`。

`http://127.0.0.1` 属于 Chrome 的可信来源，不受 HTTPS 页面的混合内容拦截，
因此油猴脚本可以直接 `fetch`；`GM_xmlhttpRequest` 更不受 CORS 约束。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/v1/health` | `{ok, version, port}`，扩展用来判断应用是否在跑 |
| POST | `/api/v1/tasks` | 新建任务，返回 `{id, status}` |
| GET | `/api/v1/tasks` | 任务摘要列表 |
| GET | `/api/v1/tasks/:id` | 详情，含 segment 与 worker 实时状态 |
| PATCH | `/api/v1/tasks/:id` | 改文件名、目录、连接数上限 |
| POST | `/api/v1/tasks/:id/pause` \| `/resume` \| `/cancel` \| `/retry` | 控制 |
| DELETE | `/api/v1/tasks/:id` | 删除任务（可选是否删文件） |
| GET | `/api/v1/settings` | 设置（token 脱敏） |
| POST | `/api/v1/settings` | 改设置 |

新建任务请求体：

```json
{
  "url": "https://example.com/file.mp4",
  "filename": "可选，默认取 Content-Disposition",
  "dir": "可选，默认取设置里的下载目录",
  "referer": "https://example.com/page",
  "userAgent": "Mozilla/5.0 ...",
  "cookies": "k=v; k2=v2",
  "headers": { "X-Extra": "1" },
  "maxConnections": 8,
  "engine": "auto|http|hls|bt"
}
```

### 7.2 Chrome/Edge 扩展（MV3）

**接管路径**（零改动接管现有油猴脚本的关键）：

1. 油猴的 `GM_download()` 最终会触发浏览器下载栈。
2. 扩展的 background service worker 监听 `chrome.downloads.onCreated`，
   拿到 `url`、`referrer`、`mime`、`filename`。
3. 用 `chrome.cookies.getAll({url})` 取该 URL 的 Cookie，配合 `navigator.userAgent`
   一起 POST 给 bridge。
4. 按模式处理：
   - **自动接管**：立刻 `chrome.downloads.cancel(id)`，交给本工具下载；
   - **提示接管**：在 popup 或页面内浮层提示，用户点击才接管。

扩展还负责：

- 应用未启动时，用 `idmshadow://` 协议唤起应用后重试投递。
- 角标显示进行中任务数（轮询 `/api/v1/tasks`）。
- 页面内「发送到 IDM Shadow」按钮与右键菜单（`contextMenus`）。

### 7.3 油猴桥接脚本

一个用户脚本，`@connect 127.0.0.1`、`@grant GM_xmlhttpRequest`。

职责：

- 提供 `IDMShadow.download(url, opts)` API：优先 `GM_xmlhttpRequest` POST 到 bridge，
  失败（应用未启动）时回退到 `location.href = 'idmshadow://dl?url=...'`。
- 监听页面内 `<a download>` 点击与 `<video src>` 变化，识别媒体链接，
  在旁边插入「用 IDM Shadow 下载」按钮。
- 识别 `.m3u8` 时路由到 HLS 引擎（`engine: "hls"`）。
- 这是"不吃扩展权限"的备选通道，也是以后加多清晰度选择等策略的落脚点。

### 7.4 自定义协议 `idmshadow://`

| URL | 行为 |
|---|---|
| `idmshadow://dl?url=<enc>&name=<enc>` | 新建下载任务 |
| `idmshadow://` | 唤起应用并显示主窗口 |

`url` / `name` 需 URL 编码。协议由 Tauri 在安装时注册（`tauri-plugin-deep-link`）。

## 8. UI

Tauri 2 窗口 + React/TS 前端。风格取向：信息密集、克制、工具感。

- **主窗口**：任务列表。每行显示图标、文件名、大小进度、速度、剩余时间、状态、连接数。
  顶部工具栏：新建、暂停全部、继续全部、删除、搜索、设置入口。
  底部状态栏：总速度、并发占用、bridge 端口与连接状态。
- **任务详情**：segment 分布可视化（各连接持有的区间与速率）、错误信息、操作按钮。
- **设置**：bridge 端口与 token（带复制按钮）、默认下载目录、默认连接数、全局限速、
  开机自启。
- **托盘**：图标 + 当前总速度文字；菜单 = 显示主窗口 / 暂停全部 / 继续全部 / 设置 / 退出。

数据流：前端通过 `tauri::command` 直接调用 Rust，不绕 HTTP。
进度通过 Tauri Event 推送，节流 4 次/秒，只推变化过的任务。

## 9. 错误处理

- 任务级错误存 `tasks.error`（错误码 + 可读消息），任务进入 Error 状态，
  可 retry。绝不因为单个任务失败而中断整个调度器。
- 桥接 API 返回结构化错误 `{error: {code, message}}`，扩展据此提示用户。
- 应用启动时读 DB，把上次残留的 Downloading 任务恢复为 Paused，
  等用户确认后续传（避免开机就抢带宽）。
- tokio task 内的 panic 被捕获并转成任务级错误，不拖垮进程。

## 10. 测试策略

| 层级 | 内容 |
|---|---|
| 单元 | 区间集合的并/差/合并；重分割算法；token bucket；文件名 sanitize；HTTP 头解析 |
| 属性（proptest） | **任意重分割与续传序列之后，segment 区间仍不重叠、不留洞，Done 集合恰好等于已下载字节数** |
| 集成（本地测试服务器） | 支持 Range 的测试服务器，能限速、能在传输中断开：验证多连接、断点续传、重分割、公平性（慢连接被淘汰后总速率上升） |
| 端到端 | 扩展与桥接脚本对 bridge API 的投递；桌面 UI 手动验证 |

属性测试是本项目正确性的核心保障——分段的 bug 往往不会立刻崩溃，
而是表现为文件损坏或进度显示错乱，必须靠 property test 兜住。

## 11. 里程碑与实施切片

P1 拆成可独立验证的切片：

| 切片 | 内容 | 验证方式 |
|---|---|---|
| S1 | workspace 骨架、任务模型、SQLite 存储、状态机 | 单元测试 |
| S2 | HTTP probe + 单连接下载 + 预分配文件写 | 本地测试服务器 |
| S3 | 多连接分段 + IntervalSet 持久化 | 集成测试 + property test |
| S4 | 动态重分割 + 淘汰 + 关键路径保护 | property test + 速率公平性测试 |
| S5 | 限速、重试、暂停/继续、进程重启续传 | 集成测试 |
| S6 | bridge HTTP API + 鉴权 + CORS | API 测试 |
| S7 | Tauri 壳：托盘、任务列表、详情、设置 | 手动 + 截图 |
| S8 | Chrome 扩展：接管、提示、唤起应用 | 端到端 |
| S9 | 油猴桥接脚本 | 端到端 |

P2 = HLS 引擎（切片并发、AES-128、合并、续传对齐）。
P3 = BT 引擎（`bip` crate，DHT/PEX/Tracker，磁力解析）。

## 12. 已知限制与风险

- **BT 不做种**：部分做种者拒绝与纯吸血端交换数据，实际可用性打折。
- **blob: URL 无法接管**：blob URL 是页面内生命周期的，引擎取不到；
  扩展遇到 blob 下载只能放行给浏览器。
- **服务器不支持 Range**：退化为单连接下载，无法分段也无法续传。
- **资源随时间变化**：ETag 变化时无法续传，任务转为错误。
- **扩展权限敏感**：读取 Cookie 需要 host 权限，首次使用需用户授权。
- **Windows 独占**：托盘与自定义协议注册依赖 Windows API；
  engine / bridge 本身可移植。

