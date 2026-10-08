//! 投掷物统计。
//!
//! 【修改】口径拆分（修复统计失真）：
//! - `accurate_count`：仅统计击中**敌方装甲**的子弹（命中率分母是 launch_count）；
//! - `rune_hit_count`：能量机关命中独立计数，不再与装甲命中混入同一口径；
//! - `counted`：已计命中的子弹集合。资源直写立即生效，不依赖 Commands 刷新，
//!   同一物理步内接触多块装甲也只计一次；子弹销毁时由观察者移除记录防泄漏。

use bevy::prelude::*;
use std::collections::HashSet;

#[derive(Resource, Default, Reflect)]
#[reflect(Resource)]
pub struct ProjectileStatistics {
    pub launch_count: u32,
    pub accurate_count: u32,
    pub rune_hit_count: u32,
    #[reflect(ignore)]
    counted: HashSet<Entity>,
}

impl ProjectileStatistics {
    pub fn increase_launch(&mut self) {
        self.launch_count += 1;
    }

    /// 记录一次命中；返回 `true` 表示该子弹首次命中（去重通过）。
    pub fn mark_hit(&mut self, projectile: Entity) -> bool {
        if self.counted.insert(projectile) {
            self.accurate_count += 1;
            true
        } else {
            false
        }
    }

    /// 子弹销毁时移除记录，防止集合无限增长。
    pub fn forget(&mut self, projectile: Entity) {
        self.counted.remove(&projectile);
    }

    pub fn increase_rune_hit(&mut self) {
        self.rune_hit_count += 1;
    }

    /// 命中率，范围 [0, 1]；由 HashSet 去重保证不会超过 1。
    pub fn accurate_pct(&self) -> f32 {
        if self.launch_count == 0 {
            return 0.0;
        }
        (self.accurate_count as f32) / (self.launch_count as f32)
    }
}
