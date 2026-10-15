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
pub struct TalosEnabled(pub AtomicBool);// 原子布尔值，用于控制是否接收视觉程序的自瞄指令。

/// 【调试·临时】自瞄指令到达率统计。用于判断云台"卡顿"出在**指令供应**还是**执行端**。
/// 确认结论后，删除本资源、`log_cmd_rate` 系统及其注册即可。
#[derive(Resource, Default)]
pub struct TalosCmdStats {
    /// 本统计窗口内 `process_subscription` 被调度的次数。
    /// 该系统仅在自瞄开启时运行，所以它 = "本窗口内仿真**期望**消费指令的帧数"。
    frames: u64,
    /// 本窗口内收到的**新**指令条数（靠指纹比对去重，见下）。
    new_cmds: u64,
    /// 上一条指令的指纹；相同即视为"槽位没更新、重复读到的同一条旧指令"。
    last_cmd_key: Option<(u64, [u32; 3], u8)>,
    /// 最近一次收到的指令（供日志展示，确认数据确实在变）。
    latest_cmd: Option<GimbalCmd>,
    /// 距下次打印累计的时长（秒）。
    window_secs: f32,
}

/// 捕获插件配置：分辨率与视场角（fov）。
pub struct TalosPluginConfig {
    pub width: u32,
    pub height: u32,
    pub fov_y: f32, //fov_y 视场角（弧度）
    pub texture_format: TextureFormat,// 纹理像素格式（Rgba8UnormSrgb: 离屏捕获）
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
            // ok 成功成功返回 IPC 实例，失败返回错误。
            Ok(ipc) => ipc,
            // err 错误返回错误信息，插件装配中止。
            Err(e) => {
                error!("cannot create talos ipc: {}", e);
                return;
            }
        };

        // 把发布端塞进 `Arc<Mutex<...>>`：捕获系统、心跳系统、真值系统都要用，Mutex 用共享所有权 + 互斥锁保证同一时刻只有一个线程发数据。
        // Arc::new()：创建一个新 Arc(Arc是智能指针，用于跨线程共享所有权的)，初始引用计数为 1。
        let publisher = Arc::new(Mutex::new(ipc.publisher));

        // 图像格式走 RGB8（3 通道），分辨率取自插件配置。
        let capture_config = CaptureConfig {
            width: self.config.width,  // 离屏捕获宽度
            height: self.config.height, // 离屏捕获高度
            texture_format: self.config.texture_format, // 纹理像素格式（Rgba8UnormSrgb: 离屏捕获）
            frame_kind: CapturedFrameKind::Rgb8, // 离屏捕获帧格式（Rgb8: 离屏捕获）
        };

        // 捕获上下文：发布端句柄 + 视场角（弧度），随捕获插件一起注入。
        let capture_context = TalosCaptureContext {
            publisher: publisher.clone(), // `.clone()` 只增引用计数，不拷贝底层数据
            fov_y: self.config.fov_y, //fov_y 视场角（弧度）
        };

        // 帧序号/时间戳资源：每帧在 Last 阶段递增（见 capture.rs）。
        app.init_resource::<TalosFrameStamp>();

        // 这一行才是真正注册离屏捕获管线的地方（捕获相机、渲染目标、提取位姿等）。
        app.add_plugins(TalosCapturePlugin {
            config: capture_config, // 捕获配置
            context: capture_context, // 捕获上下文
        });

        // 订阅端可用时注册为资源；不可用（对端未起）时只发不收。some() 检查是否为 Some。
        // `ipc.subscriber` 是 `Option<TalosIpcSubscriber>`，`if let Some(subscriber) = ipc.subscriber` 检查是否为 Some。
        if let Some(subscriber) = ipc.subscriber {
            // 如果订阅端可用，注册为资源。并打印日志。
            // `info!("talos subscriber ready")`：打印日志，提示订阅端已就绪。
            info!("talos subscriber ready");
            // TalosSubscriberRes 是订阅端资源，用于在系统中访问订阅端。
            app.insert_resource(TalosSubscriberRes(Arc::new(Mutex::new(subscriber))));
        } else {
            info!("talos subscriber not available");
        }

        // 总开关：初值 true（但真正是否回传自瞄指令由 SubscribeAutoAim 另行门控）。
        app.insert_resource(TalosEnabled(AtomicBool::new(true)));
        // `Last` = 每帧最末阶段。先递增帧号/时间戳，再发心跳（保证心跳带最新时间）。
        // `advance_talos_frame_stamp`：递增帧号/时间戳，单位 ns。
        // `heartbeat_system`：发送心跳，单位 ns。
        app.add_systems(Last, (advance_talos_frame_stamp, heartbeat_system));
        // `.after(...)` 显式排序：运行状态要在帧号更新之后发出，才能带上正确时间戳。
        app.add_systems(
            Last,//Last 阶段(表示每帧最末阶段)。
            // `publish_talos_runtime_state_system`：发布运行状态，单位 ns。
            // `after(advance_talos_frame_stamp)`：在递增帧号/时间戳之后运行，确保时间戳正确。
            publish_talos_runtime_state_system.after(advance_talos_frame_stamp),
        );
        // 真值又排在运行状态之后，形成 advance → runtime_state → ground_truth 的链。
        app.add_systems(
            Last,//Last 阶段(表示每帧最末阶段)。
            // ground_truth_system 是发布真值的系统，单位 ns。
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

        // 【调试·临时】指令到达率统计：每秒打印一次"新指令/s vs 帧/s"。
        // 用于确认云台卡顿到底出在指令供应还是执行端；确认后连同上面两项一起删除。
        app.init_resource::<TalosCmdStats>();
        app.add_systems(Last, log_cmd_rate.after(process_subscription));
    }
}

/// 每帧（仅在自瞄订阅开启时）拉取视觉程序回传的云台指令，驱动本机云台追踪并择机开火。
fn process_subscription(
    // `Option<Res<...>>`：订阅端可能不存在（对端未起），存在才处理。
    context: Option<Res<TalosSubscriberRes>>,
    // `Commands`：延迟命令队列——这里排队的改动在系统结束后的 apply 阶段才生效。
    mut commands: Commands,
    // 【调试·临时】指令到达率统计。
    mut stats: ResMut<TalosCmdStats>,
    // `Single<数据, 过滤器>`：等价于"必须恰好命中一个实体"的 Query，命中 0 或多个会
    // 报错跳过。过滤器四件套锁定"本机受控、纯云台实体"：
    // With 要求挂有该组件，Without 要求没有（云台实体不带底盘/发射口组件，以此区分）。
    gimbal: Single<
        (Entity, Option<&mut GimbalAimTracker>),
        (
            With<Controlled>,// 本机受控
            With<InfantryGimbal>,// 纯云台实体
            Without<InfantryChassis>,// 不带底盘组件
            Without<InfantryLaunchOffset>,// 不带发射口组件
        ),
    >,
) {
    // 【调试·临时】本系统仅在自瞄开启时被调度，故每加一次 = 一帧"应该消费指令"的机会。
    stats.frames += 1;

    // 捕获上下文
    let Some(ctx) = context else {
        return;
    };
    let (gimbal_entity, tracker) = gimbal.into_inner(); // Single 取内部元组

    // `recv_gimbal_cmd` 返回 Option：本帧没收到新指令就不做任何事。
    // ⚠ 注意：网络/共享内存订阅端返回的是"槽位里最近一条指令"，槽位没更新时会**重复返回同一条**，
    // 所以下面不靠"返回 Some"计数，而是对指令做指纹比对，只有指纹变化才算一条**新**指令。
    let Some(cmd) = recv_gimbal_cmd(&ctx) else {
        return;
    };

    // 【调试·临时】指令指纹 = 出解算时刻 + 载荷比特。视觉端出一次新解算 → 指纹必变；
    // 槽位没更新 → 指纹不变、不计入。用 `to_bits` 比对是为了避开浮点 NaN 比较问题。
    let cmd_key = (
        cmd.timestamp_ns,
        [
            cmd.yaw_deg.to_bits(),
            cmd.pitch_deg.to_bits(),
            cmd.distance_m.to_bits(),
        ],
        cmd.fire_advice,
    );
    if stats.last_cmd_key != Some(cmd_key) {
        stats.last_cmd_key = Some(cmd_key);
        stats.new_cmds += 1;
    }
    stats.latest_cmd = Some(cmd);

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

/// 【调试·临时】每秒打印一次自瞄指令到达率，用于判断"卡顿"出在**指令供应**还是**执行端**。
///
/// 判读方法：
/// - `新指令/s` 明显低于 `帧/s`（例如 帧 100/s、指令 20/s）→ **视觉端出指令慢**，云台只能"走走停停"；
/// - 两者接近（每帧都有新指令）→ 指令供应充足，卡顿不在这一环，要往 PID 之前的链路找；
/// - `帧/s` 本身很低 → 自瞄开启时被门控的帧本来就不多，瓶颈在功能开关或帧率。
///
/// 确认结论后，删除本系统、`TalosCmdStats` 资源及注册即可。
fn log_cmd_rate(time: Res<Time>, mut stats: ResMut<TalosCmdStats>) {
    stats.window_secs += time.delta_secs();
    if stats.window_secs < 1.0 {
        return; // 不够一秒，继续累计
    }
    let secs = stats.window_secs;
    stats.window_secs = 0.0;
    // 取走并清零本窗口计数（下一窗口重新累计）。
    let frames = std::mem::take(&mut stats.frames);
    let new_cmds = std::mem::take(&mut stats.new_cmds);

    if frames == 0 {
        info!("auto-aim cmd rate: 本窗口自瞄未开启（process_subscription 0 帧被调度）");
        return;
    }

    let frames_hz = frames as f32 / secs;
    let cmds_hz = new_cmds as f32 / secs;
    // 新鲜度 = 多少比例的帧拿到了**新**指令；100% 表示每帧都有新解算结果。
    let fresh_pct = new_cmds as f32 / frames as f32 * 100.0;

    match stats.latest_cmd {
        Some(cmd) => info!(
            "auto-aim cmd rate: 新指令 {cmds_hz:.1}/s | 帧 {frames_hz:.1}/s | 每帧新鲜度 {fresh_pct:.0}% \
             | 最近 yaw={:.2} pitch={:.2} dist={:.3} fire={}",
            cmd.yaw_deg, cmd.pitch_deg, cmd.distance_m, cmd.fire_advice
        ),
        None => info!(
            "auto-aim cmd rate: 新指令 {cmds_hz:.1}/s | 帧 {frames_hz:.1}/s | 每帧新鲜度 {fresh_pct:.0}% \
             | 尚未收到任何指令（订阅端槽位为空）"
        ),
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
    let align_rot_mat = M_ALIGN_MAT3;   // 对齐旋转矩阵
    // 直接左乘对齐矩阵，即可得到 ROS 坐标系下的向量。
    let new_translation = align_rot_mat * vec3;
    // 单位沿用输入（本项目世界坐标单位是**米**），此处只做轴重排/符号翻转，不改变尺度。
    new_translation
}

/// 四元数（旋转）Bevy→ROS：`A * q * A⁻¹`（A 为由 `M` 生成的旋转四元数）。
/// 这是"换基"公式——把同一个物理旋转在另一套坐标轴下重新表达：
/// 左乘 A 转到新轴，右乘 A⁻¹ 再把作用轴还原，净效果是坐标轴整体转了 M。
pub fn to_ros_quat(quat: Quat) -> Quat {
    let align_rot_mat = M_ALIGN_MAT3;   // 对齐旋转矩阵
    // `Quat::from_mat3`：由 3x3 旋转矩阵转出等价四元数；`.inverse()` 求逆（单位四元数即共轭）。
    let align_quat = Quat::from_mat3(&align_rot_mat);
    // 为什么是这个公式？因为 Bevy 的四元数是右手系，ROS 是左手系。
    // 所以需要先左乘对齐四元数，再右乘对齐四元数的逆，才能得到 ROS 坐标系下的四元数。（线性代数证明格式就是如果从一个坐标系旋转到另一个坐标系，需要先左乘旋转矩阵，再右乘旋转矩阵的逆。）
    // new_rotation： ROS 坐标系下的四元数
    let new_rotation = align_quat * quat * align_quat.inverse();
    // 单位沿用输入（本项目世界坐标单位是**四元数**），此处只做轴重排/符号翻转，不改变尺度。
    new_rotation
}
