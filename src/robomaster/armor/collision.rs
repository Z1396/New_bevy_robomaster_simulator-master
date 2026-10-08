//! 装甲命中判定：子弹打中敌方装甲时给命中率统计 +1。
//!
//! 新手导览——本文件演示的两个 Bevy 核心概念：
//! 1. **观察者（Observer）**：不是每帧执行的"系统"，而是"事件发生时被调用"的回调。
//!    普通系统适合"每帧都要做的事"，观察者适合"发生了才要做的事"（如碰撞、销毁）。
//!    avian3d 物理引擎在两个碰撞体开始接触时会发出 `CollisionStart` 事件，
//!    `add_observer` 注册的函数就被自动调用，事件内容通过第一个参数 `On<...>` 传入。
//! 2. **系统参数注入**：观察者函数的其余参数（`ResMut`、`Query`...）由引擎按类型
//!    自动提供，顺序无关——写函数时只管"声明我需要什么"。
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

/// 每当物理引擎检测到"两个碰撞体开始接触"时被调用。
/// 参数由引擎注入：`event` 是碰撞事件（含碰撞双方），`Query` 用过滤器筛出我们关心的实体。
fn handle_armor_collision(
    // `On<CollisionStart>`：事件载荷，body1/body2 是相撞双方的刚体实体，
    // collider1/collider2 是对应的碰撞体（碰撞体常挂在刚体或其子实体上）。
    event: On<CollisionStart>,
    // `ResMut` = 可读写访问的全局资源（Res 是只读版）。
    mut stats: ResMut<ProjectileStatistics>,
    // `Query` + `With<Projectile>`：筛出"挂有 Projectile 组件"的实体，只要 ID 不要数据。
    projectiles: Query<Entity, With<Projectile>>,
    // 筛出所有装甲实体（要读它的 team 字段判断敌我）。
    armors: Query<&Armor>,
    // `ChildOf` = "父实体"关系组件；用于沿父子链向上找（见下方 iter_ancestors）。
    child_of: Query<&ChildOf>,
    // 本机玩家操控的战车（Controlled 组件全局唯一），用于比较阵营。
    my_vehicle: Query<&Infantry, With<Controlled>>,
) {
    // `and_then` 是 Option 的链式处理：body1 是子弹就取 Some(实体)，否则 None。
    // 细节拆解：`projectiles.get(body)` 返回 Result（查询不一定命中），
    // `.ok()` 把 Result 转成 Option（Err 丢弃、Ok 取值）——之后全用 Option 链处理。
    let projectile_body1 = event.body1.and_then(|body| projectiles.get(body).ok());
    let projectile_body2 = event.body2.and_then(|body| projectiles.get(body).ok());

    // `let Some(x) = ... else { return };` 是 Rust 的"提前返回"写法：
    // 解构 Option，若是 None 就直接退出函数（等价于 match，但更紧凑）。
    // `.or(...)`：两边都试，先拿到子弹实体的那边赢——碰撞双方谁挂了子弹都行。
    let Some(projectile_entity) = projectile_body1.or(projectile_body2) else {
        return;
    };

    // 子弹是 body1 时对方就是 collider2，反之亦然——算出"被打中的那方"。
    let other_collider = if projectile_body1.is_some() {
        event.collider2
    } else {
        event.collider1
    };

    // 命中的是装甲本身，或是挂在装甲根下的子部件。
    // `iter_ancestors`：沿父子关系一路向上爬（碰撞体常是装甲模型的子节点），
    // `find_map` 找到第一个能查到 Armor 组件的祖先就停。
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

    // 只统计敌方装甲：与本机 Controlled 战车（全局唯一）比较阵营。
    // `.single()`：Query 的"恰好一个"取值——0 个或多个都会返回 Err；
    // 没有玩家战车在场上时（如纯观战）就不做统计。
    let Ok(mine) = my_vehicle.single() else {
        return;
    };
    if armor.team == mine.team {
        return;
    }

    // 资源直写立即生效：同一物理步内命中多块装甲也只计一次。
    // （这就是不用 Commands 的原因：Commands 插入的资源/组件要到下一次
    //  apply 阶段才可见，本帧内的后续观察者看不到，防重复合数会失效。）
    stats.mark_hit(projectile_entity);
}

/// 子弹销毁时清理去重记录，防止集合无限增长。
/// `On<Remove, Projectile>`：Bevy 内置的"组件/实体被移除"事件——子弹 despawn 时
/// 触发；`event.entity` 是刚销毁的实体 ID（字段，不是方法）。
fn on_projectile_removed(
    event: On<bevy::prelude::Remove, Projectile>,
    mut stats: ResMut<ProjectileStatistics>,
) {
    stats.forget(event.entity);
}

// `pub(super)`：可见性限定——只对父模块（armor/）可见，比 pub 更收紧。
// `#[derive(Default)]`：自动实现"无参默认构造"，让下面的 Plugin 用 `.default()` 创建。
#[derive(Default)]
pub(super) struct ArmorCollisionPlugin;

// `impl Plugin for ...`：实现 Bevy 的插件 trait。插件在 main.rs 的 add_plugins
// 里被安装时，build() 执行一次——这里是本文件两个观察者的注册入口。
impl Plugin for ArmorCollisionPlugin {
    fn build(&self, app: &mut bevy::app::App) {
        app.add_observer(handle_armor_collision);
        app.add_observer(on_projectile_removed);
    }
}
