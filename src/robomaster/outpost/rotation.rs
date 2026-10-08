//! 前哨站旋转的纯逻辑（不碰 ECS）：方向、模式与"旋转控制器"。
//!
//! 游戏语言：前哨站可以顺时针或逆时针转；调试时还能切"正转 / 停 / 反转"三种模式观察。
//!
//! 协作者：update.rs 每帧从 `Time` 取 dt，调用 `RotationController::step` 转动 Transform；
//! construct.rs 依据阵营决定初始旋转方向。
//!
//! 新手阅读顺序：`RotationDirection`（方向与符号）→ `RotationMode`（三档模式）
//! → `RotationController`（把方向/模式/速度合成一次旋转）。

use crate::robomaster::outpost::consts::ROTATION_SPEED;
// 只依赖 Transform，不引入任何系统/查询——所以本文件能脱离 ECS 单独测试。
use bevy::prelude::Transform;

/// 旋转方向：顺时针 / 逆时针。
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum RotationDirection {
    Clockwise,
    CounterClockwise,
}

impl RotationDirection {
    /// 把方向转成角速度的正负号：顺时针 +1.0、逆时针 -1.0（单位：无量纲乘数）。
    /// `const fn`：编译期可求值；`self` 按值接收（枚举是 Copy）。
    pub const fn sign(self) -> f32 {
        match self {
            Self::Clockwise => 1.0,
            Self::CounterClockwise => -1.0,
        }
    }
}

/// 旋转模式（调试用）：正转 / 停 / 反转。作为每帧转角的一个乘数。
/// `#[default]` 指定枚举默认变体——配合派生 `Default` 时，`default()` 会给出 `Forward`。
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub enum RotationMode {
    #[default]
    Forward,
    Stopped,
    Reverse,
}

impl RotationMode {
    /// 模式对应的转角缩放系数：正转 ×1.0、停 ×0.0、反转 ×-1.0。
    pub const fn scale(self) -> f32 {
        match self {
            Self::Forward => 1.0,
            Self::Stopped => 0.0,
            Self::Reverse => -1.0,
        }
    }

    /// 循环切到下一个模式：Forward→Stopped→Reverse→Forward（调试按 C 键触发）。
    pub const fn next(self) -> Self {
        match self {
            Self::Forward => Self::Stopped,
            Self::Stopped => Self::Reverse,
            Self::Reverse => Self::Forward,
        }
    }
}

/// 旋转控制器：保存角速度与方向，对外暴露"走一步"的逻辑。
/// 无任何 ECS 依赖——纯数据 + 方法，便于单独测试（见文件末尾）。
pub struct RotationController {
    speed: f32,                   // 角速度，单位：弧度/秒（rad/s）
    direction: RotationDirection, // 旋转方向（决定符号）
}

impl RotationController {
    /// 用默认角速度（`ROTATION_SPEED`）和给定方向新建。
    pub fn new(direction: RotationDirection) -> Self {
        Self {
            speed: ROTATION_SPEED,
            direction,
        }
    }

    /// 绕本地 Y 轴转 `angle` 弧度（Y 轴朝上 → 在水平面内自转）。
    /// `transform.rotate_y` 是"增量旋转"：会把它累加到当前的旋转上。
    fn rotate(&self, transform: &mut Transform, angle: f32) {
        transform.rotate_y(angle);
    }

    /// 推进一步旋转：`dt` 是上一帧到本帧的时长（秒）。
    /// 本步转角 = 方向(±1) × 模式(1/0/-1) × 角速度(rad/s) × dt(s)，单位：弧度（rad）。
    pub fn step(&self, transform: &mut Transform, dt: f32, mode: RotationMode) {
        self.rotate(
            transform,
            self.direction.sign() * mode.scale() * self.speed * dt,
        );
    }
}

// 单元测试：`#[cfg(test)]` 只在 `cargo test` 时编入。用纯逻辑验证方向符号与模式循环顺序。
#[cfg(test)]
mod tests {
    // `use super::*;` 把父模块全部条目导入测试作用域。
    use super::*;

    #[test]
    fn rotation_direction_sign_matches_legacy_bool() {
        // 锁死方向→符号映射：顺时针为正、逆时针为负。
        assert_eq!(RotationDirection::Clockwise.sign(), 1.0);
        assert_eq!(RotationDirection::CounterClockwise.sign(), -1.0);
    }

    #[test]
    fn rotation_mode_cycles_in_debug_order() {
        // 锁死模式的环形顺序，防止调试循环顺序被改坏。
        assert_eq!(RotationMode::Forward.next(), RotationMode::Stopped);
        assert_eq!(RotationMode::Stopped.next(), RotationMode::Reverse);
        assert_eq!(RotationMode::Reverse.next(), RotationMode::Forward);
    }
}
