//! 场景装配：决定"竞技场里最终出现哪些实体、它们按什么顺序出现"。
//!
//! 本模块在游戏里管什么：把 assets/ 下的一张张 glTF（地面、标定板、前哨站、科技核心、
//! 能量机关、双方机器人）按固定顺序加载进世界，并给其中的关键节点打上游戏标签
//!（标签的具体装配逻辑在 setup.rs 的 setup_* 系列里）。
//!
//! 协作者与新手阅读顺序：
//! 1. 先读 `load_scene`——它是一条直线式的 async 加载序列，顺序本身就是设计；
//! 2. `ScenePlugin`——把 `load_scene` 包成 `AsyncWorldTask` 资源并在 `PreUpdate` 驱动；
//! 3. `util/async_world.rs`——提供 `AsyncWorld` 这个可跨帧 await 的世界句柄
//!    （Bevy 资产加载天然跨多帧，本模块用它把"散落的回调"还原成"顺序的代码"）；
//! 4. 每个 `w.run(setup_xxx, ...)`——调用 setup.rs 里的一次性系统给刚加载的世界打标签。
//!
//! 为什么机器人必须最后生成（本项目最关键的一条知识）：
//! glTF 世界在"它那一帧资产刚加载完成"时才就绪。若所有 world 一次性 spawn，
//! `setup_vehicle` 有可能先于环境碰撞体把机器人设成动态刚体（`RigidBody::Dynamic`），
//! 机器人随即穿透尚未生成的地板一路下落。因此加载被排成一条线性任务：每个
//! `w.spawn(...).await` 都要等该世界实例就绪才返回，而机器人的 await 排在最后。
//!
//! 每个世界"就绪时"的装配（命名、碰撞层、云台接线）仍留在 `WorldInstanceReady`
//! 观察者里，因为运行时才生成的子弹、UAV 同样需要它。原先这些观察者之间**隐式**
//! 的先后关系，如今由本文件的线性 await 序列变成**显式**的。

// `*` 通配导入：avian3d 物理引擎的常用类型（RigidBody、ColliderConstructorHierarchy、
// CollisionLayers 等）。详见 main.rs 对 `use` 的说明。
use avian3d::prelude::*;
use bevy::prelude::*;
// Bevy 的"世界资产/场景"类型：一个 .glb 加载完成后成为一个 WorldAssetRoot，
// 生效时挂上 WorldInstance（内含 InstanceId，用于按名字枚举该世界的实体）。
use bevy::world_serialization::{InstanceId, WorldAssetRoot, WorldInstance};
use std::collections::HashMap;

// 本项目自建的游戏标签组件：给场景里的节点贴标签，系统再按标签筛选。
// Controlled=本机操控；SlapperInfantry=可被 Tab 选中的 AI 车；ActiveSlapper=当前选中；
// Spinning=展示战车标记；Infantry=步兵类单位的阵营+配置；GroundRoot=地面根；
// GameLayer=物理碰撞层；PreciousCollision=一次性碰撞构造指令的载体。
use crate::components::{
    ActiveSlapper, Controlled, GameLayer, GroundRoot, Infantry, PreciousCollision, SlapperInfantry,
    Spinning,
};
use crate::robomaster::power_rune::construct::setup_power_rune;
// 机器人配置（三号步兵、英雄）与各场景根的标记类型。
use crate::robomaster::prelude::{
    HERO_ROBOT_CONFIG, INFANTRY_THREE_CONFIG, PowerRuneRoot, Team, TechCoreRoot,
};
use crate::robomaster::tech_core::construct::setup_tech_core;
// setup.rs 的一次性装配系统：本文件用 `w.run(...)` 在确定时机显式调用它们。
use crate::setup::{
    ScanOutpost, setup_collision, setup_dart_launch, setup_outposts, setup_vehicle,
};
use crate::util::async_world::{AsyncWorld, AsyncWorldTask, drive_async_world};

/// Resolves once Avian has built every collider `setup_collision` queued under `root`.
///
/// `setup_collision` puts a [`ColliderConstructorHierarchy`] on the named descendants of a world as
/// soon as its instance is ready, so by the time this runs the queue for `root` is complete.
/// Selecting and observing happen in one world job, so constructors finishing early are simply not
/// selected rather than awaited forever.
///
/// 中文：等到 avian 把 `setup_collision` 排队给 `root` 下的碰撞体全部建好后才返回。
/// 时机：`setup_collision` 在实例就绪的瞬间就给世界的指定后代挂上
/// [`ColliderConstructorHierarchy`]（"请按这棵网格树生成碰撞体"），所以本函数运行到
/// 这里时 `root` 下的"待建队列"已经填满。
async fn colliders_ready(w: &AsyncWorld, root: Entity) {
    // `observe_all::<E, _>(select)`：对 `select` 闭包返回的每个实体挂一次性观察者，
    // 全部触发 `E` 事件后才 resolve；这里 `E = ColliderConstructorHierarchyReady`
    //（avian 建完一棵层次碰撞体后发的信号）。
    // 关键契约：**选择（select）与挂观察者发生在同一个 world job 内**——中间不会跑
    // 其他系统，所以不存在"观察者挂上去之前事件已经发过"的竞态；若某棵子树已先建好，
    // 它只是不在 `select` 结果里，而不会让本函数永远等下去。
    w.observe_all::<ColliderConstructorHierarchyReady, _>(move |world| {
        // 把 `root` 的整棵子树实体收集进 `queued`。
        let mut queued = Vec::new();
        collect_descendants(world, root, &mut queued);
        // 只保留"确实还挂着 ColliderConstructorHierarchy"的节点，即尚未完成的队列。
        queued.retain(|entity| world.get::<ColliderConstructorHierarchy>(*entity).is_some());
        queued
    })
    .await;
}

/// The instance id a loaded world spawned under, needed to enumerate its entities by name.
/// 中文：取一个已加载世界对应的 InstanceId——后续要按"名字"枚举它的实体就得靠它。
async fn instance_of(w: &AsyncWorld, root: Entity) -> InstanceId {
    // `with_world(闭包)`：把闭包作为"job"交给 AsyncWorld，在它持有 `&mut World` 时执行
    // 并返回结果（同帧内 resolve，机制见 async_world.rs）。
    w.with_world(move |world| {
        // `get::<WorldInstance>` 返回 `Option<&WorldInstance>`，`expect` 声明不变量：
        // 调用方只在世界实例已就绪后才调本函数。
        // `**` 双重解引用：先解 `&WorldInstance`，再借 `WorldInstance` 的 `Deref` 取到 InstanceId。
        **world
            .get::<WorldInstance>(root)
            .expect("world instance is ready, so its id must exist")
    })
    .await
}

/// 深度优先收集 `entity` 的所有后代（不含自身），写进 `out`。
fn collect_descendants(world: &World, entity: Entity, out: &mut Vec<Entity>) {
    // `let-else`：拿不到 `Children` 组件（说明是叶子节点）就直接返回。
    let Some(children) = world.get::<Children>(entity) else {
        return;
    };
    // 先把子实体拷进一个拥有所有权的 `Vec<Entity>`：`children` 借用着 `world`，
    // 而下面的递归又要再次借 `world`，拷一份即可立刻结束这次借用（否则借用检查器报错）。
    let children: Vec<Entity> = children.iter().collect();
    for child in children {
        out.push(child);
        collect_descendants(world, child, out);
    }
}

/// 把"场景加载"接入 Bevy 的插件单元（在 main.rs 里 `add_plugins(crate::scene::ScenePlugin)`）。
pub struct ScenePlugin;

impl Plugin for ScenePlugin {
    fn build(&self, app: &mut App) {
        // `AsyncWorldTask` 是"一个正在运行的异步加载任务"，作为全局资源存放；
        // `load_scene` 是任务函数体（接收 AsyncWorld 句柄、返回 Future）。
        app.insert_resource(AsyncWorldTask::new(load_scene))
            // `PreUpdate` 每帧在最前驱动这个任务；`run_if(resource_exists::<..>)` 使任务一旦
            // 完成（资源被移除）本系统自动停跑，不再空转。
            .add_systems(
                PreUpdate,
                drive_async_world.run_if(resource_exists::<AsyncWorldTask>),
            );
    }
}

/// 场景加载的"剧本"：一条从头到尾顺序执行的异步序列。按注释自上而下读即可。
async fn load_scene(w: AsyncWorld) {
    // 从世界里借出 `AssetServer` 再 `clone()`——它内部是 `Arc`，克隆很廉价；之所以要克隆，
    // 是因为后面 await 期间不能一直持有对世界的借用（见 async_world.rs 的说明）。
    let assets = w
        .with_world(|world| world.resource::<AssetServer>().clone())
        .await;
    // 小闭包：把资源路径变成"要加载的场景根"。`GltfAssetLabel::Scene(0)` 表示取 .glb 里的
    // 第 0 号场景（一个 glb 可打包多个场景，本项目约定只有一个）。
    let scene =
        |path: &'static str| WorldAssetRoot(assets.load(GltfAssetLabel::Scene(0).from_asset(path)));

    // 环境与机器人的碰撞分层由 GameLayer 统一给出（self/other/environment 三套）。
    let layers = GameLayer::environment_collision_layers();
    // 闭包：构造"按整棵网格树生成三角形网格碰撞体"的指令（静态环境用，精度最高、最贵）。
    // `TrimeshFlags::all()` 打开全部生成选项。
    let trimesh = || {
        ColliderConstructorHierarchy::new(ColliderConstructor::TrimeshFromMeshWithConfig(
            TrimeshFlags::all(),
        ))
        .with_default_layers(layers)
    };
    // `PreciousCollision` 的条目是四元组 (构造器, 碰撞层, 可见性, 可选刚体)：
    // 这里 = 三角网格碰撞体 + 环境层 + 可见 + 静态刚体（"钉死在地图上"）。
    let static_trimesh = || {
        (
            trimesh(),
            layers,
            Visibility::Visible,
            Some(RigidBody::Static),
        )
    };

    // Environment first. Every robot below lands on what these build.
    // 中文：环境优先，下面每台机器人都落在这些环境碰撞体之上。
    // `GroundRoot` 打地面标记；`Friction::new(0.5)` 是摩擦系数（无量纲）。
    let ground = w
        .spawn(scene("GROUND.glb"), (GroundRoot, Friction::new(0.5)))
        .await;
    // 在地面里找 `DART_LAUNCH_DIRECTION` 节点打标签（飞镖发射位标记）。
    w.run(setup_dart_launch, ground).await;
    // 给地面下的 `GROUND_DENSE` 子节点生成静态三角网格碰撞体。
    w.run(
        setup_collision,
        (
            ground,
            PreciousCollision(HashMap::from([(
                "GROUND_DENSE".to_string(),
                static_trimesh(),
            )])),
        ),
    )
    .await;
    // 等这些碰撞体真正建好，后续机器人才能可靠地落在地面上。
    colliders_ready(&w, ground).await;

    // 两块标定板（供 Talos 视觉标定），同一 glb 摆在两个不同位置。
    // `Transform::IDENTITY` = 无旋转无缩放的单位变换，`with_translation` 只改平移；
    // 位移单位是米（Bevy 默认 1 单位 = 1 米）。
    w.spawn(
        scene("CALIB.glb"),
        Transform::IDENTITY.with_translation(Vec3::new(1.0, 2.5, 1.0)),
    )
    .await;

    w.spawn(
        scene("CALIB.glb"),
        Transform::IDENTITY.with_translation(Vec3::new(2.0, 0.5, 2.0)),
    )
    .await;

    // 前哨站：静态刚体（`RigidBody::Static`，不可被推动）；`ScanOutpost` 是供扫描/结算用的标记。
    let outpost = w
        .spawn(scene("OUTPOST.glb"), (RigidBody::Static, ScanOutpost))
        .await;
    w.run(setup_outposts, outpost).await;

    // 科技核心：装配成可被击中/夺取的实体；`setup_tech_core` 需要 InstanceId 才能按名定位内部节点。
    let tech_core = w.spawn(scene("TECH_CORE.glb"), TechCoreRoot).await;
    w.run(
        setup_tech_core,
        (tech_core, instance_of(&w, tech_core).await),
    )
    .await;
    w.run(
        setup_collision,
        (
            tech_core,
            PreciousCollision(HashMap::from([("GROUND".to_string(), static_trimesh())])),
        ),
    )
    .await;
    colliders_ready(&w, tech_core).await;

    // 能量机关（Power Rune）：静态刚体；`CollisionMargin(0.001)` = 0.001 m 碰撞余量
    //（越小越贴合、越不易"悬空"）；`Restitution::ZERO` 弹性为 0（打上去不反弹）。
    let power_rune = w
        .spawn(
            scene("POWER.glb"),
            (
                RigidBody::Static,
                CollisionMargin(0.001),
                Restitution::ZERO,
                PowerRuneRoot,
            ),
        )
        .await;
    w.run(
        setup_power_rune,
        (power_rune, instance_of(&w, power_rune).await),
    )
    .await;
    // 能量机关的碰撞体由 `power_rune_collision` 逐个命名节点给出（见文件末尾）。
    w.run(
        setup_collision,
        (power_rune, PreciousCollision(power_rune_collision(layers))),
    )
    .await;
    colliders_ready(&w, power_rune).await;

    // Robots last, so they can never become dynamic over an empty world.
    // 中文：机器人放最后，保证它们绝不会在"空世界"上变成动态刚体（否则会掉出地板）。
    // 玩家车：`Infantry::new(红方, 三号步兵配置)` + `Controlled`（本机操控标记）。
    let player = w
        .spawn(
            scene("vehicle.glb"),
            (
                Transform::from_xyz(0.0, 1.0, 0.0),
                Infantry::new(Team::Red, INFANTRY_THREE_CONFIG),
                Controlled,
            ),
        )
        .await;
    w.run(setup_vehicle, player).await;

    // 蓝方 AI 三号步兵：`SlapperInfantry` 表示"可被 Tab 选中操控的 AI 车"。
    let slapper = w
        .spawn(
            scene("vehicle.glb"),
            (
                Transform::from_xyz(1.0, 1.0, 1.0),
                Infantry::new(Team::Blue, INFANTRY_THREE_CONFIG),
                SlapperInfantry,
            ),
        )
        .await;
    w.run(setup_vehicle, slapper).await;

    // 蓝方英雄：`ActiveSlapper` = 启动时默认选中的那台可操控车。
    let hero = w
        .spawn(
            scene("HERO.glb"),
            (
                Transform::from_xyz(2.0, 1.0, 1.0),
                Infantry::new(Team::Blue, HERO_ROBOT_CONFIG),
                SlapperInfantry,
                ActiveSlapper,
            ),
        )
        .await;
    w.run(setup_vehicle, hero).await;

    // 【修改】新增展示战车（提交 b120db1）：双形态——闲置绕 Y 轴自转（spin.rs），
    // 被 Tab 选中时停转由玩家操控，切走后自动恢复。形态切换完全由根实体上
    // ActiveSlapper 的有无驱动，Spinning 永不摘挂。
    // 位置必须在 HERO 之后（robots last）：场景是线性异步加载，环境碰撞体先就绪，
    // 车辆后生成，否则会掉出地板；也正因此用 scene() + await 而不是 SceneRoot。
    let display = w
        .spawn(
            scene("test.glb"),
            (
                Transform::from_xyz(-7.0, 2.0, -5.0),
                Infantry::new(Team::Blue, INFANTRY_THREE_CONFIG),
                Spinning,
                SlapperInfantry,
            ),
        )
        .await;
    w.run(setup_vehicle, display).await;

    // 加载全部完成，打一条日志便于确认（也是这条线性任务的终点）。
    info!("scene loaded");
}

/// Every rune target gets a voxelized collider so hits register on the spinning arms.
/// 中文：每个能量机关靶位都生成"体素化"碰撞体，这样打在旋转臂上的命中也能被检测到。
///
/// 返回值是 `setup_collision` 需要的"节点名 → 碰撞构造指令"表；键名必须与 POWER.glb
/// 里的节点名逐字一致。靶位用体素而非三角网格：三角网格碰撞体只适用于静态物体，
/// 而这些靶会随机关转动，故改用体素化的动态碰撞体。
fn power_rune_collision(
    layers: CollisionLayers,
) -> HashMap<
    String,
    (
        ColliderConstructorHierarchy,
        CollisionLayers,
        Visibility,
        Option<RigidBody>,
    ),
> {
    // 体素化闭包：`voxel_size` 是体素边长（米）；`FloodFill { detect_cavities: true }`
    // 表示填充时连内部空腔也一并识别，避免薄壁处漏出空洞。
    let voxel = |size| {
        ColliderConstructorHierarchy::new(ColliderConstructor::VoxelizedTrimeshFromMesh {
            voxel_size: size,
            fill_mode: FillMode::FloodFill {
                detect_cavities: true,
            },
        })
        .with_default_layers(layers)
    };

    // 底座 `BASE` 形状规则，用三角网格即可，并设为静态刚体（固定不动）。
    let mut collision = HashMap::from([(
        "BASE".to_string(),
        (
            ColliderConstructorHierarchy::new(ColliderConstructor::TrimeshFromMeshWithConfig(
                TrimeshFlags::all(),
            ))
            .with_default_layers(layers),
            layers,
            Visibility::Visible,
            Some(RigidBody::Static),
        ),
    )]);
    // 2 个面 × 5 个靶位 × 4 种状态 = 40 个靶节点，键名由 `format!` 拼出。
    // 靶体是旋转件，故不给刚体（`None`）、用体素碰撞体（边长 0.015 m）。
    for face in 1..=2 {
        for target in 1..=5 {
            for state in ["ACTIVATED", "ACTIVE", "COMPLETED", "DISABLED"] {
                collision.insert(
                    format!("FACE_{face}_TARGET_{target}_{state}"),
                    (voxel(0.015), layers, Visibility::Visible, None),
                );
            }
        }
    }
    collision
}
