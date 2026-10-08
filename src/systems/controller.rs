//! 输入采样与控制器状态：把键盘/手柄的原始按键，翻译成本项目统一的 `ControllerInput`
//!（归一化到 -1..1 的轴向量 + 各类"刚按下"布尔），存进全局资源 `ControllerState`。
//!
//! 分工：
//! - `clear_controller_input` 每帧先跑，清空上一帧输入（避免按键状态残留）；
//! - `sample_*` 采样键盘/手柄，写进 `ControllerState`；
//! - 其余系统（input.rs / debug.rs）只读 `ControllerState` 进行实际操控。
//!
//! 两套输入并存：`controlled`（本机）与 `remote`（遥控被选中的 AI 车）。

use bevy::input::gamepad::{GamepadRumbleIntensity, GamepadRumbleRequest};
use bevy::prelude::*;
use core::time::Duration;
use std::sync::atomic::Ordering;

use crate::components::SubscribeAutoAim;

/// 摇杆死区：位移小于此值视为 0（消除摇杆回中抖动）。
const GAMEPAD_STICK_DEADZONE: f32 = 0.12;
/// 扳机阈值：模拟扳机超过此值才算"按下"。
const GAMEPAD_TRIGGER_THRESHOLD: f32 = 0.35;
/// 精瞄倍率：按住精瞄键时云台转动速度乘以此系数（更慢更准）。
const PRECISE_GIMBAL_SCALE: f32 = 0.35;

/// 帮助文本的数据源：按输入设备（键盘/xbox）提供两套按键说明。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControllerHelp {
    source: &'static str,   // 设备名（HUD 显示用）
    manual: &'static str,   // 手动模式的按键说明
    auto_aim: &'static str, // 自瞄模式的按键说明
}

impl ControllerHelp {
    /// 键盘方案。`const fn`：可在编译期构造。
    const fn keyboard() -> Self {
        Self {
            source: "keyboard",
            manual: "F3 Camera | WASD Move | Arrows Aim | Space Shoot | G Dart | Q Gyro | U Remote Gyro | F5 AutoAim | Tab Slapper",
            auto_aim: "F5 AutoAim Off | WASD Move | Q Gyro | U Remote Gyro | external fire_advice shoots | Tab Slapper",
        }
    }

    /// Xbox 手柄方案。
    const fn xbox() -> Self {
        Self {
            source: "xbox",
            manual: "View Camera | LS Move | L3 Boost | DPad Slapper Move | RS Aim | R3+RS Slapper Roll/Pitch | LB Gyro | Y Slapper Gyro | RB Shoot | X Dart | hold RT AutoAim",
            auto_aim: "release RT AutoAim Off | LS Move | L3 Boost | DPad Slapper Move | R3+RS Slapper Roll/Pitch | LB Gyro | Y Slapper Gyro | external fire_advice shoots",
        }
    }
}

impl Default for ControllerHelp {
    fn default() -> Self {
        Self::keyboard() // 默认键盘方案
    }
}

/// 底盘"小陀螺"开关状态（两档）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ChassisSpinMode {
    #[default]
    Off,
    On,
}

impl ChassisSpinMode {
    /// 在两档之间翻转。
    fn toggle(&mut self) {
        // `*self = match self {...}`：把 match 结果写回自身（self 是 &mut，需解引用赋值）。
        *self = match self {
            Self::Off => Self::On,
            Self::On => Self::Off,
        };
    }

    /// 开时给出持续偏航输入 1.0（让底盘不停自转），关时为 0。
    fn yaw_input(self) -> f32 {
        match self {
            Self::Off => 0.0,
            Self::On => 1.0,
        }
    }

    fn is_on(self) -> bool {
        self == Self::On
    }
}

/// 单台设备的"本帧输入"快照。轴向量均已归一化到 -1..1。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ControllerInput {
    pub movement: Vec2,     // 底盘平移 (x=右/左, y=前/后)
    pub gimbal: Vec2,       // 云台转动 (x=yaw, y=pitch)
    pub chassis_yaw: f32,   // 底盘自转输入（小陀螺/方向键）
    pub chassis_roll: f32,  // 底盘横滚输入
    pub chassis_pitch: f32, // 底盘俯仰输入
    pub boost: bool,        // 加速（按住）
    pub precise_gimbal: bool, // 精瞄（降低云台转速）
    pub shoot: bool,        // 开火（按住）
    pub dart_just_pressed: bool, // 飞镖（本帧刚按下）
    pub switch_slapper_just_pressed: bool, // 切换操控车（本帧刚按下）
    pub switch_camera_just_pressed: bool,  // 切换相机（本帧刚按下）
    pub auto_aim: bool,     // 自瞄订阅（本设备请求开启）
}

impl Default for ControllerInput {
    fn default() -> Self {
        // 全零/全 false 的"无输入"状态。
        Self {
            movement: Vec2::ZERO,
            gimbal: Vec2::ZERO,
            chassis_yaw: 0.0,
            chassis_roll: 0.0,
            chassis_pitch: 0.0,
            boost: false,
            precise_gimbal: false,
            shoot: false,
            dart_just_pressed: false,
            switch_slapper_just_pressed: false,
            switch_camera_just_pressed: false,
            auto_aim: false,
        }
    }
}

impl ControllerInput {
    /// 加速倍率：按住加速键为 2.0，否则 1.0。
    pub fn boost_multiplier(self) -> f32 {
        if self.boost { 2.0 } else { 1.0 }
    }

    /// 云台转速缩放：精瞄时变慢。
    pub fn gimbal_scale(self) -> f32 {
        if self.precise_gimbal {
            PRECISE_GIMBAL_SCALE
        } else {
            1.0
        }
    }

    /// 累加平移输入（键盘与手柄可能同帧都产生输入，故用加法合并）。
    fn add_movement(&mut self, movement: Vec2) {
        self.movement = clamp_axes_vec2(self.movement + movement);
    }

    /// 累加云台输入。
    fn add_gimbal(&mut self, gimbal: Vec2) {
        self.gimbal = clamp_axes_vec2(self.gimbal + gimbal);
    }

    /// 累加底盘姿态输入，各轴夹在 -1..1。
    fn add_chassis(&mut self, yaw: f32, roll: f32, pitch: f32) {
        self.chassis_yaw = (self.chassis_yaw + yaw).clamp(-1.0, 1.0);
        self.chassis_roll = (self.chassis_roll + roll).clamp(-1.0, 1.0);
        self.chassis_pitch = (self.chassis_pitch + pitch).clamp(-1.0, 1.0);
    }
}

/// 全局控制器状态（资源）。`controlled`/`remote` 是每帧重建的输入快照；
/// 其余字段是**跨帧保持**的开关/状态（如小陀螺开关、帮助文本、当前手柄）。
#[derive(Resource, Debug, Default)]
pub struct ControllerState {
    pub controlled: ControllerInput,
    pub remote: ControllerInput,
    keyboard_auto_aim: bool,             // F5 切换的键盘自瞄开关
    controlled_chassis_spin: ChassisSpinMode,
    remote_chassis_spin: ChassisSpinMode,
    active_gamepad: Option<Entity>,      // 最近活跃的手柄实体（震动回馈用）
    help: ControllerHelp,                // 当前帮助文本方案
}

impl ControllerState {
    /// 帧首清空两个输入快照（跨帧状态字段保持不动）。
    pub fn reset_frame(&mut self) {
        self.controlled = ControllerInput::default();
        self.remote = ControllerInput::default();
    }

    /// 自瞄是否激活：键盘开关或手柄扳机任一为真。
    pub fn auto_aim_active(&self) -> bool {
        self.keyboard_auto_aim || self.controlled.auto_aim
    }

    pub fn help_source(&self) -> &'static str {
        self.help.source
    }

    /// HUD 显示的模式名。
    pub fn help_mode(&self) -> &'static str {
        if self.auto_aim_active() {
            "auto-aim"
        } else {
            "manual"
        }
    }

    /// 依据当前模式给出对应的按键说明。
    pub fn help_controls(&self) -> &'static str {
        if self.auto_aim_active() {
            self.help.auto_aim
        } else {
            self.help.manual
        }
    }

    pub fn controlled_chassis_spin(&self) -> bool {
        self.controlled_chassis_spin.is_on()
    }

    pub fn remote_chassis_spin(&self) -> bool {
        self.remote_chassis_spin.is_on()
    }

    pub fn active_gamepad(&self) -> Option<Entity> {
        self.active_gamepad
    }

    fn toggle_keyboard_auto_aim(&mut self) {
        self.keyboard_auto_aim = !self.keyboard_auto_aim;
    }

    fn toggle_controlled_chassis_spin(&mut self) {
        self.controlled_chassis_spin.toggle();
    }

    fn toggle_remote_chassis_spin(&mut self) {
        self.remote_chassis_spin.toggle();
    }

    fn use_help(&mut self, help: ControllerHelp) {
        self.help = help;
    }

    fn use_gamepad(&mut self, gamepad: Entity) {
        self.active_gamepad = Some(gamepad);
    }

    fn clear_gamepad(&mut self) {
        self.active_gamepad = None;
    }
}

/// 帧首清空输入（Input 阶段第一个系统，见 main.rs）。
pub fn clear_controller_input(mut controller: ResMut<ControllerState>) {
    controller.reset_frame();
}

/// 键盘采样：把按键映射成两个 `ControllerInput` 快照。
pub fn sample_keyboard_controller(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut controller: ResMut<ControllerState>,
) {
    // 有键盘动作就切到键盘帮助、清掉手柄焦点（表示当前用键盘操作）。
    let keyboard_used = keyboard_controller_active(&keyboard);
    if keyboard_used {
        controller.use_help(ControllerHelp::keyboard());
        controller.clear_gamepad();
    }
    // Q/U 切换两台车的小陀螺开关（`just_pressed` 防连触）。
    if keyboard.just_pressed(KeyCode::KeyQ) {
        controller.toggle_controlled_chassis_spin();
    }
    if keyboard.just_pressed(KeyCode::KeyU) {
        controller.toggle_remote_chassis_spin();
    }

    // ---- 本机（controlled）----
    let controlled_chassis_yaw = controller.controlled_chassis_spin.yaw_input();
    let controlled = &mut controller.controlled;
    controlled.add_movement(keyboard_vec2(
        &keyboard,
        KeyCode::KeyW,
        KeyCode::KeyA,
        KeyCode::KeyS,
        KeyCode::KeyD,
    ));
    controlled.add_chassis(controlled_chassis_yaw, 0.0, 0.0);
    controlled.add_gimbal(Vec2::new(
        keyboard_axis(&keyboard, KeyCode::ArrowLeft, KeyCode::ArrowRight),
        keyboard_axis(&keyboard, KeyCode::ArrowUp, KeyCode::ArrowDown),
    ));
    controlled.boost |= keyboard.pressed(KeyCode::ShiftLeft); // `|=`：键盘或手柄任一为真即为真
    controlled.shoot |= keyboard.pressed(KeyCode::Space);
    controlled.dart_just_pressed |= keyboard.just_pressed(KeyCode::KeyG);
    controlled.switch_slapper_just_pressed |= keyboard.just_pressed(KeyCode::Tab);
    controlled.switch_camera_just_pressed |= keyboard.just_pressed(KeyCode::F3);

    // ---- 遥控车（remote，IJKL 控制）----
    let remote_chassis_yaw = controller.remote_chassis_spin.yaw_input();
    let remote = &mut controller.remote;
    remote.add_movement(keyboard_vec2(
        &keyboard,
        KeyCode::KeyI,
        KeyCode::KeyJ,
        KeyCode::KeyK,
        KeyCode::KeyL,
    ));
    remote.add_chassis(
        remote_chassis_yaw,
        keyboard_axis(&keyboard, KeyCode::BracketLeft, KeyCode::BracketRight),
        keyboard_axis(&keyboard, KeyCode::Semicolon, KeyCode::Quote),
    );
    // Shift 被本机占用了（加速），故按住 Shift 时不响应遥控车的 C/B 云台偏航。
    if !keyboard.pressed(KeyCode::ShiftLeft) {
        remote.add_gimbal(Vec2::new(
            keyboard_axis(&keyboard, KeyCode::KeyC, KeyCode::KeyB),
            0.0,
        ));
    }
    remote.add_gimbal(Vec2::new(
        0.0,
        keyboard_axis(&keyboard, KeyCode::KeyF, KeyCode::KeyV),
    ));
    remote.boost |= keyboard.pressed(KeyCode::ShiftRight);

    // F5 切换键盘自瞄开关。
    if keyboard.just_pressed(KeyCode::F5) {
        controller.toggle_keyboard_auto_aim();
    }
}

/// 手柄采样：把 Xbox 手柄按键/摇杆映射进 `ControllerInput`。
pub fn sample_gamepad_controller(
    gamepads: Query<(Entity, &Gamepad)>,
    mut controller: ResMut<ControllerState>,
    mut rumble_requests: MessageWriter<GamepadRumbleRequest>,
) {
    // 只取第一个手柄（多手柄暂不支持）。
    let Some((gamepad_entity, gamepad)) = gamepads.iter().next() else {
        return;
    };

    // 摇杆先过死区再使用。
    let left_stick = apply_stick_deadzone(gamepad.left_stick());
    let right_stick = apply_stick_deadzone(gamepad.right_stick());
    let dpad = gamepad.dpad();
    if gamepad_controller_active(gamepad, left_stick, right_stick, dpad) {
        controller.use_help(ControllerHelp::xbox());
        controller.use_gamepad(gamepad_entity);
    }
    // LT 切本机小陀螺、North(Y) 切遥控车小陀螺，各带一次弱震动反馈。
    if gamepad.just_pressed(GamepadButton::LeftTrigger) {
        controller.toggle_controlled_chassis_spin();
        request_gamepad_rumble(
            gamepad_entity,
            &mut rumble_requests,
            GamepadRumbleIntensity::weak_motor(0.25),
            Duration::from_millis(70),
        );
    }
    if gamepad.just_pressed(GamepadButton::North) {
        controller.toggle_remote_chassis_spin();
        request_gamepad_rumble(
            gamepad_entity,
            &mut rumble_requests,
            GamepadRumbleIntensity::weak_motor(0.25),
            Duration::from_millis(70),
        );
    }
    // 右扳机按下瞬间给一次轻震动（提示"已进入自瞄订阅动作"）。
    if gamepad.just_pressed(GamepadButton::RightTrigger2) {
        request_gamepad_rumble(
            gamepad_entity,
            &mut rumble_requests,
            GamepadRumbleIntensity::strong_motor(0.12),
            Duration::from_millis(60),
        );
    }

    let controlled_chassis_yaw = controller.controlled_chassis_spin.yaw_input();
    let remote_chassis_yaw = controller.remote_chassis_spin.yaw_input();
    // 按住右摇杆（RightThumb）时，右摇杆改为控制底盘倾角，而不是云台。
    let adjusting_chassis_tilt = gamepad.pressed(GamepadButton::RightThumb);
    let controlled = &mut controller.controlled;
    controlled.add_movement(left_stick);
    controlled.add_chassis(controlled_chassis_yaw, 0.0, 0.0);
    if !adjusting_chassis_tilt {
        // 右摇杆水平反向（-x）以符合"推右即枪口向右"的手感。
        controlled.add_gimbal(Vec2::new(-right_stick.x, right_stick.y));
    }
    // `get(...).unwrap_or(0.0)`：模拟扳机读值 0..1，超过阈值才算按下。
    controlled.precise_gimbal |=
        gamepad.get(GamepadButton::LeftTrigger2).unwrap_or(0.0) > GAMEPAD_TRIGGER_THRESHOLD;
    controlled.boost |= gamepad.pressed(GamepadButton::LeftThumb);
    controlled.auto_aim |=
        gamepad.get(GamepadButton::RightTrigger2).unwrap_or(0.0) > GAMEPAD_TRIGGER_THRESHOLD;
    controlled.shoot |= gamepad.pressed(GamepadButton::RightTrigger);
    controlled.dart_just_pressed |= gamepad.just_pressed(GamepadButton::West);
    controlled.switch_slapper_just_pressed |= gamepad.just_pressed(GamepadButton::Start);
    controlled.switch_camera_just_pressed |= gamepad.just_pressed(GamepadButton::Select);

    let remote = &mut controller.remote;
    remote.add_movement(-dpad); // D-pad（十字键）映射到遥控车平移
    if adjusting_chassis_tilt {
        remote.add_chassis(remote_chassis_yaw, right_stick.x, right_stick.y);
    } else {
        remote.add_chassis(remote_chassis_yaw, 0.0, 0.0);
    }
}

/// 把"是否订阅自瞄"同步进全局原子开关（Input 阶段）。
pub fn update_auto_aim_subscription(
    controller: Res<ControllerState>,
    enabled: Res<SubscribeAutoAim>,
) {
    let active = controller.auto_aim_active();
    // `swap` 返回旧值：只有状态真正变化时才打日志（避免每帧刷屏）。
    if enabled.swap(active, Ordering::AcqRel) != active {
        info!(
            "Auto-aim subscription is now {}.",
            if active { "ENABLED" } else { "DISABLED" }
        );
    }
}

/// 运行条件函数：本机是否按住开火键（供 `projectile_launch` 的 `.run_if(...)` 使用）。
/// 返回 `bool` 的系统即"条件系统"，Bevy 会据此决定是否运行目标系统。
pub fn controller_shoot_pressed(controller: Res<ControllerState>) -> bool {
    controller.controlled.shoot
}

/// 运行条件函数：本机飞镖键是否刚按下（供 `dart_launch` 使用）。
pub fn controller_dart_just_pressed(controller: Res<ControllerState>) -> bool {
    controller.controlled.dart_just_pressed
}

/// 面向"当前活跃手柄"的震动请求（若没有手柄就什么都不做）。
pub fn request_controller_rumble(
    controller: Option<&ControllerState>,
    rumble_requests: &mut MessageWriter<GamepadRumbleRequest>,
    intensity: GamepadRumbleIntensity,
    duration: Duration,
) {
    // `and_then(ControllerState::active_gamepad)`：先过滤 Option，再取手柄实体——
    // 任一为空则整体为空（`Option` 链式处理的典型用法）。
    let Some(gamepad) = controller.and_then(ControllerState::active_gamepad) else {
        return;
    };
    request_gamepad_rumble(gamepad, rumble_requests, intensity, duration);
}

/// 真正的震动写入。这里解释**"消息（Message）"与"事件（Event/Observer）"的区别**：
/// - 观察者事件（如 armor/collision.rs 的 `On<CollisionStart>`）是"发生即回调"，不等读取；
/// - 消息（本项目里 `MessageWriter`/`MessageReader`）是**带缓冲的逐帧队列**：写入方只管
///   `write`，读取方在自己方便的系统里去 `read`，两份都在下一帧被统一清理。适合"每帧
///   生产、随后消费"的广播式数据（手柄震动、鼠标位移等）。
fn request_gamepad_rumble(
    gamepad: Entity,
    rumble_requests: &mut MessageWriter<GamepadRumbleRequest>,
    intensity: GamepadRumbleIntensity,
    duration: Duration,
) {
    // `write` 只是把消息压进队列，不立即生效。
    rumble_requests.write(GamepadRumbleRequest::Add {
        gamepad,
        intensity,
        duration,
    });
}

/// 四个方向键 → 底盘平移向量：(x=左右, y=前后)，各分量 -1/0/1。
fn keyboard_vec2(
    keyboard: &ButtonInput<KeyCode>,
    forward: KeyCode,
    left: KeyCode,
    backward: KeyCode,
    right: KeyCode,
) -> Vec2 {
    let mut input = Vec2::ZERO;
    if keyboard.pressed(forward) {
        input.y += 1.0;
    }
    if keyboard.pressed(backward) {
        input.y -= 1.0;
    }
    if keyboard.pressed(right) {
        input.x += 1.0;
    }
    if keyboard.pressed(left) {
        input.x -= 1.0;
    }
    input
}

/// 两个按键 → 单轴值：positive 按下 +1，negative 按下 -1，同时按下抵消为 0。
fn keyboard_axis(keyboard: &ButtonInput<KeyCode>, positive: KeyCode, negative: KeyCode) -> f32 {
    let mut input = 0.0;
    if keyboard.pressed(positive) {
        input += 1.0;
    }
    if keyboard.pressed(negative) {
        input -= 1.0;
    }
    input
}

/// 判断键盘是否"正在被使用"（用于在键盘/手柄之间切换帮助文本与手感方案）。
fn keyboard_controller_active(keyboard: &ButtonInput<KeyCode>) -> bool {
    // 持续键：`pressed` 判定（按住即算使用）。
    const HELD_KEYS: [KeyCode; 22] = [
        KeyCode::KeyW,
        KeyCode::KeyA,
        KeyCode::KeyS,
        KeyCode::KeyD,
        KeyCode::ArrowLeft,
        KeyCode::ArrowRight,
        KeyCode::ArrowUp,
        KeyCode::ArrowDown,
        KeyCode::ShiftLeft,
        KeyCode::Space,
        KeyCode::KeyI,
        KeyCode::KeyJ,
        KeyCode::KeyK,
        KeyCode::KeyL,
        KeyCode::BracketLeft,
        KeyCode::BracketRight,
        KeyCode::Semicolon,
        KeyCode::Quote,
        KeyCode::KeyC,
        KeyCode::KeyB,
        KeyCode::KeyF,
        KeyCode::KeyV,
    ];
    // 触发键：`just_pressed` 判定（按下那一帧即算使用）。
    const EDGE_KEYS: [KeyCode; 6] = [
        KeyCode::KeyG,
        KeyCode::Tab,
        KeyCode::F3,
        KeyCode::F5,
        KeyCode::KeyQ,
        KeyCode::KeyU,
    ];

    // 任一持续键按住、或任一触发键刚按下，即视为键盘活跃。
    HELD_KEYS.iter().any(|&key| keyboard.pressed(key))
        || EDGE_KEYS.iter().any(|&key| keyboard.just_pressed(key))
}

/// 判断手柄是否"正在被使用"（摇杆偏移、扳机、任意已用按键）。
fn gamepad_controller_active(
    gamepad: &Gamepad,
    left_stick: Vec2,
    right_stick: Vec2,
    dpad: Vec2,
) -> bool {
    left_stick != Vec2::ZERO
        || right_stick != Vec2::ZERO
        || dpad != Vec2::ZERO
        || gamepad.pressed(GamepadButton::LeftTrigger)
        || gamepad.pressed(GamepadButton::LeftThumb)
        || gamepad.pressed(GamepadButton::RightThumb)
        || gamepad.get(GamepadButton::LeftTrigger2).unwrap_or(0.0) > GAMEPAD_TRIGGER_THRESHOLD
        || gamepad.get(GamepadButton::RightTrigger2).unwrap_or(0.0) > GAMEPAD_TRIGGER_THRESHOLD
        || gamepad.pressed(GamepadButton::RightTrigger)
        || gamepad.just_pressed(GamepadButton::North)
        || gamepad.just_pressed(GamepadButton::West)
        || gamepad.just_pressed(GamepadButton::Start)
        || gamepad.just_pressed(GamepadButton::Select)
}

/// 摇杆死区 + 归一化：小于死区归零；超出部分按比例放大到 0..1（保证满打满算仍到 1）。
fn apply_stick_deadzone(input: Vec2) -> Vec2 {
    let length = input.length();
    if length <= GAMEPAD_STICK_DEADZONE {
        return Vec2::ZERO;
    }
    // (len - deadzone) / (1 - deadzone)：把 [deadzone,1] 线性映射到 [0,1]，再向量方向缩放。
    let scaled = ((length - GAMEPAD_STICK_DEADZONE) / (1.0 - GAMEPAD_STICK_DEADZONE)).min(1.0);
    input / length * scaled
}

/// 把向量的两个分量各自夹到 -1..1（不改变方向，只压长度）。
fn clamp_axes_vec2(input: Vec2) -> Vec2 {
    Vec2::new(input.x.clamp(-1.0, 1.0), input.y.clamp(-1.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 死区内的微小抖动必须被过滤为 0。
    #[test]
    fn deadzone_filters_small_stick_noise() {
        assert_eq!(apply_stick_deadzone(Vec2::new(0.05, 0.05)), Vec2::ZERO);
    }

    /// 满打方向应原样保留（归一化不失真）。
    #[test]
    fn deadzone_preserves_full_stick_deflection() {
        assert_eq!(apply_stick_deadzone(Vec2::X), Vec2::X);
        assert_eq!(apply_stick_deadzone(Vec2::Y), Vec2::Y);
    }

    /// 各轴独立夹紧：斜向输入不会被整体缩放，且叠加后不越界。
    #[test]
    fn controller_input_clamps_each_axis_without_changing_diagonal_input() {
        let mut input = ControllerInput::default();
        input.add_movement(Vec2::X);
        input.add_movement(Vec2::Y);

        assert_eq!(input.movement, Vec2::ONE);

        input.add_movement(Vec2::ONE);
        assert_eq!(input.movement, Vec2::ONE);
    }

    /// 帮助文本应随自瞄模式在 manual / auto-aim 之间切换。
    #[test]
    fn help_provider_switches_between_manual_and_auto_aim_modes() {
        let mut controller = ControllerState::default();
        controller.use_help(ControllerHelp::xbox());

        assert_eq!(controller.help_source(), "xbox");
        assert_eq!(controller.help_mode(), "manual");
        assert!(controller.help_controls().contains("hold RT"));

        controller.controlled.auto_aim = true;
        assert_eq!(controller.help_mode(), "auto-aim");
        assert!(controller.help_controls().contains("release RT"));
    }

    /// 帧首清空输入时，帮助文本方案（跨帧状态）不应被清掉。
    #[test]
    fn reset_frame_preserves_last_help_provider() {
        let mut controller = ControllerState::default();
        controller.use_help(ControllerHelp::xbox());

        controller.reset_frame();

        assert_eq!(controller.help_source(), "xbox");
    }

    /// 帧首清空输入时，小陀螺开关（跨帧状态）不应被清掉。
    #[test]
    fn reset_frame_preserves_chassis_spin_modes() {
        let mut controller = ControllerState::default();
        controller.toggle_controlled_chassis_spin();
        controller.toggle_remote_chassis_spin();

        controller.reset_frame();

        assert!(controller.controlled_chassis_spin());
        assert!(controller.remote_chassis_spin());
    }
}
