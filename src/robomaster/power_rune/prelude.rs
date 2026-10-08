//! 能量机关对外入口（prelude）：重导出各子模块的公共类型，并把三个插件打包成一个
//! `PowerRunePlugins`。上层 `robomaster/prelude.rs` 再把它汇总进主程序的 `add_plugins`。
//!
//! 为什么要有 prelude：调用方（如 scene.rs、main.rs）只需 `use ...::prelude::*` 就能一次
//! 拿到需要的全部类型与插件，无需关心它们分别定义在哪个子文件里。

// 逐个导入三个插件的类型，供下面的 plugin_group 打包使用。
use crate::robomaster::power_rune::collision::PowerRuneCollisionPlugin;
use crate::robomaster::power_rune::construct::PowerRuneConstructorPlugin;
use crate::robomaster::power_rune::rune::PowerRuneUpdatePlugin;
use bevy::app::plugin_group;

// `pub use ...::*;` 是"再导出"：把该模块的全部公开项登记为 prelude 的公开成员，
// 于是 `use ...::power_rune::prelude::*` 就能直接用到下面这些名字。
pub use crate::robomaster::power_rune::collision::*; // Projectile、RuneIndex、RuneHit、HitResult...
pub use crate::robomaster::power_rune::common::*; // RuneMode、RuneHitOutcome、RuneTransition...
pub use crate::robomaster::power_rune::construct::*; // PowerRuneRoot（scene.rs 需要）、setup_power_rune...
pub use crate::robomaster::power_rune::rotation::*; // PowerRuneRotation、RotationController...
pub use crate::robomaster::power_rune::rune::*; // PowerRune、PowerRuneMechanism、PowerRuneUpdatePlugin
pub use crate::robomaster::power_rune::state::*; // MechanismState、ActivationRun...
pub use crate::robomaster::visibility::Activation; // 靶位灯的显示状态枚举，一并导出方便使用

// `plugin_group!` 是 Bevy 提供的宏：生成一个"插件组"结构体，安装它即等于
// 一次性安装花括号里 `:XxxPlugin,` 列出的全部插件。
// `#[derive(Default)]` 让调用方可以用 `PowerRunePlugins` 或 `::default()` 安装。
plugin_group! {
    #[derive(Default)]
    pub struct PowerRunePlugins {
        :PowerRuneConstructorPlugin, // 装配：把场景节点加工成机关组件（无系统，靠场景流程触发）
        :PowerRuneCollisionPlugin, // 命中判定：观察者 + 去重资源 + 清理系统
        :PowerRuneUpdatePlugin, // 每帧更新：tick 状态机、刷新可视化、旋转机关
    }
}
