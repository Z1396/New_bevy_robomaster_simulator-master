//! 装甲命中判定。
//!
//! 【修改】修复统计失真（原实现三个问题）：
//! 1. 监听 `CollisionEnd`（分离才计数）→ 改为 `CollisionStart`，接触瞬间计数；
//!    原方案下子弹贴住装甲不分离则永远漏计，且计数滞后。
//! 2. 原用 `remove::<CollisionEventsEnabled>()` 防重复，但 `Commands` 延迟刷新，
//!    同一物理步内接触多块装甲会重复计数 → 改为 `ProjectileStatistics.counted`
//!    集合去重（资源直写立即生效），并注册 `On<Remove, Projectile>` 观察者在
//!    子弹销毁时清理记录。
//! 3. 原不区分敌我 → 现在只统计击中**敌方**装甲（与本机 Controlled 战车比较阵营）。

use avian3d::prelude::CollisionStart;
use bevy::prelude::{ChildOf, Entity, On, Plugin, Query, ResMut, With};

use super::construct::Armor;
use crate::components::{Controlled, Infantry};
use crate::robomaster::power_rune::prelude::Projectile;
use crate::statistic::ProjectileStatistics;

fn handle_armor_collision(
    event: On<CollisionStart>,
    mut stats: ResMut<ProjectileStatistics>,
    projectiles: Query<Entity, With<Projectile>>,
    armors: Query<&Armor>,
    child_of: Query<&ChildOf>,
    my_vehicle: Query<&Infantry, With<Controlled>>,
) {
    let projectile_body1 = event.body1.and_then(|body| projectiles.get(body).ok());
    let projectile_body2 = event.body2.and_then(|body| projectiles.get(body).ok());

    let Some(projectile_entity) = projectile_body1.or(projectile_body2) else {
        return;
    };

    let other_collider = if projectile_body1.is_some() {
        event.collider2
    } else {
        event.collider1
    };

    // 命中的是装甲本身，或是挂在装甲根下的子部件
    let armor = armors
        .get(other_collider)
        .ok()
        .or_else(|| {
            child_of
                .iter_ancestors(other_collider)
                .find_map(|ancestor| armors.get(ancestor).ok())
        });
    let Some(armor) = armor else {
        return;
    };

    // 只统计敌方装甲：与本机 Controlled 战车（全局唯一）比较阵营
    let Ok(mine) = my_vehicle.single() else {
        return;
    };
    if armor.team == mine.team {
        return;
    }

    // 资源直写立即生效：同一物理步内命中多块装甲也只计一次
    stats.mark_hit(projectile_entity);
}

/// 子弹销毁时清理去重记录，防止集合无限增长。
fn on_projectile_removed(
    event: On<bevy::prelude::Remove, Projectile>,
    mut stats: ResMut<ProjectileStatistics>,
) {
    stats.forget(event.entity);
}

#[derive(Default)]
pub(super) struct ArmorCollisionPlugin;

impl Plugin for ArmorCollisionPlugin {
    fn build(&self, app: &mut bevy::app::App) {
        app.add_observer(handle_armor_collision);
        app.add_observer(on_projectile_removed);
    }
}
