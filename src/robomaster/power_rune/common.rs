//! 能量机关共享定义：所有子模块都会用到的枚举与常量集中放这里，避免循环依赖。
//!
//! 三个概念贯穿整个模块：
//! - `RuneMode`：机关分"小/大"两种，激活规则与转速都不同；
//! - `RuneHitOutcome`：一次命中的结果分类（打错 / 打中主靶 / 打中副靶 / 激活……）；
//! - `RuneTransition`：状态机"按时间推进（tick）"时发生的状态转移类型，用于对外报告。

/// 机关规格。`Copy, Clone` 表示可以按值随处复制；`Hash, Eq` 表示可作哈希表键。
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum RuneMode {
    Small, // 小机关：5 个靶位逐个点亮，靶位随机出现
    Large, // 大机关：分"主靶 / 副靶"两阶段，且激活时转速改为正弦变速
}

/// 每个面（FACE）上的靶位数量，恒为 5。
/// 这个常量决定了 `state.rs` 里目标状态数组 `[Activation; RUNE_TARGET_COUNT]` 的长度。
pub const RUNE_TARGET_COUNT: usize = 5;

/// 一次命中的结果分类。算法语义见 `state.rs` 的 `MechanismState::hit`。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RuneHitOutcome {
    Ignored, // 机关当前不在激活流程中，本次命中被无视
    WrongTarget, // 打错了目标（不是当前该打的那个）
    PrimaryHit, // 打中主靶（正确，点亮一个目标）
    SecondaryHit, // 打中大机关的副靶（第二阶段正确）
    Activated, // 这次命中把机关彻底激活
}

impl RuneHitOutcome {
    /// 本次命中是否算"有效命中"。`matches!` 是枚举匹配宏：只要形状匹配就返回 true。
    /// `const fn`：可在编译期求值。主靶 / 副靶 / 激活都算有效。
    pub const fn is_accurate(self) -> bool {
        matches!(
            self,
            Self::PrimaryHit | Self::SecondaryHit | Self::Activated
        )
    }

    /// 本次命中是否使机关"激活"。只有 `Activated` 才是。`const fn` 同上。
    pub const fn activates_rune(self) -> bool {
        matches!(self, Self::Activated)
    }
}

/// 状态机按时间推进（`tick`）后对外报告的状态转移结果。
/// 注意：这是"粗粒度、给外部看"的转移类型；状态机内部还有更细的 `RunTransition`
/// （见 state.rs），二者不要混淆。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RuneTransition {
    None, // 本 tick 无状态变化
    Started, // 从"未激活"进入"正在激活"（开始激活流程）
    Advanced, // 激活流程内推进了一步（点亮一个靶 / 进入下一轮）
    Failed, // 激活失败
    Activated, // 成功激活
    ResetToInactive, // 回到未激活（激活保持结束 / 失败恢复结束 / 全局超时）
}
