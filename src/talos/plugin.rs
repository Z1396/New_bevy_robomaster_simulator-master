//! TalosPlugin：把 Talos 的全部能力打包成一个 Bevy 插件并注册进调度表。
//!
//! 【教学】本文件承担三件事：
//! 1. **装配**：`TalosPlugin::build` 里按环境变量建好 IPC 通道，注册离屏捕获插件
//!    （`TalosCapturePlugin`），并把心跳/真值/订阅处理等系统挂到 `Last` 阶段。
//! 2. **原子开关门控**：`SubscribeAutoAim`（全局 `AtomicBool`）决定"是否接收视觉程序
//!    回传的自瞄指令"：只有置位时 `process_subscription` 才运行——门控发生在系统级
//!    （`.run_if`），不是函数内部 if。
//! 3. **坐标对齐**：`M_ALIGN_MAT3` 是 Bevy(Y 轴向上) → ROS(Z 轴向上) 的旋转矩阵，
//!    凡是要发给外部视觉程序的位姿/坐标都必须先经 `to_ros*` 转换。
//!
//! 调度阶段：本项目把周期系统放在 `Last`（每帧最末），确保拿到的是本帧最新位姿。

use crate::capture::driver::{CaptureConfig, CapturedFrameKind};
use crate::capture::{IMAGE_HEIGHT, IMAGE_WIDTH};
use crate::components::{
    Controlled, InfantryChassis, InfantryGimbal, InfantryLaunchOffset, SubscribeAutoAim,
};
use crate::config::SimulationConfig;
use crate::talos::ipc::{TalosIpc, TalosIpcSubscriber};
use crate::systems::{GimbalAimTarget, GimbalAimTracker, projectile_launch};
use crate::talos::capture::{
    TalosCaptureContext, TalosCapturePlugin, TalosFrameStamp, advance_talos_frame_stamp,
    publish_talos_runtime_state_system,
};
use bevy::ecs::system::RunSystemOnce; // 提供 `World::run_system_once`（手动跑一次系统）
use bevy::prelude::*;
use bevy::render::render_resource::TextureFormat; // 离屏纹理像素格式
use std::sync::atomic::{AtomicBool, Ordering}; // 原子布尔 + 内存序
use std::sync::{Arc, Mutex}; // 跨线程共享所有权的发布端
use talos_ipc::*; // PoseIndex、GimbalCmd、各消息结构体等

// `#[derive(Resource)]`：把结构体注册成 Bevy 全局资源，系统参数里写 `Res<...>` 即可取用。
// 这里包一层 `Arc<Mutex<...>>`：订阅端要跨线程/跨系统共享，且会被不同系统并发读写。
#[derive(Resource)]
pub struct TalosSubscriberRes(pub Arc<Mutex<TalosIpcSubscriber>>);

// Newtype（单字段包装）模式：`Deref/DerefMut` 派生让 `TalosEnabled` 直接透传
// 内部 `AtomicBool` 的方法（如 `.load()`），无需写 `.0.load()`。
#[derive(Resource, Deref, DerefMut)]
pub struct TalosEnabled(pub AtomicBool);

/// 捕获插件配置：分辨率与视场角（fov）。
pub struct TalosPluginConfig {
    pub width: u32,
    pub height: u32,
    pub fov_y: f32,
    pub texture_format: TextureFormat,
}

impl Default for TalosPluginConfig {
    fn default() -> Self {
        let config = SimulationConfig::default();
        Self {
            width: IMAGE_WIDTH,
            height: IMAGE_HEIGHT,
            // 配置里存的是度，这里转弧度：`to_radians()`（Bevy 三角函数一律用弧度）。
            fov_y: config.camera.fov.to_radians(),
            texture_format: TextureFormat::Rgba8UnormSrgb,
        }
    }
}

// 插件本体：`#[derive(Default)]` 让它能用 `TalosPlugin::default()` 无参构造
// （main.rs 里就是这么加的）。字段 `config` 允许外部覆盖默认分辨率/视场角。
#[derive(Default)]
pub struct TalosPlugin {
    pub config: TalosPluginConfig,
}

impl Plugin for TalosPlugin {
    fn build(&self, app: &mut App) {
        // 【修改】原来这里直接 ShmPublisher::create() + ShmSubscriber::connect()（仅同机）。
        // 现在改为 TalosIpc::new()：按环境变量 TALOS_IPC=net|shm（默认 net）运行时选择
        // 网络转发或原共享内存，两种模式走同一个分发封装，下游系统代码无需感知。
        // `match` 解构 Result：失败就记错误日志并 `return`（插件装配中止，但不 panic）。
        let ipc = match TalosIpc::new() {
            Ok(ipc) => ipc,
            Err(e) => {
                error!("cannot create talos ipc: {}", e);
                return;
            }
        };

        // 把发布端塞进 `Arc<Mutex<...>>`：捕获系统、心跳系统、真值系统都要用，
        // 用共享所有权 + 互斥锁保证同一时刻只有一个线程发数据。
        let publisher = Arc::new(Mutex::new(ipc.publisher));

        // 图像格式走 RGB8（3 通道），分辨率取自插件配置。
        let capture_config = CaptureConfig {
            width: self.config.width,
            height: self.config.height,
            texture_format: self.config.texture_format,
            frame_kind: CapturedFrameKind::Rgb8,
        };

        // 捕获上下文：发布端句柄 + 视场角（弧度），随捕获插件一起注入。
        let capture_context = TalosCaptureContext {
            publisher: publisher.clone(), // `.clone()` 只增引用计数，不拷贝底层数据
            fov_y: self.config.fov_y,
        };

        // 帧序号/时间戳资源：每帧在 Last 阶段递增（见 capture.rs）。
        app.init_resource::<TalosFrameStamp>();

        // 这一行才是真正注册离屏捕获管线的地方（捕获相机、渲染目标、提取位姿等）。
        app.add_plugins(TalosCapturePlugin {
            config: capture_config,
            context: capture_context,
        });

        // 订阅端可用时注册为资源；不可用（对端未起）时只发不收。
        if let Some(subscriber) = ipc.subscriber {
            info!("talos subscriber ready");
            app.insert_resource(TalosSubscriberRes(Arc::new(Mutex::new(subscriber))));
        } else {
            info!("talos subscriber not available");
        }

        // 总开关：初值 true（但真正是否回传自瞄指令由 SubscribeAutoAim 另行门控）。
        app.insert_resource(TalosEnabled(AtomicBool::new(true)));
        // `Last` = 每帧最末阶段。先递增帧号/时间戳，再发心跳（保证心跳带最新时间）。
        app.add_systems(Last, (advance_talos_frame_stamp, heartbeat_system));
        // `.after(...)` 显式排序：运行状态要在帧号更新之后发出，才能带上正确时间戳。
        app.add_systems(
            Last,
            publish_talos_runtime_state_system.after(advance_talos_frame_stamp),
        );
        // 真值又排在运行状态之后，形成 advance → runtime_state → ground_truth 的链。
        app.add_systems(
            Last,
            crate::talos::ground_truth::publish_ground_truth_system
                .after(publish_talos_runtime_state_system),
        );
        // 【原子开关门控】`SubscribeAutoAim` 是全局 `AtomicBool`：
        // `.run_if(...)` 每帧先查它的值，只有为 true 才运行 process_subscription，
        // 否则整条"接收视觉指令并驱动云台/开火"的链路被跳过——零开销的门控。
        // `Ordering::Acquire` 是内存序：保证读到该标志后，能看见写入方在置位前的所有写入。
        app.add_systems(
            Last,
            process_subscription
                .run_if(|enabled: Res<SubscribeAutoAim>| enabled.load(Ordering::Acquire)),
        );
    }
}

/// 每帧（仅在自瞄订阅开启时）拉取视觉程序回传的云台指令，驱动本机云台追踪并择机开火。
fn process_subscription(
    // `Option<Res<...>>`：订阅端可能不存在（对端未起），存在才处理。
    context: Option<Res<TalosSubscriberRes>>,
    // `Commands`：延迟命令队列——这里排队的改动在系统结束后的 apply 阶段才生效。
    mut commands: Commands,
    // `Single<数据, 过滤器>`：等价于"必须恰好命中一个实体"的 Query，命中 0 或多个会
    // 报错跳过。过滤器四件套锁定"本机受控、纯云台实体"：
    // With 要求挂有该组件，Without 要求没有（云台实体不带底盘/发射口组件，以此区分）。
    gimbal: Single<
        (Entity, Option<&mut GimbalAimTracker>),
        (
            With<Controlled>,
            With<InfantryGimbal>,
            Without<InfantryChassis>,
            Without<InfantryLaunchOffset>,
        ),
    >,
) {
    let Some(ctx) = context else {
        return;
    };
    let (gimbal_entity, tracker) = gimbal.into_inner(); // Single 取内部元组

    // `recv_gimbal_cmd` 返回 Option：本帧没收到新指令就不做任何事。
    let Some(cmd) = recv_gimbal_cmd(&ctx) else {
        return;
    };
    // No solution from the solver: drop the target so the PID loop stops driving.
    // `distance_m == -1.0` 是解算器"No solution"的哨兵值（单位米）：
    // 移除跟踪目标，让 PID 云台闭环停下（见 main.rs 里 gimbal_pid_controls 的门控）。
    if cmd.distance_m == -1.0 {
        commands.entity(gimbal_entity).remove::<GimbalAimTracker>();
        return;
    }
    // 解算器给出开火建议（1 = 开火）：延迟执行一次发射系统（子弹生成在 PostUpdate）。
    if cmd.fire_advice == 1 {
        commands.queue(|w: &mut World| {
            w.run_system_once(projectile_launch).unwrap();
        });
    }

    // 角度转成本机目标：`-cmd.pitch_deg` 取负是坐标/正方向约定差异（视觉 pitch 朝上为正，
    // 本项目云台俯仰约定相反），故在此翻转符号。yaw 单位度，内部转为弧度。
    let target = GimbalAimTarget::from_solver_degrees(cmd.yaw_deg, -cmd.pitch_deg);
    match tracker {
        // 已有追踪器 → 热切换目标（平滑跟到新目标，不重建）。
        Some(mut tracker) => tracker.retarget(target),
        // 首次收到指令 → 新建追踪器并挂到云台实体上。
        None => {
            commands
                .entity(gimbal_entity)
                .insert(GimbalAimTracker::new(target));
        }
    }
}

/// 每帧发一次心跳（网络模式按阈值补发；共享内存模式刷新 header 时间戳）。
fn heartbeat_system(context: Option<Res<TalosCaptureContext>>) {
    if let Some(ctx) = context {
        if let Ok(mut publisher) = ctx.publisher.lock() {
            publisher.update_heartbeat();
        }
    }
}

/// 便捷封装：加锁发布端后发一个位姿（参数语义同 `TalosIpcPublisher::publish_pose`，
/// 注意坐标须已由 `to_ros*` 转到 ROS 坐标系，position 单位米、quaternion 为 `[w,x,y,z]`）。
pub fn publish_pose(
    context: &TalosCaptureContext,
    index: PoseIndex,
    position: [f32; 3],
    quaternion: [f32; 4],
    frame_seq: u64,
    timestamp_ns: u64,
) {
    // `lock()` 失败说明持锁线程 panic 过，这里静默跳过（不让发布端崩掉整个仿真）。
    if let Ok(mut publisher) = context.publisher.lock() {
        publisher.publish_pose(index, position, quaternion, frame_seq, timestamp_ns);
    }
}

/// 便捷封装：从订阅端资源里取最近一次云台指令（跨 `Mutex` 加锁访问）。
pub fn recv_gimbal_cmd(subscriber: &TalosSubscriberRes) -> Option<GimbalCmd> {
    subscriber.0.lock().ok()?.recv_gimbal_cmd()
}

/// 坐标对齐矩阵 `M`：把 **Bevy 坐标系**（Y 轴向上，右手系，-Z 朝前）下的向量
/// 变换到 **ROS 坐标系**（Z 轴向上，X 朝前，Y 朝左）。
///
/// `Mat3::from_cols(c0, c1, c2)` 按**列**填矩阵，即 `M[列号][行号]`。本矩阵展开为：
/// ```text
///        [ 0  0 -1 ]
///   M =  [-1  0  0 ]
///        [ 0  1  0 ]
/// ```
/// 于是任意向量 `(x, y, z)` 在 Bevy→ROS 下的映射为 **`(-z, -x, y)`**，逐轴对应关系：
/// - Bevy 的 Y 轴（上）→ ROS 的 Z 轴（上）；
/// - Bevy 的 -Z 方向（前）→ ROS 的 +X（前）；Bevy 的 +X（右）→ ROS 的 -Y（即朝右）。
///
/// ⚠ 凡是要发给外部视觉程序的位姿/坐标（位置、朝向、角速度、真值……）都必须先经
/// 本矩阵转换；项目中凡涉及位姿/坐标之处均会注明此对齐矩阵。
pub const M_ALIGN_MAT3: Mat3 = Mat3::from_cols(
    Vec3::new(0.0, -1.0, 0.0), // M[0,0], M[1,0], M[2,0]
    Vec3::new(0.0, 0.0, 1.0),  // M[0,1], M[1,1], M[2,1]
    Vec3::new(-1.0, 0.0, 0.0), // M[0,2], M[1,2], M[2,2]
);

/// 把整个 Bevy `Transform`（位置 + 旋转）转到 ROS 坐标系。
/// `#[inline]`：建议编译器内联展开（函数体很短，避免调用开销；是**建议**非强制）。
#[inline]
pub fn to_ros(bevy_transform: Transform) -> Transform {
    let new_rotation = to_ros_quat(bevy_transform.rotation);
    let new_translation = to_ros_translation(bevy_transform.translation);
    // `with_rotation`：链式构造——先由平移建 Transform，再补上旋转（Bevy 惯用写法）。
    Transform::from_translation(new_translation).with_rotation(new_rotation)
}

/// 平移向量 Bevy→ROS：直接左乘对齐矩阵 `M`。
/// 单位沿用输入（本项目世界坐标单位是**米**），此处只做轴重排/符号翻转，不改变尺度。
pub fn to_ros_translation(vec3: Vec3) -> Vec3 {
    let align_rot_mat = M_ALIGN_MAT3;
    let new_translation = align_rot_mat * vec3;
    new_translation
}

/// 四元数（旋转）Bevy→ROS：`A * q * A⁻¹`（A 为由 `M` 生成的旋转四元数）。
/// 这是"换基"公式——把同一个物理旋转在另一套坐标轴下重新表达：
/// 左乘 A 转到新轴，右乘 A⁻¹ 再把作用轴还原，净效果是坐标轴整体转了 M。
pub fn to_ros_quat(quat: Quat) -> Quat {
    let align_rot_mat = M_ALIGN_MAT3;
    // `Quat::from_mat3`：由 3x3 旋转矩阵转出等价四元数；`.inverse()` 求逆（单位四元数即共轭）。
    let align_quat = Quat::from_mat3(&align_rot_mat);
    let new_rotation = align_quat * quat * align_quat.inverse();
    new_rotation
}
