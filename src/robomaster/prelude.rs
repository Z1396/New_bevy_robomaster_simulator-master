//! RoboMaster 模块的**顶层门面**：一个总插件组 + 一个"按名字找子节点"的声明式宏。
//!
//! 两部分内容：
//! 1. `RoboMasterPlugins`：把 robomaster 下各子系统（装甲/能量机关/前哨站/科技核心/有状态外观）
//!    的插件打包成一个，`main.rs` 只需 `app.add_plugins(RoboMasterPlugins)` 即可全部安装；
//! 2. `entity_root!` 宏：给"从场景里按节点名匹配出一批实体、并为它们绑定回调"这件事提供声明式写法。
//!    场景中的实体层级由美术资源决定，代码在运行时靠名字去匹配——该宏就是这套匹配逻辑的通用外壳。
//!
//! 此外文件顶部大量 `pub use ...::prelude::*;` 把各子模块入口再导出，形成"一处 import、全线可用"的体验。

use crate::robomaster::{armor, outpost, power_rune, tech_core};
use bevy::app::App;
use bevy::prelude::Plugin;

// 再导出公共词汇（Team/Robot/RobotConfig 等）。
pub use crate::robomaster::common::*;
// 各子系统内部插件（非再导出，仅供下方 RoboMasterPlugins 安装）。
use crate::robomaster::outpost::prelude::OutpostPlugins;
use crate::robomaster::tech_core::prelude::TechCorePlugins;
use crate::robomaster::visibility::StatefulAppearancePlugin;
// 各子系统 prelude 的通配再导出——外部 `use robomaster::prelude::*` 即拿到全部公开类型。
pub use armor::prelude::*;
pub use outpost::prelude::*;
pub use power_rune::prelude::*;
pub use tech_core::prelude::*;

/// robomaster 的总插件：安装后，装甲、能量机关、前哨站、科技核心等系统全部就位。
/// `Default` 让它能直接以 `RoboMasterPlugins` 形式传给 `add_plugins`。
#[derive(Default)]
pub struct RoboMasterPlugins;
impl Plugin for RoboMasterPlugins {
    fn build(&self, app: &mut App) {
        // 链式 `add_plugins` 依次安装各子系统插件；顺序一般无强依赖，这里按"基础设施在前"排列。
        app.add_plugins(StatefulAppearancePlugin)
            .add_plugins(ArmorPlugins)
            .add_plugins(PowerRunePlugins)
            .add_plugins(OutpostPlugins)
            .add_plugins(TechCorePlugins);
    }
}

/// `entity_root!`：声明式宏——从某个根实体出发，按其**子节点名字**匹配出一批实体并注入变量绑定。
///
/// 它解决的痛点：场景加载后节点的父子结构与名字是"运行时才知道"的（由 glb 决定），
/// 代码想在 init 阶段就把某些子节点"认领"成变量（后续给它挂控制器/回调），就需要一套遍历+匹配的样板。
/// 本宏把这些样板收敛成一段声明，`{ match { ... } }` 块内用形如 `LABEL => name { ... }` 的条目做匹配。
///
/// 匹配方式（对应下面各 `@match` 规则臂，可用"标点前缀"表达不同语义）：
/// - `label`                —— **相等**：子节点名恰好等于该字面量；
/// - `:label`               —— **后缀**：名字以 label 结尾（`ends_with`）；
/// - `label:`               —— **前缀**：名字以 label 开头（`starts_with`）；
/// - `:label:`              —— **包含**：名字中出现 label（`contains`）。
/// 命中后把该实体绑定到 `$ident`，并在花括号里对该变量执行你写的语句（常是创建控制器）。
/// 下面各臂是宏的内部实现细节，普通使用者只需掌握上面的调用写法。
#[macro_export]
macro_rules! entity_root {
    // 入口臂：接收 `super $child_of => $children; name $name; ` 之类的上下文（查询与根实体），
    // 把参数改名为内部统一名字（_child_of/_children/_name/_root）后交给 @internal 处理。
    (
        super $child_of:expr => $children:expr;
        name $name:expr;
        $root:ident {
            $($expr:tt)*
        }
    ) => {{
        let _child_of = &$child_of;
        let _children = &$children;
        let _name = &$name;
        let _root = $root;
        $crate::entity_root!(@internal _root, _name, _child_of, _children, { $($expr)* });
    }};

    // @match 规则一：**相等匹配**。`$label:expr => $ident:ident {...}`：名字等于 label 时，
    // 把当前实体绑定为 $ident，执行其块体，然后 `continue` 处理下一个子节点；否则递归匹配剩余条目。
    (@match $root:expr, $name:ident, $child_of:ident, $children:ident,
            $name_str:ident,
            $label:expr => $ident:ident {$($tt:tt)*}; $($rest:tt)*
    )=>{
        if $name_str == $label {
            let $ident = $root;
            $crate::entity_root!(@internal $ident, $name, $child_of, $children, {$($tt)*});
            continue;
        }
        $crate::entity_root!(@match $root, $name, $child_of, $children, $name_str, $($rest)*);
    };

    // @match 规则二：**后缀匹配**（label 前带 `:`）。名字以 label 结尾即命中。
    (@match $root:expr, $name:ident, $child_of:ident, $children:ident,
            $name_str:ident,
            :$label:literal => $ident:ident {$($tt:tt)*}; $($rest:tt)*
    )=>{
        if $name_str.ends_with(&$label) {
            let $ident = $root;
            $crate::entity_root!(@internal $ident, $name, $child_of, $children, {$($tt)*});
            continue;
        }
        $crate::entity_root!(@match $root, $name, $child_of, $children, $name_str, $($rest)*);
    };

    // @match 规则三：**前缀匹配**（label 后带 `:`）。名字以 label 开头即命中。
    (@match $root:expr, $name:ident, $child_of:ident, $children:ident,
            $name_str:ident,
            $label:literal: => $ident:ident {$($tt:tt)*}; $($rest:tt)*
    )=>{
        if $name_str.starts_with(&$label) {
            let $ident = $root;
            $crate::entity_root!(@internal $ident, $name, $child_of, $children, {$($tt)*});
            continue;
        }
        $crate::entity_root!(@match $root, $name, $child_of, $children, $name_str, $($rest)*);
    };

    // @match 规则四：**包含匹配**（label 前后都带 `:`）。名字中含 label 即命中。
    (@match $root:expr, $name:ident, $child_of:ident, $children:ident,
            $name_str:ident,
            :$label:literal: => $ident:ident {$($tt:tt)*}; $($rest:tt)*
    )=>{
        if $name_str.contains(&$label) {
            let $ident = $root;
            $crate::entity_root!(@internal $ident, $name, $child_of, $children, {$($tt)*});
            continue;
        }
        $crate::entity_root!(@match $root, $name, $child_of, $children, $name_str, $($rest)*);
    };

    // @internal 之一：处理 `{ match { ... } }` 形式——遍历根实体的**直接子节点**，
    // 取每个子节点名字后交给 @match 逐条匹配。这就是"按名找直接子节点"的核心循环。
    (@internal $root:expr, $name:ident, $child_of:ident, $children:ident, {
        match {
            $($rest:tt)*
        }
    }) => {{
        if let Ok(children) = $children.get($root) {
            for &child in children.iter() {
                let Ok(name) = $name.get(child) else { continue; };
                let name_str = name.as_str();
                $crate::entity_root!(@match child, $name, $child_of, $children, name_str, $($rest)*);
            }
        }
    }};

    // @internal 之二：处理普通语句列表——直接把花括号里的语句展开执行（对绑定出的变量做操作）。
    (@internal $root:expr, $name:ident, $child_of:ident, $children:ident, {
        $($stmt:stmt);* $(;)?
    }) => {{
        let _ = $root;
        $($stmt)*
    }};

    // @internal 之三：空块（没有内容）——什么都不做，作为"无可执行语句"的兜底臂。
    (@internal $root:expr, $name:ident, $child_of:ident, $children:ident, $(;)?) => {};


    // @match 终结臂：所有条目都试完仍无剩余（空列表）——匹配结束，不做任何事。
    (@match $root:expr, $name:ident, $child_of:ident, $children:ident,
            $name_str:ident,
            $(;)?
    )=>{};
}
