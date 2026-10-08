//! 子弹与飞镖：发射、空气阻力、到期清理。
//!
//! 调度阶段：`setup_projectile` 在 Startup；`projectile_launch` / `dart_launch` 在
//! PostUpdate（需在 Transform 传播之后取炮口最新位姿）；`projectile_aerodynamics`
//! 在 FixedUpdate（固定步长；且 `main.rs` 已设空气阻力默认关闭）。

use avian3d::prelude::*;
use bevy::input::gamepad::{GamepadRumbleIntensity, GamepadRumbleRequest};
use bevy::prelude::*;
use core::{f32::consts::PI, time::Duration};

use crate::components::{
    Controlled, DartLaunch, DartProjectile, DartSetting, GameLayer, Infantry, InfantryChassis,
    InfantryGimbal, InfantryLaunchOffset, ProjectileCooldown, ProjectileLifetime,
    ProjectileSetting,
};
use crate::config::SimulationConfig;
use crate::robomaster::prelude::Projectile;
use crate::statistic::ProjectileStatistics;
use crate::systems::{ControllerState, request_controller_rumble};

/// 启动时构建子弹的共享网格/材质与飞镖资产（Startup 执行一次）。
pub fn setup_projectile(
    mut commands: Commands,
    config: Res<SimulationConfig>,
    asset_server: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // 子弹用球体网格：半径 = 直径 / 2（config 里直径单位米，17mm 弹丸）。
    commands.insert_resource(ProjectileSetting(
        meshes.add(Sphere::new(config.projectile.diameter / 2.0)),
        materials.add(StandardMaterial {
            base_color: Color::srgba(0.132866, 1.0, 0.132869, 0.85), // 荧光绿（RGBA 0~1）
            emissive: LinearRgba::new(0.132866, 1.0, 0.132869, 0.85), // 自发光，暗处也醒目
            emissive_exposure_weight: -1.0,
            alpha_mode: AlphaMode::Opaque,
            ..default()
        }),
    ));
    // 飞镖是独立 glTF 世界，这里只加载资产句柄，发射时再 spawn。
    commands.insert_resource(DartSetting(
        asset_server.load(GltfAssetLabel::Scene(0).from_asset("DART.glb")),
    ));
}

/// 本机开火：受冷却限制，从炮口沿视线方向生成一颗子弹（PostUpdate）。
pub fn projectile_launch(
    time: Res<Time>,
    mut cooldown: ResMut<ProjectileCooldown>,
    mut stats: ResMut<ProjectileStatistics>,
    config: Res<SimulationConfig>,
    _asset_server: Res<AssetServer>,
    mut commands: Commands,
    // `Option<Res<...>>`：控制器资源"可能不存在"（如纯视觉被试模式）。
    controller: Option<Res<ControllerState>>,
    // `MessageWriter<T>`：向"消息管道"写入待处理消息（消息 vs 事件的差别见 controller.rs 说明）。
    mut rumble_requests: MessageWriter<GamepadRumbleRequest>,
    setting: Res<ProjectileSetting>,
    infantry: Single<
        (&Transform, &LinearVelocity, &AngularVelocity),
        (With<Infantry>, With<Controlled>),
    >,
    gimbal: Single<
        (&GlobalTransform, &InfantryGimbal),
        (With<Controlled>, Without<InfantryChassis>),
    >,
    launch_offset: Single<&Transform, (With<Controlled>, With<InfantryLaunchOffset>)>,
) {
    // 冷却计时器：未到时间就本帧不开火（冷却值见 config.projectile.cooldown）。
    cooldown.tick(time.delta());
    if !cooldown.is_finished() {
        return;
    }
    cooldown.reset();

    stats.increase_launch();
    // 【修改】弹道方向复用第一人称相机（systems/camera.rs Robot 模式）的旋转合成，
    // 取变换后的局部 -Z。原实现取局部 +Y，与相机视线（同一合成变换下的 -Z）垂直，
    // 云台水平时子弹恒上扬形成抛物线，准星指哪打不哪。
    // Rz(90°) 是 Bevy 相机朝向修正，与 camera.rs 保持完全一致（数学上它不改变 -Z 方向）。
    let direction = (gimbal.0.rotation() * launch_offset.rotation
        * Quat::from_euler(EulerRot::ZYX, 0.0, 0.0, PI / 2.0))
    .mul_vec3(Vec3::NEG_Z)
    .normalize_or_zero();
    if direction == Vec3::ZERO {
        return;
    }
    // 出膛速度 = 车身当前速度 + 视线方向 × 弹速（config.projectile.speed，单位 m/s）——
    // 加上车速是为了让"运动中射击"更符合物理直觉。
    let vel = infantry.1.0 + direction * config.projectile.speed;
    commands.spawn((
        RigidBody::Dynamic, // 动态刚体：受重力/力影响
        Collider::sphere(config.projectile.diameter / 2.0),
        Mass(config.projectile.mass), // 质量 kg
        Friction::new(config.projectile.friction),
        Restitution::new(0.3), // 弹性（打墙会弹一下）
        LinearDamping(config.projectile.linear_damping),
        // 本机子弹的碰撞层：只打敌方装甲与环境。
        GameLayer::projectile_collision_layers(true),
        Mesh3d(setting.0.clone()),
        MeshMaterial3d(setting.1.clone()),
        LinearVelocity(vel),         // 线速度 m/s
        AngularVelocity(infantry.2.0), // 继承车身角速度，子弹出膛带一点旋转
        // 出膛位置 = 车世界位置 + (云台世界旋转 × 炮口相对偏移)。
        Transform::IDENTITY.with_translation(
            infantry.0.translation + (gimbal.0.rotation() * launch_offset.translation),
        ),
        // 寿命计时器：到期由 cleanup_projectiles 销毁。
        ProjectileLifetime(Timer::from_seconds(
            config.projectile.lifetime,
            TimerMode::Once,
        )),
        Projectile,
    ));
    // 手柄震动反馈：强/弱马达强度 + 持续时间（80ms）。
    request_controller_rumble(
        controller.as_deref(),
        &mut rumble_requests,
        GamepadRumbleIntensity {
            strong_motor: 0.45,
            weak_motor: 0.2,
        },
        Duration::from_millis(80),
    );
}

/// 空气阻力（FixedUpdate）：按 v² 的阻力公式施力，让弹道逐渐下坠。
pub fn projectile_aerodynamics(
    config: Res<SimulationConfig>,
    // `Without<DartProjectile>`：飞镖不参与空气阻力（它用另一套常量）。
    mut projectiles: Query<Forces, (With<Projectile>, Without<DartProjectile>)>,
) {
    let aero = &config.projectile.aerodynamics;
    if !aero.enabled {
        return;
    }

    let diameter = config.projectile.diameter;
    if diameter <= 0.0 {
        return;
    }
    // 各参数取下限 0，避免配置异常导致符号翻转或除零。
    let air_density = aero.air_density.max(0.0);
    let drag_coefficient = aero.drag_coefficient.max(0.0);
    if air_density == 0.0 || drag_coefficient == 0.0 {
        return;
    }

    // 迎风面积 = π r²（球体投影面积）；`k = 0.5 · ρ · Cd · A`。
    let area = PI * (diameter * 0.5).powi(2);
    let wind = Vec3::new(aero.wind[0], aero.wind[1], aero.wind[2]);
    let k = 0.5 * air_density * drag_coefficient * area;

    for mut forces in projectiles.iter_mut() {
        // 相对风速 = 子弹速度 - 风速；阻力 F = -k·|v|·v（方向与相对速度相反）。
        let v_rel = forces.linear_velocity() - wind;
        let speed = v_rel.length();
        if speed <= 1e-3 {
            continue; // 近静止时无需施加阻力，也避免除零/无意义的零向量
        }
        forces.apply_force(-k * speed * v_rel);
    }
}

/// 飞镖发射：从 `DART_LAUNCH_DIRECTION` 节点沿其朝向射出一支飞镖（PostUpdate）。
pub fn dart_launch(
    mut commands: Commands,
    config: Res<SimulationConfig>,
    mut stats: ResMut<ProjectileStatistics>,
    controller: Option<Res<ControllerState>>,
    mut rumble_requests: MessageWriter<GamepadRumbleRequest>,
    setting: Res<DartSetting>,
    // 发射位（可能不存在：地面资产缺失时 setup_dart_launch 会告警）。
    launchers: Query<&GlobalTransform, With<DartLaunch>>,
) {
    // 常量集中在此，单位见注释：飞镖向前方向、模型自身朝向、速度、质量、碰撞体尺寸、出膛偏移。
    const DART_FORWARD: Vec3 = Vec3::Y; // 发射位节点的"前"方向
    const DART_MODEL_FORWARD: Vec3 = Vec3::NEG_Y; // 模型自身的前向（与发射方向相反，需旋正）
    const DART_SPEED_MPS: f32 = 17.0; // m/s
    const DART_MASS_KG: f32 = 0.25; // kg
    const DART_COLLIDER_RADIUS_M: f32 = 0.001; // m
    const DART_COLLIDER_LENGTH_M: f32 = 0.001; // m
    const DART_SPAWN_OFFSET_M: f32 = 0.00; // m（出膛前移量）

    let Ok(launcher) = launchers.single() else {
        return; // 0 个或多个发射位都放弃发射
    };

    let direction = launcher
        .rotation()
        .mul_vec3(DART_FORWARD)
        .normalize_or_zero();
    if direction == Vec3::ZERO {
        return;
    }

    stats.increase_launch();

    // 出膛位姿：位置前移 DART_SPAWN_OFFSET_M；朝向先按发射方向，再旋正模型自身朝向。
    let transform =
        Transform::from_translation(launcher.translation() + direction * DART_SPAWN_OFFSET_M)
            .with_rotation(
                launcher.rotation() * Quat::from_rotation_arc(DART_MODEL_FORWARD, DART_FORWARD),
            );
    // 飞镖形状不规则，用体素化碰撞体（边长 0.005 m）；碰撞层同本机子弹。
    let voxel = |size| {
        ColliderConstructorHierarchy::new(ColliderConstructor::VoxelizedTrimeshFromMesh {
            voxel_size: size,
            fill_mode: FillMode::FloodFill {
                detect_cavities: true,
            },
        })
        .with_default_layers(GameLayer::projectile_collision_layers(true))
    };
    commands.spawn((
        RigidBody::Dynamic,
        voxel(0.005),
        Mass(DART_MASS_KG),
        Friction::new(config.projectile.friction),
        Restitution::new(0.55), // 飞镖弹性较高
        LinearDamping(config.projectile.linear_damping),
        GameLayer::projectile_collision_layers(true),
        WorldAssetRoot(setting.0.clone()), // 飞镖是一个完整 glTF 世界
        transform,
        LinearVelocity(direction * DART_SPEED_MPS),
        ProjectileLifetime(Timer::from_seconds(
            config.projectile.lifetime,
            TimerMode::Once,
        )),
        Projectile,
        DartProjectile, // 额外标记，把它与普通子弹区分（见 projectile_aerodynamics 的过滤）
    ));
    // 飞镖震动更强烈、更持久（140ms）。
    request_controller_rumble(
        controller.as_deref(),
        &mut rumble_requests,
        GamepadRumbleIntensity {
            strong_motor: 0.65,
            weak_motor: 0.35,
        },
        Duration::from_millis(140),
    );
}

/// 帧末清理：巡检所有子弹/飞镖的寿命计时器，到期的 despawn（Cleanup 阶段）。
pub fn cleanup_projectiles(
    time: Res<Time>,
    mut commands: Commands,
    mut projectiles: Query<(Entity, &mut ProjectileLifetime)>,
) {
    for (entity, mut lifetime) in &mut projectiles {
        lifetime.tick(time.delta());
        if lifetime.is_finished() {
            // `despawn` 是延迟指令；实体真正销毁在下一次 apply 时，届时会触发
            // armor/collision.rs 注册的 `On<Remove, Projectile>` 观察者清理统计去重记录。
            commands.entity(entity).despawn();
        }
    }
}
