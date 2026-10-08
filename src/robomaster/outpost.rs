//! 前哨站子系统：基地前的旋转靶（前哨站），被摧毁前持续绕竖轴自转。
//!
//! 游戏语言：前哨站是 RoboMaster 里的一个固定靶，红蓝双方各有一个；它会匀速旋转，
//! 装甲朝向随之变化，给射手增加难度。红队顺时针、蓝队逆时针（与赛场规则一致）。
//!
//! 子模块分工：
//! - `consts`：旋转角速度常量；
//! - `rotation`：旋转方向/模式与"旋转控制器"的纯逻辑（不依赖 ECS，可单测）；
//! - `construct`：把带 `OutpostRoot` 标记的实体组装成 Outpost + 可旋转部件；
//! - `update`：每帧驱动旋转的系统 + 调试用的模式循环；
//! - `prelude`：打包成 `OutpostPlugins` 对外导出。
//!
//! 新手阅读顺序：consts（看常量）→ rotation（纯逻辑）→ construct（组装）→ update（每帧驱动）。
mod construct;
mod consts;
pub mod prelude;
mod rotation;
mod update;
