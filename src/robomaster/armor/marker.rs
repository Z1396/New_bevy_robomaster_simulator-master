//! 装甲标记点：每块装甲上供视觉识别用的 4 个点（顶点）。
//!
//! 游戏语言：视觉程序要从相机画面里认出装甲板及其朝向，靠的就是这块装甲上固定的 4 个角点。
//! 本模块把模型网格里的那 4 个顶点读出来，装进组件供其它系统使用。
//!
//! 协作者：`extract_vertices`（定义在 construct.rs）负责"取全部顶点"；本模块的
//! `extract_markers` 在其之上加一条硬约束——顶点数**必须**正好是 4。
//! construct.rs 的 `process_marker` 会调用它来生成标记数据。
//!
//! 新手阅读顺序：很短——先看 `MarkerData`，再读 `extract_markers`。

use crate::robomaster::prelude::extract_vertices;
use bevy::math::Vec3;
use bevy::mesh::Mesh;
// `Deref`/`DerefMut`：派生宏，让 MarkerData 能"像数组一样"直接索引/迭代——
// 这是 Rust 的"解引用强制转换"（如 marker[0] 会自动变成 marker.0[0]）。
use bevy::prelude::{Component, Deref, DerefMut};

/// 4 个标记点坐标（局部空间，单位：米）。`Deref/DerefMut` 让它可直接当 `[Vec3; 4]` 使用。
/// 派生 `Clone`：构造时既写进组件、又作为返回值传出，需要复制一份。
#[derive(Component, Deref, DerefMut, Clone)]
pub struct MarkerData(pub [Vec3; 4]);

/// 从网格提取标记点，并强制"恰好 4 个顶点"的契约。
/// 返回 `Option<[Vec3; 4]>`：`extract_vertices` 失败（无网格/空）时返回 None；
/// 顶点数不为 4 时直接 `panic!`——这属于模型资产错误，宁可崩溃也不要静默错位。
pub fn extract_markers(mesh: &Mesh) -> Option<[Vec3; 4]> {
    let vertices = extract_vertices(mesh)?;
    if vertices.len() != 4 {
        // `panic!`：不可恢复错误，带格式化消息直接终止进程；调试时能立刻定位坏资产。
        panic!("Expected 4 vertices but got {}", vertices.len());
    }
    // `try_into`：把 `Vec<Vec3>` 转成定长 `[Vec3; 4]`（长度不符会返回 Err）；
    // 上面已保证长度为 4，故 `.unwrap()` 安全。
    Some(vertices.as_slice().try_into().unwrap())
}
