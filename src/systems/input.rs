//! 车辆操控：底盘运动、云台瞄准、操控车切换。
//!
//! 两组操控对象：
//! - `*_controls`（本机）：作用于带 `Controlled` 的玩家车；
//! - `remote_*_controls`（遥控）：作用于带 `ActiveSlapper` 的被选中 AI 车。
//!
//! 关键过滤组合：底盘节点用 `With<InfantryChassis>`、云台节点用 `With<InfantryGimbal>`，
//! 且各自 `Without` 对方——因为两种组件都可能同时挂 `Controlled`，必须靠过滤器区分节点。

use bevy::prelude::*;
use std::sync::atomic::Ordering;

use crate::components::{
    ActiveSlapper, Controlled, Infantry, InfantryChassis, InfantryGimbal, SlapperInfantry,
    Spinning, SubscribeAutoAim,
};
use crate::config::SimulationConfig;
use crate::robomaster::vehicle::movement::VehicleDynamic;
use crate::systems::ControllerState;
use avian3d::prelude::*;

/// 偏航角速度低于此值即视为静止（弧度/秒）。
const CHASSIS_ROTATION_STOP_EPSILON: f32 = 1e-3;
/// 底盘倾角上限 = 20°（换算成弧度：20 × π / 180）。
const CHASSIS_TILT_LIMIT: f32 = 20.0 * std::f32::consts::PI / 180.0;

/// 底盘姿态积分：把输入的方向键/摇杆量换算成 yaw 角速度并逐步加速，再积分成姿态角。
/// 参数单位：速度均为 弧度/秒，加速度 弧度/秒²，dt 为秒。
fn update_chassis_rotation(
    chassis_transform: &mut Transform,
    chassis_data: &mut InfantryChassis,
    yaw_input: f32,
    roll_input: f32,
    pitch_input: f32,
    yaw_rotation_speed: f32,
    yaw_acceleration: f32,
    tilt_rotation_speed: f32,
    dt: f32,
) {
    // 目标角速度 = 输入(-1..1) × 最大转速。
    let target_yaw_velocity = yaw_input * yaw_rotation_speed;
    // 本帧允许的最大速度增量 = 角加速度 × dt（限加速率，避免瞬间满转速）。
    let max_velocity_delta = yaw_acceleration * dt;
    chassis_data.yaw_velocity = move_towards(
        chassis_data.yaw_velocity,
        target_yaw_velocity,
        max_velocity_delta,
    );

    // 输入归零且速度已很小 → 直接吸附到 0，消除接近停止时的抖动/长尾。
    if chassis_data.yaw_velocity.abs() < CHASSIS_ROTATION_STOP_EPSILON
        && target_yaw_velocity.abs() < CHASSIS_ROTATION_STOP_EPSILON
    {
        chassis_data.yaw_velocity = 0.0;
    }

    chassis_data.yaw += chassis_data.yaw_velocity * dt;
    // roll/pitch 是"摆动"角：直接按速度积分，并夹在 ±20° 内（不会真的翻车）。
    chassis_data.roll = (chassis_data.roll + roll_input * tilt_rotation_speed * dt)
        .clamp(-CHASSIS_TILT_LIMIT, CHASSIS_TILT_LIMIT);
    chassis_data.pitch = (chassis_data.pitch + pitch_input * tilt_rotation_speed * dt)
        .clamp(-CHASSIS_TILT_LIMIT, CHASSIS_TILT_LIMIT);
    // 合成旋转：Yaw 绕 Y，Pitch 绕 X，Roll 绕 Z（EulerRot::YXZ 给出这个应用顺序）。
    chassis_transform.rotation = Quat::from_euler(
        EulerRot::YXZ,
        chassis_data.yaw,
        chassis_data.pitch,
        chassis_data.roll,
    );
}

/// 让 `current` 朝 `target` 靠近，但每步最多改变 `max_delta`（不会越过目标）。
fn move_towards(current: f32, target: f32, max_delta: f32) -> f32 {
    current + (target - current).clamp(-max_delta, max_delta)
}

/// 本机底盘驾驶：输入 → 底盘线性力 + 底盘姿态（Input 阶段；自由相机下被跳过）。
pub fn vehicle_controls(
    time: Res<Time>,
    controller: Res<ControllerState>,
    config: Res<SimulationConfig>,
    // `Forces` 是 avian 的"施力入口"；`&Mass` 读质量；`&mut VehicleDynamic` 用其非线性加速模型。
    infantry: Single<(Forces, &Mass, &mut VehicleDynamic), (With<Infantry>, With<Controlled>)>,
    // 云台（提供前进方向的参考坐标系）：底盘节点被 `Without<InfantryChassis>` 排除。
    gimbal: Single<
        (&GlobalTransform, &InfantryGimbal),
        (With<Controlled>, Without<InfantryChassis>),
    >,
    // 底盘节点：要求有 InfantryChassis、无 InfantryGimbal（与上面正好互补），排除根实体 Infantry。
    chassis: Single<
        (&mut Transform, &mut InfantryChassis),
        (
            With<Controlled>,
            Without<InfantryGimbal>,
            With<InfantryChassis>,
            Without<Infantry>,
        ),
    >,
) {
    let controller = controller.controlled;
    let input = controller.movement; // Vec2：x=左右，y=前后，各 -1..1
    let boost = controller.boost_multiplier(); // 加速倍率（未按加速键为 1.0）

    let (mut forces, &Mass(mass), mut dynamic) = infantry.into_inner();

    // dt 单位秒；`linear` 内部会按质量与 dt 施加冲量（见 vehicle/movement.rs）。
    let dt = time.delta_secs();
    dynamic.linear(
        &mut forces,
        mass,
        gimbal.into_inner().0, // 以云台朝向为"前进方向"参考（车会朝云台指的方向走）
        input,
        time.delta_secs(),
        boost,
    );

    let (mut chassis_transform, mut chassis_data) = chassis.into_inner();
    update_chassis_rotation(
        &mut chassis_transform,
        &mut chassis_data,
        controller.chassis_yaw,
        controller.chassis_roll,
        controller.chassis_pitch,
        config.vehicle.rotation_speed,
        config.vehicle.yaw_acceleration,
        config.vehicle.tilt_rotation_speed,
        dt,
    );
}

/// 遥控底盘驾驶：作用于被 Tab 选中的 AI 车（`ActiveSlapper`），逻辑与 `vehicle_controls` 同构。
pub fn remote_vehicle_controls(
    time: Res<Time>,
    controller: Res<ControllerState>,
    config: Res<SimulationConfig>,
    infantry: Single<
        (&GlobalTransform, Forces, &Mass, &mut VehicleDynamic),
        (With<ActiveSlapper>, With<Infantry>, Without<Controlled>),
    >,
    chassis: Single<
        (&mut Transform, &mut InfantryChassis),
        (
            With<ActiveSlapper>,
            With<InfantryChassis>,
            Without<InfantryGimbal>,
            Without<Infantry>,
        ),
    >,
) {
    let controller = controller.remote;
    let input = controller.movement;
    let boost = controller.boost_multiplier();

    let (infantry_global_transform, mut forces, &Mass(mass), mut dynamic) = infantry.into_inner();

    let dt = time.delta_secs();
    dynamic.linear(
        &mut forces,
        mass,
        infantry_global_transform, // 遥控车用自身朝向作前进参考（本机用云台朝向）
        input,
        time.delta_secs(),
        boost,
    );

    let (mut chassis_transform, mut chassis_data) = chassis.into_inner();
    update_chassis_rotation(
        &mut chassis_transform,
        &mut chassis_data,
        controller.chassis_yaw,
        controller.chassis_roll,
        controller.chassis_pitch,
        config.vehicle.rotation_speed,
        config.vehicle.yaw_acceleration,
        config.vehicle.tilt_rotation_speed,
        dt,
    );
}

/// 本机云台手动瞄准（自瞄订阅开启时交给 gimbal_pid 接管，本系统直接返回）。
pub fn gimbal_controls(
    time: Res<Time>,
    controller: Res<ControllerState>,
    enabled: Res<SubscribeAutoAim>,
    config: Res<SimulationConfig>,
    gimbal: Single<
        (&mut Transform, &mut InfantryGimbal),
        (With<Controlled>, Without<InfantryChassis>),
    >,
) {
    // 自瞄开启时不接受手动输入，避免两套控制互相打架。
    if enabled.load(Ordering::Acquire) {
        return;
    }

    let dt = time.delta_secs();
    let (mut gimbal_transform, mut gimbal_data) = gimbal.into_inner();

    // 反向同步：先把当前旋转解算成 yaw/pitch 写回组件，保证组件状态与 Transform 一致。
    (gimbal_data.local_yaw, gimbal_data.pitch, _) =
        gimbal_transform.rotation.to_euler(EulerRot::YXZ);

    let controller = controller.controlled;
    // 转角增量 = 速度(rad/s) × 输入 × 精度缩放 × dt。
    let rotation_speed = config.vehicle.gimbal_rotation_speed * controller.gimbal_scale() * dt;
    gimbal_data.local_yaw += controller.gimbal.x * rotation_speed;
    gimbal_data.pitch += controller.gimbal.y * rotation_speed;

    // 俯仰夹在配置的上下限内（config 的 gimbal_pitch_limit，单位弧度）。
    gimbal_data.pitch = gimbal_data.pitch.clamp(
        -config.vehicle.gimbal_pitch_limit,
        config.vehicle.gimbal_pitch_limit,
    );

    let gimbal_rotation =
        Quat::from_euler(EulerRot::YXZ, gimbal_data.local_yaw, gimbal_data.pitch, 0.0);

    gimbal_transform.rotation = gimbal_rotation;
}

/// 遥控云台瞄准：作用于被选中的 AI 车，逻辑与 `gimbal_controls` 同构（不受自瞄开关影响）。
pub fn remote_gimbal_controls(
    time: Res<Time>,
    controller: Res<ControllerState>,
    config: Res<SimulationConfig>,
    gimbal: Single<
        (&mut Transform, &mut InfantryGimbal),
        (With<ActiveSlapper>, Without<InfantryChassis>),
    >,
) {
    let dt = time.delta_secs();
    let (mut gimbal_transform, mut gimbal_data) = gimbal.into_inner();

    (gimbal_data.local_yaw, gimbal_data.pitch, _) =
        gimbal_transform.rotation.to_euler(EulerRot::YXZ);

    let controller = controller.remote;
    let rotation_speed = config.vehicle.gimbal_rotation_speed * controller.gimbal_scale() * dt;
    gimbal_data.local_yaw += controller.gimbal.x * rotation_speed;
    gimbal_data.pitch += controller.gimbal.y * rotation_speed;
    gimbal_data.pitch = gimbal_data.pitch.clamp(
        -config.vehicle.gimbal_pitch_limit,
        config.vehicle.gimbal_pitch_limit,
    );

    let gimbal_rotation =
        Quat::from_euler(EulerRot::YXZ, gimbal_data.local_yaw, gimbal_data.pitch, 0.0);

    gimbal_transform.rotation = gimbal_rotation;
}

/// Tab 切换"操控哪台车"：把 `ActiveSlapper` 从当前车摘掉、挂到环形序列的下一台。
///
/// `ActiveSlapper` 在根实体**和所有后代**上都要一致地存在/移除：根实体上的有无决定
/// 展示战车自转系统是否匹配（见 spin.rs），后代上的有无决定各操控系统能否用
/// `With/Without<ActiveSlapper>` 命中云台/底盘节点。
pub fn switch_slapper_control(
    mut commands: Commands,
    controller: Res<ControllerState>,
    children: Query<&Children>,
    // 全部可选车（有 Infantry + SlapperInfantry）。
    slapper_roots: Query<Entity, (With<Infantry>, With<SlapperInfantry>)>,
    // 当前被选中的那台。
    active_root: Query<Entity, (With<Infantry>, With<SlapperInfantry>, With<ActiveSlapper>)>,
    // 只关心"是否是展示战车"，不要数据 → `Query<(), With<Spinning>>`。
    spinning_roots: Query<(), With<Spinning>>,
) {
    if !controller.controlled.switch_slapper_just_pressed {
        return;
    }

    let roots: Vec<Entity> = slapper_roots.iter().collect();
    if roots.len() <= 1 {
        return; // 只有一台可切换车时无意义
    }

    // 找当前车在列表里的下标，取环形下一个；找不到（尚未选中任何车）则从 0 开始。
    let current = active_root.single().ok();
    let current_idx = current.and_then(|e| roots.iter().position(|&r| r == e));
    let next_idx = match current_idx {
        Some(idx) => (idx + 1) % roots.len(),
        None => 0,
    };

    // Remove ActiveSlapper from current
    // （根实体 + 全部后代都要摘，见上方文档说明。）
    if let Some(current_root) = current {
        commands.entity(current_root).remove::<ActiveSlapper>();
        for descendant in children.iter_descendants(current_root) {
            commands.entity(descendant).remove::<ActiveSlapper>();
        }
    }
    // 切走时什么都不做：ActiveSlapper 被移除后，自转系统（spin_display_vehicle）
    // 下帧自动恢复匹配，接管角速度与角阻尼。

    // Add ActiveSlapper to next
    let next_root = roots[next_idx];
    if spinning_roots.contains(next_root) {
        // 展示战车被选中：同批瞬间刹停（角速度清零 + 角阻尼恢复 setup_vehicle
        // 实配值 50.0），防止残留自转导致操控时车身抖动侧翻。
        // Spinning 组件全程不摘不挂，切换状态完全由 ActiveSlapper 的有无决定。
        commands.entity(next_root).insert((
            AngularVelocity(Vec3::ZERO),
            AngularDamping(50.0),
        ));
    }
    commands.entity(next_root).insert(ActiveSlapper);
    for descendant in children.iter_descendants(next_root) {
        commands.entity(descendant).insert(ActiveSlapper);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 角速度应从 0 平滑爬升到目标速度之间（不是瞬间到位，也不会越过目标）。
    #[test]
    fn chassis_rotation_smoothly_ramps_towards_target_speed() {
        let mut transform = Transform::default();
        let mut chassis = InfantryChassis::default();

        update_chassis_rotation(
            &mut transform,
            &mut chassis,
            1.0,
            0.0,
            0.0,
            9.42,
            60.0,
            2.0,
            0.016,
        );

        assert!(chassis.yaw_velocity > 0.0);
        assert!(chassis.yaw_velocity < 9.42);
        assert!(chassis.yaw > 0.0);
    }

    /// yaw 与倾角（roll/pitch）各自独立使用自己的速度参数。
    #[test]
    fn chassis_rotation_uses_independent_yaw_and_tilt_speeds() {
        let mut transform = Transform::default();
        let mut chassis = InfantryChassis::default();

        update_chassis_rotation(
            &mut transform,
            &mut chassis,
            1.0,
            1.0,
            -1.0,
            8.0,
            1.0,
            0.25,
            1.0,
        );

        assert_eq!(chassis.yaw_velocity, 1.0);
        assert_eq!(chassis.roll, 0.25);
        assert_eq!(chassis.pitch, -0.25);
    }

    /// 松开方向键后，约 60 帧内应减速到近乎停止。
    #[test]
    fn chassis_rotation_smoothly_brakes_to_stop() {
        let mut transform = Transform::default();
        let mut chassis = InfantryChassis {
            yaw: 0.0,
            yaw_velocity: 9.42,
            ..default()
        };

        for _ in 0..60 {
            update_chassis_rotation(
                &mut transform,
                &mut chassis,
                0.0,
                0.0,
                0.0,
                9.42,
                60.0,
                2.0,
                0.016,
            );
        }

        assert!(chassis.yaw_velocity.abs() < 1e-2);
    }

    /// roll/pitch 必须被夹在 ±CHASSIS_TILT_LIMIT 内（不会真的翻车）。
    #[test]
    fn chassis_rotation_bounds_roll_and_pitch_as_swing_angles() {
        let mut transform = Transform::default();
        let mut chassis = InfantryChassis::default();

        update_chassis_rotation(
            &mut transform,
            &mut chassis,
            0.0,
            1.0,
            -1.0,
            2.0,
            60.0,
            2.0,
            10.0,
        );
        assert_eq!(chassis.roll, CHASSIS_TILT_LIMIT);
        assert_eq!(chassis.pitch, -CHASSIS_TILT_LIMIT);

        update_chassis_rotation(
            &mut transform,
            &mut chassis,
            1.0,
            -1.0,
            1.0,
            2.0,
            60.0,
            2.0,
            10.0,
        );

        assert_eq!(chassis.roll, -CHASSIS_TILT_LIMIT);
        assert_eq!(chassis.pitch, CHASSIS_TILT_LIMIT);
        // 合成到 Transform 上的欧拉角也应与组件一致（YXZ 解算顺序）。
        let (_, pitch, roll) = transform.rotation.to_euler(EulerRot::YXZ);
        assert!((roll + CHASSIS_TILT_LIMIT).abs() < 1e-5);
        assert!((pitch - CHASSIS_TILT_LIMIT).abs() < 1e-5);
    }
}
