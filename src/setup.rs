//! 装配工厂：把"加载好的 glTF 世界"变成"能开车、能对战的游戏实体"。
//!
//! 本模块管两类工作：
//! 1. `setup`（在 main.rs 的 `Startup` 阶段执行一次）——灯光、主相机、屏幕文字等
//!    与具体场景无关的一次性初始化；
//! 2. `setup_*` 系列（由 scene.rs 用 `AsyncWorld::run` 在加载流程中**显式**调用）——
//!    给某个刚就绪的世界找节点、打游戏标签、挂碰撞体与刚体。它们被写成普通的 Bevy
//!    一次性系统（`In<Entity>` 收输入），既能用 Query/Commands，又能被安排在流程的
//!    确定位置执行，而不是等某个回调碰巧触发。
//!
//! 协作者：上游是 scene.rs（决定调用时机与顺序）；下游是各系统的筛选条件——本文件插入的
//! `Controlled` / `SlapperInfantry` / `InfantryGimbal` 等标记，正是 systems/ 里
//! `Query<..., With<...>>` 所依赖的契约。
//!
//! 新手阅读顺序：先看 `setup`（最常规的初始化），再看 `setup_vehicle`（标签装配的核心，
//! 也是理解本项目"glTF 节点命名约定"的地方），其余 `setup_*` 是同一套路的小例子。

use avian3d::prelude::*;
use bevy::anti_alias::fxaa::Fxaa; // FXAA 后处理抗锯齿（非 macOS 的默认抗锯齿）
use bevy::core_pipeline::tonemapping::Tonemapping; // 色调映射模式
use bevy::prelude::*;
use bevy_inspector_egui::bevy_egui::{EguiGlobalSettings, PrimaryEguiContext}; // 调试 UI 设置与主上下文标记

// 本项目自建标签组件：Controlled/SlapperInfantry/ActiveSlapper/Spinning 是操控相关标记，
// Infantry*/GameLayer/PreciousCollision 等是单位与物理相关类型（详见 components/）。
use crate::components::{
    ActiveSlapper, Controlled, DartLaunch, GameLayer, Infantry, InfantryChassis, InfantryGimbal,
    InfantryLaunchOffset, InfantryViewOffset, MainCamera, PreciousCollision, SlapperInfantry,
    Spinning,
};
use crate::config::SimulationConfig;
use crate::robomaster::prelude::{OutpostRoot, ScanArmor, Team};
use crate::robomaster::vehicle::movement::VehicleDynamic; // 车辆加减速模型
use crate::systems::spawn_text; // 生成左下角状态栏文字（实现在 systems/debug.rs）
use crate::util::entity_query::HierarchyQuery; // 本项目自制的层级查询 SystemParam
use bevy_metalfx::{MetalFxTemporalUpscaling, UpscaleFactor}; // macOS 时空上采样

/// 前哨站根的标记组件（供扫描/结算系统筛选）。
#[derive(Component)]
pub struct ScanOutpost;

/// 启动时的全局初始化（`Startup` 阶段只执行一次）：屏幕文字、平行光、主相机。
///
/// 注意：场景里的实体（地面/机器人等）**不**在这里生成——它们由 `ScenePlugin` 按加载顺序产出。
pub fn setup(
    // `Commands`：延迟执行的"世界修改队列"——本帧写入，稍后统一 apply 生效。
    mut commands: Commands,
    // `Res<T>`：只读访问全局资源，这里读 config.toml 解析出的仿真配置。
    config: Res<SimulationConfig>,
    // `Option<ResMut<T>>`：可写资源，但"可能不存在"——没启用 egui 时该资源就不在，
    // 用 Option 让本系统在两种情况下都能跑而不崩溃。
    egui_global_settings: Option<ResMut<EguiGlobalSettings>>,
) {
    // 关掉 egui 自动创建主上下文：本项目把 egui 挂在自己的主相机上（见下方 PrimaryEguiContext）。
    if let Some(mut egui_global_settings) = egui_global_settings {
        egui_global_settings.auto_create_primary_context = false;
    }
    // 生成左下角状态栏等 2D 文字实体。
    spawn_text(&mut commands);
    // 平行光：`illuminance` 单位是勒克斯（lux），取自 config.render.illuminance；
    // 是否投影由 config.render.shadows 决定（阴影贴图开销大，见 config.toml 的【修改】说明）。
    commands.spawn((
        DirectionalLight {
            color: Color::srgb(0.9, 0.95, 1.0), // 略偏冷的白光（sRGB 分量 0~1）
            illuminance: config.render.illuminance,
            shadow_maps_enabled: config.render.shadows,
            contact_shadows_enabled: config.render.shadows,
            ..default() // 结构体更新语法：其余字段取 Default 值
        },
        // 灯放在 (0, 4, 0)，朝原点看；上方向取 (1,1,1) 得到自然的斜射方向（单位：米）。
        Transform::from_xyz(0.0, 4.0, 0.0).looking_at(Vec3::ZERO, Vec3::new(1.0, 1.0, 1.0)),
    ));

    // Scene assets are spawned in load order by `ScenePlugin`, not here.
    // （中译：场景资产由 ScenePlugin 按加载顺序生成，不在本函数里。）

    // `spawn` 返回 `EntityCommands`，可继续 `.insert(...)` 往这个实体追加组件。
    let mut main_camera = commands.spawn((
        Camera3d::default(),
        Camera {
            // When Talos/ROS2 capture is enabled, the actual on-screen preview is a UI blit of the
            // off-screen capture texture. Keep this camera inactive to avoid rendering twice.
            // （中译：启用 Talos/ROS2 捕获时，屏幕上看到的是离屏捕获纹理经 UI 贴回的预览画面；
            //   为避免同一场景渲染两遍，这台相机保持不激活。两个 `#[cfg]` 分支在编译期二选一。）
            #[cfg(any(feature = "ros2", feature = "talos"))]
            is_active: false,
            #[cfg(not(any(feature = "ros2", feature = "talos")))]
            is_active: config.preview.enabled,
            // clear_color: ClearColorConfig::Custom(Color::BLACK),
            ..default()
        },
        // 透视投影：`fov` 需要弧度，而 config.toml 里写的是**度**，故 `to_radians()` 转换；
        // `near`/`far` 是裁剪面距离（米），far 取极大值以保证远景（天空/远山）不被裁掉。
        Projection::Perspective(PerspectiveProjection {
            fov: config.camera.fov.to_radians(),
            near: 0.1,
            far: 500000000.0,
            ..default()
        }),
        // 关闭色调映射：输出保持线性原始色彩，便于视觉算法（Talos）拿到未经美化的图像。
        Tonemapping::None,
        // 关 MSAA：Talos 按原始像素取图，抗锯齿会改变像素语义并额外耗时。
        Msaa::Off,
        // 初始位姿：原点上方 10 m、后方 15 m 处朝原点看（`Vec3::Y` 指定世界上方为"上"）。
        Transform::from_xyz(0.0, 10.0, 15.0).looking_at(Vec3::new(0.0, 0.0, 0.0), Vec3::Y),
        // `MainCamera` 是本项目的相机标记；`follow_offset` 是跟随相机相对目标的偏移（米）。
        MainCamera {
            follow_offset: Vec3::from_array(config.camera.follow_offset),
        },
    ));
    // `cfg!` 在运行期求值为 bool，但分支在编译期即被裁剪；MetalFX 是 macOS 专有的时空上采样。
    if cfg!(target_os = "macos") && config.render.metalfx_temporal {
        main_camera.insert(MetalFxTemporalUpscaling {
            factor: UpscaleFactor::clamped(config.render.metalfx_scale),
            frame_generation: config.render.metalfx_frame_generation,
        });
    } else if config.render.main_camera_fxaa {
        // 非 macOS 走 FXAA：一种便宜的屏幕空间抗锯齿。
        main_camera.insert(Fxaa::default());
    }
    if config.debug.egui {
        // 让 egui 把界面渲染到这台相机上。
        main_camera.insert(PrimaryEguiContext);
    }
    // 标记这台相机是"捕获源"：talos/ros2 的离屏渲染会从它取图（见 capture/ 模块）。
    #[cfg(any(feature = "ros2", feature = "talos"))]
    main_camera.insert(crate::capture::CaptureSource);
}

/// Tags the two outposts inside `OUTPOST.glb`.
/// 给 `OUTPOST.glb` 里的两座前哨站打标签。
///
/// `In<Entity>`：一次性系统的输入参数——调用方 `w.run(setup_outposts, outpost)` 传入的
/// `root` 就在这里。这类系统不进每帧调度，而是被显式执行一次。
pub fn setup_outposts(
    In(root): In<Entity>,
    mut commands: Commands,
    children: Query<&Children>,
    name: Query<&Name>,
) {
    // `iter_descendants`：深度优先遍历 root 的全部后代（不含 root 自身）。
    // `for_each` 与 `for` 循环等价，只是形式更紧凑。
    children.iter_descendants(root).for_each(|e| {
        // 拿不到 `Name` 组件（无名节点）就跳过——`let-else` 提前退出本次闭包调用。
        let Ok(name) = name.get(e) else {
            return;
        };
        // 按节点名匹配（OUTPOST.glb 里必须就叫这两个名字，属资产命名契约）。
        if name.as_str() == "OUTPOST_1" {
            commands.entity(e).insert(OutpostRoot::new(Team::Red));
        }
        if name.as_str() == "OUTPOST_2" {
            commands.entity(e).insert(OutpostRoot::new(Team::Blue));
        }
    })
}

/// Finds the dart launch marker inside `GROUND.glb`.
/// 在 `GROUND.glb` 里定位飞镖发射位标记节点。
pub fn setup_dart_launch(
    In(root): In<Entity>,
    mut commands: Commands,
    children: Query<&Children>,
    name: Query<&Name>,
) {
    // 这里用 `for` 而非 `for_each`，因为要在命中后提前 `return`。
    for entity in children.iter_descendants(root) {
        let Ok(name) = name.get(entity) else {
            continue;
        };
        if name.as_str() == "DART_LAUNCH_DIRECTION" {
            // 打上 `DartLaunch` 标记，供飞镖发射系统筛选（见 systems/projectile.rs）。
            commands.entity(entity).insert(DartLaunch);
            return;
        }
    }

    // 找不到就告警而不是崩溃：地图资产可能被替换过。
    warn!("GROUND.glb is missing DART_LAUNCH_DIRECTION");
}

/// Turns a loaded robot world into a driveable vehicle: body, armor layers, chassis and gimbal.
/// 把一个刚加载完成的机器人世界装配成可驾驶、可对战的车辆：刚体、装甲碰撞层、底盘、云台。
///
/// 调用时机：由 scene.rs 在每台机器人 `spawn` 并 `await` 就绪之后调用一次。
///
/// 本项目约定的 glTF 节点命名契约（缺一不可，改资产时必须保持一致）：
///   `<root> → VEHICLE → BASE`（底盘）、`VEHICLE → GIMBAL`（云台）；
///   云台下还有 `SHOT_DIRECTION`（炮口/发射点）与 `CAM_DIRECTION`（相机/瞄准点）。
///   `HierarchyQuery` 正是按这些名字找节点的（见 util/entity_query.rs）。
pub fn setup_vehicle(
    In(root): In<Entity>,
    mut commands: Commands,
    // `HierarchyQuery`：本项目自制的 SystemParam，一次性提供 children（往下遍历）与
    // child_of/name（按名字筛选）三张查询表。Bevy 会像注入普通参数那样自动构造它。
    query: HierarchyQuery,
    // 读出根实体上的身份标签。除 `Infantry` 外都写成 `Option<&..>`：表示该组件"可能有、
    // 也可能没有"，随后用 `.is_some()` 转成 bool 使用。`Option` 在这里充当"可选过滤器"。
    root_query: Query<(
        Entity,
        &Infantry,
        Option<&Controlled>,
        Option<&ActiveSlapper>,
        Option<&Spinning>,
    )>,
    sim_config: Res<SimulationConfig>,
) {
    // 一次查询取回 5 个字段的元组（Rust 的解构赋值）；`expect` 声明不变量：只在 Infantry 根上调用。
    let (root, infantry, is_local, is_active, is_spinning) = root_query
        .get(root)
        .expect("setup_vehicle called on an entity that is not an Infantry root");
    // 能直接从 `&Infantry` 借用里取出字段值，说明 `Team` 与 `RobotConfig` 都实现了 Copy
    //（拷贝语义）；否则就需要 `.clone()` 或改为借引用。
    let team = infantry.team;
    let config = infantry.config;
    let is_local = is_local.is_some();
    let is_active = is_active.is_some();
    let is_spinning = is_spinning.is_some();
    // 本机玩家车：给它的**所有后代**节点都挂 `Controlled`，这样玩家操控系统、以及
    // camera/input/projectile 里的 `With/Without<Controlled>` 过滤器都能定位到它。
    if is_local {
        query.children.iter_descendants(root).for_each(|e| {
            commands.entity(e).insert(Controlled);
        });
    } else {
        query.children.iter_descendants(root).for_each(|e| {
            // 展示战车（Spinning）不是 AI，只是能被 Tab 选中：子节点不挂 SlapperInfantry；
            // 子节点的 ActiveSlapper 由 switch_slapper_control 在选中时运行时补挂。
            if !is_spinning {
                commands.entity(e).insert(SlapperInfantry);
            }
            if is_active {
                commands.entity(e).insert(ActiveSlapper);
            }
        });
    }
    // 碰撞层区分 self/other：本机车的子弹只打敌方装甲，故"自己"与"他人"用不同层（见 components/physics.rs）。
    let vehicle_body_collision_layers = GameLayer::vehicle_body_collision_layers(is_local);
    let vehicle_armor_collision_layers = GameLayer::vehicle_armor_collision_layers(is_local);

    // 根实体 = 整车刚体。参数含义与单位：
    // - `VehicleDynamic::new(max_speed m/s, linear_acceleration m/s², acceleration_exponent 无量纲)`
    // - `Collider::compound`：组合碰撞体，每项是 (相对位置 m, 旋转, 子碰撞体)，
    //   子体 `Collider::cylinder(半径 m, 高 m)`——这里是整车底部的近似圆柱；
    // - `CollisionMargin(0.005)` 0.005 m 碰撞余量；`Mass(15.0)` 15 kg；
    //   `Restitution::new(0.01)` 弹性 0.01（几乎不反弹）；`AngularDamping(50.0)` 角阻尼，抑制打转。
    commands.entity(root).insert((
        RigidBody::Dynamic,
        VehicleDynamic::new(
            sim_config.vehicle.max_speed,
            sim_config.vehicle.linear_acceleration,
            sim_config.vehicle.acceleration_exponent,
        ),
        Collider::compound(vec![(
            Vec3::new(0.0, -0.115649, 0.0),
            Quat::IDENTITY,
            Collider::cylinder(0.2593615, 0.231298),
        )]),
        CollisionMargin(0.005),
        vehicle_body_collision_layers,
        Mass(15.0),
        Restitution::new(0.01),
        AngularDamping(50.0),
    ));

    // 装甲碰撞层挂到整车所有后代（装甲板是子节点）；与车身层分开，才能做到"子弹只打装甲"。
    query.children.iter_descendants(root).for_each(|e| {
        commands.entity(e).insert(vehicle_armor_collision_layers);
    });

    // HierarchyQuery 链式查询：`of(root)` 从根出发，`any()` 向下取一层全部子节点，
    // `exact("VEHICLE")` 只留名字恰好等于 "VEHICLE" 的，`flatten()` 把结果固化成可复用的迭代器。
    // 之后 `exact("BASE")` 表示"在 VEHICLE 下再下一层找 BASE"，`one()` 取"恰好一个"、
    // 找不到返回 None——这里 `unwrap()` 是在断言"资产必须符合上述节点命名契约"。
    let iter = query.of(root).any().exact("VEHICLE").flatten();
    let base = iter.clone().exact("BASE").one().unwrap();
    // 底盘节点：`InfantryChassis` 存底盘姿态（yaw/roll/pitch 与 yaw 角速度，单位弧度、弧度/秒）；
    // `ScanArmor` 携带阵营与装甲规格，供视觉/命中判定使用。
    commands.entity(base).insert((
        InfantryChassis::default(),
        ScanArmor::new(team, config.armor),
    ));
    let gimbal = iter.exact("GIMBAL").one().unwrap();
    // 云台节点：随动于底盘但独立瞄准（`InfantryGimbal` 存 local_yaw/pitch，单位弧度）。
    commands.entity(gimbal).insert(InfantryGimbal::default());
    // 炮口与瞄准点只对"本机"（有相机的那台）有意义。
    if is_local {
        let q = query.of(gimbal).flatten();
        // SHOT_DIRECTION：子弹从此节点的位置/朝向射出（PostUpdate 的 projectile_launch 会读它）。
        commands
            .entity(q.clone().exact("SHOT_DIRECTION").one().unwrap())
            .insert(InfantryLaunchOffset);
        // CAM_DIRECTION：第一人称/自瞄视角所在节点——与 camera.rs 的 Robot 模式保持一致的契约。
        commands
            .entity(q.exact("CAM_DIRECTION").one().unwrap())
            .insert(InfantryViewOffset);
    }
}

/// Queues Avian collider construction for the named nodes of a loaded world.
/// 给一个已加载世界里"按名字指定"的节点排队生成 avian 碰撞体。
///
/// The map is passed in rather than parked on the entity as a component: it is an instruction for
/// one moment in the load, not state the world should keep.
/// （中译：map 作为参数传入而没挂成组件——它只是"加载这一瞬间的指令"，不是世界需要长期保留的状态。）
pub fn setup_collision(
    // `In<(Entity, PreciousCollision)>`：一次性系统的输入是一个元组（根实体 + 名称→构造表）。
    In((root, map)): In<(Entity, PreciousCollision)>,
    mut commands: Commands,
    children: Query<&Children>,
    // `Query<&Name, With<Children>>`：`With<Children>` 是过滤器——只有"有子节点"的实体
    // 才会进入本查询结果（碰撞体正是挂在这些节点上）。
    name: Query<&Name, With<Children>>,
) {
    for e in children.iter_descendants(root) {
        let Ok(name) = name.get(e) else {
            continue;
        };
        // 名字不在表里就跳过。`PreciousCollision` 对 HashMap 实现了 `Deref`，故可直接 `.get(...)`；
        // `let-else` 把元组解构成四个借用字段。
        let Some((constructor, layer, visibility, rigid)) = map.get(name.as_str()) else {
            continue;
        };
        // `rigid` 是 `&Option<RigidBody>`；`if let Some(rigid)` 借出内部值。
        if let Some(rigid) = rigid {
            // 插入组件需要所有权：`*rigid`/`*layer` 拷贝（`RigidBody`/`CollisionLayers` 是 Copy），
            // `constructor.clone()` 克隆构造器。
            commands
                .entity(e)
                .insert((*rigid, constructor.clone(), *layer));
        } else {
            commands.entity(e).insert((constructor.clone(), *layer));
        }
        // 只有需要隐藏的节点才显式插入 `Visibility::Hidden`（默认可见，无需重复设置）。
        if visibility == &Visibility::Hidden {
            commands.entity(e).insert(*visibility);
        }
    }
}
