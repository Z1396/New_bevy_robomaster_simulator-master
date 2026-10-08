//! outpost 子系统的对外出口（prelude）。
//!
//! 游戏语言：把"前哨站组装"与"前哨站更新"两个插件打包成 `OutpostPlugins`，
//! 供上层 `RoboMasterPlugins` 一次性安装。
//!
//! `plugin_group!` 宏的用法详见 armor/prelude.rs（同一概念不重复详解）。
//! 注意本组额外 `#[derive(Default)]`，故可用 `OutpostPlugins`（默认构造）安装。
use crate::robomaster::outpost::construct::OutpostConstructorPlugin;
use bevy::app::plugin_group;

// 重新导出组装模块的公开类型（如 OutpostRoot），方便外部使用。
pub use crate::robomaster::outpost::construct::*;
use crate::robomaster::outpost::update::OutpostUpdatePlugin;

plugin_group! {
    #[derive(Default)]
    pub struct OutpostPlugins {
        :OutpostConstructorPlugin,
        :OutpostUpdatePlugin,
    }
}
