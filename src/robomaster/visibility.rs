//! "有状态外观"基础设施：让实体能随状态（未激活/激活中/已激活/已完成）切换**材质或可见性**。
//!
//! 典型用途：能量机关的灯带、装甲点亮的贴图、机关激活时的发光切换——同一批实体在
//! 不同状态下显示不同样子。本文件把"怎么改外观"抽象成三件套：
//! - `Activation`：四态枚举，是"当前是什么状态"的统一词汇；
//! - `Control` trait：`set(state, param)` 一个方法，把某状态应用到某个实体（或其整棵子树）；
//! - `Controller` 枚举：`Material` / `Visibility` / `Combined` 三种具体表现控制器。
//!
//! 外加两个便利工具：`StatefulAppearance`（打包"材质库 + 可见性查询"的 SystemParam，
//! 免得每个系统写一长串参数）、`MaterialCache`（把材质"熄灭副本"缓存起来，避免重复建材质）。
//! 本文件还提供三个 `macro_export` 宏（`visibility!` / `material!` / `internal_assign_hack!`），
//! 供 construct 等模块用声明式语法快速拼出 Controller。
//!
//! 面向新手：本文件是"trait + 枚举 + SystemParam + 宏"的综合范例，读时先抓 `Control::set`
//! 这条主线，再回头看宏只是"减轻样板代码"的语法糖。

use crate::util::bevy::set_visibility;
use bevy::app::App;
use bevy::asset::AssetId;
use bevy::color::LinearRgba;
use bevy::ecs::system::lifetimeless::{Read, Write};
use bevy::prelude::{Children, Plugin, Resource};
use bevy::{
    asset::{Assets, Handle},
    camera::visibility::Visibility,
    ecs::{
        entity::Entity,
        system::{Query, ResMut, SystemParam},
    },
    pbr::{MeshMaterial3d, StandardMaterial},
};
use std::collections::HashMap;
use std::hash::Hash;

/// 一次性打包"改外观"所需的四个查询/资源，作为系统参数注入。
/// `#[derive(SystemParam)]`：让自定义结构体也能当系统函数参数用——Bevy 会自动把每个字段
/// 当作独立系统参数解析并注入，从而把冗长的参数列表收拢成一个名字。
/// 四个字段缺一不可：
/// - `materials`：材质资源（只读时也要 `ResMut`，因为 `MaterialCache` 会往里 add 新材质）；
/// - `cache`：熄灭材质缓存（见 `MaterialCache`）；
/// - `mesh_materials`：**可写**查询每实体的 `MeshMaterial3d`（换材质就是改它的句柄）；
/// - `visibilities`：**可写**查询每实体的 `Visibility`（显隐切换用它）。
#[derive(SystemParam)]
pub struct StatefulAppearance<'w, 's> {
    materials: ResMut<'w, Assets<StandardMaterial>>,
    cache: ResMut<'w, MaterialCache>,
    mesh_materials: Query<'w, 's, Write<MeshMaterial3d<StandardMaterial>>>,
    visibilities: Query<'w, 's, Write<Visibility>>,
}

/// 控制器统一接口：把某个激活状态"应用"出去。
/// `&mut self`：控制器自身可能被更新（如缓存上次状态）；`param` 是上面那个打包好的参数。
/// 实现者只需关心"给定状态该显示成什么样"，不用管查询/资源怎么拿。
pub trait Control {
    fn set(&mut self, state: Activation, param: &mut StatefulAppearance);
}

/// 四态激活模型：机关从"未激活"经"激活中"到"已激活/已完成"的生命周期。
/// 显式指定判别值（`= 0..3`）是**跨文件契约**：外部（如 JSON 导出、上位机）按这些整数解读状态，
/// 故顺序不可随意改动。`Eq + Hash` 使其可作 map 键或精确比较（浮点做不到，枚举可以）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Activation {
    Deactivated = 0,
    Activating = 1,
    Activated = 2,
    Completed = 3,
}

/// 三种表现控制器——"同一状态如何呈现"的具体实现。
/// 设计要点：每个变体都把"四个状态各自对应什么"按**固定顺序**存成四个值
/// （deactivated / activating / activated / completed），`set` 时按当前状态取其一。
pub enum Controller {
    // 换材质方案：`(实体, 未激活材质, 激活中材质, 已激活材质, 已完成材质)`。
    // 四个 `Handle<StandardMaterial>` 是四个状态各自的材质句柄（`Handle` 是引用计数的资产句柄，
    // 克隆成本极低，只是拷贝一个句柄）。
    Material(
        Entity,
        Handle<StandardMaterial>,
        Handle<StandardMaterial>,
        Handle<StandardMaterial>,
        Handle<StandardMaterial>,
    ),

    // 显隐方案：四个 `Option<Entity>` 分别指"该状态要让哪个实体可见"，
    // 其余实体会被隐藏。`Option` 表示该状态可以没有对应的实体。
    Visibility(
        Option<Entity>,
        Option<Entity>,
        Option<Entity>,
        Option<Entity>,
    ),

    // 组合方案：把一批 Controller 打包成一个，`set` 时逐个下发（常用于"一个逻辑部件拆成多个网格"）。
    Combined(Vec<Controller>),
}

/// `Controller` 是 `Control` 的核心实现：按当前状态四选一，再落到材质或可见性上。
impl Control for Controller {
    fn set(&mut self, state: Activation, param: &mut StatefulAppearance) {
        // `match self` 对枚举做模式匹配；每个分支把变体里的字段解构成局部名字。
        match self {
            // 材质分支：先按状态挑出该用的句柄，再写回该实体的 MeshMaterial3d。
            Self::Material(entity, deactivated, activating, activated, completed) => {
                // 内层 match：把 Activation 映射到四个句柄中的那个。
                // 注意这里拿到的是**引用**（`self` 是 `&mut`），下面 `.clone()` 才真正复制句柄。
                let apply = match state {
                    Activation::Deactivated => deactivated,
                    Activation::Activating => activating,
                    Activation::Activated => activated,
                    Activation::Completed => completed,
                };
                // `get_mut` 返回 Result：实体没有 MeshMaterial3d 组件时安静跳过（不 panic）。
                // `mesh_material.0` 就是那份材质句柄，直接替换即完成"换材质"。
                if let Ok(mut mesh_material) = param.mesh_materials.get_mut(*entity) {
                    mesh_material.0 = apply.clone()
                }
            }
            // 显隐分支：算出"要让谁显示(show)、让谁隐藏(hide)"，hide 是个四元素数组。
            Self::Visibility(deactivated, activating, activated, completed) => {
                // 每个状态：把当前状态对应的实体作为 show，其余三个塞进 hide 数组一并隐藏。
                let (show, hide) = match state {
                    Activation::Deactivated => (deactivated, [activating, activated, completed]),
                    Activation::Activating => (activating, [deactivated, activated, completed]),
                    Activation::Activated => (activated, [deactivated, activating, completed]),
                    Activation::Completed => (completed, [deactivated, activating, activated]),
                };
                // `into_iter().flatten()`：把 `[Option<Entity>; 3]` 摊平成"只含 Some 的实体迭代器"，
                // 跳过所有 None（对应"该状态没有这个位置的实体"）。
                for entity in hide.into_iter().flatten() {
                    // `set_visibility`：本项目工具函数，写 Visibility 组件；结果按理必成功故 unwrap。
                    set_visibility(*entity, Visibility::Hidden, &mut param.visibilities).unwrap();
                }
                // 若该状态确实有 show 实体，则显式设为 Visible（可能从上次的 Hidden 恢复）。
                if let Some(show) = show {
                    set_visibility(*show, Visibility::Visible, &mut param.visibilities).unwrap();
                }
            }
            // 组合分支：递归下发给每个子控制器，状态原样透传。
            Self::Combined(vec) => {
                for c in vec {
                    c.set(state, param);
                }
            }
        }
    }
}

/// `Controller` 的三个构造函数：直接包裹对应变体，让调用端更直观。
/// （相比直接写 `Controller::Material(...)`，`Controller::new_material(...)` 可读性更好。）
impl Controller {
    // 构造显隐控制器：四个参数依次对应四种状态要显示/隐藏的实体（None 表示该状态无实体）。
    pub fn new_visibility(
        deactivated: Option<Entity>,
        activating: Option<Entity>,
        activated: Option<Entity>,
        completed: Option<Entity>,
    ) -> Self {
        Self::Visibility(deactivated, activating, activated, completed)
    }

    // 构造材质控制器：四个材质句柄依次对应四种状态。
    pub fn new_material(
        entity: Entity,
        deactivated: Handle<StandardMaterial>,
        activating: Handle<StandardMaterial>,
        activated: Handle<StandardMaterial>,
        completed: Handle<StandardMaterial>,
    ) -> Self {
        Self::Material(entity, deactivated, activating, activated, completed)
    }

    // 构造组合控制器：把一批子控制器打包。
    pub fn new_combined(v: Vec<Controller>) -> Self {
        Self::Combined(v)
    }
}

/// 全局资源：缓存"把某材质熄灭后的副本"，避免每次点亮/熄灭都新建一份材质。
/// `#[derive(Resource)]`：标记为 Bevy 全局单例资源（由 `StatefulAppearancePlugin` 初始化）。
/// `muted` 以原材质 id 为键、熄灭副本句柄为值——"一个原材质只做一份熄灭副本"。
#[derive(Resource, Default)]
struct MaterialCache {
    muted: HashMap<AssetId<StandardMaterial>, Handle<StandardMaterial>>,
}

impl MaterialCache {
    /// 取（必要时惰性创建）某材质的"熄灭版"句柄。
    /// "熄灭" = 发光色置黑 + 曝光权重置 0，即关掉自发光但保留基础色，视觉上像灯灭了。
    fn ensure_muted(
        &mut self,
        handle: &Handle<StandardMaterial>,
        materials: &mut Assets<StandardMaterial>,
    ) -> Handle<StandardMaterial> {
        // 缓存命中直接返回（只克隆句柄，零拷贝）。
        let id = handle.id();
        if let Some(existing) = self.muted.get(&id) {
            return existing.clone();
        }
        // 缓存未命中：捞出原材质本体以便克隆。
        // `let ... else { return ... };`：解构失败（材质不存在）就原样返回入参句柄，优雅兜底。
        let Some(original) = materials.get(handle) else {
            return handle.clone();
        };
        // 克隆原材质后抹掉自发光。`emissive = BLACK` 关发光；`exposure_weight = 0.0` 让曝光不再影响它。
        // 注意：这里动的是 `Materials` 资产（`Assets<StandardMaterial>`），add 后得到新句柄。
        let mut clone = original.clone();
        clone.emissive = LinearRgba::BLACK;
        clone.emissive_exposure_weight = 0.0;
        let muted_handle = materials.add(clone);
        // 记入缓存，下次同 id 直接复用。
        self.muted.insert(id, muted_handle.clone());
        muted_handle
    }
}

/// 构造宏回调的统一入参类型别名：`(要处理的实体, 可写外观参数)`。
/// `'g` 是"每次调用"的生命周期——即回调可能被反复调用（对实体及其后代），每调用一次借出一次参数。
/// 这就是 `visibility!` / `material!` 生成的闭包所接收的 `value` 类型。
pub type ConstructData<'w, 's, 'g> = (Entity, &'g mut StatefulAppearance<'w, 's>);

impl<'w, 's> StatefulAppearance<'w, 's> {
    // 判断某实体当前是否可见：查询不到 Visibility 组件时按"可见"处理（多数网格默认可见）。
    // `v != Visibility::Hidden`：只要不是明确隐藏（含 Inherited 继承态）就算可见。
    pub fn visible(&self, entity: Entity) -> bool {
        if let Ok(v) = self.visibilities.get(entity) {
            v != Visibility::Hidden
        } else {
            true
        }
    }
}

/// 批量创建控制器：给一批实体（以及它们各自的后代）逐个跑构造闭包，拼成一个 `Combined`。
/// `#[derive(SystemParam)]`：同样把内部字段打包成系统参数，并对外暴露 `appearance`。
#[derive(SystemParam)]
pub struct StatefulAppearanceCreator<'w, 's> {
    pub appearance: StatefulAppearance<'w, 's>,
    // 只读 `Children` 查询，用于向下遍历实体子树。
    children: Query<'w, 's, Read<Children>>,
}

impl<'w, 's> StatefulAppearanceCreator<'w, 's> {
    /// 对**单个实体及其所有后代**应用闭包 `f`，把成功产出的 Controller 合成一个 `Combined`。
    /// 泛型约束 `F: for<'g> Fn(ConstructData<'w,'s,'g>) -> Result<Controller, ()>` 是**高阶生命周期**
    /// （HRTB）：表示该闭包对任意 `'g` 都能调用。返回值用 `Result`，闭包可对被跳过/不匹配的实体返回 `Err`。
    fn as_combined<F: for<'g> Fn(ConstructData<'w, 's, 'g>) -> Result<Controller, ()>>(
        &mut self,
        entity: Entity,
        f: &F,
    ) -> Controller {
        let mut swaps = vec![];
        // 先处理实体自身；`if let Ok(v)` 忽略闭包返回的 Err（表示"这个实体不需要控制"）。
        if let Ok(v) = f((entity, &mut self.appearance)) {
            swaps.push(v);
        }
        // `iter_descendants`：以该实体为根，深度优先遍历所有子孙节点。
        for child in self.children.iter_descendants(entity) {
            if let Ok(v) = f((child, &mut self.appearance)) {
                swaps.push(v);
            }
        }
        Controller::new_combined(swaps)
    }

    /// 对一批实体分别调用 `as_combined`（每个实体各含其子树），再合成一个大 `Combined`。
    /// 与 `material_raw`/构造闭包配合，实现"按实体根 + 后代批量生成控制器"。
    pub fn create_controller<F: for<'g> Fn(ConstructData<'w, 's, 'g>) -> Result<Controller, ()>>(
        &mut self,
        entities: Vec<Entity>,
        f: F,
    ) -> Controller {
        let mut controllers = Vec::new();
        for entity in entities {
            controllers.push(self.as_combined(entity, &f));
        }
        Controller::new_combined(controllers)
    }
}

/// 材质构造的底层适配器：把"只关心 (实体, 亮材质, 灭材质) 的简单闭包 `f`"包装成标准构造闭包。
/// 关键动作：进实体时把它当前的材质取走存成 `on`，把它的槽位换成由 `cache` 提供的 `off`（熄灭版）。
/// 这样闭包只需决定四个状态各用 on/off 的哪种组合，不必操心底层缓存与句柄管理。
/// 返回 `impl Fn(...)`（返回闭包的"不透明类型"写法，调用方无需知道闭包具体类型）。
pub fn material_raw<F>(f: F) -> impl Fn(ConstructData) -> Result<Controller, ()>
where
    F: Fn(Entity, Handle<StandardMaterial>, Handle<StandardMaterial>) -> Controller,
{
    move |value: ConstructData| -> Result<Controller, ()> {
        let (entity, param) = value;
        // 实体必须有 MeshMaterial3d 才可换材质，否则返回 Err 让上层跳过。
        if let Ok(mut mesh_material) = param.mesh_materials.get_mut(entity) {
            // 先确保存在"熄灭版"（会写入缓存）。
            let off = param
                .cache
                .ensure_muted(&mesh_material.0, &mut param.materials);
            // `std::mem::replace`：把槽位里原有的"亮材质"句柄取出来（同时把槽位填成 off），
            // 一次操作完成"读旧值 + 写新值"，避免额外克隆。取出的 old 就是 `on`。
            let on = std::mem::replace(&mut mesh_material.0, off.clone());
            Ok(f(entity, on, off))
        } else {
            Err(())
        }
    }
}

/// 内部辅助宏（本文件另两个宏的"共用地基"）：按状态名把值写进对应的局部变量。
/// 它解决的问题：`visibility!` / `material!` 里已经声明了 `_deactivated/_activating/_activated/_completed`
/// 四个局部变量，但**该给哪个变量赋值**取决于宏调用时写的状态名（如 `deactivated`）。
/// 宏匹配到状态标识符后会展开成对应变量的赋值语句，从而无需重复写四遍。
///
/// `#[macro_export]`：把宏导出到 crate 根，供其它 crate/模块以 `$crate::internal_assign_hack!` 使用。
/// `(@internal ...)` 这种以 `@` 开头的"标签"是宏的惯用内部规则写法，用于区分外层调用与内部递归。
/// 注意各臂参数收尾的变量名并非都用得上（如 deactivated 臂只写 `$d`），这是刻意的"占位"。
#[macro_export]
macro_rules! internal_assign_hack {
    // this is literally a hack（这就是个不优雅的小技巧）
    // 状态=deactivated 时：把值写进第 3 个变量（约定为 _deactivated）。
    (@internal deactivated, $value:expr, $d:ident, $a:ident, $ac:ident, $c:ident) => {
        $d = $value;
    };
    // 状态=activating 时：写进 _activating。
    (@internal activating, $value:expr, $d:ident, $a:ident, $ac:ident, $c:ident) => {
        $a = $value;
    };
    // 状态=activated 时：写进 _activated。
    (@internal activated, $value:expr, $d:ident, $a:ident, $ac:ident, $c:ident) => {
        $ac = $value;
    };
    // 状态=completed 时：写进 _completed。
    (@internal completed, $value:expr, $d:ident, $a:ident, $ac:ident, $c:ident) => {
        $c = $value;
    };
}

/// 声明式构造"显隐控制器"的宏。
/// 用法：`visibility!(deactivated, activated)` —— 列出**哪些状态**要显示当前实体（`value.0`），
/// 未列出的状态其对应字段保持 `None`（即该状态下不显示这个实体）。
/// 展开结果是一个可直接交给 `material_raw`/`create_controller` 的标准构造闭包。
/// 语法：`$($state:ident),* $(,)?` 匹配"逗号分隔的状态标识符列表"，允许末尾多余逗号。
#[macro_export]
macro_rules! visibility {
    ($($state:ident),* $(,)?) => {
        |value: $crate::robomaster::visibility::ConstructData| -> Result<$crate::robomaster::visibility::Controller, ()> {
            use ::std::option::Option::{Some, None};
            use $crate::internal_assign_hack;
            // 4 optional fields（四个可选字段，初始都是 None，由下面的宏调用填 Some）
            let mut _deactivated = None;
            let mut _activating = None;
            let mut _activated = None;
            let mut _completed = None;

            // `$( ... )*`：对每个列出的状态重复展开——把 Some(当前实体) 写进对应变量。
            $(
                internal_assign_hack!(@internal $state, Some(value.0), _deactivated, _activating, _activated, _completed);
            )*

            Ok($crate::robomaster::visibility::Controller::new_visibility(_deactivated, _activating, _activated, _completed))
        }
    };
}

/// 声明式构造"材质控制器"的宏。
/// 用法：`material!(on = {activated, completed})` —— 列出**哪些状态**要显示"亮材质(on)"，
/// 其余状态一律用"灭材质(off)"。
/// 与 `visibility!` 的差异：隐式依赖 `material_raw`，由它把实体当前材质取成 on、缓存成 off 再回调，
/// 故这里闭包接收的 `on` / `off` 都是现成句柄，直接按状态填进四个字段即可。
/// 语法：`on = {$($on:ident),* $(,)?}` 匹配等号右侧花括号里的状态列表。
#[macro_export]
macro_rules! material {
    ( on = {$($on:ident),* $(,)?}) => {
         $crate::robomaster::visibility::material_raw(|entity, on, off| {
             use $crate::internal_assign_hack;

             // 四个状态先全部默认成"灭"，再由下面按 on 列表覆盖成"亮"。
             let mut _deactivated = off.clone();
             let mut _activating = off.clone();
             let mut _activated = off.clone();
             let mut _completed = off.clone();

             // 对每个被列为 on 的状态，把对应变量覆盖为亮材质句柄。
             $(
                internal_assign_hack!(@internal $on, on.clone(), _deactivated, _activating, _activated, _completed);
             )*
             $crate::robomaster::visibility::Controller::new_material(entity, _deactivated, _activating, _activated, _completed)
         })
    };
}

/// 本模块的插件：唯一职责是创建 `MaterialCache` 全局资源。
/// `pub(super)`：只对父模块（robomaster）可见——由 `robomaster::prelude::RoboMasterPlugins` 安装。
#[derive(Default)]
pub(super) struct StatefulAppearancePlugin;

impl Plugin for StatefulAppearancePlugin {
    fn build(&self, app: &mut App) {
        // `init_resource`：以 Default 创建 `MaterialCache`（其 muted map 一开始为空）。
        app.init_resource::<MaterialCache>();
    }
}
