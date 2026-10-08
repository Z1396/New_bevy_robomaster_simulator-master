//! 能量机关的激活状态机（本组最核心的文件）：把"子弹打中靶位"和"时间流逝"翻译成
//! 机关的激活 / 失败 / 复位，并向外输出每个靶位该显示成什么样子。
//!
//! 顶层状态 `MechanismState` 有 4 个：
//! - `Inactive`：未激活，倒计时 `INACTIVE_WAIT` 秒后自动进入激活流程；
//! - `Activating`：正在激活，内部又分小机关单阶段 / 大机关"主靶-副靶"两阶段（见 `ActivationRound`）；
//! - `Activated`：已激活，保持 `ACTIVATED_HOLD` 秒后回到未激活；
//! - `Failed`：激活失败，停留 `FAILURE_RECOVER` 秒后回到未激活。
//!
//! 两条驱动路径：
//! - `tick(delta_secs, rng)`：每帧按时间推进（由 rune.rs 调用），返回粗粒度 `RuneTransition`；
//! - `hit(target, rng)`：每次命中时调用（由 collision.rs 调用），返回命中结果 `RuneHitOutcome`。
//!
//! 计时约定：所有 `remaining` 字段都是"剩余秒数"；`expire_after` 负责递减并在归零时报告到期；
//! `tick_two_timers` 同时推进"全局超时"与"当前阶段超时"两个计时器，谁先到就触发谁。
//! 规则概要——小机关：随机点亮 1 个靶，打中即点亮下一个，5 个全亮即激活；
//! 大机关：随机点亮 2 个主靶，打中 1 个后在 1 秒内再打中另一个（副靶），
//! 副靶打不中不算失败、只是进入下一轮；累计打中 5 个主靶即激活。

use crate::robomaster::power_rune::common::{
    RUNE_TARGET_COUNT, RuneHitOutcome, RuneMode, RuneTransition,
};
use crate::robomaster::power_rune::consts::{
    ACTIVATED_HOLD, ACTIVATION_GLOBAL_TIMEOUT, ACTIVATION_PRIMARY_TIMEOUT, FAILURE_RECOVER,
    FUNNY_IGNORE_WRONG_TARGET_FAILURE, INACTIVE_WAIT, LARGE_SECONDARY_TIMEOUT,
};
use crate::robomaster::visibility::Activation;
use rand::Rng;
use rand::prelude::SliceRandom;

/// 5 个靶位的显示状态数组，长度由 `RUNE_TARGET_COUNT` 决定。
pub type RuneTargetStates = [Activation; RUNE_TARGET_COUNT];

/// 顶层状态机。每个变体都携带 `mode`（避免状态转移时丢失规格）：
/// - `Inactive` / `Activated` / `Failed` 只带一个 `remaining` 剩余计时（秒）；
/// - `Activating` 带一个 `ActivationRun`（一次激活流程的全部内部数据）。
/// `Clone, PartialEq` 便于复制与测试断言。
#[derive(Debug, Clone, PartialEq)]
pub enum MechanismState {
    Inactive { mode: RuneMode, remaining: f32 }, // 未激活，remaining=INACTIVE_WAIT 倒计时（秒）
    Activating(ActivationRun), // 正在激活（内部状态见 ActivationRun）
    Activated { mode: RuneMode, remaining: f32 }, // 已激活，remaining=保持时长倒计时（秒）
    Failed { mode: RuneMode, remaining: f32 }, // 失败，remaining=恢复时长倒计时（秒）
}

/// 一次激活流程的全部内部数据（只有处于 Activating 时才存在）。
#[derive(Debug, Clone, PartialEq)]
pub struct ActivationRun {
    global_remaining: f32,     // 全局激活超时倒计时（秒），从 ACTIVATION_GLOBAL_TIMEOUT 起递减
    targets: RuneTargetStates, // 5 个靶位当前的显示状态
    round: ActivationRound,    // 当前轮次：小机关 / 大机关
}

/// 当前轮次类型：小机关单阶段，或大机关的两阶段流程。
#[derive(Debug, Clone, PartialEq)]
enum ActivationRound {
    Small(SmallRound),
    Large(LargeRun),
}

/// 小机关的一轮：只有"主靶"超时——务必在 `primary_remaining` 秒内打中当前点亮的目标。
#[derive(Debug, Clone, PartialEq)]
struct SmallRound {
    primary_remaining: f32, // 主靶超时倒计时（秒），从 ACTIVATION_PRIMARY_TIMEOUT 起
}

/// 大机关的整段激活流程：`completed_groups` 记录已打中的主靶组数（满 5 组即激活），
/// `phase` 表示当前处于主靶阶段还是副靶阶段。
#[derive(Debug, Clone, PartialEq)]
struct LargeRun {
    completed_groups: usize, // 已成功完成的主靶组数（0..=5）
    phase: LargePhase,       // 当前阶段
}

/// 大机关的两个阶段。
#[derive(Debug, Clone, PartialEq)]
enum LargePhase {
    // 主靶阶段：有 2 个主靶点亮，需在 primary_remaining 秒内打中其中一个。
    Primary {
        primary_remaining: f32, // 主靶超时倒计时（秒）
    },
    // 副靶阶段：打中主靶后进入，需在 secondary_remaining 秒内打中"另一个"靶。
    Secondary {
        secondary_remaining: f32, // 副靶超时倒计时（秒）
        target: Option<usize>,     // 副靶的靶位下标（None 表示没有可打的副靶）
    },
}

/// `ActivationRun` 内部 tick 的结果——比对外 `RuneTransition` 更细粒度，只在 state.rs 内用。
/// `MechanismState::tick` 负责把它翻译成 `RuneTransition` 并更新顶层状态。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum RunTransition {
    None,            // 无变化
    Advanced,        // 流程推进（进入下一轮 / 下一阶段）
    Failed,          // 本轮超时失败
    Activated,       // 全流程完成，机关激活
    ResetToInactive, // 全局超时，直接回到未激活
}

impl MechanismState {
    /// 构造"未激活"，倒计时为 INACTIVE_WAIT（秒）。
    pub fn inactive(mode: RuneMode) -> Self {
        Self::Inactive {
            mode,
            remaining: INACTIVE_WAIT,
        }
    }

    /// 构造"正在激活"，交给 `ActivationRun::new` 初始化一轮激活。
    pub fn start(mode: RuneMode, rng: &mut impl Rng) -> Self {
        Self::Activating(ActivationRun::new(mode, rng))
    }

    /// 取当前规格（小/大）。无论处于哪个状态都要能拿到，所以用 match 覆盖所有变体。
    pub fn mode(&self) -> RuneMode {
        match self {
            Self::Inactive { mode, .. }
            | Self::Activated { mode, .. }
            | Self::Failed { mode, .. } => *mode, // `..` 忽略其余字段；解引用取出 mode
            Self::Activating(run) => run.mode(), // 激活态从内部 run 取
        }
    }

    /// 每帧按 `delta_secs`（秒）推进状态机，返回本次发生的粗粒度转移。
    /// 写法要点：先算出 `transition` 和"下一状态 next"，最后统一写回 `*self`，
    /// 避免在 match 期间产生可变/不可变借用冲突。
    pub fn tick(&mut self, delta_secs: f32, rng: &mut impl Rng) -> RuneTransition {
        // 防负数：即使 dt 异常取负，也不让计时器倒退。
        let delta_secs = delta_secs.max(0.0);
        let mut next = None; // 待写入的下一状态；None 表示状态不变

        let transition = match self {
            Self::Inactive { mode, remaining } => {
                // 未激活倒计时走完 → 真正开始激活。
                if expire_after(remaining, delta_secs) {
                    next = Some(Self::start(*mode, rng));
                    RuneTransition::Started
                } else {
                    RuneTransition::None
                }
            }
            Self::Activating(run) => match run.tick(delta_secs, rng) {
                // 把内部细粒度转移"翻译"成对外转移，并按需设置下一状态。
                RunTransition::None => RuneTransition::None,
                RunTransition::Advanced => RuneTransition::Advanced,
                RunTransition::Failed => {
                    next = Some(Self::failed(run.mode()));
                    RuneTransition::Failed
                }
                RunTransition::Activated => {
                    next = Some(Self::activated(run.mode()));
                    RuneTransition::Activated
                }
                RunTransition::ResetToInactive => {
                    next = Some(Self::inactive(run.mode()));
                    RuneTransition::ResetToInactive
                }
            },
            Self::Activated { mode, remaining } => {
                // 激活保持时间走完 → 回到未激活。
                if expire_after(remaining, delta_secs) {
                    next = Some(Self::inactive(*mode));
                    RuneTransition::ResetToInactive
                } else {
                    RuneTransition::None
                }
            }
            Self::Failed { mode, remaining } => {
                // 失败恢复时间走完 → 回到未激活。
                if expire_after(remaining, delta_secs) {
                    next = Some(Self::inactive(*mode));
                    RuneTransition::ResetToInactive
                } else {
                    RuneTransition::None
                }
            }
        };

        // 统一写回：只有 next 为 Some 时才替换自身状态。
        if let Some(state) = next {
            *self = state;
        }

        transition
    }

    /// 一次命中：把 (靶位下标, rng) 交给内部流程处理，返回对外命中结果。
    /// 只有"正在激活"时命中有意义，其它状态一律返回 Ignored（`let ... else` 提前返回）。
    pub fn hit(&mut self, target_index: usize, rng: &mut impl Rng) -> RuneHitOutcome {
        let Self::Activating(run) = self else {
            return RuneHitOutcome::Ignored;
        };
        let mode = run.mode(); // 先取出规格，避免后面改 *self 时借用冲突

        match run.hit(target_index, rng) {
            RuneHitOutcome::WrongTarget => {
                // FUNNY_IGNORE_WRONG_TARGET_FAILURE 为 true：打错不判失败，只报告 WrongTarget；
                // 为 false：一打错立即转入 Failed。
                if !FUNNY_IGNORE_WRONG_TARGET_FAILURE {
                    *self = Self::failed(mode);
                }
                RuneHitOutcome::WrongTarget
            }
            RuneHitOutcome::Activated => {
                // 流程完成，直接进入已激活。
                *self = Self::activated(mode);
                RuneHitOutcome::Activated
            }
            // 其它结果（PrimaryHit / SecondaryHit / Ignored）原样返回。
            outcome => outcome,
        }
    }

    /// 是否正在激活（激活流程进行中）。
    pub fn is_activating(&self) -> bool {
        matches!(self, Self::Activating(_))
    }

    /// 是否正在激活"大机关"（模式匹配里用 `..` 忽略其它字段，只匹配 Large 轮次）。
    pub fn is_activating_large(&self) -> bool {
        matches!(
            self,
            Self::Activating(ActivationRun {
                round: ActivationRound::Large(_),
                ..
            })
        )
    }

    /// 大机关已完成的主靶组数；非大机关激活态返回 None。
    pub fn large_progress(&self) -> Option<usize> {
        match self {
            Self::Activating(run) => run.large_progress(),
            // 其它三个状态都返回 None（`|` 把多个分支合并）。
            Self::Inactive { .. } | Self::Activated { .. } | Self::Failed { .. } => None,
        }
    }

    /// 把内部状态翻译成"5 个靶位各自的显示阶段"，供 visual.rs 使用。
    /// Inactive/Failed 全灭；Activating 取内部数组；Activated 全亮为 Completed。
    pub fn target_states(&self) -> RuneTargetStates {
        match self {
            Self::Inactive { .. } | Self::Failed { .. } => {
                [Activation::Deactivated; RUNE_TARGET_COUNT]
            }
            Self::Activating(run) => run.targets, // Activation 是 Copy，数组按值返回
            Self::Activated { .. } => [Activation::Completed; RUNE_TARGET_COUNT],
        }
    }

    /// 面根灯（底座）的显示阶段：只有"正在激活"或"已激活"才亮，其余为 Deactivated。
    pub fn root_activation(&self) -> Activation {
        match self {
            Self::Inactive { .. } | Self::Failed { .. } => Activation::Deactivated,
            Self::Activating(_) | Self::Activated { .. } => Activation::Activated,
        }
    }

    /// 私有构造：进入"已激活"，保持时长为 ACTIVATED_HOLD。
    fn activated(mode: RuneMode) -> Self {
        Self::Activated {
            mode,
            remaining: ACTIVATED_HOLD,
        }
    }

    /// 私有构造：进入"失败"，恢复时长为 FAILURE_RECOVER。
    fn failed(mode: RuneMode) -> Self {
        Self::Failed {
            mode,
            remaining: FAILURE_RECOVER,
        }
    }
}

impl ActivationRun {
    /// 初始化一轮激活：全局超时从 ACTIVATION_GLOBAL_TIMEOUT 起，靶位状态全灭，
    /// 然后按规格建对应轮次，并调用 `start_round` 点亮首批目标。
    fn new(mode: RuneMode, rng: &mut impl Rng) -> Self {
        let mut run = Self {
            global_remaining: ACTIVATION_GLOBAL_TIMEOUT,
            targets: [Activation::Deactivated; RUNE_TARGET_COUNT],
            round: match mode {
                RuneMode::Small => ActivationRound::Small(SmallRound {
                    primary_remaining: ACTIVATION_PRIMARY_TIMEOUT,
                }),
                RuneMode::Large => ActivationRound::Large(LargeRun {
                    completed_groups: 0,
                    phase: LargePhase::Primary {
                        primary_remaining: ACTIVATION_PRIMARY_TIMEOUT,
                    },
                }),
            },
        };
        run.start_round(mode, rng);
        run
    }

    /// 当前轮次对应的规格。
    pub fn mode(&self) -> RuneMode {
        match &self.round {
            ActivationRound::Small(_) => RuneMode::Small,
            ActivationRound::Large(_) => RuneMode::Large,
        }
    }

    /// 取靶位显示状态（Copy，按值返回）。
    pub fn target_states(&self) -> RuneTargetStates {
        self.targets
    }

    /// 大机关已完成组数；小机关返回 None。
    pub fn large_progress(&self) -> Option<usize> {
        match &self.round {
            ActivationRound::Large(run) => Some(run.completed_groups),
            ActivationRound::Small(_) => None,
        }
    }

    /// 按时间推进本轮的计时器，返回细粒度转移。
    /// 小机关：同时推进"全局超时"与"主靶超时"；主靶超时→Failed，全局超时→复位。
    /// 大机关主靶阶段同上；副靶阶段把"副靶超时"到期的结果设为 Advanced（进入下一轮主靶）。
    fn tick(&mut self, delta_secs: f32, rng: &mut impl Rng) -> RunTransition {
        match &mut self.round {
            ActivationRound::Small(round) => tick_two_timers(
                &mut self.global_remaining,
                &mut round.primary_remaining,
                delta_secs,
                RunTransition::ResetToInactive, // 全局超时 → 回到未激活
                RunTransition::Failed,          // 主靶超时 → 失败
            ),
            ActivationRound::Large(run) => match &mut run.phase {
                LargePhase::Primary { primary_remaining } => tick_two_timers(
                    &mut self.global_remaining,
                    primary_remaining,
                    delta_secs,
                    RunTransition::ResetToInactive,
                    RunTransition::Failed,
                ),
                LargePhase::Secondary {
                    secondary_remaining,
                    ..
                } => match tick_two_timers(
                    &mut self.global_remaining,
                    secondary_remaining,
                    delta_secs,
                    RunTransition::ResetToInactive,
                    RunTransition::Advanced, // 副靶窗超时 → 推进（不失败）
                ) {
                    // 副靶窗到期后要开始下一轮主靶（点亮新的 2 个主靶）。
                    RunTransition::Advanced => self.start_large_primary_round(rng),
                    transition => transition, // 其它（None / ResetToInactive）原样返回
                },
            },
        }
    }

    /// 命中分派：越界下标直接判 WrongTarget；否则按轮次/阶段转给对应处理函数。
    fn hit(&mut self, target_index: usize, rng: &mut impl Rng) -> RuneHitOutcome {
        if target_index >= RUNE_TARGET_COUNT {
            return RuneHitOutcome::WrongTarget;
        }

        match &self.round {
            ActivationRound::Small(_) => self.hit_small(target_index, rng),
            ActivationRound::Large(run) => match &run.phase {
                LargePhase::Primary { .. } => self.hit_large_primary(target_index, rng),
                LargePhase::Secondary { target, .. } => {
                    self.hit_large_secondary(target_index, *target)
                }
            },
        }
    }

    /// 小机关命中：只有"当前正在闪烁（Activating）"的靶位才算打中。
    /// 打中后置为 Activated；若 5 个全点亮则返回 Activated，否则点亮下一个目标。
    fn hit_small(&mut self, target_index: usize, rng: &mut impl Rng) -> RuneHitOutcome {
        if self.targets[target_index] != Activation::Activating {
            return RuneHitOutcome::WrongTarget;
        }

        self.targets[target_index] = Activation::Activated;
        if self.all_targets_activated() {
            RuneHitOutcome::Activated
        } else {
            self.start_small_round(rng);
            RuneHitOutcome::PrimaryHit
        }
    }

    /// 大机关"主靶阶段"命中：把打中的主靶置为 Activated，组数 +1；满 5 组即 Activated，
    /// 否则进入副靶阶段（并记下"另一个仍点亮的靶"作为副靶）。
    /// 参数 `_rng` 带下划线表示本函数不用它（占位以保持与其它 hit_* 一致的签名）。
    fn hit_large_primary(&mut self, target_index: usize, _rng: &mut impl Rng) -> RuneHitOutcome {
        if self.targets[target_index] != Activation::Activating {
            return RuneHitOutcome::WrongTarget;
        }

        // 在当前点亮的靶里找"除打中的这个之外"的下标作为副靶（`then_some` 把条件转 Option）。
        let secondary_target = self.targets.iter().enumerate().find_map(|(idx, state)| {
            (idx != target_index && *state == Activation::Activating).then_some(idx)
        });

        self.targets[target_index] = Activation::Activated;
        // `let ... else` 取内部 run：此处必然是大机关轮次，否则逻辑有 bug（unreachable! 会 panic）。
        let ActivationRound::Large(run) = &mut self.round else {
            unreachable!("large primary hit requires a large run");
        };
        run.completed_groups += 1;
        if run.completed_groups == RUNE_TARGET_COUNT {
            return RuneHitOutcome::Activated;
        }

        // 进入副靶阶段：给出副靶窗时长与副靶下标。
        run.phase = LargePhase::Secondary {
            secondary_remaining: LARGE_SECONDARY_TIMEOUT,
            target: secondary_target,
        };
        RuneHitOutcome::PrimaryHit
    }

    /// 大机关"副靶阶段"命中：必须正好是此前记下的那个副靶、且仍在点亮，才算 SecondaryHit。
    fn hit_large_secondary(
        &mut self,
        target_index: usize,
        secondary_target: Option<usize>,
    ) -> RuneHitOutcome {
        if secondary_target != Some(target_index)
            || self.targets[target_index] != Activation::Activating
        {
            return RuneHitOutcome::WrongTarget;
        }

        self.targets[target_index] = Activation::Activated;
        RuneHitOutcome::SecondaryHit
    }

    /// 按规格开启第一轮（供 `new` 使用）。
    fn start_round(&mut self, mode: RuneMode, rng: &mut impl Rng) -> RunTransition {
        match mode {
            RuneMode::Small => self.start_small_round(rng),
            RuneMode::Large => self.start_large_primary_round(rng),
        }
    }

    /// 小机关开一轮：清掉之前"临时点亮"的状态，随机挑 1 个靶点亮（主靶）。
    /// 若没有可选目标（说明 5 个都已点亮）则直接返回 Activated。
    fn start_small_round(&mut self, rng: &mut impl Rng) -> RunTransition {
        self.clear_transient_targets();
        // `choose_targets(1, rng).into_iter().next()` 取随机选中的那 1 个；
        // `let Some(...) = ... else { return ... }` 取不到就返回 Activated。
        let Some(target) = self.choose_targets(1, rng).into_iter().next() else {
            return RunTransition::Activated;
        };
        self.targets[target] = Activation::Activating;
        self.round = ActivationRound::Small(SmallRound {
            primary_remaining: ACTIVATION_PRIMARY_TIMEOUT,
        });
        RunTransition::Advanced
    }

    /// 大机关开一轮主靶：保留已完成组数，清空全部靶位后随机挑 2 个点亮（主靶）。
    fn start_large_primary_round(&mut self, rng: &mut impl Rng) -> RunTransition {
        // 先取出累计已完成组数（若从副靶阶段过来则沿用；否则视为 0）。
        let completed_groups = match &self.round {
            ActivationRound::Large(run) => run.completed_groups,
            ActivationRound::Small(_) => 0,
        };
        self.clear_all_targets();
        let targets = self.choose_targets_from_all(2, rng);
        if targets.is_empty() {
            return RunTransition::Activated;
        }
        // 逐个点亮选中的主靶。
        for target in targets {
            self.targets[target] = Activation::Activating;
        }
        self.round = ActivationRound::Large(LargeRun {
            completed_groups,
            phase: LargePhase::Primary {
                primary_remaining: ACTIVATION_PRIMARY_TIMEOUT,
            },
        });
        RunTransition::Advanced
    }

    /// 从"尚未 Activated 的靶"里随机挑 count 个（小机关挑下一个主靶用）。
    fn choose_targets(&self, count: usize, rng: &mut impl Rng) -> Vec<usize> {
        // 先收集所有还没被点亮的靶位下标。
        let mut available = self
            .targets
            .iter()
            .enumerate()
            .filter_map(|(idx, state)| (*state != Activation::Activated).then_some(idx))
            .collect::<Vec<_>>();
        available.shuffle(rng); // 洗牌（SliceRandom），实现随机挑选
        available.truncate(count.min(available.len())); // 截断到 count 个（不足则全取）
        available
    }

    /// 从全部 5 个靶里随机挑 count 个（大机关每轮主靶都重新随机，不受历史影响）。
    fn choose_targets_from_all(&self, count: usize, rng: &mut impl Rng) -> Vec<usize> {
        let mut targets = (0..RUNE_TARGET_COUNT).collect::<Vec<_>>();
        targets.shuffle(rng);
        targets.truncate(count.min(RUNE_TARGET_COUNT));
        targets
    }

    /// 清掉"临时"状态：把非 Activated 的靶位都复位成 Deactivated（保留已点亮的）。
    fn clear_transient_targets(&mut self) {
        for state in &mut self.targets {
            if *state != Activation::Activated {
                *state = Activation::Deactivated;
            }
        }
    }

    /// 清空全部靶位为 Deactivated。
    fn clear_all_targets(&mut self) {
        self.targets = [Activation::Deactivated; RUNE_TARGET_COUNT];
    }

    /// 是否 5 个靶全部点亮（即已激活）。
    fn all_targets_activated(&self) -> bool {
        self.targets
            .iter()
            .all(|state| *state == Activation::Activated)
    }
}

/// 单个计时器推进：剩余时间不足以覆盖 `delta_secs` 时就"到期"（返回 true 并清零），
/// 否则扣减 `delta_secs` 并返回 false。
fn expire_after(remaining: &mut f32, delta_secs: f32) -> bool {
    if delta_secs >= *remaining {
        *remaining = 0.0;
        true
    } else {
        *remaining -= delta_secs;
        false
    }
}

/// 同时推进"全局超时"和"当前阶段超时"两个计时器，返回"先到期的那个"对应的转移。
/// delta 不足以触发任何到期时，两个计时器一起扣减并返回 None。
fn tick_two_timers(
    global_remaining: &mut f32,
    local_remaining: &mut f32,
    delta_secs: f32,
    global_transition: RunTransition, // 全局超时到期时应返回的转移
    local_transition: RunTransition,  // 阶段超时到期时应返回的转移
) -> RunTransition {
    let next_event = (*global_remaining).min(*local_remaining); // 距最近一次到期的秒数
    if delta_secs < next_event {
        // 本帧不足以触发任何到期：两个计时器都扣减。
        *global_remaining -= delta_secs;
        *local_remaining -= delta_secs;
        return RunTransition::None;
    }

    // 全局先到期（或两者同时，取等号分支）：触发全局转移，全局清零。
    if *global_remaining <= *local_remaining {
        *global_remaining = 0.0;
        global_transition
    } else {
        // 阶段先到期：把 delta 中"用掉的部分"补给全局计时器，阶段清零。
        *global_remaining -= *local_remaining;
        *local_remaining = 0.0;
        local_transition
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试辅助：列出当前处于 Activating（正在闪烁）的靶位下标。
    fn active_indices(state: &MechanismState) -> Vec<usize> {
        state
            .target_states()
            .iter()
            .enumerate()
            .filter_map(|(idx, activation)| (*activation == Activation::Activating).then_some(idx))
            .collect()
    }

    /// 测试辅助：统计已激活（Activated）的靶位数。
    fn activated_count(state: &MechanismState) -> usize {
        state
            .target_states()
            .iter()
            .filter(|activation| **activation == Activation::Activated)
            .count()
    }

    /// 小机关：点亮 1 个靶，打中后点亮下一个（始终维持 1 个正在闪烁）。
    #[test]
    fn small_rune_lights_one_target_and_advances_on_hit() {
        let mut rng = rand::rng();
        let mut state = MechanismState::start(RuneMode::Small, &mut rng);
        let active = active_indices(&state);

        assert_eq!(active.len(), 1);
        assert_eq!(state.hit(active[0], &mut rng), RuneHitOutcome::PrimaryHit);
        assert_eq!(activated_count(&state), 1);
        assert_eq!(active_indices(&state).len(), 1);
    }

    /// funny 模式：打错目标后仍保持 Activating（不判失败），且点亮的目标不变。
    #[test]
    fn funny_mode_keeps_small_rune_activating_after_wrong_target() {
        let mut rng = rand::rng();
        let mut state = MechanismState::start(RuneMode::Small, &mut rng);
        let active = active_indices(&state)[0];
        let wrong = (0..RUNE_TARGET_COUNT).find(|idx| *idx != active).unwrap();

        assert_eq!(state.hit(wrong, &mut rng), RuneHitOutcome::WrongTarget);
        assert!(matches!(state, MechanismState::Activating(_)));
        assert_eq!(active_indices(&state), vec![active]);
    }

    /// 小机关：主靶超时会判失败（进入 Failed）。
    #[test]
    fn small_rune_primary_timeout_fails() {
        let mut rng = rand::rng();
        let mut state = MechanismState::start(RuneMode::Small, &mut rng);

        assert_eq!(
            state.tick(ACTIVATION_PRIMARY_TIMEOUT, &mut rng),
            RuneTransition::Failed
        );
        assert!(matches!(state, MechanismState::Failed { .. }));
    }

    /// 大机关：开局点亮 2 个主靶，打中 1 个后进入副靶窗（进度 +1，剩余 1 个仍在闪烁）。
    #[test]
    fn large_rune_lights_two_targets_and_enters_secondary_window() {
        let mut rng = rand::rng();
        let mut state = MechanismState::start(RuneMode::Large, &mut rng);
        let active = active_indices(&state);

        assert_eq!(active.len(), 2);
        assert_eq!(state.hit(active[0], &mut rng), RuneHitOutcome::PrimaryHit);
        assert!(state.is_activating_large());
        assert_eq!(state.large_progress(), Some(1));
        assert_eq!(activated_count(&state), 1);
        assert_eq!(active_indices(&state).len(), 1);
    }

    /// 大机关：打中副靶得到 SecondaryHit，但进度要等副靶窗超时后才推进（并点亮新的 2 个主靶）。
    #[test]
    fn large_rune_secondary_hit_waits_for_window_timeout() {
        let mut rng = rand::rng();
        let mut state = MechanismState::start(RuneMode::Large, &mut rng);
        let active = active_indices(&state);

        assert_eq!(state.hit(active[0], &mut rng), RuneHitOutcome::PrimaryHit);
        assert_eq!(state.hit(active[1], &mut rng), RuneHitOutcome::SecondaryHit);
        assert_eq!(state.large_progress(), Some(1));
        assert_eq!(active_indices(&state).len(), 0);

        // 半程未到期：无转移。
        assert_eq!(
            state.tick(LARGE_SECONDARY_TIMEOUT * 0.5, &mut rng),
            RuneTransition::None
        );
        assert_eq!(state.large_progress(), Some(1));
        assert_eq!(active_indices(&state).len(), 0);

        // 再走半程到期：推进到下一轮主靶（点亮 2 个），进度仍为 1。
        assert_eq!(
            state.tick(LARGE_SECONDARY_TIMEOUT * 0.5, &mut rng),
            RuneTransition::Advanced
        );
        assert_eq!(state.large_progress(), Some(1));
        assert_eq!(active_indices(&state).len(), 2);
    }

    /// 大机关：副靶窗超时未打中副靶也不算失败，只是推进到下一轮。
    #[test]
    fn large_rune_secondary_timeout_advances_without_failure() {
        let mut rng = rand::rng();
        let mut state = MechanismState::start(RuneMode::Large, &mut rng);
        let first = active_indices(&state)[0];
        state.hit(first, &mut rng);

        assert_eq!(
            state.tick(LARGE_SECONDARY_TIMEOUT, &mut rng),
            RuneTransition::Advanced
        );
        assert!(matches!(state, MechanismState::Activating(_)));
        assert!(state.is_activating_large());
        assert_eq!(state.large_progress(), Some(1));
        assert_eq!(active_indices(&state).len(), 2);
    }

    /// 大机关：连打 5 次主靶后彻底激活。
    #[test]
    fn large_rune_activates_after_five_primary_hits() {
        let mut rng = rand::rng();
        let mut state = MechanismState::start(RuneMode::Large, &mut rng);

        // 前 4 轮：每轮打中 1 个主靶（进度 +1），再让副靶窗超时推进到下一轮。
        for expected_progress in 1..RUNE_TARGET_COUNT {
            let active = active_indices(&state);
            assert_eq!(active.len(), 2);
            assert_eq!(state.hit(active[0], &mut rng), RuneHitOutcome::PrimaryHit);
            assert_eq!(state.large_progress(), Some(expected_progress));
            assert_eq!(
                state.tick(LARGE_SECONDARY_TIMEOUT, &mut rng),
                RuneTransition::Advanced
            );
        }

        // 第 5 次主靶命中即激活。
        let active = active_indices(&state);
        assert_eq!(active.len(), 2);
        assert_eq!(state.hit(active[0], &mut rng), RuneHitOutcome::Activated);
        assert!(matches!(state, MechanismState::Activated { .. }));
    }

    /// 大机关：主靶超时会判失败。
    #[test]
    fn large_rune_primary_timeout_fails() {
        let mut rng = rand::rng();
        let mut state = MechanismState::start(RuneMode::Large, &mut rng);

        assert_eq!(
            state.tick(ACTIVATION_PRIMARY_TIMEOUT, &mut rng),
            RuneTransition::Failed
        );
        assert!(matches!(state, MechanismState::Failed { .. }));
    }

    /// 大机关：全局超时优先于阶段超时，直接复位到 Inactive（而不是 Failed）。
    #[test]
    fn large_rune_global_timeout_resets_to_inactive() {
        // 手工构造：全局只剩 1 秒，主靶还剩一整段；此时 tick 1 秒应触发全局超时。
        let mut state = MechanismState::Activating(ActivationRun {
            global_remaining: 1.0,
            targets: [
                Activation::Activating,
                Activation::Activating,
                Activation::Deactivated,
                Activation::Deactivated,
                Activation::Deactivated,
            ],
            round: ActivationRound::Large(LargeRun {
                completed_groups: 1,
                phase: LargePhase::Primary {
                    primary_remaining: ACTIVATION_PRIMARY_TIMEOUT,
                },
            }),
        });
        let mut rng = rand::rng();

        assert_eq!(state.tick(1.0, &mut rng), RuneTransition::ResetToInactive);
        assert!(matches!(
            state,
            MechanismState::Inactive {
                mode: RuneMode::Large,
                ..
            }
        ));
    }
}
