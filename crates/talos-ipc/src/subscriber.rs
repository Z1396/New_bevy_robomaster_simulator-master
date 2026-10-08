//! 共享内存订阅端（消费者侧 API）——从共享内存读取发布端写好的数据。
//!
//! 【教学】与 publisher.rs 对称：这里用 `TripleBufferConsumer` 从三缓冲读最新一帧，
//! 或直接读整块覆盖写的字段。`connect` 时会校验魔数与版本，头不对就拒绝连接，
//! 避免把不兼容的共享内存当成自家布局误读。订阅端只读不写，因此多数方法是 `&self`。

use crate::layout::*;
use crate::shm::{ShmError, ShmRegion};
use crate::triple_buffer::TripleBufferConsumer;

/// 共享内存订阅端。只持有元数据区（读云台指令/底盘观测等）；图像按需另开映射再读。
pub struct ShmSubscriber {
    meta_region: ShmRegion,
}

impl ShmSubscriber {
    /// 打开已存在的元数据共享内存并校验魔数/版本；不匹配则返回错误。
    pub fn connect() -> Result<Self, ShmError> {
        let meta_region = ShmRegion::open(SHM_NAME_META, size_of::<ShmMetaRegion>())?;

        unsafe {
            let meta = meta_region.as_ref::<ShmMetaRegion>();
            // 魔数不对：这块内存不是 Talos 的（或布局完全不同）。
            if meta.header.magic != SHM_MAGIC {
                return Err(ShmError::InvalidSize);
            }
            // 版本不对：布局可能已变，强行解读会错位，直接拒绝。
            if meta.header.version != SHM_VERSION {
                return Err(ShmError::InvalidSize);
            }
        }

        Ok(Self { meta_region })
    }

    /// 读取最新云台指令；无新数据时返回 `None`。
    /// `consumer.borrow()` 返回 `Option<&GimbalCmd>`，`.copied()` 把它变成 `Option<GimbalCmd>`
    /// （因为 `GimbalCmd: Copy`，按值取出即可，不必持有对共享内存的引用）。
    pub fn recv_gimbal_cmd(&mut self) -> Option<GimbalCmd> {
        unsafe {
            let meta = self.meta_region.as_mut::<ShmMetaRegion>();
            let mut consumer = TripleBufferConsumer::new(
                &meta.gimbal_cmd.state,
                &mut meta.gimbal_cmd.read_idx,
                &meta.gimbal_cmd.slots,
            );

            consumer.borrow().copied()
        }
    }

    /// 是否有尚未读走的新云台指令（只查 FLAG_NEW，不消费）。
    pub fn has_gimbal_cmd(&self) -> bool {
        unsafe {
            let meta = self.meta_region.as_ref::<ShmMetaRegion>();
            (meta
                .gimbal_cmd
                .state
                .load(std::sync::atomic::Ordering::Acquire)
                & FLAG_NEW)
                != 0
        }
    }

    /// 读取底盘观测；`timestamp_ns == 0` 视为"发布端还没写过"，返回 `None`。
    pub fn chassis_observation(&self) -> Option<ChassisObservation> {
        unsafe {
            let meta = self.meta_region.as_ref::<ShmMetaRegion>();
            let observation = meta.chassis_observation; // Copy 一份出来，避免持续占用共享内存引用
            if observation.timestamp_ns == 0 {
                None
            } else {
                Some(observation)
            }
        }
    }
}
