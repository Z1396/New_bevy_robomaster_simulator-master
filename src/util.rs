//! util 模块聚合：本项目自研的一组通用工具。
//!
//! 本文件本身不含逻辑，只做"模块声明"——把 util/ 目录下的每个 `.rs` 文件登记成子模块，
//! 之后别处就能用 `crate::util::xxx` 访问。各子模块职责：
//! - `entity_query`：自研层级查询（`HierarchyQuery` + `query!` 宏），按实体"名字"在父子树里
//!   找节点，是本项目最重要的自制工具之一；
//! - `async_world`：单任务异步运行时，让"等资源加载完成再继续"能写成顺序的 `async fn`；
//! - `bevy`：零散的 Bevy 操作小工具（按条件批量取出实体、给整棵子树插组件、改可见性）；
//! - `derive`：两个声明式宏，消除样板代码；
//! - `either`：`Either<L, R>` 二选一枚举，并手写 `Iterator`（为层级查询提供"零成本空迭代"）。

// `pub mod 名字;` = 声明一个子模块并对外公开。编译器据此去找 util/名字.rs（或
// util/名字/mod.rs）并把它编译进模块树；没有这一行，对应的 .rs 文件根本不会被编译。
pub mod async_world;
pub mod bevy;
pub mod derive;
pub mod either;
pub mod entity_query;
