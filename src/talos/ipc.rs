//! 运行时 IPC 通道选择：网络转发（默认，对接 Linux 侧 talos_ipc_bridge）或共享内存。
//!
//! 【修改】新增文件（提交 9a85cc8）。原先 plugin.rs 只会创建共享内存
//! ShmPublisher/ShmSubscriber（仅限同机）；仿真迁到 Windows、视觉程序在局域网
//! NUC 上后，需要网络通道，但同机 Linux 调试仍要旧方式，因此用环境变量在
//! 运行时二选一，两侧代码都保留。
//!
//! 环境变量：
//! - `TALOS_IPC=net|shm`（默认 `net`）：`net` 走网络转发，`shm` 走原有 memmap 共享内存
//!   （同机 Linux 场景仍可用旧方式）。
//! - `TALOS_IPC_REMOTE`：网络模式对端 NUC 的 IP（默认 `192.168.2.200`）。
//!
//! 【教学】为什么用 enum 而不是 trait 对象？
//! 本文件是"用 enum 模拟接口/trait 对象"的典型写法：`TalosIpcPublisher` 有两种
//! 具体实现（`ShmPublisher` / `NetIpcPublisher`），它们由不同文件实现、方法名相同，
//! 但**没有**共同 trait。做法是把两者各当一个 enum 变体包起来，每个方法都写成
//! `match self { Self::Shm(p) => p.foo(...), Self::Net(p) => p.foo(...) }`——
//! 于是调用方只认 `TalosIpcPublisher` 这一个类型，运行时按变体分发到底层实现。
//! 与 `Box<dyn TalosPublisher>`（动态 trait 对象）相比：无堆分配、无虚表间接调用、
//! 编译期就知道所有分支；代价是每加一种通道，所有方法都要多写一条 match 分支。
//! （`TalosIpcSubscriber` 同理——只是网络模式的"订阅端"只是一块共享槽位，见下。）

use bevy::prelude::*;
use std::net::IpAddr; // IP 地址类型（v4/v6 通用）
use std::sync::{Arc, Mutex}; // 跨线程共享 + 互斥访问（见下方网络订阅端）
use talos_ipc::net_ipc::NetIpcPublisher; // 网络发布端
use talos_ipc::*; // 共享内存发布/订阅端、各消息结构体（ShmPublisher 等）

// 网络模式默认对端地址（局域网内视觉程序所在 NUC），可被 TALOS_IPC_REMOTE 覆盖。
const DEFAULT_REMOTE: &str = "192.168.2.200";

// `#[derive(...)]` 自动派生常用 trait：Debug（可打印）、Clone/Copy（可按值复制，
// 因为只是个标签）、PartialEq/Eq（可用 == 比较）——纯"通道种类标签"，无字段负载。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IpcMode {
    Net,
    Shm,
}

/// 读环境变量 `TALOS_IPC` 决定运行时通道；只有恰好为 "shm" 才走共享内存，其余（含未设置）一律网络。
fn ipc_mode() -> IpcMode {
    // 三步串成一条链：`var` 读环境变量（Result，未设置时为 Err）→
    // `unwrap_or_default` 把 Err 变成空字符串 ""→ `to_ascii_lowercase` 统一小写 →
    // `as_str` 转成 &str 供 match 比较（match 的分支是字符串字面量）。
    match std::env::var("TALOS_IPC")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "shm" => IpcMode::Shm,
        _ => IpcMode::Net, // 默认 net
    }
}

/// 发布端：包装共享内存 / 网络两种实现，接口与原 `ShmPublisher` 一致。
/// 每个变体括住的类型不同（`Shm(ShmPublisher)` / `Net(NetIpcPublisher)`），
/// 这就是"带数据的 enum 变体"——Rust 的 enum 不只是标签，还能各自携带任意载荷。
pub enum TalosIpcPublisher {
    Shm(ShmPublisher),
    Net(NetIpcPublisher),
}

/// 订阅端：共享内存订阅器或网络云台指令槽位。
/// 网络模式的"订阅"没有独立对象：TCP/UDP 接收线程把最新云台指令写进一块槽位，
/// 这里直接持有该槽位的共享句柄（`Arc<Mutex<Option<...>>>`）——
/// `Arc` 让多个线程共享同一块数据，`Mutex` 保证同一时刻只有一个线程能读写，
/// `Option` 表示"还没收到过任何指令"。
pub enum TalosIpcSubscriber {
    Shm(ShmSubscriber),
    Net(Arc<Mutex<Option<GimbalCmd>>>),
}

impl TalosIpcSubscriber {
    /// 取最近一次收到的云台指令；没有则返回 `None`。
    pub fn recv_gimbal_cmd(&mut self) -> Option<GimbalCmd> {
        match self {
            Self::Shm(subscriber) => subscriber.recv_gimbal_cmd(),
            // `.lock()` 可能因其他线程 panic 而失败 → `.ok()` 吞掉错误变 Option；
            // `?` 在 None 时提前返回 None；`.clone()` 复制出指令（GimbalCmd: Copy）。
            Self::Net(slot) => slot.lock().ok()?.clone(),
        }
    }
}

impl TalosIpcPublisher {
    /// 发布一个位姿槽位（**收发契约**，两种通道共用同一语义）：
    /// - `index`：写入哪一个逻辑槽位（`PoseIndex`，如 Odom/Gimbal/Muzzle/Camera）；
    /// - `position`：`[x, y, z]`，单位 **米**，已转到 ROS 坐标系（见 plugin.rs 的 `M_ALIGN_MAT3`）；
    /// - `quaternion`：`[w, x, y, z]` 四元数（标量在前），单位/归一化；
    /// - `frame_seq`：本帧递增序号，接收端用它判新旧、丢乱序；
    /// - `timestamp_ns`：采集时刻，单位 **纳秒**（Unix 纪元）。
    // `#[allow(...)]`：关掉 clippy 就"参数过多"发出的 lint 警告——这里参数多是接口契约使然。
    #[allow(clippy::too_many_arguments)]
    pub fn publish_pose(
        &mut self,
        index: PoseIndex,
        position: [f32; 3],
        quaternion: [f32; 4],
        frame_seq: u64,
        timestamp_ns: u64,
    ) {
        match self {
            Self::Shm(publisher) => {
                publisher.publish_pose(index, position, quaternion, frame_seq, timestamp_ns);
            }
            Self::Net(publisher) => {
                publisher.publish_pose(index, position, quaternion, frame_seq, timestamp_ns);
            }
        }
    }

    /// 带辅助数据的位姿发布：`aux_f32` 四个 f32 塞进位姿结构体尾部预留的 `_pad`
    /// 字段（布局见 talos-ipc/layout.rs 的 `PoseMeta`），用于旧的"底盘观测"兼容槽位。
    pub fn publish_pose_with_aux(
        &mut self,
        index: PoseIndex,
        position: [f32; 3],
        quaternion: [f32; 4],
        aux_f32: [f32; 4],
        frame_seq: u64,
        timestamp_ns: u64,
    ) {
        match self {
            Self::Shm(publisher) => publisher.publish_pose_with_aux(
                index,
                position,
                quaternion,
                aux_f32,
                frame_seq,
                timestamp_ns,
            ),
            Self::Net(publisher) => publisher.publish_pose_with_aux(
                index,
                position,
                quaternion,
                aux_f32,
                frame_seq,
                timestamp_ns,
            ),
        }
    }

    /// 发布底盘运动学观测（线/角速度、加速度、轮速、姿态角等，各字段单位见 layout.rs 的 `ChassisObservation`）。
    pub fn publish_chassis_observation(&mut self, observation: ChassisObservation) {
        match self {
            Self::Shm(publisher) => publisher.publish_chassis_observation(observation),
            Self::Net(publisher) => publisher.publish_chassis_observation(observation),
        }
    }

    /// 发布本帧全体真值（敌方战车位姿/朝向 + 能量机关状态），字段与单位见 ground_truth.rs 与 layout.rs。
    pub fn publish_ground_truth(&mut self, batch: &GroundTruthBatch) {
        match self {
            Self::Shm(publisher) => publisher.publish_ground_truth(batch),
            Self::Net(publisher) => publisher.publish_ground_truth(batch),
        }
    }

    /// 发布相机内参（焦距 fx/fy、主点 cx/cy、畸变系数，单位像素），供接收端做投影/反投影。
    pub fn set_camera_info(&mut self, info: CameraInfo) {
        match self {
            Self::Shm(publisher) => publisher.set_camera_info(info),
            Self::Net(publisher) => publisher.set_camera_info(info),
        }
    }

    /// 发布运行状态（如自瞄订阅开关是否开启），用于告知对端"仿真当前是否在接收视觉指令"。
    pub fn publish_runtime_state(&mut self, state: RuntimeState) {
        match self {
            Self::Shm(publisher) => publisher.publish_runtime_state(state),
            Self::Net(publisher) => publisher.publish_runtime_state(state),
        }
    }

    /// 心跳保活：每帧调用，网络模式按阈值补发心跳包，供对端判断仿真是否存活。
    pub fn update_heartbeat(&mut self) {
        match self {
            Self::Shm(publisher) => publisher.update_heartbeat(),
            Self::Net(publisher) => publisher.update_heartbeat(),
        }
    }

    /// 图像发布：`data` 为 1440x1080 RGB 原始帧。
    /// `before_commit` 在图像提交前后调用（共享内存模式保持原有同步握手语义）。
    /// 返回 `bool`：共享内存模式下为"本帧是否成功提交"，网络模式恒为 `true`。
    /// `F: FnOnce(&mut Self)` 是泛型闭包参数——调用方传进来的回调只允许被调用一次，
    /// 这样回调内部能借用整个 publisher（如顺便发位姿），而不触发借用冲突。
    pub fn try_publish_synchronized_image<F>(
        &mut self,
        data: &[u8],
        seq: u64,
        timestamp_ns: u64,
        before_commit: F,
    ) -> bool
    where
        F: FnOnce(&mut Self),
    {
        match self {
            Self::Shm(publisher) => {
                // 共享内存：走原子三缓冲的"消费者已取走上一帧"握手，成功才继续。
                if publisher.try_publish_synchronized_image(data, seq, timestamp_ns, |_| {}) {
                    before_commit(self);
                    true
                } else {
                    false
                }
            }
            Self::Net(publisher) => {
                // 网络模式只保留最新一帧，允许跳帧，无需同步握手
                publisher.publish_image_raw(data, seq, timestamp_ns);
                before_commit(self);
                true
            }
        }
    }
}

/// 按环境变量创建 IPC 通道（发布端 + 订阅端）。
/// `subscriber` 用 `Option` 包着：共享内存下若对端还没建好则连接失败为 `None`，
/// 此时只发不收（发布端照常工作）。
pub struct TalosIpc {
    pub publisher: TalosIpcPublisher,
    pub subscriber: Option<TalosIpcSubscriber>,
}

impl TalosIpc {
    /// 返回 `Result<Self, Box<dyn std::error::Error + Send + Sync>>`：
    /// `Box<dyn ...>` 是错误处理的"统一出口"——把各种具体的错误类型（`ShmError`、
    /// 字符串解析错误等）装箱成同一个 trait 对象；`Send + Sync` 约束保证该错误
    /// 可以跨线程传递。调用方只需知道"成功/失败 + 可打印的错误"。
    pub fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        match ipc_mode() {
            IpcMode::Shm => {
                info!("talos ipc: shm (memmap) mode");
                let publisher = ShmPublisher::create()?;
                let subscriber = ShmSubscriber::connect().ok();
                Ok(Self {
                    publisher: TalosIpcPublisher::Shm(publisher),
                    subscriber: subscriber.map(TalosIpcSubscriber::Shm),
                })
            }
            IpcMode::Net => {
                // 读对端 IP：环境变量缺失时回退到 DEFAULT_REMOTE。
                let remote = std::env::var("TALOS_IPC_REMOTE")
                    .unwrap_or_else(|_| DEFAULT_REMOTE.to_string());
                // `parse()` 返回 Result；`.map_err(|e| format!(...))?` 把解析错误
                // 重包装成带上下文（原始值 + 原因）的字符串错误再向上传播（`?`）。
                let remote_ip: IpAddr = remote
                    .parse()
                    .map_err(|e| format!("invalid TALOS_IPC_REMOTE '{remote}': {e}"))?;
                info!(
                    "talos ipc: net mode -> {} (udp {}, tcp {})",
                    remote_ip,
                    talos_ipc::net_ipc::NET_UDP_PORT,
                    talos_ipc::net_ipc::NET_TCP_PORT
                );
                let publisher = NetIpcPublisher::connect(remote_ip)?;
                // 网络订阅端 = 复用发布端内部的云台指令槽位句柄。
                let subscriber = TalosIpcSubscriber::Net(publisher.gimbal_slot());
                Ok(Self {
                    publisher: TalosIpcPublisher::Net(publisher),
                    subscriber: Some(subscriber),
                })
            }
        }
    }
}
