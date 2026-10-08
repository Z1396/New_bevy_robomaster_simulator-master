use bevy::{
    asset::AssetServer,
    audio::AudioPlayer,
    ecs::{
        observer::On,
        system::{Commands, Query, Res, ResMut},
    },
    transform::components::Transform,
};

use crate::{
    robomaster::prelude::{PowerRune, RuneActivated, RuneHit},
    statistic::ProjectileStatistics,
};

pub fn on_activate(
    ev: On<RuneActivated>,
    mut commands: Commands,
    query: Query<&PowerRune>,
    asset_server: Res<AssetServer>,
) {
    let Ok(_rune) = query.get(ev.rune) else {
        return;
    };
    commands.spawn(AudioPlayer::new(asset_server.load("rune_activated.ogg")));
}

pub fn on_hit(
    ev: On<RuneHit>,
    mut stats: ResMut<ProjectileStatistics>,
    _commands: Commands,
    query: Query<(&Transform, &PowerRune)>,
) {
    let Ok((_transform, _rune)) = query.get(ev.rune) else {
        return;
    };
    if ev.result.accurate() {
        // 【修改】符命中改用独立计数：原来混入 accurate_count 导致命中率口径混乱
        stats.increase_rune_hit();
        //commands.spawn(AudioPlayer::new(asset_server.load("rune_activated.ogg")));
    }
}
