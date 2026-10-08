//! 装甲子系统：把战车/前哨站/基地模型上名为 *ARMOR* 的节点"扫描组装"成可命中的装甲板。
//!
//! 游戏语言：装甲板是 RoboMaster 里唯一的受击判定面，打中即记分/扣血；每块装甲还带
//! 编号（1~5/哨兵/前哨站/基地）、阵营颜色灯条、贴纸与 4 点标记（供视觉识别）。
//!
//! 子模块分工：
//! - `common`：装甲类型/编号枚举与贴纸槽位表（纯数据，无系统）；
//! - `construct`：核心——扫描场景树、提取网格顶点、生成 Armor 等组件（实体组装）；
//! - `collision`：命中判定观察者（子弹打中敌方装甲时统计 +1）；
//! - `marker`：装甲标记点数据（视觉识别用的 4 个顶点）；
//! - `prelude`：把上述插件打包成 `ArmorPlugins` 对外导出。
//!
//! 新手阅读顺序：common（认识概念）→ construct（看实体怎么被造出来）→ marker → prelude。
mod collision;
mod common;
mod construct;
mod marker;
// 只有 prelude 对外可见：外部通过它拿到 ArmorPlugins 及装甲相关的公开类型。
pub mod prelude;
