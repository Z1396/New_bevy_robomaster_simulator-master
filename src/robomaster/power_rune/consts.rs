//! 能量机关的时间与旋转常量。除特别说明外，所有时间单位都是**秒（s）**，
//! 角速度单位是**弧度/秒（rad/s）**。这些数字来自竞赛规则与手感调参，
//! 修改它们会直接改变机关的难度与节奏。各常量在 state.rs / rotation.rs 中被引用。

/// 主靶超时（秒）：某轮要点亮的目标出现后，玩家必须在这段时间内打中它，
/// 否则本轮失败（进入 Failed）。小机关与大机关的主靶阶段都用它。
pub(super) const ACTIVATION_PRIMARY_TIMEOUT: f32 = 2.5;
/// 大机关副靶超时（秒）：主靶打中后出现的"副靶窗口"的时长。
/// 窗口内打中副靶则正常推进；超时未打中也不判失败，而是直接进入下一轮主靶。
pub(super) const LARGE_SECONDARY_TIMEOUT: f32 = 1.0;
/// 未激活等待（秒）：机关处于"未激活"时被打中，会先等待这段时间再真正开始激活流程
///（给玩家一点预告/反应时间）。
pub(super) const INACTIVE_WAIT: f32 = 1.0;
/// 失败恢复（秒）：一次激活失败后，机关在 Failed 状态停留这段时间，然后回到未激活，
/// 使玩家可以重新开始。
pub(super) const FAILURE_RECOVER: f32 = 1.5;
/// 激活保持（秒）：成功激活后，机关维持"已激活"显示的时长；结束后回到未激活。
pub(super) const ACTIVATED_HOLD: f32 = 6.0;
/// 全局激活超时（秒）：整个激活流程（含所有轮次与副靶窗）的总时长上限，
/// 无论当前进行到哪一步，累计耗时超过它都无条件回到未激活。
pub(super) const ACTIVATION_GLOBAL_TIMEOUT: f32 = 20.0; // 20秒全局激活超时；此常量即该默认值（秒）
/// 小机关的基准角速度（rad/s）：π/3 rad/s ≈ 1.047 rad/s ≈ 60°/s。
/// 小机关始终以此恒定角速度旋转；大机关在非激活时也回落到这个值。
pub(super) const ROTATION_BASELINE_SMALL: f32 = std::f32::consts::PI / 3.0; // 小机关固定角速度；单位 rad/s（π/3 ≈ 1.047，即 60°/s）
/// "打错目标是否判失败"的开关（工程上叫 feature flag）。
/// 为 true 时：打错目标只返回 WrongTarget、不打断当前激活流程（"funny"= 容忍式玩法）；
/// 为 false 时：一打错就立刻判 Failed。具体分支见 state.rs 的 `MechanismState::hit`。
pub(super) const FUNNY_IGNORE_WRONG_TARGET_FAILURE: bool = true;
