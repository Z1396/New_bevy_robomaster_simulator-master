//! 展示战车双形态：闲置时绕 Y 轴匀速自转，被 Tab 选中后由玩家操控。

use avian3d::prelude::*;
use bevy::prelude::*;

use crate::components::{ActiveSlapper, Spinning};

/// 展示战车闲置自转。
///
/// 过滤条件 `Without<ActiveSlapper>` 是形态切换的关键：Tab 选中后根实体出现
/// `ActiveSlapper`（Input 阶段插入，GameLogic 阶段之前生效），本系统同帧失配，
/// 刚体交由操控独占；切走后 `ActiveSlapper` 被移除，本系统下帧恢复匹配，
/// 自动接管角速度与角阻尼，无需任何摘挂操作。
pub fn spin_display_vehicle(
    mut spinning: Query<
        (&mut AngularVelocity, &mut AngularDamping),
        (With<Spinning>, Without<ActiveSlapper>),
    >,
) {
    for (mut angular_velocity, mut damping) in &mut spinning {
        damping.0 = 0.0; // 阻尼清零，保证自转匀速不被衰减
        angular_velocity.0 = Vec3::new(0.0, 5.0, 0.0);
    }
}
