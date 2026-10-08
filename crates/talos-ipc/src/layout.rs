//! 消息二进制布局契约（跨进程 / 跨语言的 ABI）。
//!
//! 【本文件是做什么的】仿真器（Rust）与视觉程序/桥（C++）通过共享内存或网络交换**裸字节**，
//! 双方必须对"每个消息在内存里长什么样"完全一致。本文件用 `#[repr(C)]` 结构体把这份契约
//! 固化下来，并用编译期断言（`const _: () = assert!(...)`）锁死大小与关键字段偏移——
//! 一旦有人改了字段顺序/类型导致布局变化，**编译就会失败**，避免运行期才爆诡异错位。
//!
//! 【关键约定】
//! - `#[repr(C)]`：按 C 语言规则排布字段（顺序固定、按对齐补 padding），禁止编译器重排。
//! - `align(N)`：指定结构体对齐字节数（常见 32/64 = 缓存行大小），让热点字段不跨缓存行。
//! - 所有多字节整数/浮点均为**小端**（Rust 在 x86/ARM 上即小端，故直接 memcpy 不做转换）。
//! - 字段名后缀标注单位，如 `_ns`=纳秒、`_m`=米、`_deg`=度、`_radps`=弧度每秒、
//!   `_mps`=米每秒、`_mps2`=米每二次方秒、`_radps2`=弧度每二次方秒。
//! - `_pad*` 是显式填充字段：只为凑齐对齐/预留扩展位，内容无意义，协议两端都忽略。

use std::sync::atomic::AtomicU8;

// 图像分辨率约定：所有图像消息都按此尺寸，逐像素 RGB（3 字节）。单位：像素。
pub const IMAGE_WIDTH: u32 = 1440;
pub const IMAGE_HEIGHT: u32 = 1080;

pub const CACHE_LINE_SIZE: usize = 64; // 缓存行大小，用于对齐热点数据
pub const SHM_MAGIC: u32 = 0x54414C05; // 共享内存魔数（ASCII "TAL\x05"），用于识别/校验
pub const SHM_VERSION: u32 = 2; // 布局版本号：改动布局时递增，两端版本不符即拒用

pub const IMAGE_CHANNELS: u32 = 3; // RGB 三通道
pub const IMAGE_SIZE: usize = (IMAGE_WIDTH * IMAGE_HEIGHT * IMAGE_CHANNELS) as usize; // 单帧字节数
pub const IMAGE_POOL_SIZE: usize = IMAGE_SIZE * 3; // 图像三缓冲池总字节数（3 帧）
pub const SHM_NAME_META: &str = "talos_ipc_meta"; // 元数据共享内存名
pub const SHM_NAME_IMAGE_POOL: &str = "talos_ipc_image_pool"; // 图像池共享内存名

pub const FLAG_NEW: u8 = 0x80; // 三缓冲 state 的最高位：置位表示"有新数据待读"
pub const INDEX_MASK: u8 = 0x03; // state 低 2 位掩码：取槽位下标（0/1/2）

/// 图像帧元数据（描述一帧图像，真正的像素在图像池里）。总大小 32 字节，对齐 32。
/// 字段偏移：seq@0, timestamp_ns@8, width@16, height@20, buffer_id@24, format@25, _pad@26。
#[repr(C, align(32))]
#[derive(Debug, Clone, Copy, Default)]
pub struct ImageMeta {
    pub seq: u64,          // @0  帧序号（单调递增，用于判新旧）
    pub timestamp_ns: u64, // @8  采集时刻，单位纳秒
    pub width: u32,        // @16 图像宽，单位像素
    pub height: u32,       // @20 图像高，单位像素
    pub buffer_id: u8,     // @24 该帧像素落在图像池的第几个 buffer（0..=2）
    pub format: u8,        // @25 像素格式（0 = RGB8）
    pub _pad: [u8; 6],     // @26 填充至 32 字节
}
// 编译期断言：大小不符即编译失败——这是"布局契约"的硬约束。
const _: () = assert!(size_of::<ImageMeta>() == 32);

/// 位姿元数据（一个逻辑槽位一帧）。总大小 64 字节，对齐 64（一整条缓存行）。
/// 字段偏移：frame_seq@0, position@8, quaternion@20, timestamp_ns@40, _pad@48。
#[repr(C, align(64))]
#[derive(Debug, Clone, Copy)]
pub struct PoseMeta {
    pub frame_seq: u64,        // @0  帧序号
    pub position: [f32; 3],    // @8  [x, y, z]，单位米（ROS 坐标系）
    pub quaternion: [f32; 4],  // @20 四元数 [w, x, y, z]（单位四元数）
    pub timestamp_ns: u64,     // @40 采集时刻，单位纳秒（前面有 4 字节对齐填充）
    pub _pad: [u8; 16],        // @48 填充，兼作"辅助数据"通道（见 publisher.rs）
}
const _: () = assert!(size_of::<PoseMeta>() == 64);

impl Default for PoseMeta {
    fn default() -> Self {
        Self {
            frame_seq: 0,
            position: [0.0; 3],
            quaternion: [0.0; 4],
            timestamp_ns: 0,
            _pad: [0; 16],
        }
    }
}

/// 云台指令（视觉程序 → 仿真）。总大小 32 字节，对齐 32。
/// 字段偏移：timestamp_ns@0, yaw_deg@8, pitch_deg@12, distance_m@16, fire_advice@20, _pad@21。
#[repr(C, align(32))]
#[derive(Debug, Clone, Copy, Default)]
pub struct GimbalCmd {
    pub timestamp_ns: u64, // @0  出解算结果时刻，单位纳秒
    pub yaw_deg: f32,      // @8  目标偏航角，单位度
    pub pitch_deg: f32,    // @12 目标俯仰角，单位度
    pub distance_m: f32,   // @16 目标距离，单位米（-1.0 为"No solution"哨兵值）
    pub fire_advice: u8,   // @20 开火建议：1=建议开火，其余=不开火
    pub _pad: [u8; 11],    // @21 填充至 32 字节
}
const _: () = assert!(size_of::<GimbalCmd>() == 32);

/// 相机内参（针孔模型）。总大小 128 字节，对齐 64（尾部有 16 字节对齐填充）。
/// 字段偏移：timestamp_ns@0, fx@8, fy@16, cx@24, cy@32, distortion@40, width@80, height@84, _pad@88。
#[repr(C, align(64))]
#[derive(Debug, Clone, Copy, Default)]
pub struct CameraInfo {
    pub timestamp_ns: u64,     // @0  标定时刻，单位纳秒
    pub fx: f64,               // @8  水平焦距，单位像素
    pub fy: f64,               // @16 垂直焦距，单位像素
    pub cx: f64,               // @24 主点 x，单位像素
    pub cy: f64,               // @32 主点 y，单位像素
    pub distortion: [f64; 5],  // @40 畸变系数 [k1, k2, p1, p2, k3]（仿真为全 0）
    pub width: u32,            // @80 图像宽，单位像素
    pub height: u32,           // @84 图像高，单位像素
    pub _pad: [u8; 24],        // @88 填充至 128 字节
}
const _: () = assert!(size_of::<CameraInfo>() == 128);

/// 底盘运动学观测（供视觉/控制算法做状态估计）。总大小 128 字节，对齐 64。
/// 字段偏移：frame_seq@0, timestamp_ns@8, dt_s@16, v_body@20, wz_radps@28,
/// wheel_linear_mps@32, wheel_angular_radps@48, a_body@64, alpha_z_radps2@72,
/// rpy_rad@76, gyro_xyz_radps@88, accel_xyz_mps2@100, _pad@112。
#[repr(C, align(64))]
#[derive(Debug, Clone, Copy)]
pub struct ChassisObservation {
    pub frame_seq: u64,              // @0  帧序号
    pub timestamp_ns: u64,           // @8  采集时刻，单位纳秒
    pub dt_s: f32,                   // @16 距上一帧的时间步长，单位秒
    pub v_body: [f32; 2],            // @20 车体系平面速度 [vx, vy]，单位 m/s
    pub wz_radps: f32,               // @28 偏航角速度，单位 rad/s
    pub wheel_linear_mps: [f32; 4],  // @32 四轮线速度，单位 m/s
    pub wheel_angular_radps: [f32; 4], // @48 四轮角速度，单位 rad/s
    pub a_body: [f32; 2],            // @64 车体系平面加速度 [ax, ay]，单位 m/s²
    pub alpha_z_radps2: f32,         // @72 偏航角加速度，单位 rad/s²
    pub rpy_rad: [f32; 3],           // @76 横滚/俯仰/偏航角 [roll, pitch, yaw]，单位 rad
    pub gyro_xyz_radps: [f32; 3],    // @88 三轴陀螺仪读数，单位 rad/s
    pub accel_xyz_mps2: [f32; 3],    // @100 三轴加速度计读数，单位 m/s²
    pub _pad: [u8; 16],              // @112 填充至 128 字节
}
const _: () = assert!(size_of::<ChassisObservation>() == 128);

impl Default for ChassisObservation {
    fn default() -> Self {
        Self {
            frame_seq: 0,
            timestamp_ns: 0,
            dt_s: 0.0,
            v_body: [0.0; 2],
            wz_radps: 0.0,
            wheel_linear_mps: [0.0; 4],
            wheel_angular_radps: [0.0; 4],
            a_body: [0.0; 2],
            alpha_z_radps2: 0.0,
            rpy_rad: [0.0; 3],
            gyro_xyz_radps: [0.0; 3],
            accel_xyz_mps2: [0.0; 3],
            _pad: [0; 16],
        }
    }
}

/// 图像三缓冲控制块。总大小 192 字节，对齐 64。
/// 偏移：state@0, write_idx@1, read_idx@2, _pad1@3(61B), slots@64(3×32B)。
/// 三缓冲原理见 triple_buffer.rs；`state` 同时编码"有无新数据"与"就绪槽下标"。
#[repr(C, align(64))]
pub struct ImageTripleBuffer {
    pub state: AtomicU8,   // @0  原子的控制字：最高位 FLAG_NEW + 低 2 位槽索引
    pub write_idx: u8,     // @1  生产者的可写槽下标（0..=2）
    pub read_idx: u8,      // @2  消费者上次读的槽下标（0..=2）
    pub _pad1: [u8; 61],   // @3  填充至 64 字节
    pub slots: [ImageMeta; 3], // @64 三个数据槽
}
const _: () = assert!(size_of::<ImageTripleBuffer>() == 192);

/// 位姿三缓冲控制块。总大小 256 字节，对齐 64。
/// 偏移：state@0, write_idx@1, read_idx@2, _pad1@3(61B), slots@64(3×64B)。
#[repr(C, align(64))]
pub struct PoseTripleBuffer {
    pub state: AtomicU8,       // @0  控制字（同 ImageTripleBuffer）
    pub write_idx: u8,         // @1  生产者的可写槽下标
    pub read_idx: u8,          // @2  消费者上次读的槽下标
    pub _pad1: [u8; 61],       // @3  填充至 64 字节
    pub slots: [PoseMeta; 3],  // @64 三个数据槽
}
const _: () = assert!(size_of::<PoseTripleBuffer>() == 256);

/// 云台指令三缓冲控制块（方向相反：视觉端写、仿真端读）。总大小 192 字节，对齐 64。
/// 偏移：state@0, write_idx@1, read_idx@2, _pad1@3(61B), slots@64(3×32B)。
#[repr(C, align(64))]
pub struct GimbalTripleBuffer {
    pub state: AtomicU8,        // @0  控制字
    pub write_idx: u8,          // @1  可写槽下标
    pub read_idx: u8,           // @2  消费者上次读的槽下标
    pub _pad1: [u8; 61],        // @3  填充至 64 字节
    pub slots: [GimbalCmd; 3],  // @64 三个数据槽
}
const _: () = assert!(size_of::<GimbalTripleBuffer>() == 192);

/// 共享内存头（魔数/版本/心跳）。总大小 64 字节，对齐 64。
/// 偏移：magic@0, version@4, created_ns@8, heartbeat_ns@16, image_width@24, image_height@28, _pad@32。
#[repr(C, align(64))]
pub struct ShmHeader {
    pub magic: u32,          // @0  魔数 SHM_MAGIC，用于确认是自家共享内存
    pub version: u32,        // @4  布局版本 SHM_VERSION，两端不符即拒用
    pub created_ns: u64,     // @8  创建时刻，单位纳秒
    pub heartbeat_ns: u64,   // @16 最近心跳时刻，单位纳秒（对端据此判断存活）
    pub image_width: u32,    // @24 约定图像宽，单位像素
    pub image_height: u32,   // @28 约定图像高，单位像素
    pub _pad: [u8; 32],      // @32 填充至 64 字节
}
const _: () = assert!(size_of::<ShmHeader>() == 64);

pub const GROUND_TRUTH_MAX_TARGETS: usize = 16; // 真值批次里战车的容量上限
pub const GROUND_TRUTH_MAX_RUNES: usize = 4; // 真值批次里能量机关的容量上限

/// 单个战车真值。总大小 64 字节，对齐 32。
/// 偏移：frame_seq@0, timestamp_ns@8, team@16, armor_label@17, is_outpost@18,
/// _pad1@19, position@20, vyaw@32, yaw@36, _pad@40。
#[repr(C, align(32))]
#[derive(Debug, Clone, Copy, Default)]
pub struct GroundTruthTarget {
    pub frame_seq: u64,      // @0  帧序号
    pub timestamp_ns: u64,   // @8  采集时刻，单位纳秒
    pub team: u8,            // @16 阵营：0=红, 1=蓝
    pub armor_label: u8,     // @17 装甲编号
    pub is_outpost: u8,      // @18 是否前哨站：0/1
    pub _pad1: u8,           // @19 填充
    pub position: [f32; 3],  // @20 世界位置 [x, y, z]，单位米（ROS 坐标系）
    pub vyaw: f32,           // @32 偏航角速度，单位 rad/s
    pub yaw: f32,            // @36 偏航角，单位 rad
    pub _pad: [u8; 24],      // @40 填充至 64 字节
}
const _: () = assert!(size_of::<GroundTruthTarget>() == 64);

/// 单个能量机关真值。总大小 128 字节，对齐 64。
/// 偏移：frame_seq@0, timestamp_ns@8, team@16, rune_mode@17, mechanism_state@18, _pad1@19,
/// r_center_odom@20, radius@32, current_angle@36, v_roll@40, direction@44,
/// sin_amplitude@48, sin_omega@52, sin_phase@56, sin_offset@60, relative_time@64,
/// blade_id@68, target_activations@72, _pad@77。
#[repr(C, align(64))]
#[derive(Debug, Clone, Copy)]
pub struct GroundTruthRune {
    pub frame_seq: u64,             // @0  帧序号
    pub timestamp_ns: u64,          // @8  采集时刻，单位纳秒
    pub team: u8,                   // @16 阵营：0=红, 1=蓝
    pub rune_mode: u8,              // @17 大小符：0=小, 1=大
    pub mechanism_state: u8,        // @18 机关状态：0=未激活,1=激活中,2=已激活,3=失败
    pub _pad1: u8,                  // @19 填充
    pub r_center_odom: [f32; 3],    // @20 中心位置 [x, y, z]，单位米（ROS 系）
    pub radius: f32,                // @32 半径，单位米（当前未填）
    pub current_angle: f32,         // @36 当前转角，单位 rad
    pub v_roll: f32,                // @40 滚转角速度，单位 rad/s（当前未填）
    pub direction: i32,             // @44 旋转方向：+1 顺时针 / -1 逆时针
    pub sin_amplitude: f32,         // @48 变速正弦振幅，单位 rad/s
    pub sin_omega: f32,             // @52 变速角频率，单位 rad/s
    pub sin_phase: f32,             // @56 正弦相位（当前未填）
    pub sin_offset: f32,            // @60 变速速度偏移，单位 rad/s
    pub relative_time: f32,         // @64 累计时间，单位 s
    pub blade_id: i32,              // @68 叶片编号（-1 表示未填）
    pub target_activations: [u8; 5], // @72 5 个靶位激活状态（0..=3）
    pub _pad: [u8; 20],             // @77 填充至 128 字节
}
const _: () = assert!(size_of::<GroundTruthRune>() == 128);

impl Default for GroundTruthRune {
    fn default() -> Self {
        Self {
            frame_seq: 0,
            timestamp_ns: 0,
            team: 0,
            rune_mode: 0,
            mechanism_state: 0,
            _pad1: 0,
            r_center_odom: [0.0; 3],
            radius: 0.0,
            current_angle: 0.0,
            v_roll: 0.0,
            direction: 0,
            sin_amplitude: 0.0,
            sin_omega: 0.0,
            sin_phase: 0.0,
            sin_offset: 0.0,
            relative_time: 0.0,
            blade_id: -1,
            target_activations: [0; 5],
            _pad: [0; 20],
        }
    }
}

/// 一帧真值批次（定长数组 + 计数）。总大小 1664 字节，对齐 64。
/// 偏移：frame_seq@0, timestamp_ns@8, target_count@16, rune_count@20,
/// targets@32(16×64B), runes@1088(4×128B), _pad@1600。
#[repr(C, align(64))]
#[derive(Debug, Clone, Copy)]
pub struct GroundTruthBatch {
    pub frame_seq: u64,                  // @0    帧序号
    pub timestamp_ns: u64,               // @8    采集时刻，单位纳秒
    pub target_count: u32,               // @16   本帧有效的 targets 个数（≤ 16）
    pub rune_count: u32,                 // @20   本帧有效的 runes 个数（≤ 4）
    pub targets: [GroundTruthTarget; GROUND_TRUTH_MAX_TARGETS], // @32   战车数组
    pub runes: [GroundTruthRune; GROUND_TRUTH_MAX_RUNES],       // @1088 机关数组
    pub _pad: [u8; 64],                  // @1600 填充至 1664 字节
}
const _: () = assert!(size_of::<GroundTruthBatch>() == 1664);

impl Default for GroundTruthBatch {
    fn default() -> Self {
        Self {
            frame_seq: 0,
            timestamp_ns: 0,
            target_count: 0,
            rune_count: 0,
            targets: [GroundTruthTarget::default(); GROUND_TRUTH_MAX_TARGETS],
            runes: [GroundTruthRune::default(); GROUND_TRUTH_MAX_RUNES],
            _pad: [0; 64],
        }
    }
}

/// 运行状态（如"仿真是否在接收视觉指令"）。总大小 64 字节，对齐 64。
/// 偏移：timestamp_ns@0, following@8, _pad@9。
#[repr(C, align(64))]
#[derive(Debug, Clone, Copy)]
pub struct RuntimeState {
    pub timestamp_ns: u64, // @0  状态时刻，单位纳秒
    pub following: u8,     // @8  是否跟随视觉指令：0=否, 1=是
    pub _pad: [u8; 55],    // @9  填充至 64 字节
}
const _: () = assert!(size_of::<RuntimeState>() == 64);

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            timestamp_ns: 0,
            following: 0,
            _pad: [0; 55],
        }
    }
}

/// 共享内存"元数据区"的顶层布局：整个区域的首地址即按此结构解释。
/// 总大小 3712 字节；各字段偏移：header@0, image@64, poses@256, gimbal_cmd@1536,
/// camera_info@1728, chassis_observation@1856, ground_truth@1984, runtime_state@3648。
/// （图像**像素**不放这里，而另开一块 `SHM_NAME_IMAGE_POOL` 大区，此结构只放 32 字节元数据。）
#[repr(C)]
pub struct ShmMetaRegion {
    pub header: ShmHeader,                       // @0    头：魔数/版本/心跳
    pub image: ImageTripleBuffer,                // @64   图像元数据三缓冲
    pub poses: [PoseTripleBuffer; 5],            // @256  5 个位姿槽（各一个三缓冲）
    pub gimbal_cmd: GimbalTripleBuffer,          // @1536 云台指令三缓冲（视觉→仿真）
    pub camera_info: CameraInfo,                 // @1728 相机内参
    pub chassis_observation: ChassisObservation, // @1856 底盘观测
    pub ground_truth: GroundTruthBatch,          // @1984 真值批次
    pub runtime_state: RuntimeState,             // @3648 运行状态
}
const _: () = assert!(size_of::<ShmMetaRegion>() == 3712);
// 关键字段偏移断言：把"协议里写死的偏移"钉死，防止插入字段后静默错位。
const _: () = assert!(std::mem::offset_of!(ShmMetaRegion, camera_info) == 1728);
const _: () = assert!(std::mem::offset_of!(ShmMetaRegion, chassis_observation) == 1856);
const _: () = assert!(std::mem::offset_of!(ShmMetaRegion, ground_truth) == 1984);
const _: () = assert!(std::mem::offset_of!(ShmMetaRegion, runtime_state) == 3648);

/// 位姿逻辑槽编号，同时作为 `poses[]` 数组下标（`#[repr(u8)]`：底层用 1 字节存储，
/// 与协议里"槽编号是 u8"一致）。改名会改变协议编号，两端须同步。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoseIndex {
    Gimbal = 0, // 云台朝向
    Odom = 1,   // 云台世界位置（里程计原点）
    Muzzle = 2, // 枪口相对云台偏移
    Camera = 3, // 相机相对云台偏移
    // Legacy compatibility channel.
    // New integrations should consume `ShmMetaRegion::chassis_observation` instead.
    // 兼容旧消费者的第 4 槽（内容复用为底盘观测），新集成应改读 chassis_observation。
    ChassisObservation = 4,
}

// 三缓冲的**初始状态**必须显式设定，不能被零填充破坏（见 publisher.rs 的强调）：
//   state=1  → 就绪槽是 1 且**未置** FLAG_NEW（消费者首次读到"无新数据"而非脏数据）；
//   write_idx=0 → 生产者可写槽 0；read_idx=2 → 消费者上次读槽 2。
// 这样三槽职责清晰：0 写、1 就绪、2 已读。
impl Default for ImageTripleBuffer {
    fn default() -> Self {
        Self {
            state: AtomicU8::new(1),
            write_idx: 0,
            read_idx: 2,
            _pad1: [0; 61],
            slots: [ImageMeta::default(); 3],
        }
    }
}

impl Default for PoseTripleBuffer {
    fn default() -> Self {
        Self {
            state: AtomicU8::new(1),
            write_idx: 0,
            read_idx: 2,
            _pad1: [0; 61],
            slots: [PoseMeta::default(); 3],
        }
    }
}

impl Default for GimbalTripleBuffer {
    fn default() -> Self {
        Self {
            state: AtomicU8::new(1),
            write_idx: 0,
            read_idx: 2,
            _pad1: [0; 61],
            slots: [GimbalCmd::default(); 3],
        }
    }
}

impl Default for ShmHeader {
    fn default() -> Self {
        Self {
            magic: SHM_MAGIC,
            version: SHM_VERSION,
            created_ns: 0,
            heartbeat_ns: 0,
            image_width: IMAGE_WIDTH,
            image_height: IMAGE_HEIGHT,
            _pad: [0; 32],
        }
    }
}

impl Default for ShmMetaRegion {
    fn default() -> Self {
        Self {
            header: ShmHeader::default(),
            image: ImageTripleBuffer::default(),
            poses: [
                PoseTripleBuffer::default(),
                PoseTripleBuffer::default(),
                PoseTripleBuffer::default(),
                PoseTripleBuffer::default(),
                PoseTripleBuffer::default(),
            ],
            gimbal_cmd: GimbalTripleBuffer::default(),
            camera_info: CameraInfo::default(),
            chassis_observation: ChassisObservation::default(),
            ground_truth: GroundTruthBatch::default(),
            runtime_state: RuntimeState::default(),
        }
    }
}
