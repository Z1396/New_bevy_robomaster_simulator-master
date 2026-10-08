//! 能量机关（Power Rune）模块：RoboMaster 场地中央的打靶机关。
//!
//! 玩法概述：机关分为多个"面"（FACE），每个面有 5 个靶位（TARGET）。子弹命中靶位时，
//! 状态机（state.rs）判定这次命中是"打中正确目标"还是"打错了"；正确命中会点亮靶位，
//! 把一面上的靶位按规则全部点亮即"激活"机关。大机关还需要在副靶的时间窗内补刀。
//!
//! 谁在用本模块：scene.rs 加载 POWER.glb 之后调用本模块的 `setup_power_rune`
//! （construct.rs）完成装配；prelude.rs 把三个插件打包成 `PowerRunePlugins`，
//! 再由 robomaster/prelude.rs 汇总进主程序（见 src/main.rs 的 add_plugins）。
//!
//! 新手阅读顺序（先看"入口"再看"规则"，最后看"表现"）：
//! 1. common.rs   —— 共享枚举：小/大机关、命中结果、状态转移；
//! 2. consts.rs   —— 各阶段计时常量（单位全是秒）与旋转基准角速度；
//! 3. collision.rs—— 子弹打中靶位的"观察者"（命中判定入口）；
//! 4. state.rs    —— 核心状态机：激活各阶段、计时、命中如何推进/回退；
//! 5. rune.rs     —— 把状态机挂到实体上，每帧 tick / 同步旋转 / 刷新显示；
//! 6. rotation.rs —— 机关旋转运动学（小机关匀速、大机关正弦变速）；
//! 7. visual.rs   —— 根据状态切换灯与材质；
//! 8. construct.rs—— 从场景节点装配出上述全部组件。

// `mod` 声明子模块：私有 `mod` 只在本模块树内可见，`pub mod` 对外可见。
// 这里 `construct` 必须是 pub，因为 scene.rs 要跨模块直接调用 `setup_power_rune`。
mod collision; // 命中判定观察者，以及 Projectile / RuneIndex / 命中事件
mod common; // 共享枚举与常量：RuneMode、RuneHitOutcome、RuneTransition...
pub mod construct; // 从场景节点装配能量机关（含 setup_power_rune）
mod consts; // 各阶段超时时间与旋转基准角速度
pub mod prelude; // 插件打包与公共类型重导出（`pub use`）
mod rotation; // 机关旋转运动学（RotationController）
mod rune; // 组件宿主 + 每帧更新系统（PowerRuneUpdatePlugin）
mod state; // 激活状态机核心（MechanismState）
mod visual; // 灯 / 材质可视化控制器
