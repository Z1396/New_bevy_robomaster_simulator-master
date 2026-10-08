//! Talos 离屏图像捕获：把仿真相机渲染出的一帧图像，连同该帧的位姿，一起经 IPC 发出。
//!
//! 【与 `src/capture/` 管线的关系】本文件**不重复造轮子**，而是复用通用捕获管线：
//! - `src/capture/`（driver.rs 等）是引擎中立的"离屏渲染 + GPU 回读"框架——
//!   负责建离屏相机、把渲染结果拷回 CPU 内存，并通过 `GpuCaptureHandler` /
//!   `SnapshotSync` / `SnapshotAsync` 三个 trait 把"拿到帧之后干什么"交给调用方；
//! - 本文件只实现这三个 trait 的 Talos 版本（`TalosSnapshotCreator` / `TalosSnapshotSync`
//!   / `TalosSnapshot`），在回调里把图像数据 + 位姿字节发出去。别的模块（如 ros2）
//!   也复用同一套管线，只是回调换成自己的实现。
//!
//! 【帧同步的关键】图像、位姿、真值必须来自**同一次世界快照**，否则会出现"画面是上一帧、
//! 位姿是这一帧"的错配。做法：在 `ExtractSchedule`（渲染线程从主世界拷贝数据的阶段）
//! 里一次性抓取相机/云台/枪口位姿到 `ExtractedPoseData`，之后 GPU 回读完成时直接用这份快照。
//!
//! 【Extract 渲染阶段】Bevy 渲染在独立子 App（`RenderApp`）里跑；`Extract` 是每帧把主世界
//! 数据搬进渲染世界的参数包装器（见 `extract_pose_data` 的形参），没被 `Extract<>` 包着的
//! 数据在渲染阶段读不到。

use crate::capture::{
    CameraFov, CaptureBundle, CaptureSource, ImageHandle, compute_camera_intrinsics,
    driver::{
        CaptureConfig, CaptureFrameId, CapturedFrame, CapturedFrameKind, GpuCaptureHandler,
        SnapshotAsync, SnapshotSync,
    },
    setup_capture_camera, setup_preview_window, sync_capture_camera,
};
use crate::components::{Controlled, InfantryGimbal, InfantryLaunchOffset, SubscribeAutoAim};
use crate::systems::{ChassisObservationFrame, GameplaySystems};
use crate::talos::ipc::TalosIpcPublisher;
use crate::talos::plugin::{to_ros_quat, to_ros_translation};
use bevy::ecs::world::DeferredWorld;
use bevy::prelude::*;
use bevy::render::{Extract, ExtractSchedule, RenderApp, RenderSystems};
use std::f32::consts::PI;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use talos_ipc::*;

// 全局帧序号计数器：`AtomicU64` 保证多线程自增安全；用 `static`（全局单例）而非资源，
// 是因为它的生命周期贯穿整个进程、无需随 App 创建。每帧 `fetch_add(1)` 递增。
static FRAME_SEQ: AtomicU64 = AtomicU64::new(0);

/// 每帧的帧号与时间戳，作为资源供各系统读取，保证一帧内所有消息共享同一时间基准。
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct TalosFrameStamp {
    /// 单调递增帧序号（从 0 开始），接收端用它判新旧/丢乱序。
    pub frame_seq: u64,
    /// 采集时刻，单位 **纳秒**（Unix 纪元至今）。
    pub timestamp_ns: u64,
}

/// 每帧在 `Last` 阶段递增帧号并记录当前时间戳（注册见 plugin.rs）。
pub fn advance_talos_frame_stamp(mut stamp: ResMut<TalosFrameStamp>) {
    // `fetch_add` 返回**自增前**的旧值，正好当作本帧序号；`Relaxed` 序足够（只需原子性）。
    stamp.frame_seq = FRAME_SEQ.fetch_add(1, Ordering::Relaxed);
    stamp.timestamp_ns = now_ns();
}

/// Extracted pose data from MainApp to RenderApp for synchronized publishing
/// 在 `ExtractSchedule` 抓拍、随渲染世界携带到图像回读完成的"位姿快照"。
#[derive(Resource, Clone, Default)]
pub struct ExtractedPoseData {
    pub frame_seq: u64,
    pub timestamp_ns: u64,
    // 字段私有：外部只经 `valid` 判断可用性，避免读到半成品快照。
    pose: Option<CapturedPoseData>,
    pub valid: bool,
}

/// Pose data captured at frame snapshot time
/// 一帧内一次性抓取的位姿集合（**均已转到 ROS 坐标系**）。
#[derive(Clone)]
struct CapturedPoseData {
    /// 云台在世界系下的位置 `[x, y, z]`，单位 **米**（ROS 系）。
    gimbal_ros: [f32; 3],
    /// 云台朝向四元数 `[w, x, y, z]`（ROS 系，单位四元数）。
    gimbal_quat: [f32; 4],
    /// 枪口相对云台的平移 `[x, y, z]`，单位 **米**（ROS 系，云台为原点）。
    muzzle_rel: [f32; 3],
    /// 相机相对云台的平移 `[x, y, z]`，单位 **米**（ROS 系，云台为原点）。
    camera_rel: [f32; 3],
    /// 底盘运动学观测（各字段单位见 talos-ipc/layout.rs 的 `ChassisObservation`）。
    chassis_observation: ChassisObservation,
}

/// 取当前 Unix 时间，单位 **纳秒**；测量失败（系统时钟早于 1970）时回退 0。
fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

// 捕获管线要求的两段式回调的一环：先在主世界（快照时刻）构造这个"同步快照"，
// 它再被交给渲染线程，待 GPU 回读完成时转成 `SnapshotAsync` 真正执行发送。
struct TalosSnapshotSync {
    frame_seq: u64,
    timestamp_ns: u64,
    pose: CapturedPoseData,
}

impl SnapshotSync for TalosSnapshotSync {
    // `self: Box<Self>`：以装箱形式**按值消耗**自身——回调只可能被调用一次，
    // 借此把内部数据（pose）move 出去，避免拷贝。
    fn captured(
        self: Box<Self>,
        world: &mut DeferredWorld,
        _config: &CaptureConfig,
    ) -> Box<dyn SnapshotAsync> {
        // 从（渲染）世界取回发布端句柄；`.clone()` 只增 Arc 引用计数。
        let ctx = world.resource::<TalosCaptureContextShared>().0.clone();

        // 返回 trait 对象：调用方只认 `SnapshotAsync` 接口，不关心具体类型。
        Box::new(TalosSnapshot {
            ctx,
            frame_seq: self.frame_seq,
            timestamp_ns: self.timestamp_ns,
            pose: self.pose,
        })
    }
}

struct TalosSnapshot {
    ctx: Arc<Mutex<TalosIpcPublisher>>,
    frame_seq: u64,
    timestamp_ns: u64,
    pose: CapturedPoseData,
}

impl SnapshotAsync for TalosSnapshot {
    // `CapturedFrame<'_>` 带生命周期的借用：`frame.data` 是渲染回读缓冲的切片，
    // 只在本次回调内有效，故不复制、直接发（零拷贝）。
    fn captured(&mut self, frame: CapturedFrame<'_>) {
        // 只要 RGB8（其余如深度图本次不用）——`!=` 直接跳过。
        if frame.kind != CapturedFrameKind::Rgb8 {
            return;
        }

        // RGB8 每像素 3 字节，做长度自检防止越界/错帧。
        let expected_size = (frame.width * frame.height * 3) as usize;
        if frame.data.len() != expected_size {
            warn!(
                "图像大小不匹配: expected {} bytes, got {} bytes",
                expected_size,
                frame.data.len()
            );
            return;
        }

        // 分辨率必须严格等于约定值，否则视觉端内参对不上。
        if frame.width != IMAGE_WIDTH || frame.height != IMAGE_HEIGHT {
            warn!(
                "image reesolution mismatched: expected {}x{}, got {}x{}",
                IMAGE_WIDTH, IMAGE_HEIGHT, frame.width, frame.height
            );
            return;
        }

        if let Ok(mut publisher) = self.ctx.lock() {
            // 加锁发布"图像 + 位姿"这一对：`before_commit` 闭包保证位姿紧贴图像一同提交，
            // 两者共享同一 frame_seq/timestamp_ns，接收端可据此配对。返回值此处忽略。
            let _ = publisher.try_publish_synchronized_image(
                frame.data,
                self.frame_seq,
                self.timestamp_ns,
                |publisher| {
                    publish_pose_data(publisher, self.frame_seq, self.timestamp_ns, &self.pose);
                },
            );
        }
    }
}

// 捕获管线实现的第三段：管线在需要抓拍时调用 `captured`，本类型据此构造同步快照。
#[derive(Default)]
struct TalosSnapshotCreator {}

impl GpuCaptureHandler for TalosSnapshotCreator {
    // `&World` 只读访问世界（此处即主世界）；返回 `Option` 表示"这帧有没有可发的位姿"。
    fn captured(
        &self,
        world: &World,
        _frame_id: Option<CaptureFrameId>,
    ) -> Option<Box<dyn SnapshotSync>> {
        // Timestamp, frame sequence and pose must come from the same ExtractSchedule snapshot.
        // 时间戳/帧号/位姿必须来自同一次 Extract 快照，否则图文错位。
        // `world.get_resource::<...>()?`：资源不存在（如首帧尚未提取）就返回 None。
        let extracted = world.get_resource::<ExtractedPoseData>()?;
        // `valid` 为 false 说明该帧没抓到位姿（相机/云台缺失），跳过。
        if !extracted.valid {
            return None;
        }
        // `pose` 是私有 Option 字段，`.clone()?`：无则 None，有则复制一份快照。
        let pose = extracted.pose.clone()?;

        // 打包成同步快照交给渲染线程（真正的发送发生在 GPU 回读完成后）。
        Some(Box::new(TalosSnapshotSync {
            frame_seq: extracted.frame_seq,
            timestamp_ns: extracted.timestamp_ns,
            pose,
        }))
    }
}

// 【修改】类型从 ShmPublisher 换成 TalosIpcPublisher（分发封装）：网络模式下
// 图像/位姿走 UDP/TCP 转发，共享内存模式下行为与原版完全一致（含同步握手）。
// 该资源同时注册到主世界与渲染世界，两边共享同一个发布端句柄。
#[derive(Resource, Clone, Deref, DerefMut)]
pub struct TalosCaptureContextShared(pub Arc<Mutex<TalosIpcPublisher>>);

/// 捕获上下文资源：跨系统共享的发布端句柄 + 相机视场角（fov_y，单位**弧度**）。
#[derive(Resource, Clone)]
pub struct TalosCaptureContext {
    pub publisher: Arc<Mutex<TalosIpcPublisher>>,
    pub fov_y: f32,
}

/// 捕获插件：把通用捕获管线（`CaptureBundle`）接上 Talos 的回调与调度。
pub struct TalosCapturePlugin {
    pub config: CaptureConfig,
    pub context: TalosCaptureContext,
}

/// 把"自瞄订阅开关"作为运行状态发给对端（告知视觉程序当前是否在接收其指令）。
pub fn publish_talos_runtime_state_system(
    context: Option<Res<TalosCaptureContext>>,
    frame_stamp: Res<TalosFrameStamp>,
    following: Res<SubscribeAutoAim>,
) {
    let Some(ctx) = context else {
        return;
    };

    if let Ok(mut publisher) = ctx.publisher.lock() {
        publisher.publish_runtime_state(RuntimeState {
            timestamp_ns: frame_stamp.timestamp_ns,
            // `u8::from(bool)`：把原子布尔读出的 bool 转成协议约定的 0/1 字节。
            // `Ordering::Acquire` 保证读到开关值时能看到其前的所有相关写入。
            following: u8::from(following.load(Ordering::Acquire)),
            _pad: [0; 55],
        });
    }
}

impl Plugin for TalosCapturePlugin {
    fn build(&self, app: &mut App) {
        // 建通用捕获管线（离屏相机 + 渲染目标 + GPU 回读），并挂上 Talos 的回调
        // （`GpuCaptureHandler` 盒装成 trait 对象放进 Vec，插件管线性地不关心具体实现）。
        let capture = CaptureBundle::color(
            app,
            self.config.clone(),
            vec![Box::new(TalosSnapshotCreator::default())],
        );
        // 取出渲染目标纹理句柄（`color_target()` 返回 Option，`.unwrap()` 表示这里必须成功）。
        let render_target_handle = capture.color_target().unwrap().clone();

        // 开一块作用域：算相机内参并一次性发给对端（内参不变，启动时发一次即可）。
        {
            let mut publisher = self.context.publisher.lock().unwrap();
            // 由分辨率 + 垂直 fov 推出焦距 fx/fy 与主点 cx/cy（单位像素）。
            let intrinsics = compute_camera_intrinsics(
                self.config.width,
                self.config.height,
                self.context.fov_y,
            );

            publisher.set_camera_info(CameraInfo {
                timestamp_ns: now_ns(),
                fx: intrinsics.fx,
                fy: intrinsics.fy,
                cx: intrinsics.cx,
                cy: intrinsics.cy,
                distortion: [0.0; 5], // 仿真理想相机，畸变系数全 0
                width: intrinsics.width,
                height: intrinsics.height,
                _pad: [0; 24],
            });
        }

        // 注册捕获管线资源与三个系统：
        // - Startup：建相机、建预览窗口（各跑一次）；
        // - Update：`sync_capture_camera` 把捕获相机对齐到当前视角，排在相机跟随之后、
        //   渲染之前（保证渲染用的是本帧最新视角）。
        app.add_plugins(capture)
            .insert_resource(ImageHandle(render_target_handle))
            .insert_resource(CameraFov(self.context.fov_y))
            .insert_resource(self.context.clone())
            .add_systems(Startup, setup_capture_camera)
            .add_systems(Startup, setup_preview_window)
            .add_systems(
                Update,
                sync_capture_camera
                    .after(GameplaySystems::Camera)
                    .before(RenderSystems::Render),
            );

        // `sub_app_mut(RenderApp)`：进入渲染子 App 注册资源与系统。
        // `ExtractSchedule` 是每帧把主世界数据搬进渲染世界的阶段，这里挂 `extract_pose_data`
        // 抓取位姿快照；三个资源同时注入主/渲染两个世界以共享同一发布端与快照。
        app.sub_app_mut(RenderApp)
            .insert_resource(TalosCaptureContextShared(self.context.publisher.clone()))
            .insert_resource(self.context.clone())
            .insert_resource(ExtractedPoseData::default())
            .add_systems(ExtractSchedule, extract_pose_data);
    }
}

/// Extract pose data from MainApp to RenderApp
/// 在 `ExtractSchedule` 一次性抓取本帧位姿，写入 `ExtractedPoseData` 供渲染线程后续发送。
fn extract_pose_data(
    mut pose_data: ResMut<ExtractedPoseData>,
    // `Extract<T>`：渲染阶段的"跨世界取值"包装——虽然系统跑在渲染世界，但被
    // `Extract<...>` 包住的参数会去**主世界**读取（帧号、实体查询、观测帧都在主世界）。
    // 不包 `Extract` 的参数（如上面的 `pose_data`）则取自渲染世界。
    frame_stamp: Extract<Res<TalosFrameStamp>>,
    camera: Extract<Query<&GlobalTransform, With<CaptureSource>>>, // 捕获相机世界变换
    gimbal: Extract<Query<&GlobalTransform, (With<Controlled>, With<InfantryGimbal>)>>, // 本机云台
    muzzle_offset: Extract<
        Query<(&GlobalTransform, &Transform), (With<InfantryLaunchOffset>, With<Controlled>)>,
    >, // 枪口：世界变换 + 局部变换（算相对云台偏移用）
    chassis_obs: Extract<Res<ChassisObservationFrame>>, // 本帧底盘观测（速度/加速度等）
) {
    pose_data.frame_seq = frame_stamp.frame_seq;
    pose_data.timestamp_ns = frame_stamp.timestamp_ns;

    // `single()` 返回 Result：恰好命中一个才 Ok，0 个或多个都 Err（不 panic）。
    // 任一位姿源缺失就标记"本帧无有效位姿"，避免发出残缺数据。
    let Ok(cam_transform) = camera.single() else {
        pose_data.pose = None;
        pose_data.valid = false;
        return;
    };
    let Ok(gimbal_transform) = gimbal.single() else {
        pose_data.pose = None;
        pose_data.valid = false;
        return;
    };
    let Ok((muzzle_global, muzzle_local)) = muzzle_offset.single() else {
        pose_data.pose = None;
        pose_data.valid = false;
        return;
    };

    // 全部位姿齐备 → 组装快照并置 valid。
    pose_data.pose = Some(captured_pose_data(
        cam_transform,
        gimbal_transform,
        muzzle_global,
        muzzle_local,
        &chassis_obs,
        pose_data.frame_seq,
        pose_data.timestamp_ns,
    ));
    pose_data.valid = true;
}

fn captured_pose_data(
    cam_transform: &GlobalTransform,
    gimbal_transform: &GlobalTransform,
    muzzle_global: &GlobalTransform,
    muzzle_local: &Transform,
    chassis_obs: &ChassisObservationFrame,
    frame_seq: u64,
    timestamp_ns: u64,
) -> CapturedPoseData {
    // `reparented_to`：求出"若把 self 挂到另一变换之下"得到的相对变换。
    // 于是 `cam_rel` = 相机相对云台的位姿，`muzzle_rel` = 枪口相对云台的位姿（单位米）。
    let cam_rel = cam_transform.reparented_to(gimbal_transform);
    let muzzle_rel = muzzle_global.reparented_to(gimbal_transform);

    // 枪口"瞄准方向"四元数：云台世界朝向 × 枪口局部朝向 × 绕 Z 轴 +90°(`PI/2`)。
    // 末尾的固定旋转用于把枪口模型坐标系对齐到"前向"约定（打乱会得到错误朝向）；
    // `EulerRot::ZYX` 指定欧拉角解算顺序（这里只有 Z 分量非零）。
    let gimbal_rot = gimbal_transform.rotation()
        * muzzle_local.rotation
        * Quat::from_euler(EulerRot::ZYX, 0.0, 0.0, PI / 2.0);

    // 全部转到 ROS 坐标系（Bevy Y-up → ROS Z-up，见 plugin.rs 的 M_ALIGN_MAT3）。
    let gimbal_ros = to_ros_translation(gimbal_transform.translation());
    let gimbal_rot = to_ros_quat(gimbal_rot);
    let muzzle = to_ros_translation(muzzle_rel.translation);
    let camera = to_ros_translation(cam_rel.translation);

    CapturedPoseData {
        // 拆成 `[x, y, z]` 数组（单位米，ROS 系）——IPC 结构体用固定长度数组，不用 Vec3。
        gimbal_ros: [gimbal_ros.x, gimbal_ros.y, gimbal_ros.z],
        // 四元数按 `[w, x, y, z]`（标量在前）排列，与协议约定一致。
        gimbal_quat: [gimbal_rot.w, gimbal_rot.x, gimbal_rot.y, gimbal_rot.z],
        muzzle_rel: [muzzle.x, muzzle.y, muzzle.z],
        camera_rel: [camera.x, camera.y, camera.z],
        chassis_observation: ChassisObservation {
            frame_seq,
            timestamp_ns,
            dt_s: chassis_obs.dt_s, // 本帧时间步长，单位秒
            v_body: [chassis_obs.v_body.x, chassis_obs.v_body.y], // 车体系平面速度 [vx, vy]，m/s
            wz_radps: chassis_obs.wz_radps, // 偏航角速度，rad/s
            wheel_linear_mps: chassis_obs.wheel_linear_mps, // 四轮线速度，m/s
            wheel_angular_radps: chassis_obs.wheel_angular_radps, // 四轮角速度，rad/s
            a_body: [chassis_obs.a_body.x, chassis_obs.a_body.y], // 车体系平面加速度，m/s²
            alpha_z_radps2: chassis_obs.alpha_z_radps2, // 偏航角加速度，rad/s²
            rpy_rad: [
                chassis_obs.rpy_rad.x,
                chassis_obs.rpy_rad.y,
                chassis_obs.rpy_rad.z,
            ], // 横滚/俯仰/偏航角，rad
            gyro_xyz_radps: [
                chassis_obs.gyro_xyz_radps.x,
                chassis_obs.gyro_xyz_radps.y,
                chassis_obs.gyro_xyz_radps.z,
            ], // 三轴陀螺仪读数，rad/s
            accel_xyz_mps2: [
                chassis_obs.accel_xyz_mps2.x,
                chassis_obs.accel_xyz_mps2.y,
                chassis_obs.accel_xyz_mps2.z,
            ], // 三轴加速度计读数，m/s²
            _pad: [0; 16], // 预留填充，保证结构体对齐（见 layout.rs）
        },
    }
}

/// 把本帧所有位姿/观测打包发布（`index` 决定写到哪个逻辑槽位，见 layout.rs 的 `PoseIndex`）。
fn publish_pose_data(
    publisher: &mut TalosIpcPublisher,
    frame_seq: u64,
    timestamp_ns: u64,
    pose: &CapturedPoseData,
) {
    // Odom 槽：云台世界位置 + 单位四元数 `[1,0,0,0]`（位置槽不带旋转，故给恒等旋转）。
    publisher.publish_pose(
        PoseIndex::Odom,
        pose.gimbal_ros,
        [1.0, 0.0, 0.0, 0.0],
        frame_seq,
        timestamp_ns,
    );

    // Gimbal 槽：位置置零（原点即云台），只发朝向四元数。
    publisher.publish_pose(
        PoseIndex::Gimbal,
        [0.0, 0.0, 0.0],
        pose.gimbal_quat,
        frame_seq,
        timestamp_ns,
    );

    // Muzzle 槽：枪口相对云台的位置 + 单位四元数。
    publisher.publish_pose(
        PoseIndex::Muzzle,
        pose.muzzle_rel,
        [1.0, 0.0, 0.0, 0.0],
        frame_seq,
        timestamp_ns,
    );

    // Camera 槽：相机相对云台的位置 + 单位四元数。
    publisher.publish_pose(
        PoseIndex::Camera,
        pose.camera_rel,
        [1.0, 0.0, 0.0, 0.0],
        frame_seq,
        timestamp_ns,
    );

    // 底盘观测：先补齐本帧序号/时间戳，再走独立通道发布。
    let mut observation = pose.chassis_observation;
    observation.frame_seq = frame_seq;
    observation.timestamp_ns = timestamp_ns;
    publisher.publish_chassis_observation(observation);

    // Legacy compatibility path for consumers still reading pose slot 4.
    // 兼容旧消费者：把底盘观测的关键量塞进 PoseIndex::ChassisObservation(4) 槽的
    // position 与 aux_f32 两个数组（新代码应改读上面的 chassis_observation 通道）。
    publisher.publish_pose_with_aux(
        PoseIndex::ChassisObservation,
        [
            observation.v_body[0],
            observation.v_body[1],
            observation.wz_radps,
        ], // position 复用为 [vx, vy, wz]
        observation.wheel_angular_radps, // quaternion 复用为四轮角速度
        [
            observation.a_body[0],
            observation.a_body[1],
            observation.alpha_z_radps2,
            observation.dt_s,
        ], // aux 复用为 [ax, ay, alpha_z, dt]
        frame_seq,
        timestamp_ns,
    );
}
