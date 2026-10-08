//! 前哨站每帧更新：驱动旋转的系统 + 调试用的模式切换。
//!
//! 游戏语言：每帧让前哨站的旋转部件转过一点点，形成持续自转。
//!
//! 协作者：`rotation` 提供纯旋转逻辑，本文件从 `Time` 取帧时长喂给它；construct.rs 把
//! `Outpost`（阵营）与 `OutpostRotator`（旋转部件）挂到实体上。
//!
//! 新手阅读顺序：Outpost / OutpostRotator 组件 → 调试系统（按键切模式）
//! → 旋转系统（真正每帧转）→ 插件注册。

use crate::robomaster::outpost::rotation::{RotationController, RotationDirection, RotationMode};
use crate::robomaster::prelude::Team;
use bevy::app::Update;
use bevy::log::info;
// `IntoScheduleConfigs` 提供 `.chain()` 等系统集合组合方法（Bevy 0.19 的新 trait）。
use bevy::prelude::{
    ButtonInput, Component, IntoScheduleConfigs, KeyCode, Query, Res, ResMut, Resource, Time,
    Transform,
};
// `Hash`/`Hasher`：手动实现哈希所需的 trait（下面为 Outpost 手写 hash）。
use std::hash::{Hash, Hasher};

/// 前哨站组件：记录阵营（判定归属/敌我用）。
#[derive(Component)]
pub struct Outpost {
    team: Team,
}

// 手写 Hash：只把 team 参与哈希（效果同 `#[derive(Hash)]`，此处显式实现便于对照）。
impl Hash for Outpost {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.team.hash(state);
    }
}

// 手写 PartialEq：两块前哨站"阵营相同即相等"（忽略其它内部状态）。
impl PartialEq for Outpost {
    fn eq(&self, other: &Self) -> bool {
        self.team == other.team
    }
}

// `Eq`：标记"相等关系满足自反/对称/传递"——在 PartialEq 基础上承诺它是全等价关系。
impl Eq for Outpost {}

impl Outpost {
    /// 读取阵营。
    pub fn team(&self) -> Team {
        self.team
    }

    /// 新建。`pub(super)`：只对父模块 outpost/ 可见（construct.rs 用它）。
    pub(super) fn new(team: Team) -> Self {
        Self { team }
    }
}

/// 可旋转部件组件：挂着它的实体会被旋转系统每帧转动。
#[derive(Component)]
pub struct OutpostRotator {
    rotation: RotationController,
}

impl OutpostRotator {
    /// 用给定方向构造旋转控制器。
    /// `pub(crate)`：整个 crate 内可见（比 `pub` 收窄、比 `pub(super)` 放宽）。
    pub(crate) fn new(direction: RotationDirection) -> Self {
        Self {
            rotation: RotationController::new(direction),
        }
    }
}

/// 全局旋转模式资源（调试用），默认 `Forward`（由 RotationMode 的 `#[default]` 提供）。
/// newtype：把 RotationMode 包一层，以便让它作为独立 Resource 存在。
#[derive(Resource, Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
struct OutpostRotationMode(RotationMode);

/// 调试系统：按住 Shift 的同时按 C，循环切换旋转模式。
fn debug_cycle_outpost_rotation(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut mode: ResMut<OutpostRotationMode>,
) {
    // 必须"Shift 按住"且"C 刚按下"这一瞬间才切换，否则直接返回：
    // `pressed` 表示当前是否按住；`just_pressed` 只在按下那一帧为 true（避免按住连切）。
    if !(keyboard.pressed(KeyCode::ShiftLeft) || keyboard.pressed(KeyCode::ShiftRight))
        || !keyboard.just_pressed(KeyCode::KeyC)
    {
        return;
    }

    mode.0 = mode.0.next(); // 取下一个模式（环形）
    info!("Outpost rotation mode: {:?}", mode.0); // `{:?}` 用 Debug 打印枚举
}

/// 主旋转系统：每帧按帧时长驱动所有前哨站旋转。
fn outpost_rotation_system(
    time: Res<Time>,
    mode: Res<OutpostRotationMode>,
    // `Query<(&mut Transform, &OutpostRotator)>`：筛出同时挂这两个组件的实体；
    // `&mut Transform` 表示要就地修改它的旋转。
    mut outposts: Query<(&mut Transform, &OutpostRotator)>,
) {
    let dt = time.delta_secs(); // 上一帧到本帧的时长（秒）
    for (mut transform, outpost) in &mut outposts {
        outpost.rotation.step(&mut transform, dt, mode.0);
    }
}

#[derive(Default)]
pub(super) struct OutpostUpdatePlugin;

impl bevy::app::Plugin for OutpostUpdatePlugin {
    fn build(&self, app: &mut bevy::app::App) {
        // 先注册资源（模式初值），再注册两个系统。
        // `.chain()`：强制"先切模式、后旋转"——否则同帧内二者顺序不定，
        // 按键切换后可能要下一帧才生效。
        app.init_resource::<OutpostRotationMode>().add_systems(
            Update,
            (debug_cycle_outpost_rotation, outpost_rotation_system).chain(),
        );
    }
}
