//! 机关旋转运动学：决定能量机关"转多快、往哪转"。
//!
//! 两种转速模型：
//! - 小机关：恒定角速度 `ROTATION_BASELINE_SMALL`（π/3 rad/s ≈ 60°/s）；
//! - 大机关：**只有在激活流程中**才启用 `VariableRotation` 的正弦变速
//!   （速度以 `omega` 为角频率随时间起伏），非激活时回落到基准速度。
//! 旋转方向由 `clockwise`（是否顺时针）决定，最终角速度再乘上 ±1。
//!
//! `PowerRuneRotation` 是挂在"面"实体上的组件；rune.rs 每帧调用它的 `rotate` 让机关转动，
//! construct.rs 在装配时按队伍随机决定顺/逆时针。

use crate::robomaster::power_rune::common::RuneMode;
use crate::robomaster::power_rune::consts::ROTATION_BASELINE_SMALL;
use bevy::math::Dir3;
use bevy::prelude::{Component, Transform};
use rand::{Rng, RngExt};

/// 大机关的"变速"旋转模型：角速度是时间的正弦函数。
/// 物理量：`a` 是正弦振幅（rad/s），`omega` 是正弦角频率（rad/s），`t` 是累计时间（秒）。
/// 三个量都是随机取值，使每次激活的运动轨迹略有不同。
struct VariableRotation {
    a: f32,     // 正弦振幅（rad/s）
    omega: f32, // 正弦角频率（rad/s）
    t: f32,     // 已累计时间（秒）
}

impl VariableRotation {
    /// 随机生成一组参数（由外部传入的随机数源决定）。
    /// `a ∈ [0.780, 1.045]`，`omega ∈ [1.884, 2.0]`（单位见字段注释）。
    pub fn random(rng: &mut impl Rng) -> Self {
        let a = rng.random_range(0.780..=1.045);
        let omega = rng.random_range(1.884..=2.0);
        Self { a, omega, t: 0.0 }
    }

    /// 时间推进：`dt` 是这一帧的时长（秒）。
    pub fn advance(&mut self, dt: f32) {
        self.t += dt;
    }

    /// 当前角速度（rad/s）= `a·sin(omega·t) + (2.090 − a)`。
    /// 常数 2.090 是偏移量：让 t=0 时的初速度为 `2.090 − a`（与 a 同量级），
    /// 与正弦项配合使速度恒为非负、且随时间缓慢起伏（a ≤ 1.045 保证不会变负）。
    pub fn speed(&self) -> f32 {
        let b = 2.090 - self.a;
        self.a * (self.omega * self.t).sin() + b
    }
}

/// 一台机关的旋转控制器（纯逻辑，不含 Transform）。
/// 内部持有：基准角速度、旋转轴方向、可选的变速模型、顺时针标志。
pub struct RotationController {
    baseline: f32,                      // 基准角速度（rad/s）
    direction: Dir3,                    // 旋转轴（单位方向向量）
    variable: Option<VariableRotation>, // 大机关激活时的变速模型；None 表示匀速
    clockwise: bool,                    // 是否顺时针
}

impl RotationController {
    /// 新建：基准速度用 `ROTATION_BASELINE_SMALL`，旋转轴取归一化的 (-1, 0, -1)。
    /// `Dir3::from_xyz(...).unwrap()`：Dir3 要求向量非零，这里必然非零所以 unwrap 安全。
    pub fn new(clockwise: bool) -> Self {
        Self {
            baseline: ROTATION_BASELINE_SMALL,
            direction: Dir3::from_xyz(-1.0, 0.0, -1.0).unwrap(),
            variable: None,
            clockwise,
        }
    }

    /// 读取当前变速参数 `(a, omega, t)`；仅大机关激活时返回 Some。供测试检查。
    pub fn variable_params(&self) -> Option<(f32, f32, f32)> {
        self.variable.as_ref().map(|v| (v.a, v.omega, v.t))
    }

    /// 是否顺时针旋转。
    pub fn is_clockwise(&self) -> bool {
        self.clockwise
    }

    /// 绕自身局部轴旋转 `angle` 弧度。`angle` 单位是弧度（rad）。
    pub fn rotate(&self, transform: &mut Transform, angle: f32) {
        transform.rotate_local_axis(self.direction, angle);
    }

    /// 启用变速：随机生成一组正弦参数。
    /// `impl Rng` 是"接受任何实现了 Rng trait 的随机数生成器"的写法（静态分发）。
    pub fn set_variable(&mut self, rng: &mut impl Rng) {
        self.variable = Some(VariableRotation::random(rng));
    }

    /// 关闭变速，恢复匀速。
    pub fn clear_variable(&mut self) {
        self.variable = None;
    }

    /// 开始激活：小机关保持匀速；大机关启用变速。先清空再按模式决定。
    pub fn begin_activation(&mut self, mode: RuneMode, rng: &mut impl Rng) {
        self.clear_variable();
        if mode == RuneMode::Large {
            self.set_variable(rng);
        }
    }

    /// 结束激活：清掉变速模型，回到匀速。
    pub fn end_activation(&mut self) {
        self.clear_variable();
    }

    /// 按当前"模式 + 是否正在激活"同步变速状态。元组 match 穷尽三种关键情形：
    /// - 大机关 + 正在激活 + 还没变速 → 生成新的变速参数；
    /// - 大机关 + 正在激活 + 已有变速 → 保持不动（不重置速度与相位，避免抖动）；
    /// - 其它任何情况（`_`）→ 清空变速。
    pub fn sync_activation(&mut self, mode: RuneMode, activating: bool, rng: &mut impl Rng) {
        match (mode, activating, self.variable.is_some()) {
            (RuneMode::Large, true, false) => self.set_variable(rng),
            (RuneMode::Large, true, true) => {}
            _ => self.clear_variable(),
        }
    }

    /// 计算当前角速度（rad/s），并按 `dt` 推进变速模型的内部时间。
    /// 因为要推进内部状态，所以接 `&mut self`（不是只读）。
    pub fn current_speed(&mut self, mode: RuneMode, dt: f32) -> f32 {
        // 方向符号：顺时针取 +1、逆时针取 −1，最终角速度乘上它。
        let sgn = if self.clockwise { 1.0 } else { -1.0 };
        if mode == RuneMode::Small {
            return sgn * self.baseline;
        }
        // 大机关只有在激活状态下使用变量旋转；否则回落到基准速度（下 if-let 命中即激活态）。
        if let Some(variable) = &mut self.variable {
            let speed = variable.speed();
            variable.advance(dt);
            return sgn * speed;
        }
        sgn * self.baseline
    }
}

/// 挂在"面"实体上的旋转组件——把 `RotationController` 包成 Bevy 组件（`#[derive(Component)]`）。
#[derive(Component)]
pub struct PowerRuneRotation {
    controller: RotationController,
}

impl PowerRuneRotation {
    /// 新建，传入是否顺时针。构造时控制器默认匀速。
    pub fn new(clockwise: bool) -> Self {
        Self {
            controller: RotationController::new(clockwise),
        }
    }

    /// 只读访问内部控制器。
    pub fn controller(&self) -> &RotationController {
        &self.controller
    }

    /// 开始激活（转发给控制器）。
    pub fn begin_activation(&mut self, mode: RuneMode, rng: &mut impl Rng) {
        self.controller.begin_activation(mode, rng);
    }

    /// 结束激活（转发给控制器）。
    pub fn end_activation(&mut self) {
        self.controller.end_activation();
    }

    /// 同步变速状态（转发给控制器）。
    pub fn sync_activation(&mut self, mode: RuneMode, activating: bool, rng: &mut impl Rng) {
        self.controller.sync_activation(mode, activating, rng);
    }

    /// 按当前模式算出角速度，并让 `transform` 实际转动。
    /// 本帧转过的角度 = 角速度 · dt，单位弧度（rad）。
    pub fn rotate(&mut self, mode: RuneMode, transform: &mut Transform, dt: f32) {
        let speed = self.controller.current_speed(mode, dt);
        self.controller.rotate(transform, speed * dt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 小机关：任意 dt 下角速度恒为基准值，且不使用变速。
    #[test]
    fn small_rune_rotation_is_baseline_speed() {
        let mut controller = RotationController::new(true);

        assert_eq!(
            controller.current_speed(RuneMode::Small, 0.25),
            ROTATION_BASELINE_SMALL
        );
        assert!(controller.variable_params().is_none());
    }

    /// 大机关：begin_activation 生成一组全新正弦参数；初速度应为 2.090−a，
    /// 并随 dt 逐步推进内部时间 t。
    #[test]
    fn large_rune_activation_uses_fresh_sine_params() {
        let mut rng = rand::rng();
        let mut controller = RotationController::new(true);

        controller.begin_activation(RuneMode::Large, &mut rng);
        let (a, omega, t) = controller.variable_params().unwrap();
        assert!((0.780..=1.045).contains(&a));
        assert!((1.884..=2.0).contains(&omega));
        assert_eq!(t, 0.0);

        let expected_initial_speed = 2.090 - a;
        assert_eq!(
            controller.current_speed(RuneMode::Large, 0.5),
            expected_initial_speed
        );
        assert_eq!(controller.variable_params().unwrap().2, 0.5);

        controller.current_speed(RuneMode::Large, 0.5);
        assert_eq!(controller.variable_params().unwrap().2, 1.0);

        controller.end_activation();
        assert!(controller.variable_params().is_none());
    }

    /// 逆时针：角速度应取负。
    #[test]
    fn counter_clockwise_rotation_negates_speed() {
        let mut controller = RotationController::new(false);

        assert_eq!(
            controller.current_speed(RuneMode::Small, 0.25),
            -ROTATION_BASELINE_SMALL
        );
    }

    /// sync_activation：持续处于"大机关 + 激活"时重复调用不会重置参数（速度/相位连续）；
    /// 一旦退出激活态，变速被清空。
    #[test]
    fn large_rune_sync_preserves_active_variable_rotation() {
        let mut rng = rand::rng();
        let mut controller = RotationController::new(true);

        controller.sync_activation(RuneMode::Large, true, &mut rng);
        let first_params = controller.variable_params().unwrap();

        controller.sync_activation(RuneMode::Large, true, &mut rng);
        assert_eq!(controller.variable_params().unwrap(), first_params);

        controller.sync_activation(RuneMode::Large, false, &mut rng);
        assert!(controller.variable_params().is_none());
    }
}
