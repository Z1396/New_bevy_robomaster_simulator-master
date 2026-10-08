//! 能量机关的可视化：根据状态机给出的"每个靶位当前处于哪个显示阶段"，切换对应的灯 / 材质。
//!
//! 机制：`visibility.rs` 提供两类东西：
//! - `Controller`：持有一组实体，能把它们整体切换成 4 种显示阶段之一；
//! - `Activation`：那 4 种显示阶段（Deactivated / Activating / Activated / Completed）。
//! `Controller::set(activation, appearance)`（来自 `Control` trait）负责实际改材质/可见性。
//! 本文件把这些控制器按"面 → 靶位 → 装饰件"组织起来，并按状态机输出逐帧 apply。
//!
//! 注意：这里的 `Activation` 是**显示**阶段，不等于 state.rs 的内部状态机；state.rs 通过
//! `target_states()` / `root_activation()` 把内部状态"翻译"成 `Activation` 再交到这里。

use crate::all_arg_constructor;
use crate::robomaster::power_rune::common::{RUNE_TARGET_COUNT, RuneMode};
use crate::robomaster::power_rune::state::MechanismState;
use crate::robomaster::visibility::{Activation, Control, Controller, StatefulAppearance};
use bevy::prelude::Component;

// `all_arg_constructor!` 是项目自定义宏（见 src/util/derive.rs）：以下面的字段定义为准，
// 自动生成同名字段的结构体，并生成 `pub fn new(各字段类型...) -> Self` 构造函数。
all_arg_constructor!(
    pub struct RuneVisual {
        target: Controller,               // 靶位主体（灯）
        legging_segments: [Controller; 3], // 3 段"腿"装饰
        padding_segments: Controller,     // 衬垫装饰
        progress_segments: Controller,    // 进度条装饰
    }
);

/// 单个靶位的可视化控制器集合（字段见上方宏定义）。由宏生成字段与 `new`。
impl RuneVisual {
    /// 决定这个靶位在各显示阶段该亮哪一组部件。
    /// `mode`：小/大机关的灯组不同；`activation`：它当前的显示阶段。
    pub fn apply(
        &mut self,
        mode: RuneMode,
        activation: Activation,
        appearance: &mut StatefulAppearance,
    ) {
        // match 穷尽两种规格。
        match mode {
            RuneMode::Small => {
                // 小机关：靶位本体与 3 段腿都跟随同一个显示阶段。
                self.target.set(activation, appearance);
                for swap in &mut self.legging_segments {
                    swap.set(activation, appearance);
                }
            }
            RuneMode::Large => {
                // 大机关：靶位本体在"已激活"显示阶段要显示为"未激活"（熄灭），其余阶段透传。
                // 内层 match 把 Activated 映射成 Deactivated，再交给 set。
                self.target.set(
                    match activation {
                        Activation::Activated => Activation::Deactivated,
                        _ => activation, // 其它阶段原样透传
                    },
                    appearance,
                );
                // 腿：完全激活时只亮第一段（收束效果），否则三段都跟随。
                match activation {
                    Activation::Activated => self.legging_segments[0].set(activation, appearance),
                    _ => {
                        for legging in &mut self.legging_segments {
                            legging.set(activation, appearance);
                        }
                    }
                }
            }
        }

        // 衬垫与进度条无论规格、无论阶段，都跟随当前显示阶段。
        self.padding_segments.set(activation, appearance);
        self.progress_segments.set(activation, appearance);
    }
}

/// 整台"面"（FACE）的可视化组件：一个根控制器（底座灯）+ 5 个靶位的 `RuneVisual`。
#[derive(Component)]
pub struct PowerRuneVisuals {
    root: Controller,                         // 面根部的灯（未激活 / 激活两态）
    targets: [RuneVisual; RUNE_TARGET_COUNT], // 5 个靶位，数组长度由常量决定
}

impl PowerRuneVisuals {
    /// 新建（直接接收已构造好的根控制器与靶位数组）。
    pub fn new(root: Controller, targets: [RuneVisual; RUNE_TARGET_COUNT]) -> Self {
        Self { root, targets }
    }

    /// 按状态机当前状态刷新整个面：根灯取 `root_activation`，
    /// 每个靶位取 `target_states()` 里对应的一项（数组按索引一一对应）。
    /// `zip` 把 5 个靶位控制器与 5 个显示状态配对后遍历。
    pub fn apply(
        &mut self,
        mode: RuneMode,
        state: &MechanismState,
        appearance: &mut StatefulAppearance,
    ) {
        self.root.set(state.root_activation(), appearance);
        for (target, activation) in self.targets.iter_mut().zip(state.target_states()) {
            target.apply(mode, activation, appearance);
        }
    }
}
