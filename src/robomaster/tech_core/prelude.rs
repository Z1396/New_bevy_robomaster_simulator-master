//! tech_core 的**对外门面（prelude）**：聚合重导出 + 插件组。
//!
//! 新手理解"prelude"惯例：把本模块最常用的一批类型集中 `pub use` 出来，
//! 这样别处只需 `use crate::robomaster::tech_core::prelude::*;` 一行就能拿到全部入口，
//! 而不必记住每个类型定义在哪个子文件里（类型实际上住在 `construct`）。
//!
//! 这里导出两类东西：
//! 1. 状态/数据类型的**只读访问面**（枚举、`TechCore` 组件、JSON 导出函数）；
//! 2. `TechCorePlugins`——把内部私有插件 `TechCorePlugin` 包成一个可安装的插件组，
//!    供 `robomaster::prelude` 的 `RoboMasterPlugins` 安装（见 `plugin_group!` 下方说明）。

// 内部插件本体：`construct` 里以 `pub(super)` 定义，只有同父模块能拿到，故此处可导入。
use crate::robomaster::tech_core::construct::TechCorePlugin;
// `plugin_group!` 是 Bevy 提供的便捷宏，用于"把多个插件打包成一个插件组"。
use bevy::app::plugin_group;

// `#[allow(unused_imports)]`：抑制"导入了但本文件没直接用"的警告——
// 这些名字是**故意**为重导出而引入的，本文件并不使用它们（属于正常的 prelude 模式）。
#[allow(unused_imports)]
// `pub use ...::{A, B, ...}`：批量重导出（re-export）——把 construct 里的公开项
// 搬到本模块的命名空间下，外部通过 prelude::TechCore 即可访问。
pub use crate::robomaster::tech_core::construct::{
    AssemblyLightProgram, BlinkRate, LightColor, LightProgram, TechCore, TechCoreFirstLightSegment,
    TechCoreLightGroup, TechCorePhase, TechCoreRoot, TechCoreStep5Lights, tech_core_state_json,
    tech_core_state_json_from_phases,
};

// `plugin_group! { ... }` 宏展开后会生成一个实现了 `Plugin` 的结构体 `TechCorePlugins`，
// 其行为是"在 build 时依次安装花括号内列出的所有插件"。
// 语法要点：`:TechCorePlugin` —— 以冒号前缀的条目表示"用默认构造安装该插件"，
// 等价于手写 `app.add_plugins(TechCorePlugin)`；这正是插件组合的语法糖。
// `#[derive(Default)]`：让 `TechCorePlugins` 可无参创建（通常写 `TechCorePlugins` 即可，
// 因为它是个零字段结构体；RoboMasterPlugins 也是同样套路）。
plugin_group! {
    #[derive(Default)]
    pub struct TechCorePlugins {
        :TechCorePlugin,
    }
}
