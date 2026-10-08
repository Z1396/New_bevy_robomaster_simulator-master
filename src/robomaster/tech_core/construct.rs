//! 科技核心（能量机关）的状态机与灯光程序——本组最大、最复杂的文件。
//!
//! 一句话职责：读一场比赛里"科技核心"当前处于哪个阶段（`TechCorePhase`），据此把红/蓝双方
//! 三组灯的材质换成对应颜色/闪烁/流光，并对外导出一份描述当前灯光状态的 JSON（供上位机/裁判系统读取）。
//!
//! 分节导读（可按此顺序读）：
//!   1. 词汇层：`LightColor`/`BlinkRate`/`AssemblyLightProgram`/`LightProgram` —— "灯长什么样"的抽象；
//!   2. 几何层：`TechCoreFirstLightSegment`/`TechCoreStep5Lights`/`FlowActivation` —— 第一组灯 18 段寻址与跑马灯步进；
//!   3. 状态机：`TechCorePhase`（九阶段）+ `programs()` —— 阶段 → 三组灯程序的映射表，是全场逻辑核心；
//!   4. 导出层：`tech_core_state_json*` 系列 —— 把上面的状态序列化成 JSON；
//!   5. 运行时：`TechCore` 组件 + `update_tech_core_lights` 每帧上色 + `setup_tech_core` 加载时找灯。
//!
//! 关键契约：灯光节点命名见 `consts`（`{TEAM}_LIGHT_1/2/3` 及 `_1_L_/_R_{1..18}`）；段数 18、流光频率 12Hz 也由 `consts` 提供；`BLUE_LIGHT_*`/`RED_LIGHT_*` 是场景里灯节点的名字，勿改。
//!
//! 阅读建议：先看 `TechCorePhase::programs()`（阶段→灯光映射）和 `LightProgram::active_color`（程序→实际颜色），再看 `update_tech_core_lights` 把这些落到材质，其余多为配套的序列化与查找工具。

use super::consts::{
    BLUE_LIGHT_NAMES, FIRST_LIGHT_SEGMENT_COUNT, FLOW_SEGMENT_HZ, RED_LIGHT_NAMES,
};
use crate::robomaster::common::Team;
use bevy::app::{App, Update};
use bevy::color::LinearRgba;
use bevy::ecs::system::Local;
use bevy::pbr::{MeshMaterial3d, StandardMaterial};
use bevy::prelude::{
    Assets, ButtonInput, Children, Color, Commands, Component, Entity, Handle, In, KeyCode, Name,
    Plugin, Query, Res, ResMut, Time, info, warn,
};
// `world_serialization`：Bevy 的世界序列化模块——场景（glb）加载后用它按 InstanceId 遍历其实体。
use bevy::world_serialization::{InstanceId, WorldInstanceSpawner};
// serde_json：`json!` 宏直接拼 JSON；`Value` 是动态 JSON 值类型。
use serde_json::{Value, json};
use std::collections::HashMap;

/// 标记组件：挂在科技核心的根实体上，用于标识"这是核心的入口节点"。
/// `#[derive(Component)]`：声明它可作为组件挂到实体上。零字段结构体（unit struct）——只当标签用。
#[derive(Component, Debug)]
pub struct TechCoreRoot;

/// 三组灯的分组标识：第一组（可分段的灯带）、第二组、第三组。
/// 与 `consts` 里的 `*_LIGHT_1/2/3` 一一对应——`First` 即 `_LIGHT_1`（含 18 段子灯），
/// `Second`/`Third` 为整体单节点。`programs()` 返回的 `[LightProgram; 3]` 就按此顺序索引。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TechCoreLightGroup {
    First,
    Second,
    Third,
}

impl TechCoreLightGroup {
    // 全部三组的固定顺序数组，供"遍历三组灯"使用（顺序与 programs() 数组下标对齐）。
    pub const ALL: [Self; 3] = [Self::First, Self::Second, Self::Third];

    // 组 → 数组下标（0/1/2），private：仅本模块用于索引 programs()/lights。
    const fn index(self) -> usize {
        match self {
            Self::First => 0,
            Self::Second => 1,
            Self::Third => 2,
        }
    }

    // 组 → 人类可读编号（1/2/3），用于 JSON 输出（"group": 1..3）。
    pub const fn number(self) -> u8 {
        self.index() as u8 + 1
    }
}

/// 灯色的**逻辑颜色**（不是具体 RGBA）：白、阵营色、绿。
/// `Team` 是"随本方阵营变红或变蓝"的占位色——具体红蓝在 `as_str_for_team` / `resolve_color` 时按队解析。
/// 做成逻辑色而非写死 RGB，是为了同一份阶段定义能同时驱动红蓝两侧。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LightColor {
    White,
    Team,
    Green,
}

impl LightColor {
    /// 逻辑色 → 对外字符串（"white"/"green"/"red"/"blue"），`Team` 依传入阵营展开成 red 或 blue。
    /// 这也是 JSON 导出里的颜色字符串来源（跨文件契约：上位机按这些词解读）。
    pub const fn as_str_for_team(self, team: Team) -> &'static str {
        match self {
            Self::White => "white",
            Self::Green => "green",
            Self::Team => match team {
                Team::Red => "red",
                Team::Blue => "blue",
            },
        }
    }
}

/// 整组闪烁频率：1Hz 或 3Hz。是**离散档位**而非任意频率——阶段规格只用到这两档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlinkRate {
    Hz1,
    Hz3,
}

impl BlinkRate {
    /// 档位 → 数值频率，单位：Hz（次/秒）。`active_color` 用它计算明暗相位。
    pub const fn hz(self) -> f64 {
        match self {
            Self::Hz1 => 1.0,
            Self::Hz3 => 3.0,
        }
    }
}

/// 第五步（组装/装配）专用子程序：进行中 or 已完成。
/// 与其它灯色不同，第五步用"第 1 组灯的指定段"来指示目标段与能量单元位置（见 `TechCoreStep5Lights`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssemblyLightProgram {
    InProgress,
    Completed,
}

/// 一组灯要执行的"灯光程序"——本文件的核心抽象。每个阶段为三组灯各指定一个 `LightProgram`。
/// 各变体语义：
/// - `Off`：全灭；
/// - `Solid(color)`：整组恒亮某色；
/// - `Blink { color, rate }`：整组按 rate（Hz）闪烁该色（占空比 50%，见 `active_color`）；
/// - `Flow { color }`：跑马灯——第 1 组灯逐段流动该色（`Flow` 只对可分段的第 1 组有意义）；
/// - `Assembly(program)`：第五步组装引导——用第 1 组灯的**指定段**标出目标/能量单元（白/阵营色/绿）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LightProgram {
    Off,
    Solid(LightColor),
    Blink { color: LightColor, rate: BlinkRate },
    Flow { color: LightColor },
    Assembly(AssemblyLightProgram),
}

impl LightProgram {
    /// Returns the active color only for programs with a uniform group color.
    /// Segmented assembly guidance is described by its active segments instead.
    /// 译注：仅对"整组单色"的程序返回当前该亮的颜色；分段式组装引导改用其活跃段来描述。
    ///
    /// 入参 `elapsed_secs`：**当前阶段已持续的时间**（秒），用于计算闪烁/流光的瞬时相位——
    /// 传入 0 时表示阶段刚切换（见 `TechCore::phase_elapsed_secs`）。
    /// 返回 `Option<LightColor>`：`None` = 此刻该组无颜色（全灭）。
    pub fn active_color(self, elapsed_secs: f64) -> Option<LightColor> {
        match self {
            Self::Off => None,
            // Solid 恒亮：直接返回其颜色。
            Self::Solid(color) => Some(color),
            // Blink：用"已过时间 × 频率"的小数部分做相位，<0.5 亮、≥0.5 灭 → 50% 占空比。
            // `.then_some(color)`：条件成立返回 Some(color)，否则 None（Rust 惯用的三元替代）。
            Self::Blink { color, rate } => {
                ((elapsed_secs * rate.hz()).fract() < 0.5).then_some(color)
            }
            // Flow：整组颜色恒定，只是"哪几段亮"在变（段位由 active_segments/材质分配决定）。
            Self::Flow { color } => Some(color),
            // 组装进行中：颜色由目标段/能量单元段分别给出，无统一色 → None。
            Self::Assembly(AssemblyLightProgram::InProgress) => None,
            // 组装完成：目标段变绿，视为该组当前色 = 绿。
            Self::Assembly(AssemblyLightProgram::Completed) => Some(LightColor::Green),
        }
    }

    /// 把灯光程序序列化成 JSON 片段（供 `tech_core_state_json` 使用）。
    /// `team` 用于把 `Team` 逻辑色展开成具体 "red"/"blue"。
    /// 这是**对外契约**：`"mode"` 字段的取值（off/solid/blink/flow/step5_*）被上位机解析，勿随意改。
    fn json_value(self, team: Team) -> Value {
        match self {
            Self::Off => json!({ "mode": "off" }),
            Self::Solid(color) => json!({
                "mode": "solid",
                "color": color.as_str_for_team(team),
            }),
            Self::Blink { color, rate } => json!({
                "mode": "blink",
                "color": color.as_str_for_team(team),
                "hz": rate.hz(),
            }),
            // Flow 额外带上段进频率，方便上位机推算流光相位。
            Self::Flow { color } => json!({
                "mode": "flow",
                "color": color.as_str_for_team(team),
                "segment_hz": FLOW_SEGMENT_HZ,
            }),
            Self::Assembly(AssemblyLightProgram::InProgress) => json!({
                "mode": "step5_in_progress",
                "target_color": LightColor::Team.as_str_for_team(team),
                "energy_unit_color": LightColor::White.as_str_for_team(team),
            }),
            Self::Assembly(AssemblyLightProgram::Completed) => json!({
                "mode": "step5_completed",
                "target_color": LightColor::Green.as_str_for_team(team),
            }),
        }
    }
}

/// 第一组灯带中的**一段**的位置（0 基索引的 newtype）。
/// newtype 模式：用 `struct (usize)` 包一层，使"段号"成为独立类型——不会被误当成普通数字，
/// 也便于挂方法（如角度换算）。内部 `self.0` 是 0..=17（共 18 段），外部用 `number()` 得到 1..=18。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TechCoreFirstLightSegment(usize);

impl TechCoreFirstLightSegment {
    // 对外的 1 基段号范围（含端点）：1..=18。用于 `from_number` 的合法性边界。
    pub const MIN_NUMBER: usize = 1;
    pub const MAX_NUMBER: usize = FIRST_LIGHT_SEGMENT_COUNT;

    // 由 0 基索引构造：0..=17 合法，越界返回 None。
    pub const fn from_zero_based(index: usize) -> Option<Self> {
        if index < FIRST_LIGHT_SEGMENT_COUNT {
            Some(Self(index))
        } else {
            None
        }
    }

    // 由 1 基段号构造：1..=18 合法，越界返回 None（内部转成 0 基存储）。
    pub const fn from_number(number: usize) -> Option<Self> {
        if number >= Self::MIN_NUMBER && number <= Self::MAX_NUMBER {
            Some(Self(number - 1))
        } else {
            None
        }
    }

    /// 由**角度**（弧度）映射到段号：把圆均匀切成 18 份，落在哪一份就是哪段。
    /// 入参 `radians` 单位：弧度；`rem_euclid(TAU)` 先归一化到 [0, 2π)，故负角度也能正确回绕。
    /// `TAU` = 2π ≈ 6.2832；`floor` 后 `.min(17)` 防止正好等于 2π 时越界（钳到末段）。
    pub fn from_angle_radians(radians: f64) -> Self {
        let normalized = radians.rem_euclid(std::f64::consts::TAU);
        let index = (normalized / std::f64::consts::TAU * FIRST_LIGHT_SEGMENT_COUNT as f64).floor()
            as usize;

        Self(index.min(FIRST_LIGHT_SEGMENT_COUNT - 1))
    }

    /// 弧度版的便捷封装：入参 `degrees` 单位：**度**，内部转成弧度再套用 `from_angle_radians`。
    pub fn from_angle_degrees(degrees: f64) -> Self {
        Self::from_angle_radians(degrees.to_radians())
    }

    // 取 0 基索引（private）。
    const fn index(self) -> usize {
        self.0
    }

    // 取 1 基段号（用于显示/JSON）。
    pub const fn number(self) -> usize {
        self.0 + 1
    }
}

/// 第五步组装引导要亮的两段位置：**目标段** + **能量单元段**（都指向第一组灯的某一段）。
/// 由外部（游戏逻辑/上位机）用角度或段号设置，灯带据此点亮对应段来引导玩家操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TechCoreStep5Lights {
    target: TechCoreFirstLightSegment,
    energy_unit: TechCoreFirstLightSegment,
}

impl TechCoreStep5Lights {
    // 由两个段位置直接构造（private-ish，pub 供外部设置时使用）。
    pub const fn new(
        target: TechCoreFirstLightSegment,
        energy_unit: TechCoreFirstLightSegment,
    ) -> Self {
        Self {
            target,
            energy_unit,
        }
    }

    // 读目标段。
    pub const fn target(self) -> TechCoreFirstLightSegment {
        self.target
    }

    // 读能量单元段。
    pub const fn energy_unit(self) -> TechCoreFirstLightSegment {
        self.energy_unit
    }
}

impl Default for TechCoreStep5Lights {
    /// 默认：目标段 = 第 1 段；能量单元段 = 正对面那一段（第 `18/2=9` 段）。
    /// `unwrap()` 在这里安全：索引由常量算出且必定在合法范围。
    fn default() -> Self {
        Self {
            target: TechCoreFirstLightSegment::from_zero_based(0).unwrap(),
            energy_unit: TechCoreFirstLightSegment::from_zero_based(FIRST_LIGHT_SEGMENT_COUNT / 2)
                .unwrap(),
        }
    }
}

/// 跑马灯在某一时刻的激活形态：某一段亮，或全部段同时亮。
/// private（模块内部用），是 `flow_activation` 的输出类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FlowActivation {
    // 当前高亮第 index 段（0 基）。
    Segment(usize),
    // 全部 18 段同时高亮（"流到头"的收尾状态）。
    All,
}

/// 计算跑马灯在 `elapsed_secs` 时的激活形态——一个"前进 → 折返 → 全亮"的三段式节奏。
/// 入参 `elapsed_secs`：阶段已持续时间，单位：秒。
/// 逻辑：`step = 已过时间 × 12Hz` 是"已经走了几步"。前 18 步正向走（0→17），
/// 接下来 18 步反向走（17→0），走完一个来回（共 36 步）后进入 `All`（全亮常亮）。
/// `round_trip_len` = 36 就是"来回总步数"。
fn flow_activation(elapsed_secs: f64) -> FlowActivation {
    // `.max(0.0)`：防御性处理负时间（理论上不会发生），避免未定义行为。
    let elapsed_secs = elapsed_secs.max(0.0);
    // `floor()` 取整 → 当前处于第几步（离散化）。`as usize` 转换后为非负整数。
    let step = (elapsed_secs * FLOW_SEGMENT_HZ).floor() as usize;
    let forward_len = FIRST_LIGHT_SEGMENT_COUNT;
    let round_trip_len = forward_len * 2;

    if step < forward_len {
        // 前半程：正向递增，第 step 段亮。
        FlowActivation::Segment(step)
    } else if step < round_trip_len {
        // 后半程：反向递减（`36-1-step`），实现"走到头再折返"。
        FlowActivation::Segment(round_trip_len - 1 - step)
    } else {
        // 一个来回走完：全亮。
        FlowActivation::All
    }
}

/// 把跑马灯"当前哪些段亮"导出成 JSON 数组（每段含 side/index）。
/// 左右两排灯同步流动，故 `Segment` 时左右各输出同一段；`All` 时输出全部 2×18 段。
fn flow_active_segments_json(elapsed_secs: f64) -> Value {
    // 内嵌小工具函数：拼"某一侧的第 index 段"的 JSON（index 输出为 1 基）。
    fn segment_json(side: &'static str, index: usize) -> Value {
        json!({
            "side": side,
            "index": index + 1,
        })
    }

    match flow_activation(elapsed_secs) {
        FlowActivation::Segment(index) => {
            // 当前段：左右各一段。
            json!([segment_json("left", index), segment_json("right", index),])
        }
        // 全亮：`flat_map` 把每段展开成 [左, 右] 两元素再摊平，得到 36 个段。
        FlowActivation::All => Value::Array(
            (0..FIRST_LIGHT_SEGMENT_COUNT)
                .flat_map(|index| [segment_json("left", index), segment_json("right", index)])
                .collect(),
        ),
    }
}

/// 把**同一段的左右两侧**拼成一对 JSON 元素（固定返回 2 个），并带上颜色与角色（target/energy_unit）。
/// 供第五步组装引导的 `active_segments` 使用。
fn segment_pair_json(
    segment: TechCoreFirstLightSegment,
    color: &'static str,
    role: &'static str,
) -> [Value; 2] {
    [
        json!({
            "side": "left",
            "index": segment.number(),
            "color": color,
            "role": role,
        }),
        json!({
            "side": "right",
            "index": segment.number(),
            "color": color,
            "role": role,
        }),
    ]
}

/// 第五步组装引导"当前哪些段亮"的 JSON：进行中=目标段(阵营色)+能量单元段(白)；
/// 已完成=仅目标段(绿)。能量单元段与目标段重合时只输出目标段（去重）。
fn step5_active_segments_json(
    team: Team,
    assembly: AssemblyLightProgram,
    step5_lights: TechCoreStep5Lights,
) -> Value {
    // 预分配 4：最多两段 × 左右两侧 = 4 个元素。
    let mut segments = Vec::with_capacity(4);
    let target = step5_lights.target();

    match assembly {
        AssemblyLightProgram::InProgress => {
            // 目标段用阵营色（Team）标出。
            segments.extend(segment_pair_json(
                target,
                LightColor::Team.as_str_for_team(team),
                "target",
            ));

            // 能量单元段用白色；若与目标段重合则不重复输出。
            let energy_unit = step5_lights.energy_unit();
            if energy_unit != target {
                segments.extend(segment_pair_json(
                    energy_unit,
                    LightColor::White.as_str_for_team(team),
                    "energy_unit",
                ));
            }
        }
        AssemblyLightProgram::Completed => {
            // 完成后目标段转绿。
            segments.extend(segment_pair_json(
                target,
                LightColor::Green.as_str_for_team(team),
                "target",
            ));
        }
    }

    Value::Array(segments)
}

/// 科技核心的**状态机**：一场比赛里机关所处的九个阶段（按 `DEBUG_SEQUENCE` 顺序推进/循环）。
/// 每个阶段通过 `programs()` 决定三组灯怎么亮，通过 `id()`/`as_str()` 对外表达自己。
/// 阶段语义（从空转到完成、再到确认恢复）：
/// - `MatchRunningIdle`：比赛进行中、机关空闲（未开始挑战）——初始态；
/// - `DifficultySelectedArmNotReady`：已选难度、机关"臂"未就绪——第 1 组跑马灯流动示意；
/// - `DifficultySelectedArmReady`：臂已就绪、可击打——三组整组 1Hz 闪烁；
/// - `Step2Completed`/`Step3Completed`/`Step4Completed`：第 2/3/4 步先后完成（闪烁频率随之提升）；
/// - `Step5InProgress`：第五步组装进行中——第 1 组按目标/能量单元段引导；
/// - `Step5Completed`：第五步完成——目标段变绿；
/// - `ConfirmedRecovering`：确认恢复中——三组 3Hz 快闪。
/// 状态迁移是**外部驱动**的：本文件只提供 `set_phase`/`advance_debug`，实际切换由游戏逻辑或调试键触发。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TechCorePhase {
    MatchRunningIdle,
    DifficultySelectedArmNotReady,
    DifficultySelectedArmReady,
    Step2Completed,
    Step3Completed,
    Step4Completed,
    Step5InProgress,
    Step5Completed,
    ConfirmedRecovering,
}

impl TechCorePhase {
    // 调试用的固定遍历顺序（按 id 0..8 排列），`next_debug` 与测试都依赖它。
    pub const DEBUG_SEQUENCE: [Self; 9] = [
        Self::MatchRunningIdle,
        Self::DifficultySelectedArmNotReady,
        Self::DifficultySelectedArmReady,
        Self::Step2Completed,
        Self::Step3Completed,
        Self::Step4Completed,
        Self::Step5InProgress,
        Self::Step5Completed,
        Self::ConfirmedRecovering,
    ];

    /// 阶段 → 三组灯程序 的**核心映射表**（数组下标 0/1/2 对应第 1/2/3 组灯）。
    /// `const fn`：编译期可求值，保证映射是纯函数、无副作用；也是本状态机的"规格说明书"。
    /// 规律：第 1 组承担全部复杂表现（跑马灯/组装引导），第 2/3 组基本只做同色常亮或整组闪烁。
    pub const fn programs(self) -> [LightProgram; 3] {
        use AssemblyLightProgram::{Completed, InProgress};
        use BlinkRate::{Hz1, Hz3};
        use LightColor::{Team, White};
        use LightProgram::{Assembly, Blink, Flow, Off, Solid};

        match self {
            // 空闲：1 组灭，2/3 组常亮阵营色。
            Self::MatchRunningIdle => [Off, Solid(Team), Solid(Team)],
            // 选难度、臂未就绪：1 组白光跑马灯示意"待激活"。
            Self::DifficultySelectedArmNotReady => {
                [Flow { color: White }, Solid(Team), Solid(Team)]
            }
            // 臂已就绪：1 组白 1Hz、2/3 组阵营色 1Hz，整体齐闪提示"可击打"。
            Self::DifficultySelectedArmReady => [
                Blink {
                    color: White,
                    rate: Hz1,
                },
                Blink {
                    color: Team,
                    rate: Hz1,
                },
                Blink {
                    color: Team,
                    rate: Hz1,
                },
            ],
            // 第2步完成：改为 2 组同速、3 组提速到 3Hz，提示进度推进。
            Self::Step2Completed => [
                Blink {
                    color: White,
                    rate: Hz1,
                },
                Blink {
                    color: Team,
                    rate: Hz1,
                },
                Blink {
                    color: Team,
                    rate: Hz3,
                },
            ],
            // 第3步完成：2/3 组都提到 3Hz。
            Self::Step3Completed => [
                Blink {
                    color: White,
                    rate: Hz1,
                },
                Blink {
                    color: Team,
                    rate: Hz3,
                },
                Blink {
                    color: Team,
                    rate: Hz3,
                },
            ],
            // 第4步完成：2/3 组转为常亮阵营色（不再闪），表示进入组装前的稳定态。
            Self::Step4Completed => [
                Blink {
                    color: White,
                    rate: Hz1,
                },
                Solid(Team),
                Solid(Team),
            ],
            // 第五步组装进行中：1 组走组装引导程序，2/3 组常亮。
            Self::Step5InProgress => [Assembly(InProgress), Solid(Team), Solid(Team)],
            // 第五步完成：1 组组装程序切到完成态（目标段转绿）。
            Self::Step5Completed => [Assembly(Completed), Solid(Team), Solid(Team)],
            // 确认恢复中：三组统一 3Hz 快闪，提示"正在复位"。
            Self::ConfirmedRecovering => [
                Blink {
                    color: White,
                    rate: Hz3,
                },
                Blink {
                    color: Team,
                    rate: Hz3,
                },
                Blink {
                    color: Team,
                    rate: Hz3,
                },
            ],
        }
    }

    /// 阶段 → 数字 id（0..8），对外契约（JSON `phase.id` 与测试都依赖它，顺序不可改）。
    pub const fn id(self) -> u8 {
        match self {
            Self::MatchRunningIdle => 0,
            Self::DifficultySelectedArmNotReady => 1,
            Self::DifficultySelectedArmReady => 2,
            Self::Step2Completed => 3,
            Self::Step3Completed => 4,
            Self::Step4Completed => 5,
            Self::Step5InProgress => 6,
            Self::Step5Completed => 7,
            Self::ConfirmedRecovering => 8,
        }
    }

    /// 阶段 → 对外字符串名（snake_case），JSON `phase.name` 用它（跨文件契约，勿改拼写）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MatchRunningIdle => "match_running_idle",
            Self::DifficultySelectedArmNotReady => "difficulty_selected_arm_not_ready",
            Self::DifficultySelectedArmReady => "difficulty_selected_arm_ready",
            Self::Step2Completed => "step2_completed",
            Self::Step3Completed => "step3_completed",
            Self::Step4Completed => "step4_completed",
            Self::Step5InProgress => "step5_in_progress",
            Self::Step5Completed => "step5_completed",
            Self::ConfirmedRecovering => "confirmed_recovering",
        }
    }

    /// 调试用：返回 `DEBUG_SEQUENCE` 里的下一个阶段（末尾回绕到开头）。
    /// `.position(...)` 找到当前阶段的下标（找不到则 `unwrap_or(0)` 兜底），`% len` 实现环形递增。
    /// 调用时机：调试键 Shift+C 触发（见 `debug_cycle_tech_core_phase`）。
    pub fn next_debug(self) -> Self {
        let index = Self::DEBUG_SEQUENCE
            .iter()
            .position(|phase| *phase == self)
            .unwrap_or(0);
        Self::DEBUG_SEQUENCE[(index + 1) % Self::DEBUG_SEQUENCE.len()]
    }
}

/// 阵营 → 对外字符串（"red"/"blue"），JSON `team` 字段用。
fn team_name(team: Team) -> &'static str {
    match team {
        Team::Red => "red",
        Team::Blue => "blue",
    }
}

/// 单个核心、单个阵营、单组灯的 JSON 片段（`tech_core_state_json` 的基本单元）。
/// 含四个固定字段：team/group/program/active_color；对第 1 组的 Flow/Assembly 还会追加 `active_segments`。
/// 入参 `elapsed_secs`：该核心**当前阶段已持续的时间**（秒），用于即时判断闪烁此刻是亮还是灭。
fn light_json_value(
    phase: TechCorePhase,
    team: Team,
    group: TechCoreLightGroup,
    elapsed_secs: f64,
    step5_lights: TechCoreStep5Lights,
) -> Value {
    // 从阶段映射表里取该组的程序（index 与组一一对应）。
    let program = phase.programs()[group.index()];
    // active_color：组装程序单独处理（进行中="mixed"，完成=绿），其余走 active_color() 求瞬时色。
    let active_color = match program {
        LightProgram::Assembly(AssemblyLightProgram::InProgress) => "mixed",
        LightProgram::Assembly(AssemblyLightProgram::Completed) => {
            LightColor::Green.as_str_for_team(team)
        }
        _ => program
            .active_color(elapsed_secs)
            .map(|color| color.as_str_for_team(team))
            .unwrap_or("off"),
    };

    let mut value = json!({
        "team": team_name(team),
        "group": group.number(),
        "program": program.json_value(team),
        "active_color": active_color,
    });

    // 仅当"第 1 组 + 跑马灯程序"时，才附加"当前活跃段"列表。
    // `matches!(program, LightProgram::Flow { .. })`：只关心是不是 Flow 变体，忽略其字段。
    if matches!(program, LightProgram::Flow { .. }) && group == TechCoreLightGroup::First {
        // `as_object_mut`：把 Value 当对象改（若不是对象则返回 None，静默跳过）。
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "active_segments".to_string(),
                flow_active_segments_json(elapsed_secs),
            );
        }
    }

    // 仅当"第 1 组 + 组装程序"时，附加组装引导的活跃段。
    if let LightProgram::Assembly(assembly) = program {
        if group == TechCoreLightGroup::First {
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "active_segments".to_string(),
                    step5_active_segments_json(team, assembly, step5_lights),
                );
            }
        }
    }

    value
}

/// 把"某核心在某阶段的样子"整体序列化：phase(id/name) + 六盏灯（2 阵营 × 3 组）。
fn tech_core_phase_json_value(
    phase: TechCorePhase,
    elapsed_secs: f64,
    step5_lights: TechCoreStep5Lights,
) -> Value {
    // 预分配 6：红蓝各 3 组。
    let mut lights = Vec::with_capacity(6);
    // 外层遍历阵营、内层遍历三组，顺序决定 lights 数组的下标（红在前，组序 1/2/3）。
    for team in [Team::Red, Team::Blue] {
        for group in TechCoreLightGroup::ALL {
            lights.push(light_json_value(
                phase,
                team,
                group,
                elapsed_secs,
                step5_lights,
            ));
        }
    }

    json!({
        "phase": {
            "id": phase.id(),
            "name": phase.as_str(),
        },
        "lights": lights,
    })
}

/// 对外状态导出的**纯函数版**：给定一批阶段（不依赖 ECS 世界）生成状态 JSON 字符串。
/// 入参：`stamp_sec`/`stamp_nanosec` 是时间戳（秒 + 纳秒，通常是 ROS 时间），
/// `elapsed_secs` 各核心共用（因为这里只有阶段、没有各自的阶段计时），`phases` 任意可迭代的阶段集合。
/// 泛型 `I: IntoIterator<Item = TechCorePhase>`：任何能"迭代出阶段"的东西都能传（Vec、数组、迭代器...）。
/// 调用时机：测试与"只需按阶段推演"的场景；实时运行时用下面那个带 `TechCore` 的版本。
pub fn tech_core_state_json_from_phases<I>(
    stamp_sec: i32,
    stamp_nanosec: u32,
    elapsed_secs: f64,
    phases: I,
) -> String
where
    I: IntoIterator<Item = TechCorePhase>,
{
    // 每个阶段 → 一个 core 条目；step5 灯光此处统一用默认值（无实体上下文）。
    let cores = phases
        .into_iter()
        .map(|phase| {
            tech_core_phase_json_value(phase, elapsed_secs, TechCoreStep5Lights::default())
        })
        .collect::<Vec<_>>();

    json!({
        "stamp": {
            "sec": stamp_sec,
            "nanosec": stamp_nanosec,
        },
        "cores": cores,
    })
    .to_string()
}

/// 对外状态导出的**实时版**（本模块最重要的对外接口）：从实际的 `TechCore` 组件生成 JSON。
/// 与纯函数版的区别：这里每个核心用自己的阶段计时（`phase_elapsed_secs`）和自己的 step5 灯光，
/// 因此如实反映"每个核心此刻的样子"。返回 `String`（已是序列化好的 JSON 文本）。
/// 泛型 `I: IntoIterator<Item = &'a TechCore>`：接受"`TechCore` 引用"的可迭代集合（如 `Query` 的迭代）。
pub fn tech_core_state_json<'a, I>(
    stamp_sec: i32,
    stamp_nanosec: u32,
    elapsed_secs: f64,
    cores: I,
) -> String
where
    I: IntoIterator<Item = &'a TechCore>,
{
    let cores = cores
        .into_iter()
        .map(|core| {
            tech_core_phase_json_value(
                core.phase(),
                // 用该核心自己的"阶段已持续秒数"——这样闪烁/流光的瞬时相位才是对的。
                core.phase_elapsed_secs(elapsed_secs),
                core.step5_lights(),
            )
        })
        .collect::<Vec<_>>();

    json!({
        "stamp": {
            "sec": stamp_sec,
            "nanosec": stamp_nanosec,
        },
        "cores": cores,
    })
    .to_string()
}

/// 第一组灯在场景中对应的**实体集合**：既有"整体兜底节点"，也有左右两排的逐段节点。
/// `whole`：整组一个节点（当模型没有分段时的降级方案）；`left`/`right`：左右各 18 段。
/// `Option<Entity>` 允许缺失（模型可能没有整组节点或某些段），配合 `has_segments` 决定走哪条路径。
#[derive(Debug, Clone, Copy)]
struct FirstLightSet {
    whole: Option<Entity>,
    left: [Option<Entity>; FIRST_LIGHT_SEGMENT_COUNT],
    right: [Option<Entity>; FIRST_LIGHT_SEGMENT_COUNT],
}

impl FirstLightSet {
    // 直接字段构造。
    fn new(
        whole: Option<Entity>,
        left: [Option<Entity>; FIRST_LIGHT_SEGMENT_COUNT],
        right: [Option<Entity>; FIRST_LIGHT_SEGMENT_COUNT],
    ) -> Self {
        Self { whole, left, right }
    }

    // 是否至少存在一段分段灯（左或右任一非空）。有分段 → 走分段上色；否则回退到 whole。
    fn has_segments(&self) -> bool {
        self.left
            .iter()
            .chain(self.right.iter())
            .any(Option::is_some)
    }

    // 收集"缺失的段名"清单（形如 `PREFIX_L_3`），用于给美术资源问题打警告。
    fn missing_segments(&self, prefix: &str) -> Vec<String> {
        let mut missing = Vec::new();

        // 段名里的编号是 1 基的（`_1` 起），与美术资产命名保持一致。
        for (side, segments) in [("L", &self.left), ("R", &self.right)] {
            for (index, entity) in segments.iter().enumerate() {
                if entity.is_none() {
                    missing.push(format!("{prefix}_{side}_{}", index + 1));
                }
            }
        }

        missing
    }

    // 迭代所有非空的分段实体（左右合并，跳过 None）。
    // 返回 `impl Iterator<Item = Entity> + '_`：不透明迭代器，借用 self 存活。
    fn segment_entities(&self) -> impl Iterator<Item = Entity> + '_ {
        self.left
            .iter()
            .chain(self.right.iter())
            .filter_map(|entity| *entity)
    }

    // 把同一个材质应用到整组：有分段就逐段上色，否则退到整组节点。
    fn assign_all(
        &self,
        handle: Handle<StandardMaterial>,
        children: &Query<&Children>,
        mesh_materials: &mut Query<&mut MeshMaterial3d<StandardMaterial>>,
    ) {
        if self.has_segments() {
            for entity in self.segment_entities() {
                // 每段克隆一次句柄（句柄克隆很廉价）。
                assign_material(entity, handle.clone(), children, mesh_materials);
            }
        } else if let Some(entity) = self.whole {
            assign_material(entity, handle, children, mesh_materials);
        }
    }

    // 跑马灯上色：先整组涂"灭"，再把当前活跃段涂"亮"。
    fn assign_flow(
        &self,
        team: Team,
        color: LightColor,
        elapsed_secs: f64,
        handles: &TechCoreMaterialHandles,
        children: &Query<&Children>,
        mesh_materials: &mut Query<&mut MeshMaterial3d<StandardMaterial>>,
    ) {
        // 无分段：无法做流动，退化为整组常亮。
        if !self.has_segments() {
            self.assign_all(handles.resolve_color(team, color), children, mesh_materials);
            return;
        }

        // 先把整组涂灭，再点亮活跃段——这样每帧只改动少量段。
        self.assign_all(handles.off.clone(), children, mesh_materials);
        let active_handle = handles.resolve_color(team, color);

        match flow_activation(elapsed_secs) {
            FlowActivation::Segment(index) => {
                // 该段左右两侧一起点亮。
                for entity in [self.left[index], self.right[index]].into_iter().flatten() {
                    assign_material(entity, active_handle.clone(), children, mesh_materials);
                }
            }
            FlowActivation::All => {
                // 全亮：整组涂亮色。
                self.assign_all(active_handle, children, mesh_materials);
            }
        }
    }

    // 点亮"某一段"的左右两侧（用于第五步组装引导）。
    fn assign_segment_pair(
        &self,
        segment: TechCoreFirstLightSegment,
        handle: Handle<StandardMaterial>,
        children: &Query<&Children>,
        mesh_materials: &mut Query<&mut MeshMaterial3d<StandardMaterial>>,
    ) {
        let index = segment.index();
        for entity in [self.left[index], self.right[index]].into_iter().flatten() {
            assign_material(entity, handle.clone(), children, mesh_materials);
        }
    }

    // 第五步组装引导上色（目标段 + 能量单元段）。
    fn assign_assembly(
        &self,
        team: Team,
        assembly: AssemblyLightProgram,
        step5_lights: TechCoreStep5Lights,
        handles: &TechCoreMaterialHandles,
        children: &Query<&Children>,
        mesh_materials: &mut Query<&mut MeshMaterial3d<StandardMaterial>>,
    ) {
        // 无分段：退化为"整组用单个颜色"（进行中用阵营色，完成用绿）。
        if !self.has_segments() {
            let fallback_color = match assembly {
                AssemblyLightProgram::InProgress => LightColor::Team,
                AssemblyLightProgram::Completed => LightColor::Green,
            };
            self.assign_all(
                handles.resolve_color(team, fallback_color),
                children,
                mesh_materials,
            );
            return;
        }

        // 先整组涂灭，再点亮目标/能量单元段。
        self.assign_all(handles.off.clone(), children, mesh_materials);

        match assembly {
            AssemblyLightProgram::InProgress => {
                let target = step5_lights.target();
                let energy_unit = step5_lights.energy_unit();

                // 能量单元段用白色；若与目标段重合则跳过（避免覆盖）。
                if energy_unit != target {
                    self.assign_segment_pair(
                        energy_unit,
                        handles.resolve_color(team, LightColor::White),
                        children,
                        mesh_materials,
                    );
                }

                // 目标段用阵营色；放在后面涂，保证与能量单元段重合时目标色优先。
                self.assign_segment_pair(
                    target,
                    handles.resolve_color(team, LightColor::Team),
                    children,
                    mesh_materials,
                );
            }
            AssemblyLightProgram::Completed => {
                // 完成后目标段变绿。
                self.assign_segment_pair(
                    step5_lights.target(),
                    handles.resolve_color(team, LightColor::Green),
                    children,
                    mesh_materials,
                );
            }
        }
    }

    // 按灯光程序分派上色：Flow/Assembly 走专用路径，其余（Off/Solid/Blink）统一解析成单个句柄后 assign_all。
    fn assign_program(
        &self,
        team: Team,
        program: LightProgram,
        elapsed_secs: f64,
        step5_lights: TechCoreStep5Lights,
        handles: &TechCoreMaterialHandles,
        children: &Query<&Children>,
        mesh_materials: &mut Query<&mut MeshMaterial3d<StandardMaterial>>,
    ) {
        match program {
            LightProgram::Flow { color } => {
                self.assign_flow(team, color, elapsed_secs, handles, children, mesh_materials);
            }
            LightProgram::Assembly(assembly) => {
                self.assign_assembly(
                    team,
                    assembly,
                    step5_lights,
                    handles,
                    children,
                    mesh_materials,
                );
            }
            // `_` 兜底：Off/Solid/Blink 逻辑一致——先 resolve 出"此刻该用的句柄"（可能是灭）再整组涂。
            _ => {
                let handle = handles.resolve(team, program, elapsed_secs);
                self.assign_all(handle, children, mesh_materials);
            }
        }
    }
}

/// 一个阵营全部三组灯对应的实体打包：`first`（含分段的集合）+ `second`/`third`（单个节点）。
/// `team` 存下来，方便上色时解析 `Team` 逻辑色（红方红、蓝方蓝）。
#[derive(Debug, Clone, Copy)]
struct TeamCoreLights {
    team: Team,
    first: FirstLightSet,
    second: Entity,
    third: Entity,
}

impl TeamCoreLights {
    // 直接字段构造。
    fn new(team: Team, first: FirstLightSet, second: Entity, third: Entity) -> Self {
        Self {
            team,
            first,
            second,
            third,
        }
    }
}

/// 挂在核心根实体上的**运行时组件**：保存当前阶段、阶段计时起点、step5 段位置，以及红蓝两套灯实体。
/// 字段说明：
/// - `phase`：当前状态机阶段（外部可 `set_phase` 驱动迁移）；
/// - `last_rendered_phase`：上一次渲染时的阶段——用于检测"阶段刚切换"，从而重置计时；
/// - `phase_started_at_secs`：本阶段开始时的全局时间（秒），闪烁/流光相位据此计算；
/// - `step5_lights`：第五步要引导的两段；
/// - `red`/`blue`：红蓝各自的灯实体集合。
#[derive(Component, Debug)]
pub struct TechCore {
    phase: TechCorePhase,
    last_rendered_phase: TechCorePhase,
    phase_started_at_secs: f64,
    step5_lights: TechCoreStep5Lights,
    red: TeamCoreLights,
    blue: TeamCoreLights,
}

impl TechCore {
    // 初始状态：空闲阶段、计时从 0 起、step5 用默认段。
    fn new(red: TeamCoreLights, blue: TeamCoreLights) -> Self {
        Self {
            phase: TechCorePhase::MatchRunningIdle,
            last_rendered_phase: TechCorePhase::MatchRunningIdle,
            phase_started_at_secs: 0.0,
            step5_lights: TechCoreStep5Lights::default(),
            red,
            blue,
        }
    }

    // 读当前阶段。
    pub fn phase(&self) -> TechCorePhase {
        self.phase
    }

    // 设置阶段（外部驱动状态迁移的入口）。
    pub fn set_phase(&mut self, phase: TechCorePhase) {
        self.phase = phase;
    }

    // 读 step5 段位置。
    pub const fn step5_lights(&self) -> TechCoreStep5Lights {
        self.step5_lights
    }

    // 整体设置 step5 段位置。
    pub fn set_step5_lights(&mut self, step5_lights: TechCoreStep5Lights) {
        self.step5_lights = step5_lights;
    }

    // 按段号设置"目标段"。
    pub fn set_step5_target_segment(&mut self, segment: TechCoreFirstLightSegment) {
        self.step5_lights.target = segment;
    }

    // 按段号设置"能量单元段"。
    pub fn set_step5_energy_unit_segment(&mut self, segment: TechCoreFirstLightSegment) {
        self.step5_lights.energy_unit = segment;
    }

    // 用弧度设置目标段（内部换算成段号）。
    pub fn set_step5_target_angle_radians(&mut self, radians: f64) {
        self.set_step5_target_segment(TechCoreFirstLightSegment::from_angle_radians(radians));
    }

    // 用弧度设置能量单元段。
    pub fn set_step5_energy_unit_angle_radians(&mut self, radians: f64) {
        self.set_step5_energy_unit_segment(TechCoreFirstLightSegment::from_angle_radians(radians));
    }

    // 用**度**设置目标段。
    pub fn set_step5_target_angle_degrees(&mut self, degrees: f64) {
        self.set_step5_target_segment(TechCoreFirstLightSegment::from_angle_degrees(degrees));
    }

    // 用**度**设置能量单元段。
    pub fn set_step5_energy_unit_angle_degrees(&mut self, degrees: f64) {
        self.set_step5_energy_unit_segment(TechCoreFirstLightSegment::from_angle_degrees(degrees));
    }

    // 调试：把阶段推进到下一个（Shift+C 调用链终点）。
    pub fn advance_debug(&mut self) {
        self.phase = self.phase.next_debug();
    }

    // 求"当前阶段已持续的时间"（秒）：阶段未变才返回差值，否则返回 0（刚切换，相位从零开始）。
    fn phase_elapsed_secs(&self, elapsed_secs: f64) -> f64 {
        if self.phase == self.last_rendered_phase {
            (elapsed_secs - self.phase_started_at_secs).max(0.0)
        } else {
            0.0
        }
    }

    // 渲染用计时：发现阶段切换就更新 last_rendered_phase 并把起点设为当前时间，再返回阶段已过时间。
    // 调用时机：`update_tech_core_lights` 每帧开头，保证闪烁/流光相位正确。
    fn render_elapsed_secs(&mut self, elapsed_secs: f64) -> f64 {
        if self.phase != self.last_rendered_phase {
            self.last_rendered_phase = self.phase;
            self.phase_started_at_secs = elapsed_secs;
        }

        self.phase_elapsed_secs(elapsed_secs)
    }

    // 返回红蓝两套灯，方便统一遍历上色。
    fn teams(&self) -> [TeamCoreLights; 2] {
        [self.red, self.blue]
    }
}

/// 科技核心用到的**五种材质句柄**：灭、白、红、蓝、绿。
/// 在系统首次运行时懒创建一次（见 `update_tech_core_lights` 的 `Local`），之后所有上色都只是换这些句柄。
/// `#[derive(Clone)]`：传参时常按值克隆（句柄克隆廉价）。
#[derive(Clone)]
struct TechCoreMaterialHandles {
    off: Handle<StandardMaterial>,
    white: Handle<StandardMaterial>,
    red: Handle<StandardMaterial>,
    blue: Handle<StandardMaterial>,
    green: Handle<StandardMaterial>,
}

impl TechCoreMaterialHandles {
    /// 创建五个 `StandardMaterial` 并纳入材质库。
    /// 每个材质的发光强度硬编码在内：off=0（不发光）、white=1.5、红/蓝/绿=1.8（灯更亮）。
    fn new(materials: &mut Assets<StandardMaterial>) -> Self {
        Self {
            off: materials.add(material(0.02, 0.02, 0.02, 0.0)),
            white: materials.add(material(1.0, 1.0, 1.0, 1.5)),
            red: materials.add(material(1.0, 0.0, 0.0, 1.8)),
            blue: materials.add(material(0.0, 0.12, 1.0, 1.8)),
            green: materials.add(material(0.0, 1.0, 0.18, 1.8)),
        }
    }

    // 按"团队 + 程序 + 瞬时相位"解析出该用的句柄：程序此刻无颜色 → off，否则再按逻辑色解析。
    fn resolve(
        &self,
        team: Team,
        program: LightProgram,
        elapsed_secs: f64,
    ) -> Handle<StandardMaterial> {
        let Some(color) = program.active_color(elapsed_secs) else {
            return self.off.clone();
        };

        self.resolve_color(team, color)
    }

    // 把逻辑色解析成具体句柄：白/绿固定；`Team` 按阵营取红或蓝。
    fn resolve_color(&self, team: Team, color: LightColor) -> Handle<StandardMaterial> {
        match color {
            LightColor::White => self.white.clone(),
            LightColor::Green => self.green.clone(),
            LightColor::Team => match team {
                Team::Red => self.red.clone(),
                Team::Blue => self.blue.clone(),
            },
        }
    }
}

/// 构造"自发光"材质的工具：`base_color` 是基础色，`emissive` 由色值 × `emissive_strength` 得到。
/// 入参 `red/green/blue`：线性 sRGB 色分量；`emissive_strength`：自发光强度（0 = 不发光，越大越亮）。
/// `emissive_exposure_weight = -1.0`：让发光不随相机曝光调整（保证灯色稳定）。
fn material(red: f32, green: f32, blue: f32, emissive_strength: f32) -> StandardMaterial {
    StandardMaterial {
        base_color: Color::srgb(red, green, blue),
        emissive: LinearRgba::new(
            red * emissive_strength,
            green * emissive_strength,
            blue * emissive_strength,
            1.0,
        ),
        emissive_exposure_weight: -1.0,
        ..Default::default()
    }
}

/// 按命名契约从名字表里捞出"第一组灯"的全部实体：整组节点 + 左右各 18 段。
/// 命名规则：整组 = `prefix`；单段 = `{prefix}_L_{1..18}` / `{prefix}_R_{1..18}`（见 consts 契约）。
/// 缺失的项留 `None`，不去 panic——真实资源可能没有分段或没有整组节点。
fn find_first_light_set(name_map: &HashMap<String, Entity>, prefix: &str) -> FirstLightSet {
    let mut left = [None; FIRST_LIGHT_SEGMENT_COUNT];
    let mut right = [None; FIRST_LIGHT_SEGMENT_COUNT];

    // 遍历 18 段，逐段查名字（1 基编号拼进名字）。
    for index in 0..FIRST_LIGHT_SEGMENT_COUNT {
        left[index] = name_map.get(&format!("{prefix}_L_{}", index + 1)).copied();
        right[index] = name_map.get(&format!("{prefix}_R_{}", index + 1)).copied();
    }

    FirstLightSet::new(name_map.get(prefix).copied(), left, right)
}

/// 由"三组灯名字"找齐一个阵营的全部灯：第一组（分段）用 `find_first_light_set`，
/// 第二/三组是单节点，直接查名字；缺失则 `warn!` 并返回 `None`（调用方据此放弃绑定）。
/// `let [first_name, second_name, third_name] = names;`：数组解构，一次拆成三个变量。
fn find_team_lights(
    name_map: &HashMap<String, Entity>,
    team: Team,
    names: [&str; 3],
) -> Option<TeamCoreLights> {
    let [first_name, second_name, third_name] = names;
    let first = find_first_light_set(name_map, first_name);
    // `let Some(...) = ... else { return None };`：查不到就直接返回 None（提前退出）。
    let Some(second) = name_map.get(second_name).copied() else {
        warn!("TECH_CORE.glb is missing {second_name}");
        return None;
    };
    let Some(third) = name_map.get(third_name).copied() else {
        warn!("TECH_CORE.glb is missing {third_name}");
        return None;
    };

    Some(TeamCoreLights::new(team, first, second, third))
}

/// 资源体检：第一组灯分段不完整时打印告警（缺失整组节点、或缺若干段）。
/// 只警告、不阻断——灯少了也能跑，只是效果不全。
fn warn_incomplete_first_light_set(prefix: &str, lights: &FirstLightSet) {
    // 完全没有任何分段：再看有没有整组兜底节点，都没有才告警。
    if !lights.has_segments() {
        if lights.whole.is_none() {
            warn!(
                "TECH_CORE.glb is missing {prefix} and segmented {prefix}_{{L,R}}_1..{FIRST_LIGHT_SEGMENT_COUNT}"
            );
        }
        return;
    }

    let missing = lights.missing_segments(prefix);
    if missing.is_empty() {
        return;
    }

    // 报告里最多列 6 个缺失段名，其余用 "(+N more)" 概括，避免刷屏。
    let preview = missing
        .iter()
        .take(6)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = if missing.len() > 6 {
        format!(" (+{} more)", missing.len() - 6)
    } else {
        String::new()
    };

    warn!("TECH_CORE.glb has incomplete {prefix} segments; missing {preview}{suffix}");
}

/// 场景加载完成后的一次性初始化：按名字认领红蓝灯实体，并把 `TechCore` 组件挂到核心根实体上。
/// 入参 `In((root, instance))`：`setup_tech_core` 由场景加载流程以"带输入的系统"调用，
/// `root` 是核心根实体、`instance` 是这次加载的场景实例 id（用于枚举它生成的所有实体）。
/// `pub(crate)`：仅本 crate 可见——仅供场景加载序列调用，不对外。
pub(crate) fn setup_tech_core(
    In((root, instance)): In<(Entity, InstanceId)>,
    mut commands: Commands,
    scene_spawner: Res<WorldInstanceSpawner>,
    names: Query<&Name>,
) {
    // 构造"节点名 → 实体"的索引表：遍历该实例生成的所有实体，取其 Name 建表。
    // `filter_map`：没有 Name 的实体直接跳过；`.collect::<HashMap<_, _>>()` 收成哈希表。
    let name_map = scene_spawner
        .iter_instance_entities(instance)
        .filter_map(|entity| {
            names
                .get(entity)
                .map(|name| (name.to_string(), entity))
                .ok()
        })
        .collect::<HashMap<_, _>>();

    // 红蓝任一方灯找不全就放弃（不挂组件，保持场景原样）。
    let Some(red) = find_team_lights(&name_map, Team::Red, RED_LIGHT_NAMES) else {
        return;
    };
    let Some(blue) = find_team_lights(&name_map, Team::Blue, BLUE_LIGHT_NAMES) else {
        return;
    };

    // 资源体检（缺段/缺组时告警）。
    warn_incomplete_first_light_set(RED_LIGHT_NAMES[0], &red.first);
    warn_incomplete_first_light_set(BLUE_LIGHT_NAMES[0], &blue.first);

    // 把运行态组件挂到核心根实体——之后 `update_tech_core_lights` 就有据可依了。
    commands.entity(root).insert(TechCore::new(red, blue));
    info!("Tech core lights bound");
}

/// 给某实体**及其所有后代**统一换材质句柄——灯常常是父节点下的一堆网格子节点。
/// 实体自身没有 MeshMaterial3d 也照样往下遍历子孙。
fn assign_material(
    root: Entity,
    handle: Handle<StandardMaterial>,
    children: &Query<&Children>,
    mesh_materials: &mut Query<&mut MeshMaterial3d<StandardMaterial>>,
) {
    if let Ok(mut mesh_material) = mesh_materials.get_mut(root) {
        mesh_material.0 = handle.clone();
    }

    // `iter_descendants`：深度优先遍历整棵子树。
    for child in children.iter_descendants(root) {
        if let Ok(mut mesh_material) = mesh_materials.get_mut(child) {
            mesh_material.0 = handle.clone();
        }
    }
}

/// 每帧主循环：读当前时间，把每个核心的三组灯按其所处阶段刷成对应材质。
/// 参数里几个新手点：
/// - `Local<Option<TechCoreMaterialHandles>>`：`Local` 是"系统私有、跨帧持久"的存储（不是全局资源），
///   这里用 `Option` 让它第一次运行才创建五个材质（懒初始化）；
/// - `Query<&mut TechCore>`：拿到所有核心组件并可写（因为要更新阶段计时）。
fn update_tech_core_lights(
    time: Res<Time>,
    mut handles: Local<Option<TechCoreMaterialHandles>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    children: Query<&Children>,
    mut mesh_materials: Query<&mut MeshMaterial3d<StandardMaterial>>,
    mut cores: Query<&mut TechCore>,
) {
    // `get_or_insert_with`：若 Local 还是 None 就用闭包创建（首次运行时建材质）；否则复用。
    let handles = handles.get_or_insert_with(|| TechCoreMaterialHandles::new(&mut materials));
    // `elapsed_secs_f64`：应用启动至今的秒数（f64 精度），作为统一时间基准。
    let elapsed_secs = time.elapsed_secs_f64();

    for mut core in &mut cores {
        // 先算本核心"当前阶段已过多久"（并顺带在阶段切换时复位计时）。
        let phase_elapsed_secs = core.render_elapsed_secs(elapsed_secs);
        // `programs()` 是纯映射表，一次算出三组灯要用的程序。
        let programs = core.phase.programs();
        let step5_lights = core.step5_lights();
        for team in core.teams() {
            for group in TechCoreLightGroup::ALL {
                let program = programs[group.index()];
                match group {
                    // 第一组：可能分段/跑马灯/组装引导，交给 FirstLightSet 处理。
                    TechCoreLightGroup::First => team.first.assign_program(
                        team.team,
                        program,
                        phase_elapsed_secs,
                        step5_lights,
                        handles,
                        &children,
                        &mut mesh_materials,
                    ),
                    // 第二/三组：单节点，直接把程序解析成句柄后整棵子树涂色。
                    TechCoreLightGroup::Second => {
                        let handle = handles.resolve(team.team, program, phase_elapsed_secs);
                        assign_material(team.second, handle, &children, &mut mesh_materials);
                    }
                    TechCoreLightGroup::Third => {
                        let handle = handles.resolve(team.team, program, phase_elapsed_secs);
                        assign_material(team.third, handle, &children, &mut mesh_materials);
                    }
                }
            }
        }
    }
}

/// 调试：按住 Shift 再按 C，把阶段推进到下一个并打印日志。
/// `pressed`（持续按住）与 `just_pressed`（本帧刚按下）配合，保证一次只推进一格。
fn debug_cycle_tech_core_phase(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut cores: Query<&mut TechCore>,
) {
    if !(keyboard.pressed(KeyCode::ShiftLeft) || keyboard.pressed(KeyCode::ShiftRight))
        || !keyboard.just_pressed(KeyCode::KeyC)
    {
        return;
    }

    for mut core in &mut cores {
        core.advance_debug();
        info!("Tech core phase: {:?}", core.phase());
    }
}

/// 本模块插件：把两个 Update 系统登记进调度表。
/// `pub(super)`：仅对父模块（tech_core）可见，由 `tech_core::prelude::TechCorePlugins` 安装。
#[derive(Default)]
pub(super) struct TechCorePlugin;

impl Plugin for TechCorePlugin {
    fn build(&self, app: &mut App) {
        // `setup_tech_core` is invoked by the scene load sequence, not by an event.
        // 译注：`setup_tech_core` 由场景加载流程调用，而**不是**作为事件/常规系统在此注册（故这里不含它）。
        app.add_systems(
            Update,
            (debug_cycle_tech_core_phase, update_tech_core_lights),
        );
    }
}

// 单元测试模块（`cargo test` 时编译运行）：覆盖阶段→程序映射、调试序遍历、角度→段号、
// 流光三段式节奏、以及各处 JSON 导出字段——这些是"对外契约"，回归价值高。
#[cfg(test)]
mod tests {
    use super::*;

    // 校验九个阶段的 programs() 与设计规格逐一对齐（防止误改映射表）。
    #[test]
    fn tech_core_phase_programs_match_spec() {
        use AssemblyLightProgram::{Completed, InProgress};
        use BlinkRate::{Hz1, Hz3};
        use LightColor::{Team, White};
        use LightProgram::{Assembly, Blink, Flow, Off, Solid};

        assert_eq!(
            TechCorePhase::MatchRunningIdle.programs(),
            [Off, Solid(Team), Solid(Team)]
        );
        assert_eq!(
            TechCorePhase::DifficultySelectedArmNotReady.programs(),
            [Flow { color: White }, Solid(Team), Solid(Team)]
        );
        assert_eq!(
            TechCorePhase::DifficultySelectedArmReady.programs(),
            [
                Blink {
                    color: White,
                    rate: Hz1
                },
                Blink {
                    color: Team,
                    rate: Hz1
                },
                Blink {
                    color: Team,
                    rate: Hz1
                },
            ]
        );
        assert_eq!(
            TechCorePhase::Step2Completed.programs(),
            [
                Blink {
                    color: White,
                    rate: Hz1
                },
                Blink {
                    color: Team,
                    rate: Hz1
                },
                Blink {
                    color: Team,
                    rate: Hz3
                },
            ]
        );
        assert_eq!(
            TechCorePhase::Step3Completed.programs(),
            [
                Blink {
                    color: White,
                    rate: Hz1
                },
                Blink {
                    color: Team,
                    rate: Hz3
                },
                Blink {
                    color: Team,
                    rate: Hz3
                },
            ]
        );
        assert_eq!(
            TechCorePhase::Step4Completed.programs(),
            [
                Blink {
                    color: White,
                    rate: Hz1
                },
                Solid(Team),
                Solid(Team),
            ]
        );
        assert_eq!(
            TechCorePhase::Step5InProgress.programs(),
            [Assembly(InProgress), Solid(Team), Solid(Team)]
        );
        assert_eq!(
            TechCorePhase::Step5Completed.programs(),
            [Assembly(Completed), Solid(Team), Solid(Team)]
        );
        assert_eq!(
            TechCorePhase::ConfirmedRecovering.programs(),
            [
                Blink {
                    color: White,
                    rate: Hz3
                },
                Blink {
                    color: Team,
                    rate: Hz3
                },
                Blink {
                    color: Team,
                    rate: Hz3
                },
            ]
        );
    }

    #[test]
    fn tech_core_debug_sequence_wraps() {
        let mut phase = TechCorePhase::MatchRunningIdle;
        for expected in TechCorePhase::DEBUG_SEQUENCE.into_iter().skip(1) {
            phase = phase.next_debug();
            assert_eq!(phase, expected);
        }

        assert_eq!(phase.next_debug(), TechCorePhase::MatchRunningIdle);
    }

    #[test]
    fn tech_core_phase_ids_are_stable() {
        let ids = TechCorePhase::DEBUG_SEQUENCE.map(TechCorePhase::id);
        assert_eq!(ids, [0, 1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn light_program_resolves_blink_active_color() {
        let program = LightProgram::Blink {
            color: LightColor::White,
            rate: BlinkRate::Hz1,
        };

        assert_eq!(program.active_color(0.25), Some(LightColor::White));
        assert_eq!(program.active_color(0.75), None);
    }

    // 角度→段号换算：0°→第1段、20°→第2段、359.9°→末段、-1°回绕到末段。
    #[test]
    fn tech_core_segment_maps_angles_to_first_light_indices() {
        assert_eq!(
            TechCoreFirstLightSegment::from_angle_degrees(0.0).number(),
            1
        );
        assert_eq!(
            TechCoreFirstLightSegment::from_angle_degrees(20.0).number(),
            2
        );
        assert_eq!(
            TechCoreFirstLightSegment::from_angle_degrees(359.9).number(),
            FIRST_LIGHT_SEGMENT_COUNT
        );
        assert_eq!(
            TechCoreFirstLightSegment::from_angle_degrees(-1.0).number(),
            FIRST_LIGHT_SEGMENT_COUNT
        );
    }

    // 流光节奏：前 18 步正向、后 18 步反向（折返），满 36 步转全亮。
    #[test]
    fn tech_core_flow_activation_runs_forward_back_then_all() {
        assert_eq!(flow_activation(0.0), FlowActivation::Segment(0));
        assert_eq!(
            flow_activation((FIRST_LIGHT_SEGMENT_COUNT as f64 - 1.0) / FLOW_SEGMENT_HZ),
            FlowActivation::Segment(FIRST_LIGHT_SEGMENT_COUNT - 1)
        );
        assert_eq!(
            flow_activation(FIRST_LIGHT_SEGMENT_COUNT as f64 / FLOW_SEGMENT_HZ),
            FlowActivation::Segment(FIRST_LIGHT_SEGMENT_COUNT - 1)
        );
        assert_eq!(
            flow_activation((FIRST_LIGHT_SEGMENT_COUNT as f64 * 2.0 - 1.0) / FLOW_SEGMENT_HZ),
            FlowActivation::Segment(0)
        );
        assert_eq!(
            flow_activation(FIRST_LIGHT_SEGMENT_COUNT as f64 * 2.0 / FLOW_SEGMENT_HZ),
            FlowActivation::All
        );
    }

    #[test]
    fn tech_core_state_json_contains_flow_segments() {
        let value: Value = serde_json::from_str(&tech_core_state_json_from_phases(
            0,
            0,
            0.0,
            [TechCorePhase::DifficultySelectedArmNotReady],
        ))
        .unwrap();
        let red_first = &value["cores"][0]["lights"][0];

        assert_eq!(red_first["program"]["mode"], "flow");
        assert_eq!(red_first["program"]["segment_hz"], FLOW_SEGMENT_HZ);
        assert_eq!(red_first["active_segments"][0]["side"], "left");
        assert_eq!(red_first["active_segments"][0]["index"], 1);
        assert_eq!(red_first["active_segments"][1]["side"], "right");
        assert_eq!(red_first["active_segments"][1]["index"], 1);

        let value: Value = serde_json::from_str(&tech_core_state_json_from_phases(
            0,
            0,
            FIRST_LIGHT_SEGMENT_COUNT as f64 * 2.0 / FLOW_SEGMENT_HZ,
            [TechCorePhase::DifficultySelectedArmNotReady],
        ))
        .unwrap();

        assert_eq!(
            value["cores"][0]["lights"][0]["active_segments"]
                .as_array()
                .unwrap()
                .len(),
            FIRST_LIGHT_SEGMENT_COUNT * 2
        );
    }

    #[test]
    fn tech_core_state_json_contains_resolved_light_state() {
        let value: Value = serde_json::from_str(&tech_core_state_json_from_phases(
            12,
            34,
            0.0,
            [TechCorePhase::Step5Completed],
        ))
        .unwrap();

        assert_eq!(value["stamp"]["sec"], 12);
        assert_eq!(value["stamp"]["nanosec"], 34);
        assert_eq!(value["cores"][0]["phase"]["id"], 7);
        assert_eq!(value["cores"][0]["phase"]["name"], "step5_completed");
        assert_eq!(value["cores"][0]["lights"][0]["team"], "red");
        assert_eq!(value["cores"][0]["lights"][0]["group"], 1);
        assert_eq!(
            value["cores"][0]["lights"][0]["program"]["mode"],
            "step5_completed"
        );
        assert_eq!(
            value["cores"][0]["lights"][0]["program"]["target_color"],
            "green"
        );
        assert_eq!(value["cores"][0]["lights"][0]["active_color"], "green");
        assert_eq!(
            value["cores"][0]["lights"][0]["active_segments"][0]["role"],
            "target"
        );
        assert_eq!(
            value["cores"][0]["lights"][0]["active_segments"][0]["color"],
            "green"
        );
        assert_eq!(value["cores"][0]["lights"][3]["team"], "blue");
        assert_eq!(
            value["cores"][0]["lights"][3]["program"]["target_color"],
            "green"
        );
    }

    #[test]
    fn tech_core_state_json_contains_step5_in_progress_segments() {
        let value: Value = serde_json::from_str(&tech_core_state_json_from_phases(
            0,
            0,
            0.0,
            [TechCorePhase::Step5InProgress],
        ))
        .unwrap();
        let red_first = &value["cores"][0]["lights"][0];

        assert_eq!(red_first["program"]["mode"], "step5_in_progress");
        assert_eq!(red_first["program"]["target_color"], "red");
        assert_eq!(red_first["program"]["energy_unit_color"], "white");
        assert_eq!(red_first["active_color"], "mixed");
        assert_eq!(red_first["active_segments"][0]["role"], "target");
        assert_eq!(red_first["active_segments"][0]["color"], "red");
        assert_eq!(red_first["active_segments"][2]["role"], "energy_unit");
        assert_eq!(red_first["active_segments"][2]["color"], "white");
    }

    #[test]
    fn tech_core_state_json_marks_blink_off_half() {
        let value: Value = serde_json::from_str(&tech_core_state_json_from_phases(
            0,
            0,
            0.75,
            [TechCorePhase::DifficultySelectedArmReady],
        ))
        .unwrap();

        assert_eq!(value["cores"][0]["lights"][0]["program"]["mode"], "blink");
        assert_eq!(value["cores"][0]["lights"][0]["program"]["hz"], 1.0);
        assert_eq!(value["cores"][0]["lights"][0]["active_color"], "off");
    }
}
