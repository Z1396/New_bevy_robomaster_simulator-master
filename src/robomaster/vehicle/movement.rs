//! 战车运动模型：把"摇杆输入"转成"施加在刚体上的力/冲量"，实现带最大速度的非线性加速。
//!
//! 游戏语言：玩家推摇杆 → 战车沿该方向加速；速度越接近上限，加速度越小，于是速度会
//! 平滑地趋近 vmax 而不会无限增大（像真实车辆的动力衰减）。
//!
//! 协作者：本组件挂在战车刚体上，由外部系统按固定物理步调用 `linear()`；
//! avian3d 提供 `ForcesItem`（对该刚体施加力/冲量的接口）。
//!
//! 新手阅读顺序：VehicleDynamic 字段（含单位）→ new / default → linear_accelerate（核心公式）
//! → linear（把加速度变成冲量施加）。

// `ForcesItem`：avian3d 的"力施加器"，代表对某个刚体施加力/冲量的句柄。
use avian3d::prelude::forces::ForcesItem;
use avian3d::prelude::*;
use bevy::prelude::*;

/// 战车运动参数组件：最高速、线性加速度，以及控制加速曲线的指数 `n`。
#[derive(Component, Clone, Debug)]
pub struct VehicleDynamic {
    pub max_speed: f32,           // m/s ：最高速度（米/秒）
    pub linear_acceleration: f32, // m/s^2 ：基础线性加速度（米/秒²）

    // 加速度曲线的陡峭程度（无量纲）：n 越大，速度接近上限时加速度衰减越"突然"（越像硬限速）。
    n: f32,
}

impl VehicleDynamic {
    /// 新建：`max_speed` 单位 m/s，`linear_acceleration` 单位 m/s²，
    /// `acceleration_exponent` 是无量纲的曲线指数 n。
    pub fn new(max_speed: f32, linear_acceleration: f32, acceleration_exponent: f32) -> Self {
        Self {
            max_speed,
            linear_acceleration,
            n: acceleration_exponent,
        }
    }
}

/// 默认参数：最高 4 m/s、基础加速度 8 m/s²、指数 n=10（接近上限时加速很快收敛）。
impl Default for VehicleDynamic {
    fn default() -> Self {
        Self {
            max_speed: 4.0,
            linear_acceleration: 8.0,
            n: 10.0,
        }
    }
}

impl VehicleDynamic {
    /// 一步运动更新：算出本步加速度，转成冲量交给物理引擎。
    ///
    /// 参数：
    /// - `forces`：目标刚体的力施加器；
    /// - `mass`：刚体质量（kg）；
    /// - `movement_frame`：决定"前/右"朝向的参考坐标系（通常是底盘或相机）；
    /// - `input`：x=左右、y=前后 的摇杆输入（无量纲，通常在 [-1,1]）；
    /// - `dt`：本步时长（秒）；
    /// - `boost`：加速倍率（`1.0` 为正常，>1 同时提升最高速与加速度）。
    pub fn linear(
        &mut self,
        forces: &mut ForcesItem,
        mass: f32,
        movement_frame: &GlobalTransform,
        input: Vec2,
        dt: f32,
        boost: f32,
    ) {
        let lin_vel = forces.linear_velocity(); // 当前线速度（m/s）
        let acceleration = self.linear_accelerate(input, movement_frame, lin_vel, boost);
        // 冲量 = 加速度 × 质量 × 时间，量纲 m/s²·kg·s = kg·m/s（即动量变化，单位 N·s）。
        // `apply_linear_impulse`：施加线性冲量，avian3d 会在物理步内把它积分进速度。
        forces.apply_linear_impulse(acceleration * mass * dt);
    }

    /// 核心：计算本步应施加的加速度矢量（m/s²）。
    fn linear_accelerate(
        &mut self,
        input: Vec2,
        movement_frame: &GlobalTransform,
        current_velocity: Vec3,
        boost: f32,
    ) -> Vec3 {
        // 没有输入就不加速（比较长度平方而非长度，省一次开方；等价于"是否为零向量"）。
        if input.length_squared() == 0.0 {
            return Vec3::ZERO;
        }
        // 取参考系的"前 / 右"方向，并压平到水平面（丢弃 y 分量，保证贴地运动）。
        let forward = movement_frame.forward().with_y(0.0);
        let right = movement_frame.right().with_y(0.0);
        // `normalize_or_zero`：归一化成单位向量；零向量时返回零（避免 0/0 得到 NaN）。
        let forward_xz = forward.with_y(0.0).normalize_or_zero();
        let right_xz = right.with_y(0.0).normalize_or_zero();
        // 合成期望运动方向：前后 ∝ input.y，左右 ∝ input.x，再做一次归一化得到单位方向。
        let dirc = (forward_xz * input.y + right_xz * input.x).normalize_or_zero();
        // boost 同时放大速度上限与加速度（两者同倍率，实际最高速 = base × boost）。
        let max_speed = self.max_speed * boost;
        let accel = self.linear_acceleration * boost;
        // 非线性加速公式：a = dirc · accel · (1 - (v / vmax)^n)
        //   - v 是当前速率(m/s)，vmax 是本步有效上限(m/s)；
        //   - 当 v→vmax 时 (v/vmax)^n→1，括号→0，加速度平滑归零（速度自然收敛到上限）；
        //   - n 越大收敛越"硬"；速度很小（v≪vmax）时括号≈1，近似满加速度。
        dirc * accel * (1.0 - (current_velocity.length() / max_speed).powf(self.n))
    }
}
