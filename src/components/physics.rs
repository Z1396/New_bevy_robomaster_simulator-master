//! 物理相关组件与资源：碰撞分层、投掷物（子弹/飞镖）设置、各类场景标记。
//!
//! 碰撞分层是本文件的核心：`GameLayer` 决定"谁能撞到谁"，
//! 而"子弹只打敌方装甲、不撞自家车"这条游戏规则完全由层的组合表达出来。

use avian3d::prelude::*;
use bevy::prelude::*;
use std::collections::HashMap;

/// 物理碰撞层枚举。`#[derive(PhysicsLayer)]` 是 avian 的过程宏：它按**变体顺序**把每个
/// 变体映射成一个独立的碰撞层（内部是位标志）；`#[default]` 指定默认层为 `Default`。
#[derive(PhysicsLayer, Default, Clone, Copy, Debug)]
pub enum GameLayer {
    #[default]
    Default, // 中立/未分类
    VehicleSelf,     // 本机车身
    VehicleOther,    // 敌方车身
    ProjectileSelf,  // 本机子弹
    ProjectileOther, // 敌方子弹
    Environment,     // 环境（地面/障碍）
}

impl GameLayer {
    /// 环境层：与除自身外的所有实体碰撞。
    /// `CollisionLayers::new(本实体所属层, 允许碰撞的层列表)`。
    pub fn environment_collision_layers() -> CollisionLayers {
        CollisionLayers::new(
            Self::Environment,
            [
                Self::Default,
                Self::VehicleSelf,
                Self::VehicleOther,
                Self::ProjectileSelf,
                Self::ProjectileOther,
            ],
        )
    }

    /// 车身碰撞层。`is_self` 区分本机/敌方：本机车身是 `VehicleSelf`，敌方是 `VehicleOther`。
    pub fn vehicle_body_collision_layers(is_self: bool) -> CollisionLayers {
        if is_self {
            CollisionLayers::new(
                Self::VehicleSelf,
                [Self::Default, Self::VehicleOther, Self::Environment],
            )
        } else {
            CollisionLayers::new(
                Self::VehicleOther,
                [
                    Self::Default,
                    Self::VehicleSelf,
                    Self::VehicleOther,
                    Self::Environment,
                ],
            )
        }
    }

    /// 装甲碰撞层。与车身层分开、且只接收**敌方**子弹层——这就是"子弹能打装甲、
    /// 却穿不过敌方车身"的实现方式（底盘装甲命中判定见 robomaster/armor/collision.rs）。
    pub fn vehicle_armor_collision_layers(is_self: bool) -> CollisionLayers {
        if is_self {
            CollisionLayers::new(
                Self::VehicleSelf,
                [
                    Self::Default,
                    Self::VehicleOther,
                    Self::ProjectileOther,
                    Self::Environment,
                ],
            )
        } else {
            CollisionLayers::new(
                Self::VehicleOther,
                [
                    Self::Default,
                    Self::VehicleSelf,
                    Self::ProjectileSelf,
                    Self::Environment,
                ],
            )
        }
    }

    /// 子弹碰撞层：只与**敌方**装甲和环境碰撞，不与任何车身、任何子弹碰撞。
    pub fn projectile_collision_layers(is_self: bool) -> CollisionLayers {
        if is_self {
            CollisionLayers::new(
                Self::ProjectileSelf,
                [Self::Default, Self::VehicleOther, Self::Environment],
            )
        } else {
            CollisionLayers::new(
                Self::ProjectileOther,
                [Self::Default, Self::VehicleSelf, Self::Environment],
            )
        }
    }
}

/// 子弹寿命计时器（组件）。`Deref`/`DerefMut` 到内部的 `Timer`，可直接调用
/// `tick(...)`/`finished()`（见 systems/projectile.rs 的 cleanup_projectiles）。
#[derive(Component, Deref, DerefMut)]
pub struct ProjectileLifetime(pub Timer);

/// 开火冷却计时器（全局资源，所有车/子弹共用同一节流）。
#[derive(Resource, Deref, DerefMut)]
pub struct ProjectileCooldown(pub Timer);

/// 子弹的共享渲染资源：网格（Mesh）与材质，避免每颗子弹重复建资源。
#[derive(Resource)]
pub struct ProjectileSetting(pub Handle<Mesh>, pub Handle<StandardMaterial>);

/// 飞镖世界的资产句柄（飞镖是独立 glTF 世界，运行时 spawn）。
#[derive(Resource)]
pub struct DartSetting(pub Handle<WorldAsset>);

/// 地面根节点标记（scene.rs 生成地面时挂上）。
#[derive(Component)]
pub struct GroundRoot;

/// 飞镖发射位标记（对应 glTF 节点 `DART_LAUNCH_DIRECTION`）。
#[derive(Component)]
pub struct DartLaunch;

/// 飞镖实体标记（区别于普通子弹）。
#[derive(Component)]
pub struct DartProjectile;

/// 一次性碰撞构造指令的载体：名称 → (构造器, 碰撞层, 可见性, 可选刚体)。
///
/// `Deref`/`DerefMut` 到内部 `HashMap`，所以可直接 `map.get(name)`；
/// 由 scene.rs 在加载流程里传入 setup_collision，用完即弃（不作为组件留存）。
#[derive(Component, Deref, DerefMut)]
pub struct PreciousCollision(
    pub  HashMap<
        String,
        (
            ColliderConstructorHierarchy,
            CollisionLayers,
            Visibility,
            Option<RigidBody>,
        ),
    >,
);

/// 子弹默认存活时长（秒）。到期即销毁，防止子弹无限累积。
pub const PROJECTILE_LIFETIME_SECS: f32 = 5.0;

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机子弹不能撞到自家的车身/装甲/子弹（含敌方子弹）——避免自伤与子弹互撞。
    #[test]
    fn self_projectile_ignores_self_vehicle_and_projectiles() {
        let projectile = GameLayer::projectile_collision_layers(true);

        assert!(!projectile.interacts_with(GameLayer::vehicle_body_collision_layers(true)));
        assert!(!projectile.interacts_with(GameLayer::vehicle_armor_collision_layers(true)));
        assert!(!projectile.interacts_with(GameLayer::projectile_collision_layers(true)));
        assert!(!projectile.interacts_with(GameLayer::projectile_collision_layers(false)));
    }

    /// 子弹必须能命中敌方装甲与环境（游戏规则的核心断言）。
    #[test]
    fn projectiles_hit_opposing_armor_and_environment() {
        let self_projectile = GameLayer::projectile_collision_layers(true);
        let other_projectile = GameLayer::projectile_collision_layers(false);

        assert!(self_projectile.interacts_with(GameLayer::vehicle_armor_collision_layers(false)));
        assert!(other_projectile.interacts_with(GameLayer::vehicle_armor_collision_layers(true)));
        assert!(self_projectile.interacts_with(GameLayer::environment_collision_layers()));
        assert!(other_projectile.interacts_with(GameLayer::environment_collision_layers()));
    }

    /// 子弹不应与任何一方车身碰撞（只能打装甲）——保证命中判定的唯一入口是装甲。
    #[test]
    fn projectiles_do_not_hit_vehicle_body_colliders() {
        let self_projectile = GameLayer::projectile_collision_layers(true);
        let other_projectile = GameLayer::projectile_collision_layers(false);

        assert!(!self_projectile.interacts_with(GameLayer::vehicle_body_collision_layers(false)));
        assert!(!other_projectile.interacts_with(GameLayer::vehicle_body_collision_layers(true)));
    }
}
