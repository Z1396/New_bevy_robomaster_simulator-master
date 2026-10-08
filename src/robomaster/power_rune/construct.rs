//! 能量机关装配：把 POWER.glb 场景里的节点（按名字）加工成运行期组件。
//!
//! 何时被调用：不是每帧系统，而是由场景加载流程显式调用——scene.rs 在
//! `spawn(scene("POWER.glb"))` 之后执行 `w.run(setup_power_rune, (power_rune_root, instance))`
//!（见 scene.rs 第 237 行附近）；随后 scene.rs 再用 `power_rune_collision` 表给各节点生成碰撞体。
//!
//! 命名契约（跨文件！必须与 POWER.glb 的节点名、scene.rs 的 `power_rune_collision` 三方一致）：
//! - `FACE_{i}`：第 i 个面的根（i 从 1 开始）；
//! - `FACE_{i}_R_POWERED` / `FACE_{i}_R_UNPOWERED`：面根灯的点亮/熄灭两级；
//! - `FACE_{i}_TARGET_{j}_{STATE}`：第 j 个靶位的某显示阶段节点，STATE ∈ {DISABLED, ACTIVE, ACTIVATED, COMPLETED}；
//! - `FACE_{i}_TARGET_{j}_PADDING` / `_LEGGING_PROGRESSING` / `_LEGGING_{k}`：装饰件。
//!
//! 装配做的事：遍历节点名建 `name_map` → 找出所有面 → 逐面解析大小/阵营/顺时针 →
//! 组装 5 个靶位的控制器（`RuneVisual`）→ 把 `PowerRune` / `PowerRuneMechanism` /
//! `PowerRuneRotation` / `PowerRuneVisuals` 一并插到面实体上。

use crate::robomaster::power_rune::collision::RuneIndex;
use crate::robomaster::power_rune::common::{RUNE_TARGET_COUNT, RuneMode};
use crate::robomaster::power_rune::rotation::PowerRuneRotation;
use crate::robomaster::power_rune::rune::{PowerRune, PowerRuneMechanism};
use crate::robomaster::power_rune::visual::{PowerRuneVisuals, RuneVisual};
use crate::robomaster::prelude::Team;
use crate::robomaster::visibility::{Controller, StatefulAppearanceCreator};
use crate::util::bevy::{drain_entities_by, insert_all_child};
use crate::{material, visibility};
use avian3d::prelude::CollisionEventsEnabled;
use bevy::ecs::system::SystemParam;
use bevy::prelude::{Children, Commands, Component, Entity, In, Name, Query, Res, With};
use bevy::world_serialization::{InstanceId, WorldInstanceSpawner};
use rand::RngExt;
use std::collections::HashMap;

/// 能量机关场景根的标记组件。scene.rs 用它标注 POWER.glb 的根实体；
/// `PowerRuneParam` 里的 `power_query` 也用它作为过滤条件。
#[derive(Component)]
pub struct PowerRuneRoot;

/// 构建一个面下 5 个靶位的可视化控制器。
/// 输入：面序号 `face_index`、面根实体 `face_entity`、节点名→实体表 `name_map`、
/// 全局参数 `param`（提供 Commands/Children 查询）、外观创建器 `creator`。
/// 返回 `Option<[RuneVisual; 5]>`：节点数量不对（无法凑齐 5 个）则返回 None，调用方跳过该面。
fn build_targets(
    face_index: usize,
    face_entity: Entity,
    name_map: &mut HashMap<&str, Entity>,
    param: &mut PowerRuneParam,
    creator: &mut StatefulAppearanceCreator,
) -> Option<[RuneVisual; RUNE_TARGET_COUNT]> {
    let mut targets = Vec::new();
    // 靶位编号从 1 数到 5（节点名里用的是 1..=5）。
    for target_idx in 1..=5 {
        let prefix = format!("FACE_{}_TARGET_{}", face_index, target_idx);

        // `drain_entities_by`：从 name_map 里"取出并删除"所有名字以指定前缀开头的节点
        //（每个节点只被消费一次，取走后不再参与后续匹配）。
        // `create_controller`：把这些实体 + 显示规则打包成一个控制器。
        // PADDING 段在"完成(completed)"阶段换成对应材质。
        let padding_segments = creator.create_controller(
            drain_entities_by(name_map, |name| {
                name.starts_with(&format!("{}_PADDING", prefix))
            }),
            material!(on = { completed }),
        );
        // 进度段在"正在激活(activating)"阶段显示。
        let progress_segments = creator.create_controller(
            drain_entities_by(name_map, |name| {
                name.starts_with(&format!("{}_LEGGING_PROGRESSING", prefix))
            }),
            visibility!(activating),
        );

        // 4 个显示阶段节点的完整名字，再 `remove` 取实体（同样是"取走即去重"的消费式取值）。
        let ad = format!("{}_ACTIVATED", prefix);
        let at = format!("{}_ACTIVE", prefix);
        let d = format!("{}_DISABLED", prefix);
        let c = format!("{}_COMPLETED", prefix);
        let activated = ad.as_str();
        let active = at.as_str();
        let deactivated = d.as_str();
        let completed = c.as_str();

        let activated = name_map.remove(activated);
        let activating = name_map.remove(active);
        let deactivated = name_map.remove(deactivated);
        let completed = name_map.remove(completed);

        // 逻辑下标 = 数组当前位置（0..5），与 `RuneIndex.target` 一一对应。
        let logical_index = targets.len();
        // 给上面四个阶段节点（存在的话）及其所有子孙挂 `RuneIndex` 与 `CollisionEventsEnabled`，
        // 这样子弹打中靶位的任意子部件都能被 collision.rs 识别成"打中第几个靶"。
        // `.flatten()` 跳过 None（某阶段节点不存在时）。
        for entity in [deactivated, activating, activated, completed]
            .into_iter()
            .flatten()
        {
            insert_all_child(&mut param.commands, entity, &param.children, || {
                (
                    RuneIndex {
                        target: logical_index,
                        rune: face_entity,
                    },
                    CollisionEventsEnabled,
                )
            });
        }

        // 3 段腿：先以空控制器占位，再逐个从 name_map 取实体填充。
        let mut legging_segments: [Controller; 3] = [
            Controller::new_combined(vec![]),
            Controller::new_combined(vec![]),
            Controller::new_combined(vec![]),
        ];
        for legging_idx in 1..=3 {
            // 匹配 `_LEGGING_{k}`，但要排除名字里含 PROGRESSING 的（那是进度段）。
            legging_segments[legging_idx - 1] = creator.create_controller(
                drain_entities_by(name_map, |name| {
                    name.starts_with(&format!("{}_LEGGING_{}", prefix, legging_idx))
                        && !name.contains("PROGRESSING")
                }),
                material!(on = {activated, completed}),
            )
        }

        // 组装一个靶位的全部控制器（靶体按可见性切换，腿/衬垫/进度按材质切换）。
        targets.push(RuneVisual::new(
            Controller::new_visibility(deactivated, activating, activated, completed),
            legging_segments,
            padding_segments,
            progress_segments,
        ));
    }
    // `try_into` 把 Vec 转成定长数组 `[RuneVisual; 5]`：长度不对会失败，`.ok()` 转成 Option。
    targets.try_into().ok()
}

/// `#[derive(SystemParam)]`：把若干查询/资源打包成一个"系统参数"结构体，
/// 让 `setup_power_rune` 的签名保持简洁。字段：延迟操作队列、场景实例管理器、
/// 以及按名字/父子关系定位节点所需的查询。
#[derive(SystemParam)]
pub(crate) struct PowerRuneParam<'w, 's> {
    commands: Commands<'w, 's>,
    scene_spawner: Res<'w, WorldInstanceSpawner>,

    power_query: Query<'w, 's, (), With<PowerRuneRoot>>, // 声明了但本函数未读取（保留 With<PowerRuneRoot> 过滤）
    names: Query<'w, 's, &'static Name>,                 // 节点名
    children: Query<'w, 's, &'static Children>,          // 子节点，用于递归打标签
}

/// 装配入口：由 scene.rs 的场景加载流程显式调用（不是事件驱动）。
/// `In((_root, instance))`：`In` 表示这是"带输入参数的运行器"（`w.run` 传入的实参），
/// 这里拿到场景根实体与场景实例 ID；`_root` 带下划线表示暂不使用。
pub(crate) fn setup_power_rune(
    In((_root, instance)): In<(Entity, InstanceId)>,
    mut param: PowerRuneParam,
    mut creator: StatefulAppearanceCreator,
) {
    // 把 `names` 查询先"挪"进局部变量，避免后面同时借用 `param` 的其它字段。
    let names = param.names;
    // 枚举该实例包含的所有实体，取其名字，折叠成"名字 → 实体"的 HashMap。
    // `filter_map` 跳过没有名字的实体；`fold` 把迭代器累积进一张表。
    let mut name_map = param
        .scene_spawner
        .iter_instance_entities(instance)
        .filter_map(|entity| names.get(entity).map(|n| (n.as_str(), entity)).ok())
        .fold(HashMap::new(), |mut m, (name, entity)| {
            m.insert(name, entity);
            m
        });

    // 一条名字都没匹配到：多半是建模资源不对，直接返回。
    if name_map.is_empty() {
        return;
    }

    // 找出所有"面"节点：名字形如 `FACE_{数字}`，数字后不能再有下划线（否则就是子节点）。
    // `strip_prefix` 去掉 "FACE_" 得到数字部分；解析成 usize 作为面序号。
    let mut faces: Vec<(usize, Entity)> = name_map
        .iter()
        .filter_map(|(name, &entity)| {
            let rest = name.strip_prefix("FACE_")?; // `?`：不匹配时返回 None（跳过该节点）
            if rest.contains('_') {
                return None; // 还有下划线 = 是 FACE_i 的子节点，不是面本身
            }
            let index = rest.parse::<usize>().ok()?; // 解析失败也跳过
            Some((index, entity))
        })
        .collect();

    faces.sort_by_key(|(idx, _)| *idx); // 按面序号排序，保证处理顺序确定
    if faces.is_empty() {
        return;
    }

    // 红蓝两队的旋转方向相反：先给红队随机抽一个方向，蓝队取其反。
    let red_clockwise = rand::rng().random_bool(0.5);

    for (index, face_entity) in faces {
        // 用面序号的二进制第 2 位（值 2）判断大小机关：置位为大，否则为小。
        // `index & 2 > 0`：按位与，等价于"index 的第 2 位是否为 1"。
        let mode = if index & 2 > 0 {
            RuneMode::Large
        } else {
            RuneMode::Small
        };

        // 面根灯的熄灭 / 点亮两级节点。
        let deactivated = name_map.remove(format!("FACE_{}_R_UNPOWERED", index).as_str());
        let activated = name_map.remove(format!("FACE_{}_R_POWERED", index).as_str());

        // 构建 5 个靶位；失败（None）则跳过这个面。
        let Some(targets) =
            build_targets(index, face_entity, &mut name_map, &mut param, &mut creator)
        else {
            continue;
        };

        // 从面序号最低位判断阵营：奇数 = 红，偶数 = 蓝。
        let team = if (index & 1) > 0 {
            Team::Red
        } else {
            Team::Blue
        };
        // 顺时针方向按阵营取（红蓝相反）。
        let clockwise = match team {
            Team::Red => red_clockwise,
            Team::Blue => !red_clockwise,
        };
        // 组装整面可视化：根灯用同一实体占满 4 个阶段（亮/灭由材质控制），
        // 5 个靶位沿用上面构建好的控制器。
        let mut visuals = PowerRuneVisuals::new(
            Controller::new_visibility(deactivated, activated, activated, activated),
            targets,
        );
        let mechanism = PowerRuneMechanism::new(mode);
        // 立即应用一次外观，让初始显示与状态机一致（避免第一帧闪烁）。
        visuals.apply(mode, mechanism.state(), &mut creator.appearance);

        // 把全部组件插到面实体上，装配完成。
        param.commands.entity(face_entity).insert((
            PowerRune::new(team, mode),
            mechanism,
            PowerRuneRotation::new(clockwise),
            visuals,
        ));
    }
}

/// 装配插件：本文件没有需要在 build 时注册的系统（装配由场景流程触发）。
#[derive(Default)]
pub(super) struct PowerRuneConstructorPlugin;

impl bevy::app::Plugin for PowerRuneConstructorPlugin {
    fn build(&self, _app: &mut bevy::app::App) {
        // `setup_power_rune` is invoked by the scene load sequence, not by an event.
        // 中文：`setup_power_rune` 由场景加载流程调用，而不是由事件触发，所以这里无需注册。
    }
}
