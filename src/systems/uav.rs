//! UAV（无人机）投掷物：按 P 键从玩家车炮口方向生成一个 uav.glb 世界。
//!
//! 与普通子弹不同：UAV 是一个完整的 glTF 世界（`WorldAssetRoot`），且限速为每秒最多一个。

use avian3d::prelude::*;
use bevy::prelude::*;

use crate::components::{
    Controlled, Infantry, InfantryChassis, InfantryGimbal, InfantryLaunchOffset,
};

/// 按 P 键生成 UAV（调度在 PostUpdate，需在 Transform 传播之后取炮口的最新世界坐标）。
pub fn uav_launch(
    time: Res<Time>,
    mut commands: Commands,
    // `Single<...>`：断言"恰好一个"匹配实体，0 个或多个都会 panic——这里断言玩家车唯一。
    // `(With<Infantry>, With<Controlled>)` 即"玩家操控的步兵车"。
    infantry: Single<
        (&Transform, &LinearVelocity, &AngularVelocity),
        (With<Infantry>, With<Controlled>),
    >,
    // 云台：`Without<InfantryChassis>` 把云台从底盘排除（两者都可能挂 Controlled）。
    gimbal: Single<
        (&GlobalTransform, &InfantryGimbal),
        (With<Controlled>, Without<InfantryChassis>),
    >,
    asset_server: Res<AssetServer>,
    // 炮口节点（glTF 的 SHOT_DIRECTION）。
    launch_offset: Single<&Transform, (With<Controlled>, With<InfantryLaunchOffset>)>,
    // `Local<T>`：系统私有的持久状态（不进 World，仅本系统可见）；这里作节流计时器。
    mut timer: Local<Option<Timer>>,
    keyboard: Res<ButtonInput<KeyCode>>,
) {
    // `get_or_insert`：首次运行时懒初始化计时器（1 秒一次性）。
    let timer = timer.get_or_insert(Timer::from_seconds(1.0, TimerMode::Once));
    timer.tick(time.delta());
    if !timer.is_finished() {
        return;
    }
    timer.reset();
    if keyboard.pressed(KeyCode::KeyP) {
        commands.spawn((
            RigidBody::Static, // 静态刚体：UAV 作为静止靶标，不受物理推动
            WorldAssetRoot(asset_server.load(GltfAssetLabel::Scene(0).from_asset("uav.glb"))),
            // 生成位置 = 车世界平移 + (云台世界旋转 × 炮口相对偏移)。`infantry.0` 是 Single 解出的
            // 元组第 0 项（即车身的 &Transform）。
            Transform::IDENTITY.with_translation(
                infantry.0.translation + (gimbal.0.rotation() * launch_offset.translation),
            ),
        ));
    }
}
