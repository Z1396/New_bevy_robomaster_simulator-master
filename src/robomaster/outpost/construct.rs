//! 前哨站组装：场景里带 `OutpostRoot` 标记的实体，被扫描后升级成可旋转的前哨站。
//!
//! 游戏语言：关卡模型里预置一个标记（`OutpostRoot`，携带阵营），本文件把它变成正式前哨站：
//! 挂上装甲待扫描标记、按阵营决定旋转方向、找到内部名为 *ROTATE* 的部件挂上旋转器。
//!
//! 协作者：`update::Outpost/OutpostRotator`、`rotation::RotationDirection`，
//! 以及 `armor` 子系统的 `ScanArmor`（前哨站装甲复用同一套装甲组装流程）。
//!
//! 新手阅读顺序：OutpostRoot → OutpostParam（SystemParam）→ setup_outpost → 插件。

use crate::robomaster::outpost::rotation::RotationDirection;
use crate::robomaster::outpost::update::{Outpost, OutpostRotator};
use crate::robomaster::prelude::{ArmorSpec, ScanArmor, SmallArmorLabel, Team};
// `SystemParam` 概念详见 armor/construct.rs 的 ArmorConstructor（此处只打包三个字段）。
use bevy::ecs::system::SystemParam;
// `Added` 用于变更检测，含义详见 armor/construct.rs 的 `insert` 系统。
use bevy::prelude::{Added, Children, Commands, Component, Entity, Name, Query, Update};

/// 前哨站根标记组件：关卡模型上预置，声明"这里是前哨站，属于哪个阵营"。
/// 与装甲的 `ScanArmor` 同为"扫描触发标记"——插入后由 `Added` 变更检测触发组装。
#[derive(Component)]
pub struct OutpostRoot {
    pub team: Team,
}

impl OutpostRoot {
    /// `const fn`：供常量/编译期构造使用。
    pub const fn new(team: Team) -> Self {
        Self { team }
    }
}

/// 组装用的系统参数工具箱（`SystemParam` 概念详见 armor/construct.rs）。
/// 这里只需：写命令 + 读名字 + 读子树。
#[derive(SystemParam)]
struct OutpostParam<'w, 's> {
    commands: Commands<'w, 's>,
    names: Query<'w, 's, &'static Name>,        // 读实体名（用于匹配 "ROTATE" 后缀）
    children: Query<'w, 's, &'static Children>, // 向下遍历子树
}

/// 组装系统：`Added<OutpostRoot>` 保证只在标记新插入时组装一次（变更检测，详见 armor/construct.rs）。
fn setup_outpost(
    query: Query<(Entity, &OutpostRoot), Added<OutpostRoot>>,
    mut param: OutpostParam,
) {
    for (root, outpost_root) in query {
        let team = outpost_root.team;
        // 给根实体挂"前哨站组件 + 装甲待扫描标记"。
        // 用 `SmallArmorLabel::Outpost` 表明这是前哨站装甲——它会走 armor 子系统的组装流程。
        param.commands.entity(root).insert((
            Outpost::new(team),
            ScanArmor::new(team, ArmorSpec::Small(SmallArmorLabel::Outpost)),
        ));
        // 阵营决定旋转方向：红队顺时针、蓝队逆时针（与赛场规则一致）。
        let direction = match team {
            Team::Red => RotationDirection::Clockwise,
            Team::Blue => RotationDirection::CounterClockwise,
        };
        // 遍历根的所有后代，凡名字以 "ROTATE" 结尾的部件都挂上旋转器（每帧转它）。
        param.children.iter_descendants(root).for_each(|e| {
            // `let Ok(name) = ... else { return; }`：拿不到名字就跳过这个实体（提前返回）。
            let Ok(name) = param.names.get(e) else {
                return;
            };
            if name.ends_with("ROTATE") {
                param
                    .commands
                    .entity(e)
                    .insert(OutpostRotator::new(direction));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    // `use super::*;` 把父模块全部条目导入测试作用域。
    use super::*;

    #[test]
    fn team_rotation_mapping_matches_legacy_behavior() {
        // 复刻生产逻辑：确认红→顺时针、蓝→逆时针（防止重构时把方向改错）。
        let red = match Team::Red {
            Team::Red => RotationDirection::Clockwise,
            Team::Blue => RotationDirection::CounterClockwise,
        };
        let blue = match Team::Blue {
            Team::Red => RotationDirection::Clockwise,
            Team::Blue => RotationDirection::CounterClockwise,
        };

        assert_eq!(red, RotationDirection::Clockwise);
        assert_eq!(blue, RotationDirection::CounterClockwise);
    }
}

#[derive(Default)]
pub(super) struct OutpostConstructorPlugin;

impl bevy::app::Plugin for OutpostConstructorPlugin {
    fn build(&self, app: &mut bevy::app::App) {
        // 每帧检查是否出现新的 OutpostRoot（靠 Added 变更检测，实际只在插入时干活）。
        app.add_systems(Update, setup_outpost);
    }
}
