//! Talos IPC 共享内存通信库
//!
//! 纯 Rust 实现的零拷贝共享内存 IPC，与 C++ talos-cpp 兼容。
//! 不依赖 bevy 或任何游戏引擎，可用于独立工具和服务。

mod layout;
mod publisher;
mod shm;
mod subscriber;
mod triple_buffer;

// 【修改】新增：网络转发模块（提交 9a85cc8）。
// 原库只有 memmap 共享内存 IPC（仅限同机）；仿真迁移到 Windows 后视觉程序跑在
// 局域网 Linux NUC 上，故新增基于 UDP(元数据) + TCP(JPEG 图像) 的网络通道。
// 用 net feature 门控，保持"无 bevy、可独立编译"的库定位；memmap2 原有代码不动。
#[cfg(feature = "net")]
pub mod net_ipc;

pub use layout::*;
pub use publisher::ShmPublisher;
pub use shm::{ShmError, ShmRegion};
pub use subscriber::ShmSubscriber;
