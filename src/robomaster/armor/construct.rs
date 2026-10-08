//! 装甲组装的"装配车间"：扫描场景树里的 *ARMOR_ROOT* 节点，提取网格顶点，
//! 生成 Armor / ArmorParts / LightStrip / VertexData 等组件实体。
//!
//! 游戏语言：关卡/战车模型里事先摆好了若干节点（名字如 ARMOR、MARKER、VERTEX_L、
//! 灯条、贴纸等），但它们在 ECS 里只是普通 Transform+Mesh。本模块负责识别这些节点、
//! 读出它们的网格数据、挂上游戏组件，从而"把美术模型变成可命中、可识别、可计分的装甲"。
//!
//! 协作者：
//! - `marker.rs` 提供 `extract_markers`（4 点标记提取）；
//! - `util/entity_query.rs` 提供 `query!` 宏与 `HierarchyQuery`（按名字在层级里下钻查找）；
//! - `collision.rs` 用这里生成的 `Armor` 组件做命中判定；
//! - 触发源：外部（如战车/前哨站构造）给模型根插入 `ScanArmor` 组件。
//!
//! 新手阅读顺序：ScanArmor（触发标记）→ ArmorConstructor（系统参数工具箱）
//! → process_armor_root（核心组装）→ extract_vertices/extract_triangle_vertices（顶点提取）
//! → insert/sync_armor_stickers（两个系统）→ 插件注册。

// `query!` 是本项目的实体查询宏（定义在 src/util/entity_query.rs）。它用三类字面量前缀
// 表达匹配方式：`.."NAME"`（后缀 suffix）、`"NAME"..`（前缀 prefix）、`"NAME"`（精确 exact）；
// 尾部还可接 `...`（先 any() 进入子层继续下钻）；结尾不加则默认 `.one()`（取恰好一个）。
use crate::query;
use crate::robomaster::prelude::{ArmorLabel, ArmorSpec, MarkerData, Team, extract_markers};
// `HierarchyQuery`：把 child_of / children / name 三个查询打包，支持从某实体开始"逐层下钻"
// 按名字找实体（与 query! 宏同源，见 util/entity_query.rs）。
use crate::util::entity_query::HierarchyQuery;
// avian3d 物理相关：ColliderConstructor 碰撞体构造器、ColliderConstructorHierarchy
// 按层级自动为网格生成碰撞体、CollisionLayers 碰撞层掩码（决定谁能碰谁）、
// TrimeshFlags 三角网格碰撞体的选项。
use avian3d::prelude::{
    ColliderConstructor, ColliderConstructorHierarchy, CollisionLayers, TrimeshFlags,
};
use bevy::app::App;
// `SystemParam` 派生宏：把多个查询/资源打包成一个"系统参数"，让系统函数参数表变短。
use bevy::ecs::system::SystemParam;
// `Read<T>`：把查询项标记为只读（等价于 `&T` 但不触发可变访问冲突），用于 SystemParam 字段。
use bevy::ecs::system::lifetimeless::Read;
// Bevy 网格底层类型：Indices 顶点索引表、PrimitiveTopology 图元拓扑、VertexAttributeValues 顶点属性值。
use bevy::mesh::{Indices, PrimitiveTopology, VertexAttributeValues};
use bevy::prelude::{
    Added, Assets, Changed, ChildOf, Children, Commands, Component, Entity, Mesh, Mesh3d, Name,
    Plugin, Query, Res, Update, Vec3, Visibility, With, info,
};
// 原子类型：`AtomicUsize` 无锁、跨线程安全，用于给每块装甲发一个全局唯一 ID（见 process_armor_root）。
use std::sync::atomic::{AtomicUsize, Ordering};

/// "待扫描的装甲"标记组件：挂在模型根上，声明这批装甲属于哪个阵营、什么规格。
/// 该组件一被插入，下方 `insert` 系统就靠变更检测 `Added<ScanArmor>` 触发一次扫描组装。
/// 组件会保留在实体上（不自动移除），后续可复用它触发重建。
#[derive(Component, Debug)]
pub struct ScanArmor {
    pub team: Team,      // 阵营（红/蓝），决定用哪套灯条、以及命中归属
    pub spec: ArmorSpec, // 装甲规格（大/小 + 编号）
}

impl ScanArmor {
    /// `const fn`：可在编译期常量里构造（本项目多个 `RobotConfig` 常量依赖它）。
    pub const fn new(team: Team, spec: ArmorSpec) -> Self {
        Self { team, spec }
    }
}

/// 装甲侧边的顶点数据（从网格提取），挂在 VERTEX_L / VERTEX_R 子实体上。
/// 派生 `Clone`：构造时既作为返回值、又要写进组件，需要复制一份。
#[derive(Component, Clone, Debug)]
pub struct VertexData {
    pub side: Side,        // 左/右
    pub points: Vec<Vec3>, // 顶点坐标（局部空间，单位：米）
}

/// 灯条（装甲侧面发光条）数据，挂在灯条网格实体上。
/// `visibility_id` 供着色器按 ID 单独控制显隐；`mask_triangles` 是用于遮罩的三角面顶点序列。
#[derive(Component, Clone, Debug)]
pub struct LightStrip {
    pub side: Side,
    pub visibility_id: u32,
    pub mask_triangles: Vec<Vec3>,
}

/// 一块可命中的装甲板组件——命中判定（collision.rs）与统计都会读它。
#[derive(Component, Clone, Debug)]
pub struct Armor {
    pub name: String,      // 实体名
    pub team: Team,        // 阵营
    pub spec: ArmorSpec,   // 规格
    pub label: ArmorLabel, // 编号（由 spec 推出，便于直接比较）
}

/// 贴纸实体组件：记录它挂在哪个装甲根下（root）以及它代表哪个编号（label）。
/// 派生 `Copy`：只有两个字段且都很小，直接按值复制比借用更省事。
#[derive(Component, Clone, Copy, Debug)]
pub struct ArmorSticker {
    pub root: Entity,
    pub label: ArmorLabel,
}

/// "当前选中的贴纸编号"，挂在装甲根上；改它即可让名下贴纸显隐切换。
#[derive(Component, Clone, Debug)]
pub struct ArmorStickerSelection {
    pub label: ArmorLabel,
    pub sequence_index: usize, // 在 sequence_small() 里的下标，供循环切换用
}

impl ArmorStickerSelection {
    /// 按编号新建，并把下标初始化到"小装甲序列"中的对应位置。
    pub fn new(label: ArmorLabel) -> Self {
        Self {
            label,
            sequence_index: ArmorLabel::index_from_small(label),
        }
    }

    /// 沿 `sequence_small()` 前进一格（环形：到末尾回到开头），返回新编号。
    pub fn advance_debug_sequence(&mut self) -> ArmorLabel {
        let sequence = ArmorLabel::sequence_small();
        self.sequence_index += 1;
        // `%=` 取模实现环形：下标始终落在 [0, len) 内，越界后自动回绕到开头。
        self.sequence_index %= sequence.len();
        self.label = sequence[self.sequence_index];
        self.label
    }
}

/// 装甲的左右侧（一块装甲有两侧的灯条/顶点）。
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    /// 转成数组下标：左=0、右=1，配合 `[T; 2]` 数组按侧索引。
    pub const fn index(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Right => 1,
        }
    }
}

/// 装甲构造器的"工具箱"：把组装过程需要的输入/输出打包成一个系统参数。
/// 字段全是 `Query`/`Commands`/`Res`——派生 `SystemParam` 后，系统函数只写一个
/// `constructor: ArmorConstructor` 参数即可，避免超长参数表（Bevy 系统参数过多会报错）。
/// 生命周期 `'w`(World) 与 `'s`(State) 由引擎在调用时提供，无需手动标具体值。
#[derive(SystemParam)]
pub struct ArmorConstructor<'w, 's> {
    commands: Commands<'w, 's>,                    // 延迟写操作（插组件/销毁实体）
    children: Query<'w, 's, Read<Children>>,       // 向下找子实体
    child_of: Query<'w, 's, Read<ChildOf>>,        // 向上找父实体（沿祖先链回溯）
    // 只查"有父节点"的实体名——根实体通常无名，用 With<ChildOf> 过滤更省内存。
    name: Query<'w, 's, Read<Name>, With<ChildOf>>,
    mesh_query: Query<'w, 's, Read<Mesh3d>>,       // 判断某实体是否挂了网格
    collision_layers: Query<'w, 's, Read<CollisionLayers>>, // 读原碰撞层，复用给装甲碰撞体
    mesh_assets: Res<'w, Assets<Mesh>>,            // 网格资产仓库（按句柄取 Mesh 数据）
}

/// 装甲根组件：每块装甲一个，携带全局唯一 ID。挂在 *ARMOR_ROOT* 实体上。
#[derive(Component, Clone)]
pub struct ArmorRoot {
    pub id: ArmorId,
}

impl ArmorRoot {
    /// 为某侧灯条算一个唯一可见性 ID（供着色器/显隐系统定位该灯条）。
    /// 公式 `id*2 + side_index + 1`：每块装甲占两个连续 ID（左/右），+1 让 ID 从 1 起步避开 0。
    pub fn light_visibility_id(&self, side: Side) -> u32 {
        // `try_from`：usize→u32 有溢出可能，返回 Result；用 `expect` 在越界时立刻 panic 报错
        //（宁可崩溃暴露问题，也不要静默截断导致灯条 ID 冲突）。
        u32::try_from(self.id.as_usize() * 2 + side.index() + 1)
            .expect("Armor light visibility ID exceeds u32")
    }
}

/// 装甲的全局唯一编号。newtype 模式：把裸 usize 包一层，让"装甲 ID"与普通整数类型区分开。
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct ArmorId(usize);

impl ArmorId {
    /// 取出内部数字。
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

/// 一块装甲的"零件索引"：指向它下辖的标记实体、两侧灯条实体、两侧顶点实体。
/// `[Vec<Entity>; 2]`：定长数组，下标 0=左、1=右（配合 `Side::index`）。
#[derive(Component, Clone)]
pub struct ArmorParts {
    marker: Entity,
    lights: [Vec<Entity>; 2],
    vertices: [Entity; 2],
}

// 声明宏：为避免左右两套 getter 手写重复，用一个宏按"方法名 + 字段名"生成
// `fn xxx(&self, side: Side) -> Entity { self.字段[side.index()] }`。
// `$method_name:ident` / `$field:ident` 是宏参数（标识符片段），`$` 是宏变量前缀。
macro_rules! impl_side {
    ($method_name:ident, $field:ident) => {
        #[inline]   // 建议编译器内联：这种一行小函数值得内联
        #[must_use] // 返回值若被丢弃，编译器给出警告（防误用）
        pub fn $method_name(&self, side: Side) -> Entity {
            self.$field[side.index()]
        }
    };
}

impl ArmorParts {
    // 生成 `vertex(side)` 方法（读取 vertices 字段），一左一右两次调用由 `Side::index` 区分。
    impl_side!(vertex, vertices);

    /// 某侧的全部灯条实体（返回切片借用，不复制 Vec）。
    #[inline]
    #[must_use]
    pub fn lights(&self, side: Side) -> &[Entity] {
        self.lights[side.index()].as_slice()
    }

    /// 标记实体（4 点标记，供视觉识别）。
    #[inline]
    #[must_use]
    pub fn marker(&self) -> Entity {
        self.marker
    }
}

impl ArmorConstructor<'_, '_> {
    /// 按实体拿到它的 `Mesh` 数据：先查实体上的 `Mesh3d` 句柄，再从资产仓库取网格。
    /// 两级都可能失败（实体没网格 / 句柄还没加载完），故返回 `Option`，`?` 提前返回 None。
    fn get_mesh(&self, entity: Entity) -> Option<&Mesh> {
        let mesh_handle = self.mesh_query.get(entity).ok()?;
        self.mesh_assets.get(mesh_handle)
    }

    /// 处理标记子实体：从网格提取 4 个顶点，写入 `MarkerData` 组件并设为隐藏（不参与渲染）。
    /// 返回 `Option<MarkerData>`：失败（无网格/顶点数不对）时返回 None，由调用方决定是否放弃。
    fn process_marker(
        &mut self,
        entity: Entity,
        name: &str,
        armor_data: &ScanArmor,
    ) -> Option<MarkerData> {
        let mesh = self.get_mesh(entity)?;
        let vertices = extract_markers(mesh)?;

        // 日志：记录识别到的装甲身份（阵营/类型/编号）与标记点数量，便于调试扫描结果。
        info!(
            "Armor {:?}_{:?}_{:?}@'{}': Added marker with {} points",
            armor_data.team,
            armor_data.spec.armor_type(),
            armor_data.spec.label(),
            name,
            vertices.len()
        );

        // `Commands` 延迟写入：这里插入标记数据并把实体隐藏（真正生效在本帧的 apply 阶段）。
        self.commands
            .entity(entity)
            .insert((MarkerData(vertices), Visibility::Hidden));
        Some(MarkerData(vertices))
    }

    /// 提取 VERTEX_L / VERTEX_R 子实体的所有顶点（装甲侧面轮廓点，供视觉/命中使用）。
    fn extract_vertex(
        &mut self,
        entity: Entity,
        name: &str,
        armor_data: &ScanArmor,
    ) -> Option<Vec<Vec3>> {
        let mesh = self.get_mesh(entity)?;

        let vertices = extract_vertices(mesh)?;

        // 日志：记录提取到的顶点数量（用于核对模型资产是否正确）。
        info!(
            "Armor {:?}_{:?}_{:?}@'{}': Extracted {} vertices",
            armor_data.team,
            armor_data.spec.armor_type(),
            armor_data.spec.label(),
            name,
            vertices.len()
        );

        Some(vertices)
    }

    /// 组装"一个装甲根"下的全部部件（ARMOR 子节点、灯条、标记、顶点、贴纸）。
    /// - `root`：名为 *ARMOR_ROOT* 的实体；
    /// - `armor_name`：它的名字（用 String 传入，避免与 self 的查询借用冲突）；
    /// - `armor_data`：阵营/规格（来自触发此组装的 `ScanArmor`）。
    /// 返回 `Option<ArmorRoot>`：中途缺件时返回 None（这块装甲被放弃，不半途残留在场上）。
    fn process_armor_root(
        &mut self,
        root: Entity,
        armor_name: String,
        armor_data: &ScanArmor,
    ) -> Option<ArmorRoot> {
        // 用父子层级查询从 root 出发向下钻；HierarchyQuery 需要三个查询字段来构造。
        let query = HierarchyQuery::new(self.child_of, self.children, self.name);
        // `.of(root)` 从 root 开始，`.flatten()` 把可能为空的迭代器规范化（空则进入"终止"态）。
        let root_query = query.of(root).flatten();
        {
            // `.."ARMOR"`：在本层子实体里找名字以 "ARMOR" 结尾的那个（suffix 语义）；
            // 表达式末尾的 `?` 是对 query! 返回的 Option 做提前返回：找不到就放弃这块装甲。
            let armor_entity = query!(root_query, .."ARMOR")?;
            // 若原实体带碰撞层就沿用，否则用默认层；`.copied()` 把 &T 复制成 T，`.unwrap_or_default()` 兜底。
            let collision_layers = self
                .collision_layers
                .get(armor_entity)
                .copied()
                .unwrap_or_default();
            // 给 ARMOR 实体挂"按层级自动生成三角网格碰撞体"的构造器，并沿用装甲所在碰撞层。
            self.commands.entity(armor_entity).insert(
                ColliderConstructorHierarchy::new(ColliderConstructor::TrimeshFromMeshWithConfig(
                    TrimeshFlags::MERGE_DUPLICATE_VERTICES, // 合并重复顶点，缩减碰撞网格规模
                ))
                .with_default_layers(collision_layers),
            );
        }
        {
            // 先把工具字段复制成局部只读引用（否则同时借用 self 与其字段会和 self.commands 冲突）。
            let children = self.children;

            let name = self.name;
            // `iter_descendants(root)`：深度优先遍历 root 的所有后代；
            // `filter_map`：只有能查到名字的后代才保留，产出 (名字, 实体) 对。
            children
                .iter_descendants(root)
                .filter_map(|v| name.get(v).ok().map(|name| (name, v)))
                .for_each(|(elem_name, armor_elem)| {
                    // 逐个后代挂 Armor 组件（这样任意子节点被撞都能回溯到装甲身份）。
                    self.commands.entity(armor_elem).insert(Armor {
                        name: elem_name.to_string(), // `to_string()`：把借用名复制成拥有所有权的 String
                        team: armor_data.team,
                        spec: armor_data.spec,
                        label: armor_data.spec.label(),
                    });
                });
        }
        //let _base = query!(root_query, .."BASE")?;
        // （上面这行 BASE 查询被注释掉：基地装甲的组装暂未启用，保留作将来扩展的参考。）
        // 模型里两套灯条并存：`light_roots[0]` 是常规色（L_L/L_R），`light_roots[1]` 是红色（L_*_RED）。
        let light_roots = [
            [query!(root_query, .."L_L")?, query!(root_query, .."L_R")?],
            [
                query!(root_query, .."L_L_RED")?,
                query!(root_query, .."L_R_RED")?,
            ],
        ];
        // 红队用红色灯条、蓝队用常规色；另一套作为 `hide` 待删除。
        // `match` 必须穷尽 Team 的所有变体（红/蓝正好两个分支），各分支返回 (保留, 待删) 两个值。
        let (light_roots, hide) = match armor_data.team {
            Team::Red => (light_roots[1], light_roots[0]),
            Team::Blue => (light_roots[0], light_roots[1]),
        };
        // 逐条销毁不用的那套灯条（Commands 延迟执行，本帧末统一生效）。
        for hide in hide {
            self.commands.entity(hide).despawn();
        }

        // 静态原子计数器：`static` 全进程唯一一份；`AtomicUsize` 无锁且跨线程安全。
        // 每块装甲取一个自增 ID：`fetch_add(1, ...)` 原子地返回旧值并把计数 +1。
        // `Ordering::SeqCst`：最强一致性序，保证多线程下取到的 ID 不重复、可见性确定。
        static ID: AtomicUsize = AtomicUsize::new(0);
        let ar = ArmorRoot {
            id: ArmorId(ID.fetch_add(1, Ordering::SeqCst)),
        };

        // 收集每侧灯条根下所有"挂了网格"的后代实体（即真正要发光的网格）。
        // `.map(...)` 对左右两个灯条根分别处理，返回 `[Vec<Entity>; 2]`。
        let lights = light_roots.map(|light_root| {
            self.children
                .iter_descendants(light_root)
                .filter(|entity| self.mesh_query.contains(*entity)) // 只有挂网格的才算灯条
                .collect::<Vec<_>>()
        });
        // 任一侧灯条为空，说明模型不完整，放弃这块装甲。
        if lights.iter().any(Vec::is_empty) {
            return None;
        }
        // 左灯条数组配 Side::Left、右配 Side::Right，逐一提取三角面并挂 LightStrip 组件。
        for (light_meshes, side) in [(&lights[0], Side::Left), (&lights[1], Side::Right)] {
            for &light in light_meshes {
                // 提取灯条的三角面顶点；失败则用 `?` 直接放弃整块装甲。
                let mask_triangles = self.get_mesh(light).and_then(extract_triangle_vertices)?;
                self.commands.entity(light).insert(LightStrip {
                    side,
                    visibility_id: ar.light_visibility_id(side),
                    mask_triangles,
                });
            }
        }

        // `.."MARKER", ...`：先 suffix 找到 MARKER 节点，`...` 再 any() 进入其子层继续下钻；
        // 结尾不加其它标记则默认 `.one()`：找不到返回 None 则放弃这块装甲。
        let marker = query!(root_query, .."MARKER", ...)?;
        self.process_marker(marker, &armor_name, armor_data)?;

        // 左右各取一个顶点子实体；`vertex.map` 分别提取顶点数据并挂组件，返回 `[Entity; 2]`。
        let vertex = [
            (Side::Left, query!(root_query, .."VERTEX_L", ...)?),
            (Side::Right, query!(root_query, .."VERTEX_R", ...)?),
        ];
        let vertices = vertex.map(|(side, vertex)| {
            // `.unwrap()`：此处顶点网格已确认存在，失败即视为不变量被破坏，直接 panic 暴露。
            let v = self
                .extract_vertex(vertex, &armor_name, armor_data)
                .unwrap();
            self.commands.entity(vertex).insert((
                VertexData {
                    side,
                    points: v.clone(), // 复制一份存进组件，原 Vec 仍返回给调用方
                },
                Visibility::Hidden, // 顶点网格仅作数据用，不渲染
            ));
            vertex
        });
        {
            // 结尾 `ref` 让宏返回"查询本身"而非立刻取值（下面要多次复用同一查询）。
            let c_query = query!(root_query, .."_C", ref).flatten();
            // 先把所有 "_C" 子节点（贴纸）全部隐藏，再按当前编号点亮对应那张。
            c_query.clone().any().into_iter().for_each(|e| {
                self.commands.entity(e).insert(Visibility::Hidden);
            });
            // 遍历该规格的贴纸槽位表，为每个槽位找到对应后缀的子实体并挂 ArmorSticker。
            for slot in armor_data.spec.sticker_slots() {
                // `.suffix(slot.name_suffix)` 按后缀筛选；`.one()` 取恰好一个，找不到则 `?` 放弃。
                let sticker = c_query.clone().suffix(slot.name_suffix).one()?;
                self.commands.entity(sticker).insert((
                    ArmorSticker {
                        root,
                        label: slot.label,
                    },
                    // 只有与当前装甲编号一致的贴纸可见，其余隐藏。
                    match slot.label == armor_data.spec.label() {
                        true => Visibility::Visible,
                        false => Visibility::Hidden,
                    },
                ));
            }
        }

        // 给装甲根也挂上 Armor 组件（用传入的 armor_name，供碰撞/统计回溯身份）。
        self.commands.entity(root).insert(Armor {
            name: armor_name.clone(),
            team: armor_data.team,
            spec: armor_data.spec,
            label: armor_data.spec.label(),
        });

        let parts = ArmorParts {
            marker,
            lights,
            vertices,
        };
        // 把根 ID、零件索引、贴纸选择一起挂到装甲根上（`ar.clone()`：还要作为返回值传出去）。
        self.commands.entity(root).insert((
            ar.clone(),
            parts,
            ArmorStickerSelection::new(armor_data.spec.label()),
        ));
        Some(ar)
    }
}

/// 从Mesh中提取所有顶点
// `Mesh::ATTRIBUTE_POSITION`：网格里名为 "Vertex_Position" 的属性键，存每个顶点的坐标。
// `.and_then(...)`：拿到属性后再校验类型；只有 Float32x3（每顶点 3 个 f32）才接受。
// `.filter(...)`：最后过滤掉空顶点列表——空网格返回 None。
pub fn extract_vertices(mesh: &Mesh) -> Option<Vec<Vec3>> {
    mesh.attribute(Mesh::ATTRIBUTE_POSITION)
        .and_then(|values| {
            // `if let` 匹配属性枚举的具体变体；不匹配则 None（本项目只处理 Float32x3）。
            if let VertexAttributeValues::Float32x3(vec) = values {
                // `Vec3::from(p)`：把 [f32;3] 转成 Bevy 的 Vec3；`.collect()` 收集进 Vec。
                Some(vec.iter().map(|&p| Vec3::from(p)).collect())
            } else {
                None
            }
        })
        // `.filter` 闭包返回 false 就把整体变成 None（此处丢弃空的顶点数组）。
        .filter(|points: &Vec<Vec3>| !points.is_empty())
}

/// 从三角列表网格里按索引展开出"每个三角形 3 个顶点"的坐标序列（用于灯条遮罩）。
/// 只在 TriangleList 拓扑下有效；其它拓扑（如点/线）返回 None。
fn extract_triangle_vertices(mesh: &Mesh) -> Option<Vec<Vec3>> {
    if mesh.primitive_topology() != PrimitiveTopology::TriangleList {
        return None;
    }
    let positions = extract_vertices(mesh)?;
    let mut triangles = Vec::new();
    // 闭包捕获 `triangles`(可变借用) 与 `positions`(不可变借用)，按索引安全地追加顶点。
    let mut append = |index: usize| {
        if let Some(position) = positions.get(index) {
            triangles.push(*position);
        }
    };

    // `match` 处理三种索引存储：u16 / u32 / 无索引（无索引时顶点本身按顺序即为三角序列）。
    match mesh.indices() {
        Some(Indices::U16(indices)) => {
            for &index in indices {
                append(index as usize); // u16 → usize，`.as` 是无损提升
            }
        }
        Some(Indices::U32(indices)) => {
            for &index in indices {
                append(index as usize);
            }
        }
        None => {
            // 无索引：顶点按 0,1,2 / 3,4,5 … 每三个构成一个三角形。
            for index in 0..positions.len() {
                append(index);
            }
        }
    }

    // 去掉不足一个三角形的尾部残点：顶点数必须能被 3 整除。
    let trailing = triangles.len() % 3;
    if trailing != 0 {
        triangles.truncate(triangles.len() - trailing);
    }
    // `.then_some(x)`：条件为真返回 Some(x)，否则 None（非空才返回）。
    (!triangles.is_empty()).then_some(triangles)
}

/// 组装入口系统：每当有新插入的 `ScanArmor`，就扫描一次它的子树，把装甲部件装配出来。
///
/// `Added<ScanArmor>` 是 Bevy 的"变更检测"查询过滤器——匹配"在本系统上次运行之后才被添加
/// `ScanArmor` 组件"的实体。这样组装只在需要时做一次，不必每帧重复扫描整棵场景树。
///（注意：并非只在实体创建瞬间；只要组件被 remove 再 add，也会再次触发。）
///
/// `ArmorConstructor` 是被引擎自动注入的系统参数（见其定义），此处按可变入参使用。
fn insert(
    root: Query<(Entity, Read<ScanArmor>), Added<ScanArmor>>,
    mut constructor: ArmorConstructor,
) {
    for (root_entity, armor_data) in root.iter() {
        // 把工具字段复制成局部只读引用，供后面多处以只读方式使用。
        let children = constructor.children;
        let name = constructor.name;
        // 找到整棵子树里所有名字含 "ARMOR_ROOT" 的实体（同一模型可能有多块装甲）。
        children
            .iter_descendants(root_entity)
            .filter_map(|child| {
                name.get(child)
                    .ok()
                    .filter(|name| name.contains("ARMOR_ROOT")) // `contains`：子串匹配
                    .map(|name| (child, name))
            })
            .for_each(|(ent, name)| {
                // 对每个装甲根调用组装逻辑；`to_string()` 复制名字，避免借用冲突。
                constructor.process_armor_root(ent, name.to_string(), armor_data);
            })
    }
}

/// 贴纸显隐同步：当某装甲根的 `ArmorStickerSelection` 发生变化时，把它名下所有贴纸
/// 按"是否与选中编号一致"重新设置显隐。
///
/// `Changed<ArmorStickerSelection>`：只匹配自本系统上次运行以来该组件被修改过的实体——
/// 没改过的装甲根直接跳过，省去无谓遍历（与 `Added` 同属变更检测家族）。
fn sync_armor_stickers(
    mut commands: Commands,
    selections: Query<(Entity, &ArmorStickerSelection), Changed<ArmorStickerSelection>>,
    stickers: Query<(Entity, &ArmorSticker)>,
) {
    // 外层遍历"刚被切换过"的装甲根，内层遍历全部贴纸，挑出属于当前根的那些。
    for (root, selection) in &selections {
        for (entity, sticker) in &stickers {
            if sticker.root != root {
                continue; // 跳过不属于当前装甲根的贴纸
            }
            commands
                .entity(entity)
                .insert(match sticker.label == selection.label {
                    true => Visibility::Visible, // 命中当前编号 → 显示
                    false => Visibility::Hidden, // 其它编号 → 隐藏
                });
        }
    }
}

// `#[derive(Default)]`：自动实现无参默认构造，让插件可用 `.default()` / 直接以类型名安装。
#[derive(Default)]
pub(super) struct ArmorConstructorPlugin;

impl Plugin for ArmorConstructorPlugin {
    fn build(&self, app: &mut App) {
        // 两个系统都注册在 Update 阶段：插入扫描（insert）与贴纸显隐同步（sync_armor_stickers）。
        app.add_systems(Update, (insert, sync_armor_stickers));
    }
}
