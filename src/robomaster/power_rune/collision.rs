//! 能量机关命中判定：子弹打到靶位时，把"第几个靶位被打中"交给状态机处理。
//!
//! 本文件与 `src/robomaster/armor/collision.rs` 是"同一个套路"——注册一个**观察者**响应
//! 物理引擎发出的碰撞事件。关于"观察者 vs 系统"的区别，armor/collision.rs 的文件头已详解，
//! 这里不重复。两处只有两点不同，新手可对比着看：
//! 1. 这里监听 `CollisionEnd`（两个碰撞体"结束接触"时触发），armor 那边用 `CollisionStart`；
//! 2. 这里命中后要把这颗子弹"消费掉"——同一颗子弹最多只记一次命中，并禁止它再次触发。
//!
//! 数据流：`CollisionEnd` 事件 → 找出子弹实体 → 沿父子链找到挂了 `RuneIndex` 的靶位节点
//! → 调用 `MechanismState::hit`（state.rs）算出命中结果 → 触发 `RuneHit` 事件；
//! 若结果表示"机关已激活"，再补发一个 `RuneActivated` 事件供其他模块响应。

use crate::robomaster::power_rune::common::RuneHitOutcome;
use crate::robomaster::power_rune::rotation::PowerRuneRotation;
use crate::robomaster::power_rune::rune::PowerRuneMechanism;
use avian3d::prelude::{CollisionEnd, CollisionEventsEnabled};
use bevy::prelude::{
    ChildOf, Commands, Component, Entity, EntityEvent, On, Query, ResMut, Resource, Update, With,
};
use std::collections::HashSet;

/// 子弹标记组件。空的 struct（即"标记组件"）：只用来回答"这个实体是不是子弹"。
/// `#[require(CollisionEventsEnabled)]` 是 Bevy 的"必需组件"语法：任何实体一旦插入
/// `Projectile`，引擎会自动补上 `CollisionEventsEnabled`；没有它物理引擎不会给该实体
/// 发出碰撞事件。注意 armor/collision.rs 的子弹也复用了这个类型（跨文件契约）。
#[derive(Component)]
#[require(CollisionEventsEnabled)]
pub struct Projectile;

/// 已"消费"子弹的去重集合：记录哪些子弹已经处理过命中，避免同一颗子弹重复计数。
/// `#[derive(Resource)]` 让它成为全局唯一资源（不是挂在实体上的组件）。
#[derive(Resource, Default)]
struct ConsumedRuneProjectiles(HashSet<Entity>);

/// 挂在靶位节点上的索引组件，用来回答"这是第几个面的第几个靶"。
/// - `target`：靶位在该面内的逻辑下标（0..5），等于节点名里 `TARGET_1..5` 的数字减 1；
/// - `rune`：该靶所属"面"（FACE）的根实体——拿它去查这台机关的状态机。
/// `Copy, Clone`：命中流程里常按值复制着传递，不必借用。
#[derive(Component, Debug, Copy, Clone)]
pub struct RuneIndex {
    pub target: usize,
    pub rune: Entity,
}

/// 一次命中结果的薄包装（newtype）。包一层是为了让事件载荷更有语义。
/// `PartialEq, Eq` 便于测试断言；`Copy, Clone` 便于跨事件复制。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct HitResult {
    pub outcome: RuneHitOutcome,
}

impl HitResult {
    /// 这次命中是否算"有效命中"（打中正确目标）。`const fn` 可在编译期求值；
    /// 直接转发到 `RuneHitOutcome::is_accurate`（common.rs）。
    pub const fn accurate(self) -> bool {
        self.outcome.is_accurate()
    }
}

/// 机关被激活的事件。`#[derive(EntityEvent)]` = "带目标实体的实体事件"：
/// `#[event_target]` 标注的字段就是事件目标——这里是被激活的那个面。
/// 用 `commands.trigger(...)` 发出，别的系统用 `On<RuneActivated>` 接收。
#[derive(EntityEvent)]
pub struct RuneActivated {
    #[event_target]
    pub rune: Entity,
}

/// 一次命中（无论结果好坏）都会发的事件，附带本次的 `HitResult`。
#[derive(EntityEvent)]
pub struct RuneHit {
    #[event_target]
    pub rune: Entity,
    pub result: HitResult,
}

/// 每当物理引擎报告"两个碰撞体结束接触"时被调用（观察者，不是每帧系统）。
/// 参数由引擎按类型注入；职责：定位子弹与靶位 → 判命中 → 推进状态机 → 发事件。
fn handle_rune_collision(
    // `On<CollisionEnd>`：碰撞结束事件载荷；body1/body2 是两个刚体，collider1/collider2 是对应碰撞体。
    event: On<CollisionEnd>,
    // `Commands`：延迟执行的实体操作队列（增删组件、触发事件），本帧内不会立刻生效。
    mut commands: Commands,
    // 去重集合资源（可读写）。
    mut consumed_projectiles: ResMut<ConsumedRuneProjectiles>,
    // 一次查表拿到"面"的状态机与旋转控制器（两者都要改，所以是 &mut）。
    mut runes: Query<(&mut PowerRuneMechanism, &mut PowerRuneRotation)>,
    // 只读查靶位索引。
    targets: Query<&RuneIndex>,
    // 筛出"挂了 Projectile 组件"的实体，只取 ID（数据参数用默认，即只读全部组件）。
    projectiles: Query<Entity, With<Projectile>>,
    // 父实体关系表，用于沿父子链向上找挂 RuneIndex 的节点。
    child_of: Query<&ChildOf>,
) {
    // `and_then` + `.ok()`：把"查询某个刚体是不是子弹"链式转成 Option（不是子弹则为 None）。
    let projectile_body1 = event.body1.and_then(|body| projectiles.get(body).ok());
    let projectile_body2 = event.body2.and_then(|body| projectiles.get(body).ok());

    // 元组 match：谁挂了 `Projectile` 组件谁就是子弹，另一方就是靶位碰撞体。
    // `_ => return` 兜底：两边都不是子弹（例如两块静态场景互碰）就忽略。
    let (projectile_entity, target_collider) = match (projectile_body1, projectile_body2) {
        (Some(projectile), _) => (projectile, event.collider2),
        (_, Some(projectile)) => (projectile, event.collider1),
        _ => return,
    };

    // `let Some(x) = ... else { return };`：提前返回写法——找不到靶位就退出。
    let Some(target) = find_rune_target(target_collider, &targets, &child_of) else {
        return;
    };

    // 用靶位记录的 `rune`（面的根实体）拿到该面的状态机与旋转控制器。
    // `get_mut` 返回 Result，`let Ok(...) = ... else { return }` 取不到就退出。
    let Ok((mut mechanism, mut rotation)) = runes.get_mut(target.rune) else {
        return;
    };

    // `HashSet::insert` 返回 bool：true=首次插入，false=之前已有。
    // 所以"插入失败"（`!insert`）说明这颗子弹已经消费过，直接返回——这是防重复计数的关键。
    if !consumed_projectiles.0.insert(projectile_entity) {
        return;
    }

    // 从子弹身上移除 `CollisionEventsEnabled`，之后引擎不再为它发碰撞事件。
    // 注意 `Commands` 是延迟的：真正生效在稍后的 apply 阶段；本帧的防重由上面的
    // HashSet 即时兜住（资源直写立即生效）。
    commands
        .entity(projectile_entity)
        .remove::<CollisionEventsEnabled>();

    // 建一个随机数源，透传给状态机（大机关会随机挑选下一个要点亮的目标）。
    let mut rng = rand::rng();
    // 命中推进状态机：target.target 是靶位下标（0..5），返回本次命中结果。
    let outcome = mechanism.state_mut().hit(target.target, &mut rng);
    // 命中可能改变"是否正在激活"（例如最后一次命中导致激活），据此同步旋转模式：
    // 大机关只有在激活态才启用变速旋转，非激活态回到基准转速。
    rotation.sync_activation(
        mechanism.state().mode(),
        mechanism.state().is_activating(),
        &mut rng,
    );

    // 发出命中事件（含结果），供统计/音效等外部模块订阅。
    commands.trigger(RuneHit {
        rune: target.rune,
        result: HitResult { outcome },
    });

    // 若这次命中把机关打成了"激活"，再补发一个激活事件。
    if outcome.activates_rune() {
        commands.trigger(RuneActivated { rune: target.rune });
    }
}

/// 每帧清理：把已经从世界里消失的子弹从去重集合中移除，防止集合无限增长。
/// `Query<(), With<Projectile>>`：只关心"实体是否还存在"，所以数据参数用 `()`。
fn cleanup_consumed_rune_projectiles(
    mut consumed_projectiles: ResMut<ConsumedRuneProjectiles>,
    projectiles: Query<(), With<Projectile>>,
) {
    // `retain` 只保留闭包返回 true 的项：子弹实体仍存在则保留，否则剔除。
    // `*entity` 解引用，因为 contains 接收的是按值的 Entity。
    consumed_projectiles
        .0
        .retain(|entity| projectiles.contains(*entity));
}

/// 定位靶位索引：先看碰撞体自身是否挂了 `RuneIndex`，否则沿父子链向上找最近的挂了的那层。
/// 返回 `Option<RuneIndex>`（`RuneIndex` 是 Copy，直接按值返回）。
fn find_rune_target(
    entity: Entity,
    targets: &Query<&RuneIndex>,
    child_of: &Query<&ChildOf>,
) -> Option<RuneIndex> {
    // 命中就是靶位本体时直接返回（`.copied()` 把 &RuneIndex 复制成 RuneIndex）。
    if let Ok(target) = targets.get(entity) {
        return Some(*target);
    }

    // 否则沿祖先链向上爬（碰撞体常是靶位模型的子节点），找到第一个挂 RuneIndex 的祖先。
    child_of
        .iter_ancestors(entity)
        .find_map(|ancestor| targets.get(ancestor).ok().copied())
}

/// 本文件的插件：注册去重资源、每帧清理系统，以及命中观察者。
/// `pub(super)`：可见性只对父模块（power_rune）开放。
#[derive(Default)]
pub(super) struct PowerRuneCollisionPlugin;

impl bevy::app::Plugin for PowerRuneCollisionPlugin {
    fn build(&self, app: &mut bevy::app::App) {
        // init_resource 建默认值资源；add_systems 挂每帧系统；add_observer 挂事件回调。
        app.init_resource::<ConsumedRuneProjectiles>()
            .add_systems(Update, cleanup_consumed_rune_projectiles)
            .add_observer(handle_rune_collision);
    }
}
