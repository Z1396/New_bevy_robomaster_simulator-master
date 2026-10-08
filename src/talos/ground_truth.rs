//! 真值（ground truth）发布：把仿真世界的"标准答案"发给外部视觉算法，用于评测/训练。
//!
//! 【发布内容与单位】每帧打包一批 `GroundTruthBatch`（布局见 talos-ipc/layout.rs），包含：
//! - **全体战车**（含敌方、`Without<Controlled>` 与 `With<Controlled>` 两查询合并）：
//!   世界位置（米，ROS 系）、偏航角 `yaw`（弧度）、偏航角速度 `vyaw`（rad/s）、
//!   阵营 `team`（0=红/1=蓝）、装甲标签 `armor_label`；
//! - **能量机关**：中心位置（米，ROS 系）、当前旋转角 `current_angle`（弧度）、
//!   旋转方向 `direction`、机关状态/目标激活状态，以及正弦运动参数。
//!
//! 【坐标对齐】所有位置/角速度先经 `M_ALIGN_MAT3` 从 Bevy(Y-up) 转到 ROS(Z-up) 系
//! （见 plugin.rs），与图像/位姿通道保持一致，视觉端无需再变换。
//!
//! 与图像/位姿一样在 `Last` 阶段发布（注册见 plugin.rs），保证时间戳统一。

use crate::components::{Controlled, Infantry};
use crate::robomaster::prelude::{
    Activation, MechanismState, PowerRune, PowerRuneMechanism, PowerRuneRotation, RuneMode, Team,
};
use crate::talos::capture::{TalosCaptureContext, TalosFrameStamp};
use crate::talos::plugin::M_ALIGN_MAT3;
use avian3d::prelude::AngularVelocity; // 物理引擎写入的角速度组件（rad/s），用于 vyaw
use bevy::prelude::*;
use talos_ipc::*;

/// 把 Bevy 系向量转到 ROS 系（左乘对齐矩阵 `M_ALIGN_MAT3`，单位不变）。
fn to_ros_vec3(v: Vec3) -> Vec3 {
    M_ALIGN_MAT3 * v
}

/// 阵营 → 协议字节：红=0，蓝=1。
fn team_to_u8(team: &Team) -> u8 {
    match team {
        Team::Red => 0,
        Team::Blue => 1,
    }
}

/// 激活状态 → 协议字节（0..=3）。`match` 逐一映射，协议序号在此固定。
fn activation_to_u8(a: &Activation) -> u8 {
    match a {
        Activation::Deactivated => 0,
        Activation::Activating => 1,
        Activation::Activated => 2,
        Activation::Completed => 3,
    }
}

/// 机关状态 → 协议字节。`Inactive { .. }` / `Activated { .. }` 是"忽略变体内字段"
/// 的模式：只关心是哪种状态，不读其携带的数据。
fn mechanism_state_to_u8(s: &MechanismState) -> u8 {
    match s {
        MechanismState::Inactive { .. } => 0,
        MechanismState::Activating(_) => 1,
        MechanismState::Activated { .. } => 2,
        MechanismState::Failed { .. } => 3,
    }
}

/// 能量机关模式 → 协议字节：小符=0，大符=1。
fn rune_mode_to_u8(m: &RuneMode) -> u8 {
    match m {
        RuneMode::Small => 0,
        RuneMode::Large => 1,
    }
}

/// Compute yaw in the ROS reference frame from a Bevy GlobalTransform.
///
/// The alignment matrix maps Bevy (Y-up) → ROS (Z-up).
/// We convert the rotation quaternion through the alignment to extract the Z-up yaw.
/// 返回值单位：**弧度**。做法：把旋转经 `M_ALIGN_MAT3` 换到 ROS 系后，取其绕 ROS Z 轴
/// 的偏航角（下称 `yaw`）——ZYX 欧拉分解的第三分量即 yaw。
fn ros_yaw(global_tf: &GlobalTransform) -> f32 {
    let align_quat = Quat::from_mat3(&M_ALIGN_MAT3);
    // 换基：A * q * A⁻¹（同 plugin.rs 的 to_ros_quat 思路）。
    let ros_rot = align_quat * global_tf.rotation() * align_quat.inverse();
    // `to_euler(EulerRot::ZYX)` 返回 (z, y, x) 三个角；正好忽略前两个、取第三个 yaw。
    let (_, _, yaw) = ros_rot.to_euler(EulerRot::ZYX);
    yaw
}

/// 每帧收集全体战车/能量机关的真值，打包成一批后发布（在 `Last` 阶段运行）。
pub fn publish_ground_truth_system(
    context: Option<Res<TalosCaptureContext>>,
    frame_stamp: Res<TalosFrameStamp>,
    // 敌方战车：不带 Controlled（不带 = 非本机操控）。
    infantry_query: Query<
        (&GlobalTransform, Option<&AngularVelocity>, &Infantry),
        Without<Controlled>,
    >,
    // 本机战车：带 Controlled。分两个查询是因为过滤器要分开写，再合并迭代。
    controlled_query: Query<
        (&GlobalTransform, Option<&AngularVelocity>, &Infantry),
        With<Controlled>,
    >,
    // 能量机关：读全局变换、局部变换（取当前转角）+ 三个机关组件。
    rune_query: Query<(
        &GlobalTransform,
        &Transform,
        &PowerRune,
        &PowerRuneMechanism,
        &PowerRuneRotation,
    )>,
) {
    let Some(ctx) = context else {
        return;
    };

    let frame_seq = frame_stamp.frame_seq;
    let timestamp_ns = frame_stamp.timestamp_ns;

    // 整批真值先建默认值，再逐项填充；`Default` 把所有计数字段清零、数组填充。
    let mut batch = GroundTruthBatch::default();
    batch.frame_seq = frame_seq;
    batch.timestamp_ns = timestamp_ns;

    // Collect robot ground truth from all infantry robots
    // `iter().chain(...)`：把两个查询的迭代器首尾相接，一次 for 循环遍历全部战车。
    let all_robots = infantry_query.iter().chain(controlled_query.iter());

    for (global_tf, ang_vel, infantry) in all_robots {
        // 位置先转到 ROS 系，单位米。
        let pos_ros = to_ros_vec3(global_tf.translation());
        let team = &infantry.team;
        let config = infantry.config;

        // 偏航角速度 vyaw = ROS 系角速度向量的 Z 分量，单位 rad/s。
        // `ang_vel` 是 Option（部分实体可能没挂 AngularVelocity）：
        // `.map(...).unwrap_or(0.0)` —— 有则取 z 分量，无则记 0。
        let vyaw = ang_vel
            .map(|av| {
                let ros_ang = to_ros_vec3(av.0);
                ros_ang.z
            })
            .unwrap_or(0.0);

        // 偏航角，单位弧度。
        let yaw = ros_yaw(global_tf);

        // 容量上限保护：超出 `GROUND_TRUTH_MAX_TARGETS` 的战车丢弃（避免越界写数组）。
        if (batch.target_count as usize) < GROUND_TRUTH_MAX_TARGETS {
            let idx = batch.target_count as usize;
            batch.targets[idx] = GroundTruthTarget {
                frame_seq,
                timestamp_ns,
                team: team_to_u8(team), // 阵营：0 红 / 1 蓝
                armor_label: config.armor.label() as u8, // 装甲编号
                is_outpost: 0, // 前哨站标记（本项目暂未用）
                _pad1: 0,
                position: [pos_ros.x, pos_ros.y, pos_ros.z], // 世界位置 [x,y,z]，单位米（ROS 系）
                vyaw, // 偏航角速度，rad/s
                yaw,  // 偏航角，rad
                _pad: [0; 24],
            };
            batch.target_count += 1;
        }
    }

    // Collect rune ground truth
    for (global_tf, local_tf, power_rune, mechanism, rotation) in rune_query.iter() {
        // 超过上限直接结束（与战车不同：这里用 break 跳出整个循环）。
        if (batch.rune_count as usize) >= GROUND_TRUTH_MAX_RUNES {
            break;
        }

        // 机关中心世界位置，单位米（ROS 系）。
        let pos_ros = to_ros_vec3(global_tf.translation());

        // Extract current rotation angle around the actual rune axis (-1, 0, -1).
        // The rune rotates via `rotate_local_axis(direction, angle)`, so we must
        // project the quaternion back onto that axis — not extract an Euler X angle.
        // 机关绕固定轴 (-1, 0, -1) 转（见 rotation.rs），所以必须把四元数投影回该轴，
        // 得到带符号的总转角，而不能简单取某个欧拉角分量。
        let rune_axis = Dir3::from_xyz(-1.0, 0.0, -1.0).unwrap();
        let (axis, angle) = local_tf.rotation.to_axis_angle();
        // `axis.dot(rune_axis).signum()` 判断转轴正负方向一致与否，从而给角度定正负。
        let current_angle = angle * axis.dot(*rune_axis).signum(); // 单位：弧度

        let controller = rotation.controller();
        // 旋转方向：顺时针=+1，逆时针=-1（供接收端预测下一帧角度）。
        let direction = if controller.is_clockwise() { 1 } else { -1 };

        // 变速参数（仅大机关激活时有效）：a=正弦振幅(rad/s)，omega=角频率(rad/s)，
        // t=累计时间(s)；`sin_offset` 取 `2.090 - a`（protocol 约定的速度偏移，rad/s）。
        let (sin_amplitude, sin_omega, relative_time, sin_offset) = controller
            .variable_params()
            .map(|(a, omega, t)| (a, omega, t, 2.090 - a))
            .unwrap_or((0.0, 0.0, 0.0, 0.0));

        // 5 个靶位各自的激活状态（0..=3，见 activation_to_u8），打包成定长数组。
        let mut target_activations = [0u8; 5];
        for (i, a) in mechanism.state().target_states().iter().enumerate() {
            if i < 5 {
                target_activations[i] = activation_to_u8(a);
            }
        }

        let idx = batch.rune_count as usize;
        batch.runes[idx] = GroundTruthRune {
            frame_seq,
            timestamp_ns,
            team: team_to_u8(&power_rune.team()), // 机关所属阵营
            rune_mode: rune_mode_to_u8(&power_rune.mode()), // 大/小符
            mechanism_state: mechanism_state_to_u8(mechanism.state()), // 机关状态 0..=3
            _pad1: 0,
            r_center_odom: [pos_ros.x, pos_ros.y, pos_ros.z], // 中心位置，单位米（ROS 系）
            radius: 0.0,       // 预留：半径（当前未填）
            current_angle,     // 当前转角，单位弧度
            v_roll: 0.0,       // 预留：滚转角速度（当前未填）
            direction,         // 旋转方向：+1 顺时针 / -1 逆时针
            sin_amplitude,     // 变速正弦振幅，单位 rad/s
            sin_omega,         // 变速角频率，单位 rad/s
            sin_phase: 0.0,    // 预留：相位（当前未填）
            sin_offset,        // 变速速度偏移 2.090 - a，单位 rad/s
            relative_time,     // 累计时间，单位秒
            blade_id: -1,      // 预留：叶片编号（-1 表示未填）
            target_activations,
            _pad: [0; 20],
        };
        batch.rune_count += 1;
    }

    // 加锁一次性发出整批真值（`&batch` 借用，避免把大数组按值传走）。
    if let Ok(mut publisher) = ctx.publisher.lock() {
        publisher.publish_ground_truth(&batch);
    }
}
