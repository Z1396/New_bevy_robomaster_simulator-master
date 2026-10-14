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
    chassis_transform: &mut Transform, // 底盘节点变换
    chassis_data: &mut InfantryChassis, // 底盘节点数据
    yaw_input: f32, // 云台手动瞄准输入（Yaw）
    roll_input: f32, // 云台手动瞄准输入（Roll）
    pitch_input: f32, // 云台手动瞄准输入（Pitch）
    yaw_rotation_speed: f32, // 底盘旋转速度
    yaw_acceleration: f32, // 底盘旋转加速度
    tilt_rotation_speed: f32, // 底盘摆动速度
    dt: f32, // 时间间隔
) {
    // 目标角速度 = 输入(-1..1) × 最大转速。
    let target_yaw_velocity = yaw_input * yaw_rotation_speed;
    // 本帧允许的最大速度增量 = 角加速度 × dt（限加速率，避免瞬间满转速）。
    let max_velocity_delta = yaw_acceleration * dt;
    // 本帧目标角速度 = 目标角速度 - 当前角速度（限加速率，避免瞬间满转速）。
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
    time: Res<Time>,    // 时间组件，用于获取 dt
    controller: Res<ControllerState>,   // 控制器状态，包含玩家输入的本机车输入
    config: Res<SimulationConfig>,   // 模拟配置
    // `Forces` 是 avian（bevy 版本） 的"施力入口(意思就是输入的力)"；`&Mass` 读质量；`&mut VehicleDynamic` 用其非线性加速模型。
    // (With<Infantry>, With<Controlled>) 表示只作用于本机玩家操控的战车 single 表示只有一个战车
    infantry: Single<(Forces, &Mass, &mut VehicleDynamic), (With<Infantry>, With<Controlled>)>,
    // 云台（提供前进方向的参考坐标系）：底盘节点被 `Without<InfantryChassis>` 排除。
    // (GlobalTransform, &InfantryGimbal) 表示云台的全局变换和云台组件
    // (With<Controlled>, Without<InfantryChassis>) 表示只作用于本机玩家操控的战车的云台
    gimbal: Single<(&GlobalTransform, &InfantryGimbal), (With<Controlled>, Without<InfantryChassis>)>,
    // 底盘节点：要求有 InfantryChassis、无 InfantryGimbal（与上面正好互补），排除根实体 Infantry。
    chassis: Single<(&mut Transform, &mut InfantryChassis),
        (
            With<Controlled>,   // 只作用于本机玩家操控的战车
            Without<InfantryGimbal>,   // 排除云台节点
            With<InfantryChassis>,   // 只作用于底盘节点
            Without<Infantry>,   // 排除根实体 Infantry
        ),
    >,
) {
    // 从控制器中获取输入
    let controller = controller.controlled;
    let input = controller.movement; // Vec2：x=左右，y=前后，各 -1..1
    let boost = controller.boost_multiplier(); // 加速倍率（未按加速键为 1.0）
    // 从战车中获取力、质量、动态组件
    let (mut forces, &Mass(mass), mut dynamic) = infantry.into_inner();

    // dt 单位秒；`linear` 内部会按质量与 dt 施加冲量（见 vehicle/movement.rs）。
    let dt = time.delta_secs();
    // 底盘线性力：输入 → 底盘线性力 + 底盘姿态（Input 阶段；自由相机下被跳过）
    // 参数单位：速度均为 弧度/秒，加速度 弧度/秒²，dt 为秒。
    dynamic.linear(
        &mut forces,
        mass, // 质量
        gimbal.into_inner().0, // 以云台朝向为"前进方向"参考（车会朝云台指的方向走）
        input, // 输入
        dt, // 时间间隔
        boost, // 加速倍率
    );

    // 底盘姿态：输入 → 底盘姿态（Input 阶段；自由相机下被跳过）
    // 参数单位：速度均为 弧度/秒，加速度 弧度/秒²，dt 为秒。
    let (mut chassis_transform, mut chassis_data) = chassis.into_inner();
    update_chassis_rotation(
        &mut chassis_transform, // 底盘节点变换
        &mut chassis_data, // 底盘节点数据
        controller.chassis_yaw, // 云台手动瞄准输入（Yaw）
        controller.chassis_roll, // 云台手动瞄准输入（Roll）
        controller.chassis_pitch, // 云台手动瞄准输入（Pitch）
        config.vehicle.rotation_speed, // 底盘旋转速度
        config.vehicle.yaw_acceleration, // 底盘旋转加速度
        config.vehicle.tilt_rotation_speed, // 底盘摆动速度
        dt, // 时间间隔
    );
}

/// 遥控底盘驾驶：作用于被 Tab 选中的 AI 车（`ActiveSlapper`），逻辑与 `vehicle_controls` 同构。
pub fn remote_vehicle_controls(
    time: Res<Time>,    // 时间组件，用于获取 dt
    controller: Res<ControllerState>,   // 控制器状态，包含玩家输入的遥控车输入
    config: Res<SimulationConfig>,   // 模拟配置
    // (With<ActiveSlapper>, With<Infantry>, Without<Controlled>) 表示只作用于被 Tab 选中的 AI 车
    infantry: Single<(&GlobalTransform, Forces, &Mass, &mut VehicleDynamic),(With<ActiveSlapper>, With<Infantry>, Without<Controlled>),>,
    // (With<ActiveSlapper>, With<InfantryChassis>, Without<InfantryGimbal>, Without<Infantry>) 表示只作用于被 Tab 选中的 AI 车的底盘
    chassis: Single<(&mut Transform, &mut InfantryChassis),
        (
            With<ActiveSlapper>,   // 只作用于被 Tab 选中的 AI 车
            With<InfantryChassis>,   // 只作用于底盘节点
            Without<InfantryGimbal>,   // 排除云台节点
            Without<Infantry>,   // 排除根实体 Infantry
        ),
    >,
) {
    // 从控制器中获取输入
    let controller = controller.remote;
    let input = controller.movement; // Vec2：x=左右，y=前后，各 -1..1
    let boost = controller.boost_multiplier(); // 加速倍率（未按加速键为 1.0）
    // 从战车中获取力、质量、动态组件
    let (infantry_global_transform, mut forces, &Mass(mass), mut dynamic) = infantry.into_inner();

    let dt = time.delta_secs();
    dynamic.linear(
        &mut forces,    // 底盘线性力
        mass, // 质量
        infantry_global_transform, // 遥控车用自身朝向作前进参考（本机用云台朝向）
        input,// 输入的移动向量（x=左右，y=前后，各 -1..1）
        dt,// 从上一帧到当前帧时间步长（秒）
        boost,// 加速倍率（未按加速键为 1.0）
    );
    // 底盘姿态：输入 → 底盘姿态（Input 阶段；自由相机下被跳过）
    // 参数单位：速度均为 弧度/秒，加速度 弧度/秒²，dt 为秒。
    let (mut chassis_transform, mut chassis_data) = chassis.into_inner();
    update_chassis_rotation(
        &mut chassis_transform,// 底盘节点变换
        &mut chassis_data,// 底盘组件
        controller.chassis_yaw,// 底盘横滚角度（弧度）
        controller.chassis_roll,// 底盘俯仰角度（弧度）
        controller.chassis_pitch,// 底盘俯仰角度（弧度）
        config.vehicle.rotation_speed,// 底盘旋转速度（弧度/秒）
        config.vehicle.yaw_acceleration,// 底盘横滚加速度（弧度/秒²）
        config.vehicle.tilt_rotation_speed,// 底盘俯仰旋转速度（弧度/秒）
        dt,// 从上一帧到当前帧时间步长（秒）
    );
}

/// 本机云台手动瞄准（自瞄订阅开启时交给 gimbal_pid 接管，本系统直接返回）。
/// 本机云台手动瞄准（自瞄订阅开启时交给 gimbal_pid 接管，本系统直接返回）。
pub fn gimbal_controls(
    time: Res<Time>,    // 时间组件，用于获取 dt
    controller: Res<ControllerState>,   // 控制器状态，包含玩家输入的云台手动瞄准输入
    enabled: Res<SubscribeAutoAim>,   // 自瞄订阅状态
    config: Res<SimulationConfig>,   // 模拟配置
    // (With<Controlled>, Without<InfantryChassis>) 表示只作用于被 Tab 选中的 AI 车的云台
    gimbal: Single<(&mut Transform, &mut InfantryGimbal),(With<Controlled>, Without<InfantryChassis>),>,
) {
    // 自瞄开启时不接受手动输入，避免两套控制互相打架。
    if enabled.load(Ordering::Acquire) {
        return;
    }

    let dt = time.delta_secs();
    // 从云台中获取变换、组件
    let (mut gimbal_transform, mut gimbal_data) = gimbal.into_inner();

    // 反向同步：先把当前旋转解算成 yaw/pitch 写回组件，保证组件状态与 Transform 一致。
    (gimbal_data.local_yaw, gimbal_data.pitch, _) = gimbal_transform.rotation.to_euler(EulerRot::YXZ);

    // 从控制器中获取输入
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
    // 云台姿态：输入 → 云台姿态（Input 阶段；自由相机下被跳过）
    // 参数单位：速度均为 弧度/秒，加速度 弧度/秒²，dt 为秒。
    let gimbal_rotation =
        Quat::from_euler(EulerRot::YXZ, gimbal_data.local_yaw, gimbal_data.pitch, 0.0);

    gimbal_transform.rotation = gimbal_rotation;// 更新云台变换
}

/// 遥控云台瞄准：作用于被选中的 AI 车，逻辑与 `gimbal_controls` 同构（不受自瞄开关影响）。
/// 遥控云台瞄准：作用于被选中的 AI 车，逻辑与 `gimbal_controls` 同构（不受自瞄开关影响）。
pub fn remote_gimbal_controls(
    time: Res<Time>,    // 时间组件，用于获取 dt
    controller: Res<ControllerState>,   // 控制器状态，包含玩家输入的遥控车云台输入
    config: Res<SimulationConfig>,   // 模拟配置
    // (With<ActiveSlapper>, Without<InfantryChassis>) 表示只作用于被 Tab 选中的 AI 车的云台
    gimbal: Single<
        (&mut Transform, &mut InfantryGimbal),
        (With<ActiveSlapper>, Without<InfantryChassis>),
    >,
) {
    let dt = time.delta_secs();
    // 从云台中获取变换、组件
    let (mut gimbal_transform, mut gimbal_data) = gimbal.into_inner();
    // 反向同步：先把当前旋转解算成 yaw/pitch 写回组件，保证组件状态与 Transform 一致。
    (gimbal_data.local_yaw, gimbal_data.pitch, _) =
        gimbal_transform.rotation.to_euler(EulerRot::YXZ);// 云台本体系下的 yaw, pitch

    // 从控制器中获取输入
    let controller = controller.remote;
    let rotation_speed = config.vehicle.gimbal_rotation_speed * controller.gimbal_scale() * dt; // 云台旋转速度（弧度/秒）
    gimbal_data.local_yaw += controller.gimbal.x * rotation_speed; // 累加 yaw 角度
    gimbal_data.pitch += controller.gimbal.y * rotation_speed; // 累加 pitch 角度
    // 俯仰夹在配置的上下限内（config 的 gimbal_pitch_limit，单位弧度）。
    gimbal_data.pitch = gimbal_data.pitch.clamp(
        -config.vehicle.gimbal_pitch_limit,
        config.vehicle.gimbal_pitch_limit,
    );
    // 云台姿态：输入 → 云台姿态（Input 阶段；自由相机下被跳过）
    // 参数单位：速度均为 弧度/秒，加速度 弧度/秒²，dt 为秒。
    let gimbal_rotation =
        Quat::from_euler(EulerRot::YXZ, gimbal_data.local_yaw, gimbal_data.pitch, 0.0);

    gimbal_transform.rotation = gimbal_rotation;// 更新云台变换
}

/// Tab 切换"操控哪台车"：把 `ActiveSlapper` 从当前车摘掉、挂到环形序列的下一台。
///
/// `ActiveSlapper` 在根实体**和所有后代**上都要一致地存在/移除：根实体上的有无决定
/// 展示战车自转系统是否匹配（见 spin.rs），后代上的有无决定各操控系统能否用
/// `With/Without<ActiveSlapper>` 命中云台/底盘节点。
/// Tab 切换"操控哪台车"：把 `ActiveSlapper` 从当前车摘掉、挂到环形序列的下一台。
pub fn switch_slapper_control(
    mut commands: Commands,     // 命令组件，用于修改实体组件
    controller: Res<ControllerState>,   // 控制器状态，包含玩家输入的切换车输入
    children: Query<&Children>,   //  子代查询组件
    slapper_roots: Query<Entity, (With<Infantry>, With<SlapperInfantry>)>,   //  全部可选车（有 Infantry + SlapperInfantry）。
    active_root: Query<Entity, (With<Infantry>, With<SlapperInfantry>, With<ActiveSlapper>)>,   // 当前被选中的那台。
    spinning_roots: Query<(), With<Spinning>>,// 只关心"是否是展示战车"，不要数据 → `Query<(), With<Spinning>>`
) {
    // Tab 键按下时才切换
    if !controller.controlled.switch_slapper_just_pressed {
        return;
    }

    // 收集所有可选车的实体（有 Infantry + SlapperInfantry 组件）
    let roots: Vec<Entity> = slapper_roots.iter().collect();
    // 只有一台可切换车时无意义
    if roots.len() <= 1 {
        return; 
    }

    // active_root.single() 返回当前被选中的那台车; ok()表示转为Option<Entity>类型表示如果查询失败（没有选中车），则返回 None 而不是 Result<Err, Entity>
    // 到下一行的 `and_then` 中处理 None 情况。
    let current = active_root.single().ok();
    // current.and_then(|e| roots.iter().position(|&r| r == e))：如果 `current` 有值，返回 `roots` 中 `e` 的下标；否则返回 None
    let current_idx = current.and_then(|e| roots.iter().position(|&r| r == e));
    // 判断是否有当前车的下标，有则计算下一个一台的下标，无则从0开始(注：下标是在之前的哪个plugin中设置的？顺序是实现细节，不是契约 。升级引擎版本、调整场景加载顺序、增删任何组件，都可能让 Tab 的轮换次序变化。)
    let next_idx = match current_idx {
        Some(idx) => (idx + 1) % roots.len(),
        None => 0,
    };

    // 【调试】打印本次 Tab 切换时的可选车顺序与下标，用于验证 roots 顺序是否稳定。
    // 验证完即可删除本行。
    info!(
        "tab-switch: roots={:?} current={:?} current_idx={:?} next_idx={}",
        roots, current, current_idx, next_idx
    );

    // （根实体 + 全部后代都要摘，见上方文档说明。）
    if let Some(current_root) = current {
        // 从当前车摘掉 ActiveSlapper 组件
        commands.entity(current_root).remove::<ActiveSlapper>();
        // 从当前车的所有后代摘掉 ActiveSlapper 组件
        for descendant in children.iter_descendants(current_root) {
            commands.entity(descendant).remove::<ActiveSlapper>();
        }
    }

    // 切换到下一个一台车
    let next_root = roots[next_idx];
    // 判断下一个车是否是展示战车
    // Spinning 组件全程不摘不挂，切换状态完全由 ActiveSlapper 的有无决定。
    if spinning_roots.contains(next_root) {
        // 展示战车被选中：同批瞬间刹停（角速度清零 + 角阻尼恢复）
        commands.entity(next_root).insert((
            AngularVelocity(Vec3::ZERO),// AngularVelocity表示角速度设置为0
            AngularDamping(50.0),// AngularDamping表示角阻尼恢复为 setup_vehicle 实配值 50.0
        ));
    }
    // 挂载 ActiveSlapper 到next_root的根实体及其所有后代实体
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
