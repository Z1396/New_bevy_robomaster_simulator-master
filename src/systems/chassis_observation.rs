//! 底盘运动学观测：把物理引擎里的速度/角速度换算成"机身系"的运动学量（供
//! Talos/ROS2 作为真值下发），并推导麦轮四轮的线速度/角速度。
//!
//! 坐标约定：Bevy 世界为 Y-up 右手系（前 = -Z、右 = +X、上 = +Y）；而机器人机体用"前-左-上"
//! 惯例，故本文件有 `bevy_local_to_body` / `bevy_to_body_quat` 两处轴系重映射。
//! 相关：位姿的 ROS Z-up 对齐矩阵 `M_ALIGN_MAT3` 见 talos/ipc 侧。

use avian3d::prelude::{AngularVelocity, LinearVelocity};
use bevy::prelude::*;

use crate::components::{Controlled, Infantry};
use crate::config::{MecanumConfig, SimulationConfig};

/// 麦轮数量固定为 4。
const NUM_WHEELS: usize = 4;
/// 轮半径下限（米），防止配置为 0 时除零。
const MIN_RADIUS_M: f32 = 1e-6;

/// 每帧发布一次的底盘观测帧（全局资源）。字段命名后缀即单位：
/// `_s`=秒、`_mps`=米/秒、`_radps`=弧度/秒、`_m`=米、`_rad`=弧度、`_mps2`=米/秒²。
#[derive(Resource, Debug, Clone)]
pub struct ChassisObservationFrame {
    pub stamp_s: f64,     // 采样时刻（秒）
    pub dt_s: f32,        // 距上一帧的间隔（秒）
    pub v_body: Vec2,     // 机身系线速度 (x=前, y=左)，单位 m/s
    pub wz_radps: f32,    // 机身系绕 Z 轴角速度（偏航率），单位 rad/s
    // Wheel order: [FL, FR, RL, RR]
    pub wheel_linear_mps: [f32; NUM_WHEELS], // 四轮线速度（m/s），顺序 [前左, 前右, 后左, 后右]
    // Wheel order: [FL, FR, RL, RR]
    pub wheel_angular_radps: [f32; NUM_WHEELS], // 四轮角速度（rad/s）
    pub a_body: Vec2,           // 机身系线加速度 (x=前, y=左)，单位 m/s²
    pub alpha_z_radps2: f32,    // 偏航角加速度（rad/s²）
    pub rpy_rad: Vec3,          // 机体系横滚/俯仰/偏航（rad）
    pub gyro_xyz_radps: Vec3,   // 机体系角速度（rad/s，对应陀螺仪读数）
    pub accel_xyz_mps2: Vec3,   // 机体系加速度（m/s²，对应加速度计读数）
}

impl Default for ChassisObservationFrame {
    fn default() -> Self {
        // 全零帧（车辆不存在时作为"空观测"发布）。
        Self {
            stamp_s: 0.0,
            dt_s: 0.0,
            v_body: Vec2::ZERO,
            wz_radps: 0.0,
            wheel_linear_mps: [0.0; NUM_WHEELS],
            wheel_angular_radps: [0.0; NUM_WHEELS],
            a_body: Vec2::ZERO,
            alpha_z_radps2: 0.0,
            rpy_rad: Vec3::ZERO,
            gyro_xyz_radps: Vec3::ZERO,
            accel_xyz_mps2: Vec3::ZERO,
        }
    }
}

/// 上一帧的运动学状态，用于差分求加速度（私有：字段仅本模块使用）。
#[derive(Resource, Debug, Clone, Default)]
pub struct PreviousKinematicState {
    initialized: bool,   // 首帧尚无历史，差分结果无效
    v_body: Vec2,        // 上一帧机身线速度
    wz_radps: f32,       // 上一帧偏航率
}

/// 每帧更新底盘观测帧（PostUpdate，`.after(TransformSystems::Propagate)`）。
pub fn update_chassis_observation(
    time: Res<Time>,
    config: Res<SimulationConfig>,
    mut frame: ResMut<ChassisObservationFrame>,
    mut previous: ResMut<PreviousKinematicState>,
    chassis: Query<
        (&GlobalTransform, &LinearVelocity, &AngularVelocity),
        (With<Infantry>, With<Controlled>),
    >,
) {
    // 车辆不存在（0 或多个）：发布全零帧并重置差分历史。
    let Ok((chassis_tf, linear_velocity, angular_velocity)) = chassis.single() else {
        *frame = ChassisObservationFrame::default();
        *previous = PreviousKinematicState::default();
        return;
    };

    let stamp_s = time.elapsed_secs_f64();
    let dt_s = time.delta_secs();
    let rotation = chassis_tf.compute_transform().rotation;

    // Convert from world velocity to chassis-local velocity, then remap Bevy axes
    // (right, up, back) to body axes (forward, left, up).
    // 中译：先把世界系速度旋到车身本地系（`rotation.inverse() * v`），再把 Bevy 轴
    //（右, 上, 后）× 重映射为机体轴（前, 左, 上）。
    let linear_local_bevy = rotation.inverse() * linear_velocity.0;
    let linear_body = bevy_local_to_body(linear_local_bevy);
    let v_body = Vec2::new(linear_body.x, linear_body.y);

    let angular_local_bevy = rotation.inverse() * angular_velocity.0;
    let gyro_body = bevy_local_to_body(angular_local_bevy);
    let wz_radps = gyro_body.z;

    // 用前后帧差分求加速度（首帧或 dt 异常时返回零）。
    let (a_body, alpha_z_radps2) = compute_body_acceleration(&previous, v_body, wz_radps, dt_s);

    // 机体系姿态角：先把旋转重映射到机体轴系，再解成 RPY（XYZ 顺序）。
    let body_rotation = bevy_to_body_quat(rotation);
    let (roll, pitch, yaw) = body_rotation.to_euler(EulerRot::XYZ);

    // 由机身线速度/偏航率逆推四轮速度（麦轮运动学）。
    let wheel_linear_mps = mecanum_wheel_linear(v_body.x, v_body.y, wz_radps, &config.mecanum);
    // 线速度 → 角速度：ω = v / r。
    let wheel_angular_radps =
        wheel_linear_to_angular(wheel_linear_mps, config.mecanum.wheel_radius_m);

    *frame = ChassisObservationFrame {
        stamp_s,
        dt_s,
        v_body,
        wz_radps,
        wheel_linear_mps,
        wheel_angular_radps,
        a_body,
        alpha_z_radps2,
        rpy_rad: Vec3::new(roll, pitch, yaw),
        gyro_xyz_radps: gyro_body,
        accel_xyz_mps2: Vec3::new(a_body.x, a_body.y, 0.0), // 平面运动，Z 向加速度记 0
    };

    // 存下本帧状态供下一帧差分。
    previous.initialized = true;
    previous.v_body = v_body;
    previous.wz_radps = wz_radps;
}

/// 用前后帧差分求机身系线加速度与偏航角加速度（单位 m/s²、rad/s²）。
fn compute_body_acceleration(
    previous: &PreviousKinematicState,
    v_body: Vec2,
    wz_radps: f32,
    dt_s: f32,
) -> (Vec2, f32) {
    // 无历史或 dt 退化时不给出加速度。
    if !previous.initialized || dt_s <= f32::EPSILON {
        return (Vec2::ZERO, 0.0);
    }

    let inv_dt = 1.0 / dt_s;
    (
        (v_body - previous.v_body) * inv_dt,
        (wz_radps - previous.wz_radps) * inv_dt,
    )
}

/// 麦轮正运动学逆解：由机身 (vx 前, vy 左, wz 偏航率) 求四轮线速度。
/// `k` 是轮距/轴距半和（几何尺寸，米）。返回顺序 [FL, FR, RL, RR]。
fn mecanum_wheel_linear(vx: f32, vy: f32, wz: f32, config: &MecanumConfig) -> [f32; NUM_WHEELS] {
    let k = config.half_wheelbase_m + config.half_trackwidth_m;
    [
        vx - vy - k * wz,
        vx + vy + k * wz,
        vx + vy - k * wz,
        vx - vy + k * wz,
    ]
}

/// 轮线速度 → 轮角速度：ω = v / r（半径取下限防除零）。
fn wheel_linear_to_angular(
    wheel_linear_mps: [f32; NUM_WHEELS],
    wheel_radius_m: f32,
) -> [f32; NUM_WHEELS] {
    let radius = wheel_radius_m.max(MIN_RADIUS_M);
    wheel_linear_mps.map(|wheel_linear| wheel_linear / radius)
}

/// Bevy 本地轴（右, 上, 后）→ 机体轴（前, 左, 上）的向量重映射。
fn bevy_local_to_body(vector: Vec3) -> Vec3 {
    Vec3::new(-vector.z, -vector.x, vector.y)
}

/// Bevy 本地轴系 → 机体轴系的旋转共轭变换：`align · rotation · align⁻¹`。
fn bevy_to_body_quat(rotation: Quat) -> Quat {
    // `from_cols` 按列给出"对齐基"：机体前/左/上在 Bevy 系中的方向。
    let align = Quat::from_mat3(&Mat3::from_cols(
        Vec3::new(0.0, -1.0, 0.0),
        Vec3::new(0.0, 0.0, 1.0),
        Vec3::new(-1.0, 0.0, 0.0),
    ));
    align * rotation * align.inverse()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 浮点近似断言（容差 1e-5）。
    fn approx_eq(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-5, "lhs={a}, rhs={b}");
    }

    /// 测试用麦轮几何参数（与 config.toml 默认值一致）。
    fn test_cfg() -> MecanumConfig {
        MecanumConfig {
            wheel_radius_m: 0.076,
            half_wheelbase_m: 0.18,
            half_trackwidth_m: 0.15,
        }
    }

    /// 正运动学正解：由四轮角速度反求机身 (vx, vy, wz)，用于往返一致性校验。
    fn mecanum_forward_from_angular(
        wheel_angular_radps: [f32; NUM_WHEELS],
        config: &MecanumConfig,
    ) -> (f32, f32, f32) {
        let r = config.wheel_radius_m;
        let k = config.half_wheelbase_m + config.half_trackwidth_m;
        let [fl, fr, rl, rr] = wheel_angular_radps;

        let vx = r * (fl + fr + rl + rr) * 0.25;
        let vy = r * (-fl + fr + rl - rr) * 0.25;
        let wz = r * (-fl + fr - rl + rr) / (4.0 * k);
        (vx, vy, wz)
    }

    /// 纯前进时四轮转速应完全相同。
    #[test]
    fn inverse_forward_motion_has_same_sign_and_magnitude() {
        let cfg = test_cfg();
        let linear = mecanum_wheel_linear(1.2, 0.0, 0.0, &cfg);
        approx_eq(linear[0], linear[1]);
        approx_eq(linear[1], linear[2]);
        approx_eq(linear[2], linear[3]);
    }

    /// 纯横移时左右侧转速应对称（符号相反）。
    #[test]
    fn inverse_lateral_motion_is_symmetric() {
        let cfg = test_cfg();
        let linear = mecanum_wheel_linear(0.0, 0.8, 0.0, &cfg);
        approx_eq(linear[0], -linear[1]);
        approx_eq(linear[2], -linear[3]);
        approx_eq(linear[0], linear[3]);
    }

    /// 纯自转时四轮转速应呈对角同号的麦轮特征。
    #[test]
    fn inverse_spin_motion_has_expected_pattern() {
        let cfg = test_cfg();
        let linear = mecanum_wheel_linear(0.0, 0.0, 2.0, &cfg);
        approx_eq(linear[0], -linear[1]);
        approx_eq(linear[0], linear[2]);
        approx_eq(linear[1], linear[3]);
    }

    /// 逆解→正解往返应能复原原始机身速度。
    #[test]
    fn inverse_then_forward_roundtrip_is_consistent() {
        let cfg = test_cfg();
        let samples = [(0.5, 0.3, 1.2), (1.1, -0.4, -0.7), (-0.6, 0.2, 0.9)];

        for (vx, vy, wz) in samples {
            let linear = mecanum_wheel_linear(vx, vy, wz, &cfg);
            let angular = wheel_linear_to_angular(linear, cfg.wheel_radius_m);
            let (vx_back, vy_back, wz_back) = mecanum_forward_from_angular(angular, &cfg);
            approx_eq(vx_back, vx);
            approx_eq(vy_back, vy);
            approx_eq(wz_back, wz);
        }
    }

    /// 首帧无历史时加速度必须为 0（不能凭空给出一个假值）。
    #[test]
    fn acceleration_is_zero_without_history() {
        let previous = PreviousKinematicState::default();
        let (accel, alpha) = compute_body_acceleration(&previous, Vec2::new(1.0, 1.0), 0.5, 0.01);
        approx_eq(accel.x, 0.0);
        approx_eq(accel.y, 0.0);
        approx_eq(alpha, 0.0);
    }
}
