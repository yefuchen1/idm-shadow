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
- **油猴双通道对接**：
  - Chrome/Edge 扩展：挂钩浏览器下载动作，**现有油猴脚本一个字都不用改**；
  - 油猴桥接脚本：不吃扩展权限的备选通道，也能覆盖 `GM_download`。

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

属性测试（proptest）会随机重放分段与续传序列，检查字节区间**永远不重叠、不留洞**。
这是整个项目正确性的地基——分段的 bug 往往不会立刻崩溃，而是表现为文件损坏。

## 当前进度

P1（引擎 + 桌面应用 + 对接）基本完成；P2 是 HLS/m3u8 引擎，P3 是 BT/磁力。
详见 [设计规格](docs/superpowers/specs/2026-11-12-idm-shadow-design.md)。

