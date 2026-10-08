/// 前哨站自转角速度：0.8π 弧度/秒 ≈ 144°/s（约 2.5 秒转一整圈）。
/// `std::f32::consts::PI` 是编译期常量 π；`pub(super)` 仅对父模块（outpost/）可见。
pub(super) const ROTATION_SPEED: f32 = 0.8 * std::f32::consts::PI;
