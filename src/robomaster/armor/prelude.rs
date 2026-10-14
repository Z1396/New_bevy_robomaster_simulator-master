//! armor 子系统的对外出口（prelude）：把插件打包成一个 `ArmorPlugins`。
//!
//! 游戏语言：main.rs 只认识 `RoboMasterPlugins`，它内部再装 `ArmorPlugins`——
//! 装甲相关的两个插件在这里被"捆"成一个，注册时一行搞定。
//!
//! 协作者：`ArmorCollisionPlugin`（命中判定，见 collision.rs）与
//! `ArmorConstructorPlugin`（实体组装，见 construct.rs）。
//!
//! 新手提示：`plugin_group!` 是 Bevy 提供的声明宏，用来把多个 `Plugin` 组合成一个
//! "插件组"，安装插件组 = 依次安装组内所有插件（组内顺序 = 安装顺序）。

// 导入组内插件类型（collision 里的插件是 `pub(super)`，同子系统内可引用）。
use super::collision::ArmorCollisionPlugin;
// `pub use ...::*` 重新导出：让外部 `use armor::prelude::*` 时直接拿到装甲的全部公开类型
//（如 ArmorSpec、Armor、MarkerData 等），无需知道它们各自定义在哪个文件。
pub use crate::robomaster::armor::common::*;
pub use crate::robomaster::armor::construct::*;
pub use crate::robomaster::armor::marker::*;
// `plugin_group!` 宏：用于定义"批量安装多个插件"的插件组。
use bevy::app::plugin_group;

// `plugin_group! { pub struct 组名 { :成员插件, ... } }`：冒号前缀表示"组内成员插件"。
// ！代表"组内成员插件"，即 ArmorConstructorPlugin 与 ArmorCollisionPlugin。(作用：利于在任意想要使用装甲的场景里直接注册这个插件组)
// ArmorCollisionPlugin 依次加进 App。
plugin_group! {
    pub struct ArmorPlugins {
        //告诉`plugin_group!`宏：打包这个插件组的时候，把 `ArmorConstructorPlugin`、`ArmorCollisionPlugin` 加进去。
        :ArmorConstructorPlugin,// - `ArmorConstructorPlugin`：负责创建装甲实体。
        :ArmorCollisionPlugin,// - `ArmorCollisionPlugin`：负责判定装甲实体是否被命中。
    }
}
