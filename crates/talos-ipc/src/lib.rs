//! Talos IPC 共享内存通信库
//!
//! 纯 Rust 实现的零拷贝共享内存 IPC，与 C++ talos-cpp 兼容。
//! 不依赖 bevy 或任何游戏引擎，可用于独立工具和服务。
//!
//! 【教学】这是一个**独立 crate**（子包，通过 `Cargo.toml` 的 workspace/path 依赖被主
//! crate 引用），刻意不含任何游戏引擎代码——好处是可以被独立的机器人侧程序复用，
//! 也便于单独编译测试。模块划分：
//! - `layout`：所有跨进程/跨语言消息的**二进制布局契约**（结构体字段顺序、偏移、单位）；
//! - `shm`：跨平台共享内存区域封装（底层用内存映射文件）；
//! - `triple_buffer`：无锁三缓冲读写原语（写者不阻塞读者）；
//! - `publisher` / `subscriber`：共享内存模式下的发布/订阅 API；
//! - `net_ipc`：网络转发实现（UDP 元数据 + TCP JPEG 图像），仅 `net` feature 下编译。

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

// 重导出：把各子模块的关键类型提升到 crate 根，外部写 `talos_ipc::ShmPublisher` 即可。
pub use layout::*; // 全部消息结构体 + 常量（`*` 通配重导出）
pub use publisher::ShmPublisher; // 共享内存发布端
pub use shm::{ShmError, ShmRegion}; // 共享内存区域与错误类型
pub use subscriber::ShmSubscriber; // 共享内存订阅端
