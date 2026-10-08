//! 装甲"身份数据"：类型/编号/贴纸槽位的枚举与查表，纯数据、无任何系统。
//!
//! 游戏语言：现实 RoboMaster 赛场上，每个机器人的装甲板有大小之分（小装甲/大装甲），
//! 每块还打上编号（1~5 号、哨兵 G、前哨站 O、基地 B）便于裁判与视觉识别；表面还能贴
//! 不同的"贴纸"（调试时循环切换样式）。本文件就是这些身份的表。
//!
//! 协作者：construct.rs 从模型名推断出这些枚举，做成 `ArmorSpec` 挂到装甲实体上；
//! 视觉/统计模块再据此判断打中的是哪块装甲。装甲的"受击判定"本身在 collision.rs。
//!
//! 新手阅读顺序：先看三个枚举（Type / Label / Spec）的关系，再看贴纸槽位表，
//! 最后看 `From` 转换如何把"小装甲编号"提升成通用 `ArmorLabel`。

// `#[repr(u8)]`：告诉编译器用 1 字节无符号整数保存这个枚举的判别值（discriminant），
// 于是 Small=0 / Large=1 有了稳定的内存表示——可安全地 `as u8` 转换、可跨边界
// （如与 C/网络/着色器）按相同字节布局交换。不写 repr 时编译器可自由选择布局。
// 派生含义：Debug(可打印)/Copy(按值复制)/Clone/Eq+PartialEq(可比较相等)/Hash(可作哈希键)。
#[repr(u8)]
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum ArmorType {
    Small = 0,
    Large = 1,
}

/// 装甲板的编号标签：与赛场规则一致的身份编号。
/// Sentry=哨兵(G)、One..Five=1~5 号装甲、Outpost=前哨站(O)、Base=基地(B)。
#[repr(u8)]
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum ArmorLabel {
    Sentry = 0, // 哨兵机器人装甲
    One = 1,    // 1 号装甲
    Two = 2,    // 2 号装甲
    Three = 3,  // 3 号装甲
    Four = 4,   // 4 号装甲
    Five = 5,   // 5 号装甲
    Outpost = 6, // 前哨站装甲
    Base = 7,   // 基地装甲
}

/// "小装甲"专用的编号集合：只可能出现这些编号的小装甲型号。
/// 注意其变体排列顺序与 `ArmorLabel` 不同——这里的顺序是调试循环切换的展示顺序。
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum SmallArmorLabel {
    Sentry,
    One,
    Base,
    Outpost,
    Two,
    Three,
    Four,
    Five,
}

impl SmallArmorLabel {
    /// 把"小装甲编号"映射为通用 `ArmorLabel`。
    /// `const fn`：可在编译期求值（用于 const 上下文），无副作用；`self` 按值接收（枚举 Copy）。
    /// `match` 必须穷尽所有变体，故这里列出全部 8 种。
    pub const fn label(self) -> ArmorLabel {
        match self {
            Self::One => ArmorLabel::One,
            Self::Base => ArmorLabel::Base,
            Self::Sentry => ArmorLabel::Sentry,
            Self::Outpost => ArmorLabel::Outpost,
            Self::Two => ArmorLabel::Two,
            Self::Three => ArmorLabel::Three,
            Self::Four => ArmorLabel::Four,
            Self::Five => ArmorLabel::Five,
        }
    }
}

// `From` 是 Rust 的"无损转换"标准 trait：实现后可直接用 `small_label.into()` 得到 ArmorLabel，
// 或写 `ArmorLabel::from(small_label)`——比自定义方法更符合惯用法，泛型代码也依赖它。
impl From<SmallArmorLabel> for ArmorLabel {
    fn from(label: SmallArmorLabel) -> Self {
        label.label()
    }
}

/// "大装甲"专用的编号集合：目前只有 1 号（英雄机器人的大装甲）。
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum LargeArmorLabel {
    One,
}

impl LargeArmorLabel {
    /// 同 `SmallArmorLabel::label`：映射为通用 `ArmorLabel`。
    pub const fn label(self) -> ArmorLabel {
        match self {
            Self::One => ArmorLabel::One,
        }
    }
}

// 同 SmallArmorLabel 的 From：让 `large_label.into()` 直接得到 ArmorLabel。
impl From<LargeArmorLabel> for ArmorLabel {
    fn from(label: LargeArmorLabel) -> Self {
        label.label()
    }
}

/// 装甲"规格"：把物理类型与编号打包成一个值（大装甲或小装甲 + 其携带的编号）。
/// 关联函数风格枚举：`Small(SmallArmorLabel)` 携带具体编号——不同变体可携带不同数据。
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum ArmorSpec {
    Small(SmallArmorLabel),
    Large(LargeArmorLabel),
}

impl ArmorSpec {
    /// 取出物理类型（大/小）。
    pub const fn armor_type(self) -> ArmorType {
        match self {
            // `Self::Small(_)`：`_` 忽略携带的编号数据（这里用不到），只看变体种类。
            Self::Small(_) => ArmorType::Small,
            Self::Large(_) => ArmorType::Large,
        }
    }

    /// 取出通用编号标签（转调具体类型的 `label()`）。
    pub const fn label(self) -> ArmorLabel {
        match self {
            Self::Small(label) => label.label(),
            Self::Large(label) => label.label(),
        }
    }

    /// 取出该规格对应的贴纸槽位表。
    /// 返回 `&'static [ArmorStickerSlot]`：借用一个全局常量数组，零拷贝、生命周期与程序同长。
    pub const fn sticker_slots(self) -> &'static [ArmorStickerSlot] {
        match self {
            Self::Small(_) => &SMALL_ARMOR_STICKER_SLOTS,
            Self::Large(_) => &LARGE_ARMOR_STICKER_SLOTS,
        }
    }
}

// `From<SmallArmorLabel> for ArmorSpec`：让 `small_label.into()` 直接构造 ArmorSpec::Small。
impl From<SmallArmorLabel> for ArmorSpec {
    fn from(label: SmallArmorLabel) -> Self {
        Self::Small(label)
    }
}

// 同理：`large_label.into()` → ArmorSpec::Large。
impl From<LargeArmorLabel> for ArmorSpec {
    fn from(label: LargeArmorLabel) -> Self {
        Self::Large(label)
    }
}

/// 一个"贴纸槽位"：把某个装甲编号和资产文件名的后缀关联起来。
/// 模型网格里的贴纸子节点按后缀命名（如 "...B"、"...O"），据此找到对应贴纸实体。
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct ArmorStickerSlot {
    pub label: ArmorLabel,         // 这个槽位对应哪块装甲的编号
    pub name_suffix: &'static str, // 资产名后缀（'static：指向编译进二进制里的字符串字面量）
}

/// 小装甲的贴纸槽位表（7 个槽位）。
/// `[ArmorStickerSlot; 7]`：数组长度写进类型，元素数量在编译期被检查；顺序即调试循环展示顺序。
pub const SMALL_ARMOR_STICKER_SLOTS: [ArmorStickerSlot; 7] = [
    ArmorStickerSlot {
        label: ArmorLabel::Base,
        name_suffix: "B",
    },
    ArmorStickerSlot {
        label: ArmorLabel::Sentry,
        name_suffix: "G",
    },
    ArmorStickerSlot {
        label: ArmorLabel::Outpost,
        name_suffix: "O",
    },
    ArmorStickerSlot {
        label: ArmorLabel::Two,
        name_suffix: "2",
    },
    ArmorStickerSlot {
        label: ArmorLabel::Three,
        name_suffix: "3",
    },
    ArmorStickerSlot {
        label: ArmorLabel::Four,
        name_suffix: "4",
    },
    ArmorStickerSlot {
        label: ArmorLabel::Five,
        name_suffix: "5",
    },
];

/// 大装甲的贴纸槽位表（5 个槽位）。其中 `Base` 出现两次（第 2 项后缀 "3"、末项后缀 "B"），
/// 因为资产里同一编号可能对应多个不同后缀的贴纸节点。
pub const LARGE_ARMOR_STICKER_SLOTS: [ArmorStickerSlot; 5] = [
    ArmorStickerSlot {
        label: ArmorLabel::One,
        name_suffix: "1",
    },
    ArmorStickerSlot {
        label: ArmorLabel::Base,
        name_suffix: "3",
    },
    ArmorStickerSlot {
        label: ArmorLabel::Four,
        name_suffix: "4",
    },
    ArmorStickerSlot {
        label: ArmorLabel::Five,
        name_suffix: "5",
    },
    ArmorStickerSlot {
        label: ArmorLabel::Base,
        name_suffix: "B",
    },
];

impl ArmorLabel {
    /// 调试用的"小装甲循环序列"：按 C 键换贴纸样式时依此顺序轮转（8 个编号）。
    /// 返回 `&'static [ArmorLabel; 8]`：借用编译期常量数组，长度由类型保证。
    pub fn sequence_small() -> &'static [ArmorLabel; 8] {
        &[
            ArmorLabel::Sentry,
            ArmorLabel::One,
            ArmorLabel::Two,
            ArmorLabel::Three,
            ArmorLabel::Four,
            ArmorLabel::Outpost,
            ArmorLabel::Base,
            ArmorLabel::Five,
        ]
    }

    /// 把编号映射为"小装甲序列"里的下标（0~7），供 `sequence_small()` 索引使用。
    /// 注意：这个下标与枚举判别值无关，是独立约定的顺序。
    pub fn index_from_small(label: ArmorLabel) -> usize {
        match label {
            ArmorLabel::Sentry => 0,
            ArmorLabel::One => 1,
            ArmorLabel::Two => 2,
            ArmorLabel::Three => 3,
            ArmorLabel::Four => 4,
            ArmorLabel::Outpost => 5,
            ArmorLabel::Base => 6,
            ArmorLabel::Five => 7,
        }
    }
}

// 单元测试：`#[cfg(test)]` 表示这段代码只在 `cargo test` 时编译进二进制，正式构建里被剔除。
// 三个测试分别锁死"规格→类型/编号"、"调试序列与下标顺序"、"贴纸槽位表后缀"这些数据契约，
// 防止后续重构不小心改动这些与资产/规则绑定的常量。
#[cfg(test)]
mod tests {
    // `use super::*;`：把父模块（本文件）的全部条目导入测试作用域。
    use super::*;

    #[test]
    fn armor_spec_preserves_legacy_type_and_label() {
        // 用元组数组列出 (规格, 期望类型, 期望编号) 三组用例，循环断言。
        let cases = [
            (
                ArmorSpec::Small(SmallArmorLabel::Sentry),
                ArmorType::Small,
                ArmorLabel::Sentry,
            ),
            (
                ArmorSpec::Small(SmallArmorLabel::Outpost),
                ArmorType::Small,
                ArmorLabel::Outpost,
            ),
            (
                ArmorSpec::Large(LargeArmorLabel::One),
                ArmorType::Large,
                ArmorLabel::One,
            ),
        ];

        for (spec, armor_type, label) in cases {
            assert_eq!(spec.armor_type(), armor_type);
            assert_eq!(spec.label(), label);
        }
    }

    #[test]
    fn debug_sequence_and_indexes_keep_legacy_order() {
        assert_eq!(
            ArmorLabel::sequence_small(),
            &[
                ArmorLabel::Sentry,
                ArmorLabel::One,
                ArmorLabel::Two,
                ArmorLabel::Three,
                ArmorLabel::Four,
                ArmorLabel::Outpost,
                ArmorLabel::Base,
                ArmorLabel::Five,
            ]
        );

        assert_eq!(ArmorLabel::index_from_small(ArmorLabel::Sentry), 0);
        assert_eq!(ArmorLabel::index_from_small(ArmorLabel::One), 1);
        assert_eq!(ArmorLabel::index_from_small(ArmorLabel::Two), 2);
        assert_eq!(ArmorLabel::index_from_small(ArmorLabel::Three), 3);
        assert_eq!(ArmorLabel::index_from_small(ArmorLabel::Four), 4);
        assert_eq!(ArmorLabel::index_from_small(ArmorLabel::Outpost), 5);
        assert_eq!(ArmorLabel::index_from_small(ArmorLabel::Base), 6);
        assert_eq!(ArmorLabel::index_from_small(ArmorLabel::Five), 7);
        assert_eq!(ArmorLabel::index_from_small(ArmorLabel::Base), 8);
    }

    #[test]
    fn sticker_slot_tables_keep_asset_suffixes() {
        assert_eq!(
            ArmorSpec::Small(SmallArmorLabel::Outpost).sticker_slots(),
            &[
                ArmorStickerSlot {
                    label: ArmorLabel::Base,
                    name_suffix: "B",
                },
                ArmorStickerSlot {
                    label: ArmorLabel::Sentry,
                    name_suffix: "G",
                },
                ArmorStickerSlot {
                    label: ArmorLabel::Outpost,
                    name_suffix: "O",
                },
                ArmorStickerSlot {
                    label: ArmorLabel::Two,
                    name_suffix: "2",
                },
                ArmorStickerSlot {
                    label: ArmorLabel::Three,
                    name_suffix: "3",
                },
                ArmorStickerSlot {
                    label: ArmorLabel::Four,
                    name_suffix: "4",
                },
                ArmorStickerSlot {
                    label: ArmorLabel::Five,
                    name_suffix: "5",
                },
            ]
        );

        assert_eq!(
            ArmorSpec::Large(LargeArmorLabel::One).sticker_slots(),
            &[
                ArmorStickerSlot {
                    label: ArmorLabel::One,
                    name_suffix: "1",
                },
                ArmorStickerSlot {
                    label: ArmorLabel::Three,
                    name_suffix: "3",
                },
                ArmorStickerSlot {
                    label: ArmorLabel::Four,
                    name_suffix: "4",
                },
                ArmorStickerSlot {
                    label: ArmorLabel::Five,
                    name_suffix: "5",
                },
                ArmorStickerSlot {
                    label: ArmorLabel::Base,
                    name_suffix: "B",
                },
            ]
        );
    }
}
