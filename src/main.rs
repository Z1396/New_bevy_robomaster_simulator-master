//! 程序入口：Bevy 应用的"装配车间"。
//!
//! 新手 30 秒理解 ECS 架构（本项目基于 Bevy 0.19）：
//! - Entity 实体 = 一个 ID；Component 组件 = 挂在实体上的数据；System 系统 = 普通函数，
//!   通过函数参数声明自己需要哪些数据，由引擎自动注入并并行调度；
//! - Resource 资源 = 全局唯一的数据（配置、统计计数器、输入状态等）；
//! - 游戏逻辑散布在 systems/、robomaster/ 等模块的系统函数里，本文件只负责组装：
//!   注册插件 → 创建全局资源 → 把每个系统登记到调度时间表 → 启动主循环。
//!
//! 每帧的 Update 阶段按四个 SystemSet 串行执行（见 main() 中 configure_sets）：
//!   Input（采样输入/切换操控）→ GameLogic（游戏规则）→ Camera（相机跟随）→ Cleanup（清理）。
//! 其它调度时机：Startup（启动时跑一次）、PostUpdate（每帧收尾，发射子弹在
//! 这里，为了拿到"变换传播"之后的最新位置）、FixedUpdate（固定频率，物理空气阻力在这里）。

// 内层属性（`#!` 开头作用于整个 crate）：关闭"定义了但未使用"的警告，
// 项目里保留了一些预留/调试用的代码不想被警告打扰。
#![allow(dead_code)]
// `mod` 声明模块树：告诉编译器去加载同名文件（如 src/capture.rs）或同名目录
// （如 src/systems/mod.rs），之后才能用 `crate::xxx` 路径访问它们。
mod capture;
mod components;
mod config;
mod handler;
mod robomaster;
mod scene;
mod setup;
mod statistic;
mod systems;
mod telemetry;
mod util;

// 条件编译：`#[cfg(feature = "...")]` 只在 Cargo.toml [features] 里启用了对应
// feature 时才编译这段代码。ros2 / talos 是两套互斥的"相机数据输出"方案。
#[cfg(feature = "ros2")]
mod ros2;
#[cfg(feature = "talos")]
mod talos;

// `use` 导入路径，`::` 访问模块/类型；`*` 通配导入该命名空间下全部公开项。
use avian3d::prelude::*; // avian3d 物理引擎的常用类型（Gravity、RigidBody...）
use bevy::diagnostic::{FrameTimeDiagnosticsPlugin, LogDiagnosticsPlugin}; // FPS 统计插件
use bevy::prelude::*; // Bevy 的"std"：几乎所有常用类型都从这里拿
use bevy::render::settings::{InstanceFlags, RenderCreation, WgpuSettings, WgpuSettingsPriority};
use bevy::render::{RenderPlugin, RenderSystems}; // 渲染插件与渲染阶段调度集合
use bevy::window::PresentMode; // 垂直同步/呈现方式
use bevy::winit::WinitSettings; // 窗口循环（winit 是底层窗口库）行为配置
use bevy_inspector_egui::bevy_egui::EguiPlugin; // 调试 UI 框架
use bevy_inspector_egui::quick::WorldInspectorPlugin; // 运行时实体/组件检视器
use std::sync::atomic::AtomicBool; // 原子布尔：跨线程安全读写而不加锁

use crate::components::{CameraMode, FollowingType, ProjectileCooldown, SubscribeAutoAim};
use crate::config::{ConfigPlugin, SimulationConfig};
use crate::handler::{on_activate, on_hit};
use crate::robomaster::prelude::RoboMasterPlugins;
use crate::setup::setup;
use crate::statistic::ProjectileStatistics;
use crate::systems::{
    ChassisObservationFrame, ControllerState, GameplaySystems, PreviousKinematicState,
    change_appearance, cleanup_projectiles, clear_controller_input, controller_dart_just_pressed,
    controller_shoot_pressed, dart_launch, following_controls, freecam_controls, gimbal_controls,
    gimbal_pid_controls, projectile_aerodynamics, projectile_launch, remote_gimbal_controls,
    remote_vehicle_controls, sample_gamepad_controller, sample_keyboard_controller,
    screenshot_on_f2, screenshot_saving, setup_projectile, spin_display_vehicle,
    switch_slapper_control, uav_launch, update_auto_aim_subscription, update_chassis_observation,
    update_help_text, vehicle_controls,
};
use bevy_metalfx::MetalFxPlugin;

#[cfg(feature = "ros2")]
use crate::ros2::plugin::ROS2Plugin;
#[cfg(feature = "talos")]
use talos::TalosPlugin;

/// 把 config.toml 里的垂直同步字符串转成 Bevy 的窗口呈现模式枚举。
/// 返回 `Option`：`None` 表示配置值不被认识，调用方会用 `unwrap_or_else` 回退到默认值——
/// 这是 Rust 里"可能失败但不崩溃"的惯用法。
fn present_mode_from_config(value: &str) -> Option<PresentMode> {
    // `match` 模式匹配：类似加强版 switch；`"a" | "b"` 的竖线表示"或"分支，
    // 同一处理共用一个返回值。`to_ascii_lowercase` 忽略大小写，`.trim` 去空白。
    match value.trim().to_ascii_lowercase().as_str() {
        // 命中已知字符串 → 包成 Some(枚举值) 返回
        "auto_vsync" | "vsync" => Some(PresentMode::AutoVsync),
        "auto_no_vsync" | "no_vsync" | "novsync" => Some(PresentMode::AutoNoVsync),
        "fifo" => Some(PresentMode::Fifo),
        "fifo_relaxed" | "fifo-relaxed" => Some(PresentMode::FifoRelaxed),
        "mailbox" => Some(PresentMode::Mailbox),
        "immediate" => Some(PresentMode::Immediate),
        // match 必须穷尽所有情况：`_` 兜底分支，未知配置返回 None
        _ => None,
    }
}

/// 检测程序是否运行在 WSL（Windows 内置的 Linux 子系统）里。
/// 三种探法任一命中即算：环境变量 / 读 /proc 内核版本号（WSL 内核带 "microsoft" 字样）。
fn is_wsl() -> bool {
    // `var_os` 读环境变量（OsString 可含非法 UTF-8），`.is_some()`：存在即视为 WSL
    std::env::var_os("WSL_DISTRO_NAME").is_some()
        // `||` 短路求值：前面已经 true 就不再往后试
        || std::env::var_os("WSL_INTEROP").is_some()
        || std::fs::read_to_string("/proc/sys/kernel/osrelease")
            // `.map`：读到了内容就检查是否含 "microsoft"（WSL 内核签名）
            .map(|release| release.to_ascii_lowercase().contains("microsoft"))
            // `.unwrap_or(false)`：读不到文件（如原生 Windows）就当 false
            .unwrap_or(false)
}

/// 按平台选择渲染插件配置：WSL 里的 wgpu 常常拿不到"合规"的 GPU 适配器，
/// 这里放宽校验（ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER）让仿真能在 WSL 下跑起来；
/// Windows/macOS 原生环境直接用默认配置。
fn render_plugin_for_platform() -> RenderPlugin {
    // `cfg!(target_os = "linux")`：编译期判断目标系统（注意与 #[cfg] 的区别——
    // 这个宏在运行时得到 bool，但分支在编译期就被裁剪掉）。
    if cfg!(target_os = "linux") && is_wsl() {
        return RenderPlugin {
            // `Box::new`：把配置装箱成 trait 对象（堆分配，统一类型）
            render_creation: RenderCreation::Automatic(Box::new(WgpuSettings {
                instance_flags: InstanceFlags::default()
                    | InstanceFlags::ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER,
                priority: WgpuSettingsPriority::Functionality,
                // `..default()`：结构体更新语法——其余字段全部取 Default 值，
                // 是 Bevy 代码里出现频率最高的语法糖之一。
                ..default()
            })),
            ..default()
        };
    }

    RenderPlugin::default()
}

/// 根据配置构造"固定时间步长"时钟：`.max(1.0)` 防止配置为 0 导致除零崩溃。
/// `Time<Fixed>` 是泛型时钟——物理相关系统（FixedUpdate）按这个固定节拍执行，
/// 与渲染帧率解耦（帧率波动不影响物理稳定性）。
fn fixed_time_from_config(config: &SimulationConfig) -> Time<Fixed> {
    Time::<Fixed>::from_hz(config.physics.fixed_hz.max(1.0))
}

/// talos 与 ros2 两套捕获通道的启用仲裁：ROS2 的捕获上下文已存在时默认跳过
/// talos（两者都想占用离屏相机渲染），可用环境变量强制启用 talos。
#[cfg(feature = "talos")]
fn should_enable_talos_plugin(_app: &App) -> bool {
    #[cfg(feature = "ros2")]
    let ros_capture_active = app
        .world()
        .contains_resource::<crate::ros2::capture::RosCaptureContext>();
    #[cfg(not(feature = "ros2"))]
    let ros_capture_active = false;

    let force_talos_capture = std::env::var("DAEDALUS_FORCE_TALOS_CAPTURE")
        .map(|v| v == "1")
        .unwrap_or(false);

    !ros_capture_active || force_talos_capture
}

fn main() {
    // `App` 是 Bevy 应用的容器：所有插件、资源、系统都注册到它身上，最后 run() 启动主循环。
    let config = SimulationConfig::default();
    // `unwrap_or_else(|...| ...)`：Option 是 None 时用闭包的返回值兜底（这里先打警告日志）；
    // 与 unwrap() 的区别是不会 panic。
    let present_mode = present_mode_from_config(&config.window.present_mode).unwrap_or_else(|| {
        warn!(
            "Unknown window.present_mode {:?}, falling back to auto_no_vsync",
            config.window.present_mode
        );
        PresentMode::AutoNoVsync
    });
    let mut app = App::new();
    // `DefaultPlugins` 是 Bevy 的"全家桶"：窗口、渲染、输入、资源加载、UI 等一切基础能力；
    // `.set(...)` 用来覆盖其中某个插件的默认配置。`PhysicsPlugins` 是 avian3d 物理引擎。
    app.add_plugins((
        DefaultPlugins
            .set(WindowPlugin {
                primary_window: Some(Window {
                    present_mode,
                    // `fit_canvas_to_parent`：Web 构建时画布填满父元素（本地运行无影响）
                    fit_canvas_to_parent: true,
                    ..default()
                }),
                ..default()
            })
            .set(render_plugin_for_platform()),
        PhysicsPlugins::default(),
    ));
    // 连续渲染模式：每帧都重绘（默认 winit 是"无变化不重绘"的省电模式，
    // 对游戏不合适——相机在动、物理在跑，帧帧都有变化）。
    app.insert_resource(WinitSettings::continuous());

    if config.debug.egui {
        app.add_plugins(EguiPlugin::default());
        if config.debug.inspector {
            app.add_plugins(WorldInspectorPlugin::new());
        }
    }

    // ---- 注册插件与全局资源 ----
    // `add_plugins`：插件 = 一组"注册资源+系统"的打包单元，Bevy 的一切功能都来自插件。
    // `RoboMasterPlugins` 是本项目 robomaster/ 模块的自定义插件包（车辆/装甲/能量机关）。
    app.add_plugins(RoboMasterPlugins)
        .add_plugins(MetalFxPlugin)
        .add_plugins(crate::scene::ScenePlugin)
        .add_plugins(ConfigPlugin)
        // `init_resource`：用 Default::default() 创建全局资源（类型必须实现 Default）；
        // 与 `insert_resource`（直接给值）的区别只是"谁来提供初始值"。
        .init_resource::<CameraMode>()
        .init_resource::<ProjectileStatistics>()
        .init_resource::<ChassisObservationFrame>()
        .init_resource::<PreviousKinematicState>()
        .init_resource::<ControllerState>()
        // `register_type`：注册反射信息，让 egui 检视器/序列化能在运行时枚举字段。
        .register_type::<ProjectileStatistics>()
        // 以下都是 avian 物理引擎的全局设置：重力方向、子步数（每物理步内细分求解次数，
        // 越大越稳定越费 CPU）、固定物理步长（120Hz，与渲染帧率解耦）。
        .insert_resource(Gravity(Vec3::NEG_Y * 9.81))
        .insert_resource(SubstepCount(config.physics.substep_count))
        .insert_resource(fixed_time_from_config(&config))
        // AtomicBool 原子布尔：多线程安全的开关（自瞄订阅开关，talos/ROS2 线程也会读写）。
        .insert_resource(SubscribeAutoAim(AtomicBool::new(false)))
        .insert_resource(ProjectileCooldown(Timer::from_seconds(
            config.projectile.cooldown,
            TimerMode::Once,
        )))
        // `Startup` 系统只在启动时执行一次（创建相机、灯光、子弹材质等一次性初始化）。
        .add_systems(Startup, (setup, setup_projectile))
        // `add_observer`：注册观察者——不进每帧调度表，而是"事件发生时被调用"的回调。
        // on_hit/on_activate 响应能量机关的命中/激活事件。
        .add_observer(on_hit)
        .add_observer(on_activate)
        // ---- 调度时间表 ----
        // `configure_sets` 定义四个"系统集合"，`.chain()` 让它们严格按声明顺序执行。
        // 之后再 add_systems 时用 `.in_set(...)` 把系统挂进对应集合。
        .configure_sets(
            Update,
            (
                GameplaySystems::Input,
                GameplaySystems::GameLogic,
                GameplaySystems::Camera,
                GameplaySystems::Cleanup,
            )
                .chain(),
        )
        .add_systems(
            Update,
            (
                // Input phase —— 本帧的一切从输入采样开始。
                // `.chain()`：这批系统也按声明顺序执行（先采样、再切车、再操控）。
                // `.run_if(...)`：条件系统——闭包返回 false 时本帧跳过该系统
                //（例如自由相机模式下就不跑车辆操控）。
                // 逐个系统职责：
                (
                    // 清空上一帧的输入状态，避免按键状态跨帧残留
                    clear_controller_input,
                    // 从键盘/手柄采样原始输入，汇总进全局 ControllerState 资源
                    sample_keyboard_controller,
                    sample_gamepad_controller,
                    // 自瞄订阅开关状态同步（talos/ROS2 是否要接收相机数据）
                    update_auto_aim_subscription,
                    // Tab 换相机模式：Robot 第一人称 → 第三人称 → 自由
                    following_controls,
                    // Tab 轮换"操控哪台车"（switch 键在此读取）
                    switch_slapper_control,
                    // 底盘驾驶（IJKL/摇杆）——自由相机模式下没意义，条件跳过
                    vehicle_controls.run_if(|mode: Res<CameraMode>| mode.0 != FollowingType::Free),
                    // 遥控器/上位机的远程驾驶指令
                    remote_vehicle_controls,
                    // 玩家手动控制云台（UO 俯仰）
                    gimbal_controls,
                    // 自瞄云台闭环：外部视觉回传的 yaw/pitch 指令经 PID 追踪
                    //（只在自瞄订阅开启时运行）
                    gimbal_pid_controls.run_if(|enabled: Res<SubscribeAutoAim>| {
                        enabled.load(std::sync::atomic::Ordering::Acquire)
                    }),
                    // 远程云台指令（非自瞄回路）
                    remote_gimbal_controls,
                )
                    .chain()
                    .in_set(GameplaySystems::Input),
                // GameLogic phase
                // 【修改】新增 spin_display_vehicle：展示战车闲置自转（提交 b120db1）。
                // 放在 GameLogic（晚于 Input 阶段）：Tab 选中时 Input 阶段插入的
                // ActiveSlapper 在本阶段前生效，自转系统同帧失配实现"瞬停"。
                // 另两个：C/B 键换装甲贴纸样式、刷新左下角状态栏文字
                (spin_display_vehicle, change_appearance, update_help_text)
                    .in_set(GameplaySystems::GameLogic),
                // Camera phase —— 相机在游戏逻辑算完后才跟随，保证跟的是本帧结果
                (
                    freecam_controls.run_if(|mode: Res<CameraMode>| mode.0 == FollowingType::Free),
                    systems::update_camera_follow
                        .run_if(|mode: Res<CameraMode>| mode.0 != FollowingType::Free),
                )
                .in_set(GameplaySystems::Camera)
                // 再早于渲染阶段，确保渲染用的是移动后的相机。
                .before(RenderSystems::Render),
                // Cleanup phase —— 帧末清理：到期子弹销毁、F2 截图等
                (
                    // 巡检所有子弹的寿命计时器，到期的 despawn（销毁实体）
                    cleanup_projectiles,
                    // F2 触发一次截图（`.run_if` 里读的是"刚按下"这一瞬间）
                    screenshot_on_f2
                        .run_if(|input: Res<ButtonInput<KeyCode>>| input.just_pressed(KeyCode::F2)),
                    // 截图保存期间给窗口转圈光标
                    screenshot_saving,
                )
                    .in_set(GameplaySystems::Cleanup),
            ),
        )
        // PostUpdate = 渲染前的收尾阶段。发射子弹（车/飞镖/UAV）放在这里，
        // 且 `.after(TransformSystems::Propagate)`：必须等 Bevy 把本帧的父子变换
        // 累积计算完（世界矩阵更新），才能拿到炮口的最新世界位置/朝向，否则会
        // 用上一帧的旧位置生成子弹。
        .add_systems(
            PostUpdate,
            update_chassis_observation.after(TransformSystems::Propagate),
        )
        .add_systems(
            PostUpdate,
            projectile_launch
                .after(TransformSystems::Propagate)
                .run_if(controller_shoot_pressed),
        )
        .add_systems(
            PostUpdate,
            dart_launch
                .after(TransformSystems::Propagate)
                .run_if(controller_dart_just_pressed),
        )
        .add_systems(PostUpdate, uav_launch.after(TransformSystems::Propagate))
        // FixedUpdate：固定频率调度（默认随物理 120Hz），空气阻力按固定步长计算更稳定。
        .add_systems(FixedUpdate, projectile_aerodynamics);

    // FPS/帧时间诊断日志（config.toml 的 debug.diagnostics 开关）
    if config.debug.diagnostics {
        app.add_plugins((
            FrameTimeDiagnosticsPlugin::default(),
            LogDiagnosticsPlugin::default(),
        ));
    }

    // 两个可选集成：ros2 是 ROS2 话题输出（机器人侧），talos 是本项目对接
    // 视觉程序的图像/位姿通道。都用 `#[cfg]` 块包裹，未启用 feature 时整块不编译。
    #[cfg(feature = "ros2")]
    {
        app.add_plugins(ROS2Plugin::default());
        info!("ROS2 integration enabled");
    }
    #[cfg(not(feature = "ros2"))]
    {
        info!("ROS2 integration disabled");
    }

    #[cfg(feature = "talos")]
    {
        if should_enable_talos_plugin(&app) {
            app.add_plugins(TalosPlugin::default());
            info!("talos integration enabled");
        } else {
            info!(
                "talos integration skipped: ROS2 capture already active \
                 (set DAEDALUS_FORCE_TALOS_CAPTURE=1 to override)"
            );
        }
    }

    // 一切注册完毕，进入主循环：每帧按"调度表"执行所有系统，直到窗口关闭。
    app.run();
}
