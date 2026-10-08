//! 科技核心（Tech Core / 能量机关）模块的**目录聚合文件**。
//!
//! 本文件本身几乎不含逻辑，只负责把子模块挂到模块树上，让 `crate::robomaster::tech_core::xxx`
//! 这类路径可用。Bevy 项目里这种"只声明、不实现"的聚合文件很常见：
//! `mod a;` 告诉编译器去加载同级目录 `tech_core/a.rs`（或 `tech_core/a/mod.rs`）。
//!
//! 三个子模块分工：
//! - `construct`（`pub`）：核心实现——状态机、灯光程序、实体绑定与每帧上色，是外部唯一需要的入口；
//! - `consts`（**私有**）：本模块内部共享的常量（灯光节点命名、段数、频率），不对外暴露；
//! - `prelude`（`pub`）：把 `construct` 的公开类型 + `TechCorePlugin` 重新导出，统一对外清单。
//!
//! 可见性提醒：`mod consts;` 无 `pub`，故 `consts` 只对本模块可见（父模块 robomaster 也看不到），
//! 这正是"内部实现细节不泄漏"的写法。

// `pub mod`：公开子模块——`tech_core::construct` 可被其它模块访问。
pub mod construct;
// `mod`（无 pub）：私有子模块——仅 tech_core 内部可见（常量不该外泄）。
mod consts;
// `pub mod`：公开子模块——对外统一的重导出入口。
pub mod prelude;
