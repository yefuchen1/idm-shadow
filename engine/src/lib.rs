//! IDM Shadow 下载引擎。
//!
//! 本 crate 不依赖任何 UI 框架或 HTTP 路由层，只负责下载这件事本身：
//! 任务模型、字节区间集合、分段与动态重分割、落盘与持久化、限速。

pub mod http;
pub mod bt;
pub mod hls;
pub mod interval;
pub mod limiter;
pub mod manager;
pub mod store;
pub mod task;

pub use interval::IntervalSet;
pub use manager::{DownloadManager, TaskEvent};
pub use store::Store;
pub use task::{EngineKind, Task, TaskError, TaskId, TaskRequest, TaskStatus};
