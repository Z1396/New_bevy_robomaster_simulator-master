//! Talos 共享内存 IPC 模块
//!
//! 提供与 C++ talos-cpp 通信的零拷贝共享内存接口。
//! 纯 IPC 代码已移至 `talos-ipc` crate，此处只保留 Bevy 集成。

mod capture;
mod ground_truth;
// 【修改】新增：IPC 通道分发封装（net/shm 运行时切换），见 ipc.rs 头注释。
mod ipc;
mod plugin;

pub use plugin::TalosPlugin;
