//! 投掷物统计。
//!
//! 【修改】口径拆分（修复统计失真）：
//! - `accurate_count`：仅统计击中**敌方装甲**的子弹（命中率分母是 launch_count）；
//! - `rune_hit_count`：能量机关命中独立计数，不再与装甲命中混入同一口径；
//! - `counted`：已计命中的子弹集合。资源直写立即生效，不依赖 Commands 刷新，
//!   同一物理步内接触多块装甲也只计一次；子弹销毁时由观察者移除记录防泄漏。
//!
//! 本模块只定义一个全局资源与它的方法。写给两类调用方：
//! - 发射系统调用 `increase_launch`（分母 +1）；
//! - 命中观察者调用 `mark_hit` / `increase_rune_hit`（分子 +1），子弹销毁观察者调用 `forget`。
//! 如此"发射数 / 命中数"两个口径由同一个资源保持一致，UI 只需读 `accurate_pct()`。

use bevy::prelude::*;
// 标准库哈希集合：这里用来对"已计命的子弹实体"去重。
use std::collections::HashSet;

// `#[derive(Resource)]`：标记为全局唯一资源（跨系统共享）。
// `Default`：提供初始值（全 0 / 空集合），供 main.rs 的 `init_resource` 使用。
// `Reflect`：生成运行时类型信息，供 egui 检视器/序列化使用；配合下一行 `#[reflect(Resource)]` 注册。
#[derive(Resource, Default, Reflect)]
#[reflect(Resource)]
pub struct ProjectileStatistics {
    // 累计发射子弹数（命中率的**分母**）。
    pub launch_count: u32,
    // 击中敌方装甲的子弹数（命中率的**分子**）。
    pub accurate_count: u32,
    // 能量机关命中数：独立口径，**不**计入命中率。
    pub rune_hit_count: u32,
    // `#[reflect(ignore)]`：该字段不参与反射（`HashSet<Entity>` 不便反射）。
    // 去重集合：记录已计过命中的子弹，避免同一颗子弹被重复统计。
    #[reflect(ignore)]
    counted: HashSet<Entity>,
}

impl ProjectileStatistics {
    /// 发射时调用：分母 +1。`&mut self` 表示需要可写借用（资源必须是 `ResMut`）。
    pub fn increase_launch(&mut self) {
        self.launch_count += 1;
    }

    /// 记录一次命中；返回 `true` 表示该子弹首次命中（去重通过）。
    pub fn mark_hit(&mut self, projectile: Entity) -> bool {
        // `HashSet::insert` 返回 bool：true=首次插入（此前没有），false=已存在。
        // 因此"插入成功"就说明是这颗子弹的第一次命中 → 分子 +1 并返回 true；否则返回 false。
        if self.counted.insert(projectile) {
            self.accurate_count += 1;
            true
        } else {
            false
        }
    }

    /// 子弹销毁时移除记录，防止集合无限增长。
    pub fn forget(&mut self, projectile: Entity) {
        // `remove` 的键是 `&Entity`；找不到也无妨（返回 false，被忽略）。
        self.counted.remove(&projectile);
    }

    /// 能量机关命中时调用：独立计数器 +1（不影响 `accurate_pct`）。
    pub fn increase_rune_hit(&mut self) {
        self.rune_hit_count += 1;
    }

    /// 命中率，范围 [0, 1]；由 HashSet 去重保证不会超过 1。
    pub fn accurate_pct(&self) -> f32 {
        // 分母为 0 时直接返回 0，避免整数/浮点除零。
        if self.launch_count == 0 {
            return 0.0;
        }
        // `as f32`：把 u32 转成 f32 再做浮点除法（整数除法会截断，务必先转类型）。
        (self.accurate_count as f32) / (self.launch_count as f32)
    }
}
