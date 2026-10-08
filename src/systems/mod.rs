//! 系统模块总入口：把每个子模块的系统函数与类型重导出为 `crate::systems::*`，
//! 并定义本项目的四个调度阶段（SystemSet）。
//!
//! `GameplaySystems` 在 main.rs 的 `configure_sets` 里被 `.chain()` 串起来，
//! 于是每帧严格按 Input → GameLogic → Camera → Cleanup 的顺序执行。

mod camera;
mod chassis_observation;
mod controller;
mod debug;
mod gimbal_pid;
mod input;
mod projectile;
mod spin;
mod uav;
pub use camera::*;
pub use chassis_observation::*;
pub use controller::*;
pub use debug::*;
pub use gimbal_pid::*;
pub use input::*;
pub use projectile::*;
pub use spin::*;
pub use uav::*;

use bevy::prelude::*;

/// 四个串行阶段。`#[derive(SystemSet)]` 把一个枚举变成"系统集合标签"，供
/// `add_systems(...).in_set(X)` 挂载、`configure_sets(...).chain()` 定序；
/// `Clone/PartialEq/Eq/Hash/Debug` 是 SystemSet 派生所要求的约束。
#[derive(SystemSet, Clone, PartialEq, Eq, Hash, Debug)]
pub enum GameplaySystems {
    Input,     // 采样输入、切换操控（最先）
    GameLogic, // 游戏规则结算
    Camera,    // 相机跟随（需在逻辑算完之后）
    Cleanup,   // 帧末清理（最后）
}
