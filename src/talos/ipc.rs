//! 运行时 IPC 通道选择：网络转发（默认，对接 Linux 侧 talos_ipc_bridge）或共享内存。
//!
//! 环境变量：
//! - `TALOS_IPC=net|shm`（默认 `net`）：`net` 走网络转发，`shm` 走原有 memmap 共享内存
//!   （同机 Linux 场景仍可用旧方式）。
//! - `TALOS_IPC_REMOTE`：网络模式对端 NUC 的 IP（默认 `192.168.2.200`）。

use bevy::prelude::*;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use talos_ipc::net_ipc::NetIpcPublisher;
use talos_ipc::*;

const DEFAULT_REMOTE: &str = "192.168.2.200";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IpcMode {
    Net,
    Shm,
}

fn ipc_mode() -> IpcMode {
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
pub enum TalosIpcPublisher {
    Shm(ShmPublisher),
    Net(NetIpcPublisher),
}

/// 订阅端：共享内存订阅器或网络云台指令槽位。
pub enum TalosIpcSubscriber {
    Shm(ShmSubscriber),
    Net(Arc<Mutex<Option<GimbalCmd>>>),
}

impl TalosIpcSubscriber {
    pub fn recv_gimbal_cmd(&mut self) -> Option<GimbalCmd> {
        match self {
            Self::Shm(subscriber) => subscriber.recv_gimbal_cmd(),
            Self::Net(slot) => slot.lock().ok()?.clone(),
        }
    }
}

impl TalosIpcPublisher {
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

    pub fn publish_chassis_observation(&mut self, observation: ChassisObservation) {
        match self {
            Self::Shm(publisher) => publisher.publish_chassis_observation(observation),
            Self::Net(publisher) => publisher.publish_chassis_observation(observation),
        }
    }

    pub fn publish_ground_truth(&mut self, batch: &GroundTruthBatch) {
        match self {
            Self::Shm(publisher) => publisher.publish_ground_truth(batch),
            Self::Net(publisher) => publisher.publish_ground_truth(batch),
        }
    }

    pub fn set_camera_info(&mut self, info: CameraInfo) {
        match self {
            Self::Shm(publisher) => publisher.set_camera_info(info),
            Self::Net(publisher) => publisher.set_camera_info(info),
        }
    }

    pub fn publish_runtime_state(&mut self, state: RuntimeState) {
        match self {
            Self::Shm(publisher) => publisher.publish_runtime_state(state),
            Self::Net(publisher) => publisher.publish_runtime_state(state),
        }
    }

    pub fn update_heartbeat(&mut self) {
        match self {
            Self::Shm(publisher) => publisher.update_heartbeat(),
            Self::Net(publisher) => publisher.update_heartbeat(),
        }
    }

    /// 图像发布：`data` 为 1440x1080 RGB 原始帧。
    /// `before_commit` 在图像提交前后调用（共享内存模式保持原有同步握手语义）。
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
pub struct TalosIpc {
    pub publisher: TalosIpcPublisher,
    pub subscriber: Option<TalosIpcSubscriber>,
}

impl TalosIpc {
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
                let remote = std::env::var("TALOS_IPC_REMOTE")
                    .unwrap_or_else(|_| DEFAULT_REMOTE.to_string());
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
                let subscriber = TalosIpcSubscriber::Net(publisher.gimbal_slot());
                Ok(Self {
                    publisher: TalosIpcPublisher::Net(publisher),
                    subscriber: Some(subscriber),
                })
            }
        }
    }
}
