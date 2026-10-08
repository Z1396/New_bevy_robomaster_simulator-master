//! 能量机关命中的"观察者"集合：响应 power_rune 状态机发出的事件，负责统计与音效。
//!
//! 与 `robomaster/armor/collision.rs` 是同一套观察者套路（观察者 = 事件发生时被调用的回调，
//! 不是每帧系统；详见该文件文件头），区别是这里**不判定碰撞**，只消费上游算好的结果：
//!
//! 事件契约（数据流）：
//! 1. `power_rune/collision.rs::handle_rune_collision` 检测到子弹打中靶位后，调用
//!    `MechanismState::hit`（state.rs）算出 `RuneHitOutcome`，再 `commands.trigger` 发出
//!    `RuneHit` 事件（附带 `HitResult`）；
//! 2. 若该结果表示"机关已激活"，上游再补发一个 `RuneActivated` 事件；
//! 3. 本文件的 `on_hit` / `on_activate` 分别监听这两个事件，做统计累加与播放音效。
//!
//! 两个观察者在 main.rs 用 `.add_observer(on_hit)` / `.add_observer(on_activate)` 注册。

use bevy::{
    asset::AssetServer,     // 资源服务器：按路径加载音频等资产
    audio::AudioPlayer,     // 音频播放组件：挂到实体上即播放
    ecs::{
        observer::On,                              // On<事件>：观察者收到的"事件载荷"包装类型
        system::{Commands, Query, Res, ResMut},    // 延迟命令 / 查询 / 只读资源 / 可写资源
    },
    transform::components::Transform, // 位置/旋转/缩放组件
};

use crate::{
    // power_rune 的对外类型：PowerRune=机关标记组件，RuneActivated/RuneHit=两个事件。
    robomaster::prelude::{PowerRune, RuneActivated, RuneHit},
    // 投掷物统计资源（符命中单独计数）。
    statistic::ProjectileStatistics,
};

/// 监听 `RuneActivated`：能量机关被激活时播放提示音。
/// `On<RuneActivated>` 是事件载荷，`ev.rune` 是被激活的那个"面"的实体（EntityEvent 的目标字段）。
pub fn on_activate(
    ev: On<RuneActivated>,
    mut commands: Commands,
    query: Query<&PowerRune>,
    asset_server: Res<AssetServer>,
) {
    // `let Ok(_rune) = ... else { return };`：提前返回写法——目标实体查不到就什么都不做。
    // 下划线前缀 `_rune` 表示"取到但故意不用"，只是用它验证该实体确实是 PowerRune。
    let Ok(_rune) = query.get(ev.rune) else {
        return;
    };
    // 生成一个"自带音频播放器"的实体；`load` 按路径取资源。播放完毕实体自然无害，随场景清理。
    commands.spawn(AudioPlayer::new(asset_server.load("rune_activated.ogg")));
}

/// 监听 `RuneHit`：每次命中（无论结果好坏）都被调用，仅在"有效命中"时累加统计。
/// `ev.result` 是上游状态机算出的 `HitResult`，`ev.rune` 是命中的"面"实体。
pub fn on_hit(
    ev: On<RuneHit>,
    // 可写访问统计资源（`ResMut`）；符命中数 +1。
    mut stats: ResMut<ProjectileStatistics>,
    // 下划线前缀：本函数暂未使用（保留给未来"命中特效/销毁子弹"之类）。
    _commands: Commands,
    // 查询目标实体的位置与机关标记（当前仅用于校验，两者都以 `_` 命名表示暂不使用）。
    query: Query<(&Transform, &PowerRune)>,
) {
    // 校验：命中的实体确实存在且是 PowerRune，否则提前返回。
    let Ok((_transform, _rune)) = query.get(ev.rune) else {
        return;
    };
    // `ev.result.accurate()`：只有"打中正确目标"的有效命中才计数（WrongTarget 等不计）。
    if ev.result.accurate() {
        // 【修改】符命中改用独立计数：原来混入 accurate_count 导致命中率口径混乱
        // —— 现在只加 `rune_hit_count`，不污染装甲命中率 `accurate_pct()`。
        stats.increase_rune_hit();
        // 下面一行被注释保留：激活音效改由 `on_activate` 负责，这里不再重复播放。
        //commands.spawn(AudioPlayer::new(asset_server.load("rune_activated.ogg")));
    }
}
