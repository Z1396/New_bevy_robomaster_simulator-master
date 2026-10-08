//! 步兵类单位（步兵/英雄）的组件：身份、底盘、云台，以及"操控/展示"相关标记。
//!
//! 这些标记是 setup.rs 装配与 systems/ 筛选之间的契约，改动时要同步两端。
//! 装配入口见 setup.rs::setup_vehicle；各系统的过滤器用法见 systems/。

use bevy::prelude::*;

use crate::robomaster::prelude::{RobotConfig, Team};

/// 本机玩家操控的战车标记（全局唯一；`Single`/`With<Controlled>` 等查询都靠它定位玩家车）。
#[derive(Component)]
pub struct Controlled;

/// 步兵类单位的身份组件：阵营 + 机体配置（三号步兵/英雄等）。
#[derive(Component)]
pub struct Infantry {
    /// 阵营（红/蓝）：命中统计与敌我判定用。
    pub team: Team,
    /// 机体配置（最大速度、装甲规格等）。
    pub config: RobotConfig,
}

impl Infantry {
    /// `const fn` 常量构造函数：可在编译期求值。
    pub const fn new(team: Team, config: RobotConfig) -> Self {
        Self { team, config }
    }
}

/// 底盘节点组件。角度单位一律弧度，角速度单位弧度/秒。
#[derive(Component, Default)]
pub struct InfantryChassis {
    /// 底盘偏航角 yaw（弧度）
    pub yaw: f32,
    /// 偏航角速度（弧度/秒）——"小陀螺"转动即体现在这里
    pub yaw_velocity: f32,
    /// 横滚角 roll（弧度）
    pub roll: f32,
    /// 俯仰角 pitch（弧度）
    pub pitch: f32,
}

/// 云台节点组件：云台相对底盘的偏航，与云台自身的俯仰，单位弧度。
#[derive(Component, Default)]
pub struct InfantryGimbal {
    /// 云台相对底盘的偏航角（弧度）
    pub local_yaw: f32,
    /// 云台俯仰角（弧度），受 config 的 gimbal_pitch_limit 限制
    pub pitch: f32,
}

/// 相机/瞄准点节点标记（对应 glTF 的 `CAM_DIRECTION`）。
/// 契约：systems/camera.rs 的 Robot 模式从这里取第一人称视角。
#[derive(Component)]
pub struct InfantryViewOffset;

/// 炮口/发射点节点标记（对应 glTF 的 `SHOT_DIRECTION`）。
/// 契约：systems/projectile.rs 从这里取子弹的发射位置与朝向。
#[derive(Component)]
pub struct InfantryLaunchOffset;

/// 可被 Tab 选中操控的 AI 战车标记（区别于玩家车与展示战车）。
#[derive(Component)]
pub struct SlapperInfantry;

/// Marker for the currently active (controlled) SlapperInfantry
/// 中文：当前被选中的那台"可操控 AI 车"标记——有它才响应玩家输入，由
/// `switch_slapper_control` 在 Tab 切换时运行时补挂/摘除。
#[derive(Component)]
pub struct ActiveSlapper;

/// 展示战车永久能力标记，从不摘除；形态切换完全由根实体上有无 ActiveSlapper 决定。
#[derive(Component)]
pub struct Spinning;
