//! 共享内存发布端（生产者侧 API）——把仿真数据写进共享内存，供视觉程序读取。
//!
//! 【教学】本文件是"发布/订阅"模型的**发布端**：
//! - 老数据（位姿/图像）走三缓冲（`TripleBufferProducer`），写入互不阻塞、读者拿最新；
//! - 简单数据（相机内参、底盘观测、真值、运行状态）直接整块覆盖写对应字段——
//!   它们要么低频、要么天然可容忍短暂撕裂，无需三缓冲；
//! - 全文件对共享内存的访问都在 `unsafe` 块内：把裸内存强行当作 `ShmMetaRegion` 解释，
//!   安全性由 layout.rs 的布局契约保证（大小/偏移一致）。

use crate::layout::*;
use crate::shm::{ShmError, ShmRegion};
use crate::triple_buffer::TripleBufferProducer;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

/// 共享内存发布端。持有两块映射区：元数据区（小、含控制字）与图像池（大、装像素）。
pub struct ShmPublisher {
    meta_region: ShmRegion,    // 元数据区（ShmMetaRegion 布局）
    image_pool: ShmRegion,     // 图像像素池（3 帧轮转）
    current_buffer_id: u8,     // 当前要写入的图像 buffer 下标（0..=2）
    last_synchronized_frame_seq: Option<u64>, // 上一次成功同步发布的帧号（防回退/重发）
}

impl ShmPublisher {
    /// 创建（或覆盖）共享内存并初始化到正确初态；失败返回 `ShmError`。
    pub fn create() -> Result<Self, ShmError> {
        // `size_of::<ShmMetaRegion>()` 决定元数据区大小——两端必须用同一布局值。
        let mut meta_region = ShmRegion::create(SHM_NAME_META, size_of::<ShmMetaRegion>())?;
        let image_pool = ShmRegion::create(SHM_NAME_IMAGE_POOL, IMAGE_POOL_SIZE)?;

        unsafe {
            let meta = meta_region.as_mut::<ShmMetaRegion>();

            // 写入头部：magic/version 供读端校验兼容性，时间戳用于判断发布端是否存活（心跳），
            // 尺寸字段让读端按发布端实际分辨率解释图像缓冲。
            meta.header = ShmHeader {
                magic: SHM_MAGIC,
                version: SHM_VERSION,
                created_ns: Self::now_ns(),
                heartbeat_ns: Self::now_ns(),
                image_width: IMAGE_WIDTH,
                image_height: IMAGE_HEIGHT,
                _pad: [0; 32],
            };

            // 把每个三缓冲的控制字重设为合法初态（CRITICAL：操作系统给共享内存的零填充会把
            // 控制字清成 0，而那并不是"空缓冲"的合法编码，必须逐一重设）。
            // 正确初态：state=1（有 ready 槽）、write_idx=0、read_idx=2。
            Self::init_triple_buffer(&mut meta.image);
            for pose in &mut meta.poses {
                Self::init_triple_buffer(pose);
            }
            Self::init_triple_buffer(&mut meta.gimbal_cmd);
        }

        Ok(Self {
            meta_region,
            image_pool,
            current_buffer_id: 0,
            last_synchronized_frame_seq: None,
        })
    }

    /// 发布一帧图像（不做同步握手，直接覆盖——可能跳帧）。`data` 必须是 `IMAGE_SIZE` 字节的 RGB8。
    pub fn publish_image(&mut self, data: &[u8], seq: u64, timestamp_ns: u64) {
        self.publish_image_with(data, seq, timestamp_ns, |_| {});
    }

    /// 图像发布核心：先拷像素到图像池，再提交元数据（元数据提交 = 读者可见的"完成标记"）。
    /// `before_commit` 在像素拷完、元数据提交前调用——让调用方把"配套数据"（如位姿）一起提交。
    pub fn publish_image_with<F>(
        &mut self,
        data: &[u8],
        seq: u64,
        timestamp_ns: u64,
        before_commit: F,
    ) where
        F: FnOnce(&mut Self),
    {
        assert_eq!(data.len(), IMAGE_SIZE, "Image size mismatch");

        // 轮转使用三个图像 buffer，避免覆盖读者可能正在读的那块。
        let buffer_id = self.current_buffer_id;
        self.current_buffer_id = (self.current_buffer_id + 1) % 3;

        unsafe {
            // 把像素 memcpy 到图像池中 buffer_id 对应的偏移处（零拷贝语义的写入侧）。
            let pool_ptr = self.image_pool.as_ptr();
            let dst = pool_ptr.add(buffer_id as usize * IMAGE_SIZE);
            std::ptr::copy_nonoverlapping(data.as_ptr(), dst, IMAGE_SIZE);
        }

        // Publish data associated with this image only after the expensive pixel copy. The image
        // metadata below is the commit marker observed by consumers.
        // 先拷完昂贵的像素，再调用回调提交配套数据，最后才写图像元数据（读者以此判定"可见"）。
        before_commit(self);

        unsafe {
            let meta = self.meta_region.as_mut::<ShmMetaRegion>();
            // 用一个生产者句柄写图像三缓冲：borrow_mut 拿可写槽，写完后 publish 原子发布。
            let mut producer = TripleBufferProducer::new(
                &meta.image.state,
                &mut meta.image.write_idx,
                &mut meta.image.slots,
            );

            let slot = producer.borrow_mut();
            slot.seq = seq;
            slot.timestamp_ns = timestamp_ns;
            slot.width = IMAGE_WIDTH;
            slot.height = IMAGE_HEIGHT;
            slot.buffer_id = buffer_id; // 告知读者像素在池中哪一块
            slot.format = 0; // 0 = RGB8
            producer.publish();
        }
    }

    /// 带"同步握手"的图像发布：只有确认读者已取走上一次图像与位姿后，才发新的一帧。
    /// 这样保证图像与配套位姿成对交付、不会被覆盖到一半。`#[must_use]`：返回值别丢
    /// （false 表示本帧被跳过，调用方通常应知悉）。
    #[must_use]
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
        // Never publish an older async readback, and never overwrite one half of an image/pose
        // bundle while the consumer is between the two triple buffers.
        // 两个拒绝条件：① 帧号回退（异步回读可能乱序）→ 丢弃旧帧；
        // ② 上一组图像/位姿尚未被读者取走 → 若此刻覆盖，读者可能读到"新图配旧位姿"。
        if self
            .last_synchronized_frame_seq
            .is_some_and(|last_seq| seq <= last_seq)
            || !self.synchronized_frame_consumed()
        {
            return false;
        }

        self.publish_image_with(data, seq, timestamp_ns, before_commit);
        self.last_synchronized_frame_seq = Some(seq);
        true
    }

    /// 检查"上一帧的图像与位姿是否都已被读者消费"（三缓冲 state 的 FLAG_NEW 均已清零）。
    fn synchronized_frame_consumed(&self) -> bool {
        unsafe {
            let meta = self.meta_region.as_ref::<ShmMetaRegion>();
            // FLAG_NEW 为 0 表示读者已把该槽取走（见 triple_buffer.rs 的 CAS）。
            let image_consumed = meta.image.state.load(Ordering::Acquire) & FLAG_NEW == 0;
            // Gimbal, odom, muzzle and camera are consumed with each image. Slot 4 is the legacy
            // chassis-observation channel and is intentionally not part of this handshake.
            // 只检查前 5 槽中的 0..=Camera（即 Gimbal/Odom/Muzzle/Camera）；
            // `..=PoseIndex::Camera as usize` 是"含右端点"的切片区间。
            let poses_consumed = meta.poses[..=PoseIndex::Camera as usize]
                .iter()
                .all(|pose| pose.state.load(Ordering::Acquire) & FLAG_NEW == 0);
            image_consumed && poses_consumed
        }
    }

    /// 发布一个位姿（无辅助数据，aux 全 0）。
    /// `position` 单位米、`quaternion` 为 `[w,x,y,z]`、`frame_seq`、`timestamp_ns`（纳秒）。
    pub fn publish_pose(
        &mut self,
        index: PoseIndex,
        position: [f32; 3],
        quaternion: [f32; 4],
        frame_seq: u64,
        timestamp_ns: u64,
    ) {
        self.publish_pose_with_aux(
            index,
            position,
            quaternion,
            [0.0; 4],
            frame_seq,
            timestamp_ns,
        );
    }

    /// 发布带辅助数据的位姿：aux 写进 `PoseMeta._pad`（16 字节，复用为数据通道）。
    pub fn publish_pose_with_aux(
        &mut self,
        index: PoseIndex,
        position: [f32; 3],
        quaternion: [f32; 4],
        aux_f32: [f32; 4],
        frame_seq: u64,
        timestamp_ns: u64,
    ) {
        unsafe {
            let meta = self.meta_region.as_mut::<ShmMetaRegion>();
            // `index as usize` 把 PoseIndex 当数组下标取对应位姿三缓冲。
            let pose_buf = &mut meta.poses[index as usize];
            let mut producer = TripleBufferProducer::new(
                &pose_buf.state,
                &mut pose_buf.write_idx,
                &mut pose_buf.slots,
            );

            let slot = producer.borrow_mut();
            slot.frame_seq = frame_seq;
            slot.position = position;
            slot.quaternion = quaternion;
            slot.timestamp_ns = timestamp_ns;
            slot._pad = aux_f32_to_bytes(aux_f32);

            producer.publish();
        }
    }

    /// 写相机内参（低频，直接整块覆盖，无需三缓冲）。单位：像素。
    pub fn set_camera_info(&mut self, info: CameraInfo) {
        unsafe {
            let meta = self.meta_region.as_mut::<ShmMetaRegion>();
            meta.camera_info = info;
        }
    }

    /// 写底盘观测（直接覆盖；字段单位见 layout.rs 的 `ChassisObservation`）。
    pub fn publish_chassis_observation(&mut self, observation: ChassisObservation) {
        unsafe {
            let meta = self.meta_region.as_mut::<ShmMetaRegion>();
            meta.chassis_observation = observation;
        }
    }

    /// 写真值批次（`*batch` 解引用复制整块结构，覆盖旧值）。
    pub fn publish_ground_truth(&mut self, batch: &GroundTruthBatch) {
        unsafe {
            let meta = self.meta_region.as_mut::<ShmMetaRegion>();
            meta.ground_truth = *batch;
        }
    }

    /// 写运行状态（如自瞄开关）。
    pub fn publish_runtime_state(&mut self, state: RuntimeState) {
        unsafe {
            let meta = self.meta_region.as_mut::<ShmMetaRegion>();
            meta.runtime_state = state;
        }
    }

    /// 刷新心跳时间戳（读者据此判断发布端是否存活）。
    pub fn update_heartbeat(&mut self) {
        unsafe {
            let meta = self.meta_region.as_mut::<ShmMetaRegion>();
            meta.header.heartbeat_ns = Self::now_ns();
        }
    }

    /// 取当前 Unix 时间，单位纳秒。
    fn now_ns() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    /// 初始化 TripleBuffer 到正确的初始状态
    ///
    /// ShmRegion::create() 使用零填充，会破坏 TripleBuffer 的正确初始状态。
    /// 必须手动重新初始化。
    ///
    /// 正确初始状态:
    /// - state = 1 (ready slot 是 1, 无 FLAG_NEW)
    /// - write_idx = 0 (生产者写入 slot 0)
    /// - read_idx = 2 (消费者上次读取 slot 2)
    // `impl TripleBufferInit`（参数位 impl Trait）= 泛型参数简写：接受任何实现了该 trait 的类型。
    fn init_triple_buffer(buf: &mut impl TripleBufferInit) {
        buf.init_state();
    }
}

/// 把 aux 的 4 个 f32 序列化成 16 字节小端（写进 `PoseMeta._pad`）。
fn aux_f32_to_bytes(aux_f32: [f32; 4]) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    for (i, value) in aux_f32.iter().enumerate() {
        bytes[i * 4..(i + 1) * 4].copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Trait for initializing triple buffer state
/// 三种三缓冲的初态完全一致，用一个小 trait 统一初始化入口（多态：调用方无需区分类型）。
trait TripleBufferInit {
    fn init_state(&mut self);
}

impl TripleBufferInit for ImageTripleBuffer {
    fn init_state(&mut self) {
        // state=1：就绪槽为 1 且无 FLAG_NEW；write_idx=0 可写；read_idx=2 已读。
        self.state.store(1, Ordering::Relaxed);
        self.write_idx = 0;
        self.read_idx = 2;
    }
}

impl TripleBufferInit for PoseTripleBuffer {
    fn init_state(&mut self) {
        self.state.store(1, Ordering::Relaxed);
        self.write_idx = 0;
        self.read_idx = 2;
    }
}

impl TripleBufferInit for GimbalTripleBuffer {
    fn init_state(&mut self) {
        self.state.store(1, Ordering::Relaxed);
        self.write_idx = 0;
        self.read_idx = 2;
    }
}
