//! RoboMaster 领域的**公共词汇表**：阵营、机型、机型配置常量。
//!
//! 本文件是全项目被引用最广的定义之一（通过 `robomaster::prelude::*` 再导出），
//! 内容不多但都是"业务名词"，读代码时遇到的 `Team::Red`、`Robot::Hero`、`INFANTRY_THREE_CONFIG`
//! 都出自这里。三块内容：
//! - `Team`：红/蓝阵营，并提供从字符串解析的 `Team::from`（读配置/资源时用）；
//! - `RobotConfig` + 四个机型常量：每个机型用哪种装甲、装几块（armor_count）；
//! - `Robot`：九类机器人机种的领域枚举（英雄/工程/步兵/空中/哨兵/飞镖/雷达）。
//!
//! 这些类型都派生 `Debug, Copy, Clone, Hash, PartialEq, Eq`：可打印、可廉价复制、可作 map 键、
//! 可精确相等比较——对"小而无字段的枚举/常量结构"是标准配置。

use crate::robomaster::prelude::*;

/// 比赛阵营。
/// 红蓝两方几乎所有逻辑都要比较阵营（判断敌我、选择灯光颜色、镜像场地等）。
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum Team {
    Red,
    Blue,
}

impl Team {
    /// 从字符串解析阵营：`"red"`/`"blue"`（大小写不敏感，先 `to_lowercase`）。
    /// 返回 `Option<Self>`——解析不了（如拼写错误）返回 `None`，由调用方决定如何兜底。
    /// 调用时机：读取配置文件、场景元数据里的阵营字段时。
    pub fn from(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "red" => Some(Team::Red),
            "blue" => Some(Team::Blue),
            _ => None,
        }
    }
}

/// 单个机型的配置：用哪套装甲规格 + 装几块装甲板。
/// 装甲板数量 `armor_count` 是命中判定的依据之一（打满即可摧毁该车）。
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub struct RobotConfig {
    pub armor: ArmorSpec,
    pub armor_count: usize,
}

impl RobotConfig {
    // `const fn`：编译期即可求值的构造函数——因为下面的四个机型常量要在编译期初始化，必须用它。
    pub const fn new(armor: ArmorSpec, armor_count: usize) -> Self {
        Self { armor, armor_count }
    }
}

// 四个机型的标准配置常量（`const`：编译期常量，使用时按值内联，无运行时开销）。
// 装甲规格：英雄用大装甲(Large)、其余用对应编号的小装甲(Small)。
// `armor_count = 4`：RoboMaster 规则里这几类车均为四周 4 块装甲板——改动即改变承伤次数。
pub const HERO_ROBOT_CONFIG: RobotConfig =
    RobotConfig::new(ArmorSpec::Large(LargeArmorLabel::One), 4);
pub const ENGINEER_ROBOT_CONFIG: RobotConfig =
    RobotConfig::new(ArmorSpec::Small(SmallArmorLabel::Two), 4);
pub const INFANTRY_THREE_CONFIG: RobotConfig =
    RobotConfig::new(ArmorSpec::Small(SmallArmorLabel::Three), 4);
pub const INFANTRY_FOUR_CONFIG: RobotConfig =
    RobotConfig::new(ArmorSpec::Small(SmallArmorLabel::Four), 4);

/// 机器人机种（对应 RoboMaster 赛场上的各类单位）。
/// 枚举用于区分行为、能力与外观（如是否可发射、能否飞行、装甲规格）。
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum Robot {
    /// 英雄机器人 - 唯一可以发射42mm弹丸的机器人
    /// - 编号: 1号机
    /// - 特点: 高血量、高伤害、可部署模式
    Hero,

    /// 工程机器人 - 负责抓取能量单元和团队增益
    /// - 编号: 2号机
    /// - 特点: 无发射机构、高机动性、特殊任务执行能力
    Engineer,

    /// 步兵机器人 - 基础作战单位，发射17mm弹丸
    /// - 编号: 3/4号机（两台）
    /// - 特点: 均衡性能、经验升级系统
    Infantry,

    /// 空中机器人 - 空中支援单位，发射17mm弹丸
    /// - 编号: 6号机
    /// - 特点: 飞行能力、第一视角画面、激光检测模块
    Aerial,

    /// 哨兵机器人 - 基地防守单位，可全自动或半自动运行
    /// - 编号: 7号机
    /// - 特点: 自主防御,姿态切换系统,堡垒占领能力
    Sentinel,

    /// 飞镖系统 - 远程打击系统,攻击前哨站和基地
    /// - 编号: 8号机
    /// - 特点: 飞镖发射,目标选择机制,闸门控制
    DartSystem,

    /// 雷达 - 战场信息获取和反制系统
    /// - 编号: 9号机
    /// - 特点: 激光照射,坐标标记,信息波解析
    Radar,
}

#[cfg(test)]
mod tests {
    use super::*;

    // 回归测试：确保四个机型常量的装甲类型/标签/数量不被误改（`cargo test` 时运行）。
    #[test]
    fn robot_configs_preserve_legacy_armor_values() {
        let cases = [
            (HERO_ROBOT_CONFIG, ArmorType::Large, ArmorLabel::One, 4),
            (
                ENGINEER_ROBOT_CONFIG,
                ArmorType::Small,
                ArmorLabel::Sentry,
                4,
            ),
            (
                INFANTRY_THREE_CONFIG,
                ArmorType::Small,
                ArmorLabel::Three,
                4,
            ),
            (INFANTRY_FOUR_CONFIG, ArmorType::Small, ArmorLabel::Four, 4),
        ];

        for (config, armor_type, label, armor_count) in cases {
            assert_eq!(config.armor.armor_type(), armor_type);
            assert_eq!(config.armor.label(), label);
            assert_eq!(config.armor_count, armor_count);
        }
    }
}
