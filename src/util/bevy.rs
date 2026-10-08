//! 三个零散的 Bevy 操作小工具（都不是系统，而是被系统调用的普通辅助函数）。
//!
//! - `drain_entities_by`：从"名字 → 实体"的映射表里，按条件把匹配项**取出并删除**，返回实体表；
//! - `insert_all_child`：从某实体出发，沿 `Children` 关系把整棵子树（含自身）都插上同一类组件；
//! - `set_visibility`：改某个实体的可见性，并把"查不到实体"的错误原样回报给调用方。

// 标准库容器：HashMap 是"键值映射"，VecDeque 是"双端队列"（这里当先进先出的队列用）。
use std::collections::{HashMap, VecDeque};

use bevy::{
    camera::visibility::Visibility, // 可见性枚举：Visible / Hidden / Inherited 等
    ecs::{
        bundle::Bundle,        // Bundle：一组可一起挂到实体上的组件
        entity::Entity,        // Entity：实体的轻量 ID
        hierarchy::Children,   // Children：实体的"子实体列表"组件（父子关系的下行边）
        system::{Commands, Query}, // Commands=延迟命令队列；Query=只读查询
    },
    // Bevy 提供的 HashSet（底层用更快的哈希器）；与 std 的 HashSet 用法相同。
    platform::collections::HashSet,
};

/// 从 `name_map`（名字 → 实体）里，按 `predicate` 判定，取出所有匹配条目对应的实体并返回；
/// 匹配到的条目会从表中移除（drain = 抽干）。调用方拿到 `Vec<Entity>` 做后续处理。
/// - `predicate` 接收的是**键的引用** `&T`（即名字），返回是否要取出该项。
pub fn drain_entities_by<T, F: Fn(&T) -> bool>(
    name_map: &mut HashMap<T, Entity>,
    predicate: F,
) -> Vec<Entity> {
    name_map
        // `extract_if`：边遍历边"移除满足条件的条目"，返回一个迭代器产出 (键, 值)。
        // 它要求 `&mut` 独占借用整张表（正是参数写 `&mut HashMap` 的原因）。
        .extract_if(|k, _v| predicate(k))
        // 每个条目是 (键, 值) 元组，`.1` 取值（即 Entity）。`_v` 表里已对值弃用命名。
        .map(|v| v.1)
        // `collect` 把迭代器收进 `Vec<Entity>`（返回类型由函数签名决定）。
        .collect()
}

/// 把 `bundle()` 生成的组件**逐个**插入 `root` 及其整棵子树（BFS 广度优先）。
/// - `B: Bundle + Clone`：要插入的组件包类型；
/// - `bundle: F` 是"生产 Bundle 的工厂闭包"，每遇到一个实体就调用一次得到**新的** Bundle
///   （Bundle 在 insert 时会被消费，不能多个实体共用同一个值）。
pub fn insert_all_child<B: Bundle + Clone, F: Fn() -> B>(
    commands: &mut Commands,
    root: Entity,
    query: &Query<&Children>,
    bundle: F,
) {
    // `HashSet` 记录"已访问实体"，防止同一节点被重复处理（子节点被多个父共享时会出现）。
    let mut set = HashSet::new();
    // `VecDeque` 当队列：`push_back` 入队、`pop_front` 出队 → 得到广度优先遍历顺序。
    let mut stack = VecDeque::new();
    stack.push_back(root);

    // `while let Some(x) = ...`：队列非空就取一个实体处理。
    while let Some(entity) = stack.pop_front() {
        // `HashSet::insert` 返回 bool：true=首次加入，false=之前已有。
        // 已经访问过就 `continue` 跳过（顺带切断了潜在的关系环）。
        if !set.insert(entity) {
            continue;
        }
        // 入队"插入组件"这条延迟命令；`.insert(bundle())` 每次现场生成一个新 Bundle。
        commands.entity(entity).insert(bundle());
        // `query.get` 返回 Result（该实体可能没有 Children 组件），`.ok()`→Option 靠 `if let` 取用。
        if let Ok(children) = query.get(entity) {
            // `children.iter()` 产出 `&Entity`，`.copied()` 借出即复制成 `Entity`（Entity 是 Copy）。
            stack.extend(children.iter().copied());
        }
    }
}

/// 把 `entity` 的可见性设置为 `value`。
/// 返回 `Result`：查不到该实体时把 `QueryEntityError` 交给调用方（而不是 panic）。
pub fn set_visibility(
    entity: Entity,
    value: Visibility,
    visibilities: &mut Query<&mut Visibility>,
) -> Result<(), bevy::ecs::query::QueryEntityError> {
    // `get_mut` 返回 Result；行尾的 `?` 是"错误就提前返回该错误给调用方"的语法糖。
    let mut visibility = visibilities.get_mut(entity)?;
    // `visibility` 是 `Mut<Visibility>`（可变借用包装）；`*` 解引用后整体赋值。
    *visibility = value;
    // 一切正常，返回"成功且无数据"的 `Ok(())`。
    Ok(())
}
