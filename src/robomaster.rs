//! RoboMaster 对战仿真器的"领域根模块"：把赛场上的各类实体工厂/规则按子系统
//! 拆成独立子模块，是整个 robomaster 目录树的入口（对应 src/robomaster/ 目录）。
//!
//! 游戏语言：这里管的是仿真器要模拟的 RoboMaster 战场对象——`armor`（装甲板与命中判定）、
//! `outpost`（前哨站旋转靶）、`power_rune`（能量机关）、`tech_core`（工程核心）、
//! `vehicle`（战车运动学/动力学），另有 `visibility`（伪装/显隐层）与 `common`
//! （跨子系统共享的 Team、机器人配置等基础类型）。
//!
//! 协作者：main.rs 只认识 `prelude::RoboMasterPlugins` 这一个聚合插件，
//! 由它把下面各子模块的插件一次性装进 App（见 prelude.rs）。
//!
//! 新手阅读顺序：先读 prelude.rs（看插件如何汇总）→ 挑一个子系统从它的
//! construct.rs 入手（看实体是怎样"扫描并组装"出来的）→ 再看 vehicle/movement.rs 的运动模型。
//
// `mod` 声明子模块：`mod x` 表示仅本模块内可见（私有）；`pub mod x` 表示对本 crate 其它模块
// 可见（注意 crate 外仍看不到，因为 main.rs 里的 `mod robomaster;` 本身是私有的）。
mod armor;
mod common;
mod outpost;
pub mod power_rune;
// prelude 是"对外的门面"：其它模块只从这里拿到装甲、前哨站等公开类型与总插件。
pub mod prelude;
pub mod tech_core;
pub mod vehicle;
// visibility 管理实体的伪装/显隐状态（贴纸、灯条这类可切换可见性的部件）。
mod visibility;
