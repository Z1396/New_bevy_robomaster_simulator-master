//! 调试/展示系统：左下角帮助文本（HUD）、装甲贴纸切换、F2 截图。
//!
//! `update_help_text` 把控制器状态、命中统计、自瞄开关拼成一段文本贴到屏幕上，
//! 是本仿真器运行时唯一的"仪表盘"。

use bevy::prelude::*;
use bevy::render::view::screenshot::{Capturing, Screenshot, save_to_disk};
use bevy::window::{CursorIcon, SystemCursorIcon, Window};

use crate::components::{SlapperInfantry, SubscribeAutoAim};
use crate::robomaster::prelude::{Armor, ArmorStickerSelection};
use crate::statistic::ProjectileStatistics;
use crate::systems::ControllerState;

/// 把各项状态拼成 HUD 文本。
fn create_help_text(
    auto_aim: bool,
    stats: &ProjectileStatistics,
    controller: &ControllerState,
) -> Text {
    // 【修改】pct 显示为真正的百分比（修复前显示 0~1 小数）
    format!(
        "auto-aim={} total={} accurate={} pct={:.0}% rune={}\ncontroller={} mode={} gyro={} remote-gyro={}\n{}",
        if auto_aim { "ON " } else { "OFF" },
        stats.launch_count,
        stats.accurate_count,
        stats.accurate_pct() * 100.0,
        stats.rune_hit_count,
        controller.help_source(),
        controller.help_mode(),
        if controller.controlled_chassis_spin() {
            "ON"
        } else {
            "OFF"
        },
        if controller.remote_chassis_spin() {
            "ON"
        } else {
            "OFF"
        },
        controller.help_controls()
    )
    .into() // `String` → `Text` 的 From 转换
}

/// 生成左下角的空文本实体（Startup 调用一次；内容由 update_help_text 每帧刷新）。
pub fn spawn_text(commands: &mut Commands) {
    commands.spawn((
        Text::new(""),
        Node {
            position_type: PositionType::Absolute, // 绝对定位，脱离布局流
            bottom: Val::Px(12.0),                 // 距屏幕底部 12 像素
            left: Val::Px(12.0),                   // 距屏幕左侧 12 像素
            ..default()
        },
    ));
}

/// 每帧刷新 HUD 文本（GameLogic 阶段）。
pub fn update_help_text(
    mut text: Query<&mut Text>,
    auto_aim: Res<SubscribeAutoAim>,
    stats: Res<ProjectileStatistics>,
    controller: Res<ControllerState>,
) {
    for mut text in text.iter_mut() {
        *text = create_help_text(
            // `Acquire` 顺序读原子开关，与 talos/ROS2 线程的写入侧配对。
            auto_aim.load(std::sync::atomic::Ordering::Acquire),
            &stats,
            &controller,
        );
    }
}

/// Shift+C 轮换装甲贴纸样式（调试用，作用于可被选中的 AI 车）。
pub fn change_appearance(
    keyboard: Res<ButtonInput<KeyCode>>,
    selections: Query<&mut ArmorStickerSelection, With<SlapperInfantry>>,
    owned: Query<&mut Armor, With<SlapperInfantry>>,
) {
    // `pressed`（持续按住）与 `just_pressed`（本帧刚按下）组合成"Shift + C"这个组合键。
    if keyboard.pressed(KeyCode::ShiftLeft) && keyboard.just_pressed(KeyCode::KeyC) {
        let mut n_type = None;
        for mut selection in selections {
            let new_typ = selection.advance_debug_sequence();
            n_type = Some(new_typ);
        }
        if let Some(n_type) = n_type {
            for mut own in owned {
                own.label = n_type;
            }
        }
    }
}

/// F2 触发一次截图（main.rs 的 `.run_if(just_pressed(F2))` 保证只在按下的那一帧运行）。
pub fn screenshot_on_f2(mut commands: Commands, mut counter: Local<u32>) {
    let path = format!("./screenshot-{}.png", *counter);
    *counter += 1; // `*counter`：解引用 Local 取内部值再自增
    commands
        .spawn(Screenshot::primary_window())
        // `.observe(...)`：给该实体挂观察者，截图数据就绪时把 PNG 写到磁盘。
        .observe(save_to_disk(path));
}

/// 截图进行中给窗口转圈光标（存在 `Capturing` 实体即表示截图尚未完成）。
pub fn screenshot_saving(
    mut commands: Commands,
    screenshot_saving: Query<Entity, With<Capturing>>,
    window: Single<Entity, With<Window>>,
) {
    // match 三个分支：无截图 / 有截图 / 其它（match 必须穷尽，故保留 `_`）。
    match screenshot_saving.iter().count() {
        0 => {
            commands.entity(*window).remove::<CursorIcon>();
        }
        x if x > 0 => {
            commands
                .entity(*window)
                .insert(CursorIcon::from(SystemCursorIcon::Progress));
        }
        _ => {}
    }
}
