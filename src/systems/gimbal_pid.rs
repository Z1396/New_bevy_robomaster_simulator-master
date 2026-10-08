//! 云台自瞄闭环：把外部视觉解算（Talos/ROS2）给出的世界系瞄准目标，经双轴 PID
//! 追踪成平滑的云台角速度——取代过去"一次性瞬移姿态"的做法，让云台像真实电机一样运动。
//!
//! 数据流：外部解算 → `GimbalAimTracker`（作为组件挂在云台上，代表"自瞄当前有目标"）
//! → 本文件每帧算误差 → PID → 更新 `InfantryGimbal` 与 `Transform`。

use bevy::prelude::*;

use crate::components::{Controlled, InfantryChassis, InfantryGimbal, InfantryLaunchOffset};
use crate::config::{GimbalAxisPidConfig, SimulationConfig};

/// Absolute muzzle-frame aim target produced by an external auto-aim solver.
/// 中文：外部自瞄解算给出的"炮口系绝对瞄准目标"（单位弧度）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GimbalAimTarget {
    /// 偏航角（弧度）
    pub yaw: f32,
    /// 俯仰角（弧度）
    pub pitch: f32,
}

impl GimbalAimTarget {
    /// Solver commands arrive in degrees with pitch measured from the vertical axis.
    /// 中文：解算方给出的角度以**度**为单位，且 pitch 是从**竖直轴**量起的——
    /// 故这里换算成弧度，并把 pitch 减 90° 转成"从水平面量起"（与 `InfantryGimbal` 一致）。
    pub fn from_solver_degrees(yaw_deg: f32, pitch_deg: f32) -> Self {
        Self {
            yaw: yaw_deg.to_radians(),
            pitch: (pitch_deg - 90.0).to_radians(),
        }
    }

    /// 把 (yaw, pitch) 合成为旋转四元数（YXZ 顺序）。
    fn rotation(self) -> Quat {
        Quat::from_euler(EulerRot::YXZ, self.yaw, self.pitch, 0.0)
    }
}

/// One independent single-axis PID loop.
/// 中文：单轴独立 PID 环。yaw 与 pitch 各持一个，互不共享状态。
#[derive(Clone, Copy, Debug, Default)]
struct AxisPid {
    integral: f32,       // 累积误差项（带限幅，防积分饱和）
    previous_error: f32, // 上一帧误差，用于求微分
}

impl AxisPid {
    /// Angular rate command (rad/s) for this axis given the current error.
    /// 中文：输入当前角度误差，返回本轴要施加的角速度指令（弧度/秒）。
    fn step(&mut self, error: f32, config: &GimbalAxisPidConfig, dt: f32) -> f32 {
        // 积分项累加并夹在 ±integral_limit（anti-windup：防止长时间误差把积分撑爆）。
        self.integral =
            (self.integral + error * dt).clamp(-config.integral_limit, config.integral_limit);
        // 微分项 = 误差变化率；随后记录本次误差供下帧使用。
        let derivative = (error - self.previous_error) / dt;
        self.previous_error = error;

        // 标准 PID：P·e + I·∫e + D·de/dt。
        let rate = error * config.kp + self.integral * config.ki + derivative * config.kd;
        // 输出夹在 ±max_rate（模拟电机最大角速度）。
        rate.clamp(-config.max_rate, config.max_rate)
    }
}

/// Closed-loop state for tracking a solver target. Its presence on the gimbal *is*
/// the "auto-aim has a target" state: no tracker means no target and no PID output,
/// and a freshly inserted tracker starts from clean integrator/derivative state.
/// 中文：追踪某个解算目标的闭环状态。**它挂在云台上这一事实本身就是"自瞄有目标"的语义**：
/// 没有 tracker 就没有目标、也不输出 PID；新插入的 tracker 从干净的积分/微分状态起步。
#[derive(Component, Clone, Copy, Debug)]
pub struct GimbalAimTracker {
    target: GimbalAimTarget,
    yaw: AxisPid,   // 偏航轴 PID 状态
    pitch: AxisPid, // 俯仰轴 PID 状态
}

impl GimbalAimTracker {
    /// 用初始目标创建追踪器（PID 状态清零）。
    pub fn new(target: GimbalAimTarget) -> Self {
        Self {
            target,
            yaw: AxisPid::default(),
            pitch: AxisPid::default(),
        }
    }

    /// 读取当前目标。
    pub fn target(&self) -> GimbalAimTarget {
        self.target
    }

    /// Update the setpoint while keeping the loop state, so a stream of commands
    /// drives one continuous controller instead of restarting every message.
    /// 中文：只更新设定值、**保留** PID 环状态——这样连续的目标流驱动的是同一个连续控制器，
    /// 而不是每条消息都把控制器重启一遍（否则每帧都会丢掉积分/微分历史）。
    pub fn retarget(&mut self, target: GimbalAimTarget) {
        self.target = target;
    }
}

/// 把角度归一化到 (-π, π]，取"最短绕行"路径。
fn wrap_angle(angle: f32) -> f32 {
    // rem_euclid 保证结果非负，再减去 π 移到以 0 为中心。
    let wrapped = (angle + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU);
    wrapped - std::f32::consts::PI
}

/// Tracking error as `(yaw, pitch)` in the gimbal's own local frame, i.e. in the same
/// coordinates as `InfantryGimbal`. The solver target aims the *muzzle* in world space,
/// so the muzzle's fixed mount offset and the chassis rotation are divided out first —
/// measuring the error in world space instead flips the pitch sign for any mount whose
/// local axes disagree with the world axes.
/// 中文：返回云台**本体系**下的 (yaw, pitch) 追踪误差，即与 `InfantryGimbal` 同一坐标系。
/// 解算目标瞄准的是世界系里的**炮口**，所以要先除去炮口的固定安装偏移与底盘旋转；
/// 若直接在世界系里量误差，一旦炮口局部轴与世界轴不同向（如云台转过 90° 后），pitch 符号会翻转。
fn tracking_error(
    target_rotation: Quat,
    gimbal_local: Quat,
    gimbal_world: Quat,
    muzzle_world: Quat,
) -> Vec2 {
    // World-space correction that would put the muzzle on target right now.
    // 世界系下"让炮口对齐目标"所需的修正旋转。
    let correction = target_rotation * muzzle_world.inverse();
    // Same correction expressed as the gimbal's desired local rotation.
    // 把该修正"共轭变换"到云台本体系（`g·世界修正·g⁻¹`），等价于将世界系增量旋到本体系。
    let desired_local = gimbal_local * gimbal_world.inverse() * correction * gimbal_world;

    let (desired_yaw, desired_pitch, _) = desired_local.to_euler(EulerRot::YXZ);
    let (current_yaw, current_pitch, _) = gimbal_local.to_euler(EulerRot::YXZ);

    // 误差 = 期望角 - 当前角，各自取最短绕行。
    Vec2::new(
        wrap_angle(desired_yaw - current_yaw),
        wrap_angle(desired_pitch - current_pitch),
    )
}

/// Replaces the direct pose snap that auto-aim used to apply: the solver target is
/// tracked by a rate-limited PID loop so the gimbal moves like an actuated axis.
/// 中文：取代过去"直接瞬移姿态"的做法——解算目标由**限速的 PID 环**追踪，
/// 于是云台像带电机的一样平滑转动（Input 阶段，仅在自瞄订阅开启时运行）。
pub fn gimbal_pid_controls(
    time: Res<Time>,
    config: Res<SimulationConfig>,
    // `Option<Single<...>>`：tracker 可能不存在（无目标）——那就整体跳过，不报错。
    gimbal: Option<
        Single<
            (
                &mut Transform,
                &GlobalTransform,
                &mut InfantryGimbal,
                &mut GimbalAimTracker,
            ),
            (
                With<Controlled>,
                Without<InfantryChassis>,
                Without<InfantryLaunchOffset>,
            ),
        >,
    >,
    // 炮口节点：误差在它身上量取（见 tracking_error 说明）。
    muzzle: Option<Single<&GlobalTransform, (With<InfantryLaunchOffset>, With<Controlled>)>>,
) {
    // 两者缺一都不做自瞄（`let ... else` 提前返回）。
    let (Some(gimbal), Some(muzzle)) = (gimbal, muzzle) else {
        return;
    };
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return; // dt 为 0 会让微分项除零
    }

    let (mut gimbal_transform, gimbal_global, mut gimbal_data, mut tracker) = gimbal.into_inner();

    // Error is measured on the muzzle, so chassis motion shows up as tracking error
    // instead of being cancelled out by a one-shot correction.
    // 中文：误差在炮口上量取，于是底盘转动会真实地体现为追踪误差，而不会被"一次性修正"抹平。
    let error = tracking_error(
        tracker.target().rotation(),
        gimbal_transform.rotation,
        gimbal_global.rotation(),
        muzzle.rotation(),
    );
    let (yaw_error, pitch_error) = (error.x, error.y);
    let pid = &config.vehicle.gimbal_pid;
    // 两轴各自过 PID，得到本帧的角速度指令（弧度/秒）。
    let yaw_rate = tracker.yaw.step(yaw_error, &pid.yaw, dt);
    let pitch_rate = tracker.pitch.step(pitch_error, &pid.pitch, dt);

    // 把角速度指令积分回姿态角（先反向同步当前欧拉角，再累加）。
    (gimbal_data.local_yaw, gimbal_data.pitch, _) =
        gimbal_transform.rotation.to_euler(EulerRot::YXZ);
    gimbal_data.local_yaw += yaw_rate * dt;
    gimbal_data.pitch = (gimbal_data.pitch + pitch_rate * dt).clamp(
        -config.vehicle.gimbal_pitch_limit,
        config.vehicle.gimbal_pitch_limit,
    );

    gimbal_transform.rotation =
        Quat::from_euler(EulerRot::YXZ, gimbal_data.local_yaw, gimbal_data.pitch, 0.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个只有 P 项的测试配置（便于手算断言）。
    fn axis(kp: f32, max_rate: f32) -> GimbalAxisPidConfig {
        GimbalAxisPidConfig {
            kp,
            ki: 0.0,
            kd: 0.0,
            integral_limit: 1.0,
            max_rate,
        }
    }

    /// 纯比例项的输出符号/大小应与误差一致。
    #[test]
    fn proportional_rate_follows_error_sign_and_magnitude() {
        let mut pid = AxisPid::default();

        assert!((pid.step(0.2, &axis(10.0, 20.0), 0.01) - 2.0).abs() < 1e-5);
        assert!((pid.step(-0.1, &axis(10.0, 20.0), 0.01) + 1.0).abs() < 1e-5);
    }

    /// 两轴必须使用各自的增益与限速（pitch 被 max_rate=1.0 截断）。
    #[test]
    fn each_axis_uses_its_own_gains_and_rate_limit() {
        let mut tracker = GimbalAimTracker::new(GimbalAimTarget {
            yaw: 0.0,
            pitch: 0.0,
        });
        let yaw_config = axis(10.0, 20.0);
        let pitch_config = axis(4.0, 1.0);

        let yaw_rate = tracker.yaw.step(0.5, &yaw_config, 0.01);
        let pitch_rate = tracker.pitch.step(0.5, &pitch_config, 0.01);

        assert!((yaw_rate - 5.0).abs() < 1e-5);
        assert_eq!(pitch_rate, 1.0);
    }

    /// yaw 与 pitch 的 PID 状态互不传染。
    #[test]
    fn axis_loop_state_is_not_shared_between_yaw_and_pitch() {
        let mut tracker = GimbalAimTracker::new(GimbalAimTarget {
            yaw: 0.0,
            pitch: 0.0,
        });
        let config = axis(0.0, 20.0);

        tracker.yaw.step(1.0, &config, 0.5);

        assert_eq!(tracker.yaw.integral, 0.5);
        assert_eq!(tracker.pitch.integral, 0.0);
        assert_eq!(tracker.pitch.previous_error, 0.0);
    }

    /// 积分项必须被限幅，长时间大误差也不会无限增长（anti-windup）。
    #[test]
    fn integral_term_is_bounded_against_windup() {
        let mut config = axis(0.0, 20.0);
        config.ki = 1.0;
        let mut pid = AxisPid::default();

        for _ in 0..1000 {
            pid.step(1.0, &config, 0.01);
        }

        assert_eq!(pid.integral, config.integral_limit);
    }

    /// retarget 只换设定值、保留环状态，保证命令流是连续的。
    #[test]
    fn retarget_keeps_loop_state_so_command_stream_is_continuous() {
        let mut tracker = GimbalAimTracker::new(GimbalAimTarget {
            yaw: 0.0,
            pitch: 0.0,
        });
        tracker.yaw.step(0.1, &axis(10.0, 20.0), 0.01);
        let integral = tracker.yaw.integral;

        tracker.retarget(GimbalAimTarget {
            yaw: 0.5,
            pitch: 0.1,
        });

        assert_eq!(tracker.yaw.integral, integral);
        assert_eq!(tracker.target().yaw, 0.5);
    }

    /// 无安装偏移、无底盘旋转时，误差应直接等于本体系的角增量。
    #[test]
    fn error_points_the_same_way_as_the_local_angles_on_a_bare_gimbal() {
        let target = Quat::from_euler(EulerRot::YXZ, 0.3, -0.2, 0.0);
        let error = tracking_error(target, Quat::IDENTITY, Quat::IDENTITY, Quat::IDENTITY);

        assert!((error.x - 0.3).abs() < 1e-5);
        assert!((error.y + 0.2).abs() < 1e-5);
    }

    /// Guards the pitch sign. The gimbal's pitch axis is its *local* X, which points
    /// against world X once the gimbal has yawed past 90 degrees; an error read off the
    /// world-space correction is inverted there and drives pitch away from the target.
    /// 中文：守住 pitch 符号——云台转过 90° 后其俯仰轴（本体系 X）与世界 X 反向，
    /// 若用世界系修正来读误差，这里会反号并把云台推离目标。
    #[test]
    fn pitch_error_keeps_its_sign_when_the_gimbal_faces_backwards() {
        let local = Quat::from_rotation_y(std::f32::consts::PI);
        let target = local * Quat::from_rotation_x(0.1);
        let error = tracking_error(target, local, local, local);

        assert!(error.x.abs() < 1e-5, "yaw leaked {}", error.x);
        assert!((error.y - 0.1).abs() < 1e-5, "pitch error was {}", error.y);
    }

    /// 在"底盘有偏航 + 炮口有安装偏移"的情形下，闭环应仍收敛到目标角。
    #[test]
    fn closed_loop_converges_through_a_mount_offset_and_chassis_yaw() {
        let chassis = Quat::from_rotation_y(2.8);
        let mount = Quat::from_euler(EulerRot::YXZ, 0.4, 0.25, 0.15);
        let goal = Quat::from_euler(EulerRot::YXZ, 0.3, -0.2, 0.0);
        let target = chassis * goal * mount;

        let mut tracker = GimbalAimTracker::new(GimbalAimTarget {
            yaw: 0.0,
            pitch: 0.0,
        });
        let yaw_config = axis(10.0, 20.0);
        let pitch_config = axis(6.0, 20.0);
        // 手工积分 600 步模拟闭环（dt = 1/120 s ≈ 5 s）。
        let mut local = Quat::IDENTITY;
        let dt = 1.0 / 120.0;

        for _ in 0..600 {
            let gimbal_world = chassis * local;
            let error = tracking_error(target, local, gimbal_world, gimbal_world * mount);
            let (mut yaw, mut pitch, _) = local.to_euler(EulerRot::YXZ);
            yaw += tracker.yaw.step(error.x, &yaw_config, dt) * dt;
            pitch += tracker.pitch.step(error.y, &pitch_config, dt) * dt;
            local = Quat::from_euler(EulerRot::YXZ, yaw, pitch, 0.0);
        }

        let (yaw, pitch, _) = local.to_euler(EulerRot::YXZ);
        assert!((yaw - 0.3).abs() < 1e-3, "yaw settled at {yaw}");
        assert!((pitch + 0.2).abs() < 1e-3, "pitch settled at {pitch}");
    }

    /// 角度归一化应走最短方向（360°+0.2 与 0.2 等价）。
    #[test]
    fn wrap_angle_takes_the_short_way_around() {
        assert!((wrap_angle(std::f32::consts::TAU + 0.2) - 0.2).abs() < 1e-5);
        assert!((wrap_angle(std::f32::consts::PI + 0.1) + std::f32::consts::PI - 0.1).abs() < 1e-5);
    }
}
