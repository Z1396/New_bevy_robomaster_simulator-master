//! 相机相关数据：主相机标记与跟随偏移、当前相机模式、自瞄订阅开关。
//!
//! 注意区分组件与资源：
//! - `MainCamera` 是**组件**（挂在相机实体上）；
//! - `CameraMode` / `SubscribeAutoAim` 是**资源**（`Resource`，全局唯一）。
//!
//! 协作者：`systems/camera.rs` 读 `CameraMode` 决定跟随方式；`main.rs` 也用它做系统
//! 运行条件（如自由相机模式下不跑车辆操控）。

use bevy::prelude::*;
use std::sync::atomic::AtomicBool;

/// 主相机标记组件，同时携带"跟随相机"相对目标的偏移。
#[derive(Component)]
pub struct MainCamera {
    /// 跟随相机相对目标（车/云台）的偏移，单位米；来自 config.camera.follow_offset。
    pub follow_offset: Vec3,
}

/// 当前相机模式（全局资源）。
///
/// `Deref`/`DerefMut`：为单字段新类型自动实现"解引用到内部字段"，于是 `*mode` 与
/// `mode.0` 可互换地直接拿到 `FollowingType`，不必每次写 `.0`。
#[derive(Resource, PartialEq, Deref, DerefMut)]
pub struct CameraMode(pub FollowingType);

impl Default for CameraMode {
    fn default() -> Self {
        // 启动默认第一人称（Robot 模式），三种模式见 systems/camera.rs。
        Self(FollowingType::Robot)
    }
}

/// 自瞄订阅开关（全局资源）。
///
/// 用 `AtomicBool` 而非普通 `bool`：talos/ROS2 的独立线程也会读取它，原子类型保证
/// 跨线程读写不产生数据竞争；`Deref`/`DerefMut` 让 `flag.load/store` 可直接调用。
#[derive(Resource, Deref, DerefMut)]
pub struct SubscribeAutoAim(pub AtomicBool);

/// 相机跟随模式三态。`Copy` + `PartialEq`：可被廉价拷贝、可直接比较
///（main.rs 里用 `mode.0 != FollowingType::Free` 之类的条件决定系统是否运行）。
#[derive(PartialEq, Clone, Copy)]
pub enum FollowingType {
    /// 自由观察相机（不跟随车辆）
    Free,
    /// 第一人称：绑定玩家车的 CAM_DIRECTION 节点（见 setup.rs 的节点命名契约）
    Robot,
    /// 第三人称：在车后方跟随
    ThirdPerson,
}
