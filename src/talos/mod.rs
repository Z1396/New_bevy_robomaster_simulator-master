//! Talos 集成模块：把仿真数据（图像/位姿/真值）喂给视觉算法 Talos，并回收自瞄指令。
//!
//! 新手导览——本模块是"仿真器 ↔ 外部视觉程序"的桥，分两层：
//! - 纯 IPC 机制（共享内存/网络的字节布局、三缓冲、收发 API）全部下沉到独立的
//!   `talos-ipc` crate，不依赖 Bevy，可被独立工具复用；
//! - 本模块只保留 **Bevy 集成**：从 ECS 世界取数据、打包成 IPC 消息、注册进调度表。
//!
//! 子模块职责（下方 `mod` 声明即模块树入口，`pub use` 把插件导出到 crate 外）：
//! - `ipc`：运行时在"网络转发"与"共享内存"两种通道间二选一的**分发封装**；
//! - `capture`：离屏渲染捕获图像 + 位姿，经 IPC 发布（Extract 阶段取世界快照）；
//! - `ground_truth`：发布全体战车/能量机关的绝对真值（供视觉算法评测）；
//! - `plugin`：`TalosPlugin` 把这些系统注册进 Bevy 调度表，并定义坐标对齐矩阵。
//!
//! 与 `ros2` 模块类似，仅在启用 `talos` feature 时编译（见 main.rs 的 `#[cfg]`）。

mod capture;
mod ground_truth;
// 【修改】新增：IPC 通道分发封装（net/shm 运行时切换），见 ipc.rs 头注释。
mod ipc;
mod plugin;

// `pub use` 重导出：把 `plugin::TalosPlugin` 提到本模块根路径（`talos::TalosPlugin`），
// 这样 main.rs 里 `use talos::TalosPlugin;` 就能拿到，无需写出完整子模块路径。
pub use plugin::TalosPlugin;
