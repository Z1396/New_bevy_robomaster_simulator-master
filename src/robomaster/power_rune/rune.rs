//! 能量机关的组件宿主与每帧更新：把状态机（state.rs）、旋转（rotation.rs）、
//! 可视化（visual.rs）绑到"面"实体上，并驱动它们每帧运转。
//!
//! 相关组件：
//! - `PowerRune`：静态信息（所属队伍、小/大机关规格）；
//! - `PowerRuneMechanism`：动态状态机（内含 `MechanismState`）；
//! - `PowerRuneRotation` / `PowerRuneVisuals`：分别定义在 rotation.rs / visual.rs。
//!
//! 三个更新系统按顺序 `.chain()`（见文件末的插件）：
//! `rune_activation_tick`（按时间推进状态机 + 同步旋转）→
//! `apply_power_rune_visuals`（按新状态刷新显示）→
//! `rune_rotation_system`（按当前转速旋转机关）。

use crate::robomaster::power_rune::common::RuneMode;
use crate::robomaster::power_rune::rotation::PowerRuneRotation;
use crate::robomaster::power_rune::state::MechanismState;
use crate::robomaster::power_rune::visual::PowerRuneVisuals;
use crate::robomaster::prelude::Team;
use crate::robomaster::visibility::StatefulAppearance;
use bevy::app::Update;
use bevy::prelude::{Component, IntoScheduleConfigs, Query, Res, Time, Transform};

/// "面"的静态配置组件。`Copy, Clone, Hash, Eq` 让它便于复制与比较（Team、RuneMode 都满足）。
#[derive(Component, Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub struct PowerRune {
    team: Team,     // 所属队伍（红/蓝），决定顺/逆时针与外观
    mode: RuneMode, // 小机关 / 大机关
}

/// "面"的动态状态机组件——把 `MechanismState`（定义在 state.rs）包成 Bevy 组件。
/// `Clone, PartialEq` 便于复制与比较；`Debug` 便于日志输出。
#[derive(Component, Debug, Clone, PartialEq)]
pub struct PowerRuneMechanism {
    state: MechanismState,
}

impl PowerRune {
    /// 新建，参数为所属队伍与规格。
    pub fn new(team: Team, mode: RuneMode) -> Self {
        Self { team, mode }
    }

    /// 所属队伍。
    pub fn team(&self) -> Team {
        self.team
    }

    /// 机关规格（小/大）。
    pub fn mode(&self) -> RuneMode {
        self.mode
    }
}

impl PowerRuneMechanism {
    /// 新建：初始为"未激活"状态（`MechanismState::inactive`，见 state.rs）。
    pub fn new(mode: RuneMode) -> Self {
        Self {
            state: MechanismState::inactive(mode),
        }
    }

    /// 只读访问状态机（命中判定后读取 mode / 是否正在激活等）。
    pub fn state(&self) -> &MechanismState {
        &self.state
    }

    /// 可变访问状态机（命中判定时推进、每帧 tick）。
    pub fn state_mut(&mut self) -> &mut MechanismState {
        &mut self.state
    }
}

/// 默认构造：默认小机关。`Default` 是 Bevy 里除 Component 之外最常用的惯例 trait。
impl Default for PowerRuneMechanism {
    fn default() -> Self {
        Self::new(RuneMode::Small)
    }
}

/// 每帧按时间推进状态机，并同步旋转模式。
/// `Res<Time>`：Bevy 注入的全局时钟资源，用 `delta_secs()` 取本帧时长（秒）。
fn rune_activation_tick(
    time: Res<Time>,
    mut runes: Query<(&mut PowerRuneMechanism, &mut PowerRuneRotation)>,
) {
    let delta_secs = time.delta_secs();
    // 每帧建一次随机数源，复用于本帧所有机关（tick 内部会用它随机选靶/生成变速参数）。
    let mut rng = rand::rng();

    for (mut mechanism, mut rotation) in &mut runes {
        // tick 会推进计时并可能触发状态转移（Started/Failed/Activated...），返回值此处不关心。
        mechanism.state.tick(delta_secs, &mut rng);
        // tick 可能改变"是否正在激活"，据此同步旋转的变速状态。
        rotation.sync_activation(
            mechanism.state.mode(),
            mechanism.state.is_activating(),
            &mut rng,
        );
    }
}

/// 按状态机的最新状态刷新机关外观（灯 / 材质）。
/// `StatefulAppearance` 是一个 SystemParam（见 visibility.rs），封装材质与可见性的批量修改。
fn apply_power_rune_visuals(
    mut runes: Query<(&PowerRune, &PowerRuneMechanism, &mut PowerRuneVisuals)>,
    mut appearance: StatefulAppearance,
) {
    for (rune, mechanism, mut visuals) in &mut runes {
        visuals.apply(rune.mode, mechanism.state(), &mut appearance);
    }
}

/// 让机关按当前转速实际旋转。
/// 需要 `&mut Transform`：`rotate` 会就地修改实体的局部变换。
fn rune_rotation_system(
    time: Res<Time>,
    mut runes: Query<(&PowerRune, &mut PowerRuneRotation, &mut Transform)>,
) {
    let dt = time.delta_secs();
    for (rune, mut rotation, mut transform) in &mut runes {
        rotation.rotate(rune.mode, &mut transform, dt);
    }
}

/// 更新插件：把上面三个系统按顺序挂进 `Update` 阶段。
#[derive(Default)]
pub(super) struct PowerRuneUpdatePlugin;

impl bevy::app::Plugin for PowerRuneUpdatePlugin {
    fn build(&self, app: &mut bevy::app::App) {
        // `.chain()` 保证严格按声明顺序执行：先算状态 → 再改外观 → 最后旋转。
        app.add_systems(
            Update,
            (
                rune_activation_tick,
                apply_power_rune_visuals,
                rune_rotation_system,
            )
                .chain(),
        );
    }
}
