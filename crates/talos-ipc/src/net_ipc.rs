//! 网络转发 IPC (对接 Linux 侧 talos_ipc_bridge)
//!
//! - 一个 UDP socket：向 `NUC:9571` 发送所有小数据包（位姿/底盘观测/真值/相机参数/
//!   运行状态/心跳），并在同一 socket 上接收云台指令回传（桥按收包源地址回发）。
//! - 一条 TCP 连接（独立线程）：向 `NUC:9572` 发送 JPEG 图像流，断线每 500ms 重连。
//! - 字节序：双方均为小端，结构体字节直接 memcpy，不做网络字节序转换。
//!
//! UDP 包格式：32 字节包头 + 结构体载荷；TCP 图像帧：40 字节帧头 + JPEG 字节。
//!
//! 【与共享内存实现（shm.rs/publisher.rs）的对照】两者对外语义一致（都是"发布最新、读端
//! 拿最新"），但机制不同：
//! - 共享内存：零拷贝，靠三缓冲 + 原子控制字同步，仅限同一台机器；
//! - 网络：走 socket 传字节，必须序列化/压缩（元数据直接 memcpy、图像编成 JPEG），
//!   有丢包/断线可能，故需要心跳保活与自动重连，可跨机器。
//! 元数据用 UDP（无连接、可能丢包但延迟低，且"只取最新"天然容忍丢包），
//! 图像用 TCP（有连接、保证有序，JPEG 体积大不能丢帧）。

use crate::layout::*;
use std::io::Write;
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// 协议常量与包头
// ---------------------------------------------------------------------------

pub const NET_MAGIC: u32 = 0x5441_4C06;
pub const NET_VERSION: u16 = 1;
pub const NET_UDP_PORT: u16 = 9571;
pub const NET_TCP_PORT: u16 = 9572;

// `pkt_type` 取值：接收端据此判断载荷是哪种结构体（1..=6 走 UDP，7 为对端回传的云台指令，8 走 TCP 图像）。
pub const PKT_TYPE_POSE: u8 = 1;
pub const PKT_TYPE_CHASSIS_OBSERVATION: u8 = 2;
pub const PKT_TYPE_GROUND_TRUTH: u8 = 3;
pub const PKT_TYPE_CAMERA_INFO: u8 = 4;
pub const PKT_TYPE_RUNTIME_STATE: u8 = 5;
pub const PKT_TYPE_HEARTBEAT: u8 = 6;
pub const PKT_TYPE_GIMBAL_CMD: u8 = 7;
pub const PKT_TYPE_IMAGE: u8 = 8;

/// 心跳触发阈值：距上次任意发包超过该时长就补发心跳包（保证 ≥10Hz 保活）。
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);
/// TCP 断线重连间隔。
const TCP_RECONNECT_DELAY: Duration = Duration::from_millis(500);
/// 相机参数低频刷新间隔（对端视觉程序或桥重启后自动恢复，代价可忽略）。
const CAMERA_INFO_REFRESH: Duration = Duration::from_secs(1);
/// JPEG 编码质量（规格要求 70~85）。
const JPEG_QUALITY: u8 = 75;

/// UDP 通用 32 字节包头。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PacketHeader {
    pub magic: u32,
    pub version: u16,
    pub payload_len: u16,
    pub pkt_type: u8,
    pub index: u8,
    pub _pad: u16,
    pub _pad2: u32,
    pub seq: u64,
    pub timestamp_ns: u64,
}
const _: () = assert!(size_of::<PacketHeader>() == 32);
const _: () = assert!(std::mem::offset_of!(PacketHeader, version) == 4);
const _: () = assert!(std::mem::offset_of!(PacketHeader, payload_len) == 6);
const _: () = assert!(std::mem::offset_of!(PacketHeader, pkt_type) == 8);
const _: () = assert!(std::mem::offset_of!(PacketHeader, index) == 9);
const _: () = assert!(std::mem::offset_of!(PacketHeader, seq) == 16);
const _: () = assert!(std::mem::offset_of!(PacketHeader, timestamp_ns) == 24);

/// TCP JPEG 图像流 40 字节帧头。
/// 【修改】pkt_type 必须位于偏移 4、填充在其后（桥按偏移 4 校验 type==8）。
/// 最初实现把 _pad 放在偏移 4、type 放在偏移 5，导致桥"连上就断"的高频
/// 重连循环——这是第一版联网调试时定位到的关键 bug，勿改字段顺序。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ImageFrameHeader {
    pub magic: u32,
    pub pkt_type: u8,
    pub _pad: [u8; 3],
    pub width: u32,
    pub height: u32,
    pub jpeg_len: u32,
    pub _pad2: u32,
    pub seq: u64,
    pub timestamp_ns: u64,
}
const _: () = assert!(size_of::<ImageFrameHeader>() == 40);
const _: () = assert!(std::mem::offset_of!(ImageFrameHeader, pkt_type) == 4);
const _: () = assert!(std::mem::offset_of!(ImageFrameHeader, width) == 8);
const _: () = assert!(std::mem::offset_of!(ImageFrameHeader, height) == 12);
const _: () = assert!(std::mem::offset_of!(ImageFrameHeader, jpeg_len) == 16);
const _: () = assert!(std::mem::offset_of!(ImageFrameHeader, seq) == 24);
const _: () = assert!(std::mem::offset_of!(ImageFrameHeader, timestamp_ns) == 32);

/// 取当前 Unix 时间，单位纳秒（收发包时打时间戳）。
fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// 将 `#[repr(C)]` POD 结构体按字节读取（双方均为小端，直接 memcpy 语义）。
/// 泛型 `T: Copy`：任何可平凡复制的类型都能用；返回的切片借用入参、生命期与之绑定。
fn bytes_of<T: Copy>(value: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(value as *const T as *const u8, size_of::<T>()) }
}

// ---------------------------------------------------------------------------
// 最新帧槽位（渲染线程写入，TCP 线程消费，只保留最新一帧，允许跳帧）
// ---------------------------------------------------------------------------

/// 一帧待编码的原始图像（RGB8）+ 元数据；`data` 长度 = width×height×3 字节。
struct RawImageFrame {
    data: Vec<u8>,
    width: u32,
    height: u32,
    seq: u64,
    timestamp_ns: u64,
}

/// "最新一帧"槽位：渲染线程调用 `publish` 覆盖式写入，TCP 线程调用 `take` 阻塞取走。
/// 只保留最新一帧（旧帧被直接丢弃），因此渲染端永远不会因网络慢而阻塞。
/// `Mutex` 保护数据，`Condvar`（条件变量）让 `take` 在无帧时睡眠、有帧时被唤醒，避免忙等。
struct ImageSlot {
    frame: Mutex<Option<RawImageFrame>>,
    signal: Condvar,
}

impl ImageSlot {
    fn new() -> Self {
        Self {
            frame: Mutex::new(None),
            signal: Condvar::new(),
        }
    }

    fn publish(&self, data: &[u8], width: u32, height: u32, seq: u64, timestamp_ns: u64) {
        let mut guard = self.frame.lock().unwrap();
        // 复用被替换帧的缓冲容量：每帧 1440x1080x3 ≈ 4.6MB，若每次都新分配，
        // 内存紧张的机器上高频分配/释放会加速堆耗尽。
        // `take()` 把旧帧取走（顺带取走它的 Vec 容量），`unwrap_or_default` 首帧时给空 Vec。
        let mut buffer = guard.take().map(|f| f.data).unwrap_or_default();
        buffer.clear();
        buffer.extend_from_slice(data);
        *guard = Some(RawImageFrame {
            data: buffer,
            width,
            height,
            seq,
            timestamp_ns,
        });
        // 唤醒一个正在 `take` 中等待的消费者线程。
        self.signal.notify_one();
    }

    /// 取走最新一帧；槽位为空时阻塞等待。
    fn take(&self) -> RawImageFrame {
        let mut guard = self.frame.lock().unwrap();
        // `loop` + `Condvar::wait`：醒来后必须重新检查条件（标准做法），故用循环而非一次性 if。
        loop {
            if let Some(frame) = guard.take() {
                return frame;
            }
            // `wait` 会原子地释放锁并让本线程睡眠，收到 notify 后重新加锁返回。
            guard = self.signal.wait(guard).unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// NetIpcPublisher
// ---------------------------------------------------------------------------

/// 网络发布端：UDP 元数据 + TCP JPEG 图像流。
pub struct NetIpcPublisher {
    udp: UdpSocket,               // 唯一 UDP socket：发送所有小包，并接收云台指令回包
    remote: SocketAddr,           // 对端 NUC 的 UDP 地址（IP:NET_UDP_PORT）
    seq: AtomicU64,               // 全局递增包序号（原子，供多线程发送用）
    last_packet: Mutex<Instant>,  // 上次成功发包的时刻（用于判断何时补发心跳）
    camera_info: Mutex<Option<(CameraInfo, Instant)>>, // 相机内参 + 上次发送时刻（低频重发）
    runtime_state: Mutex<Option<RuntimeState>>, // 上次发的运行状态（仅变化时重发）
    image_slot: Arc<ImageSlot>,   // 最新帧槽（渲染线程写、TCP 线程读）
    tcp_connected: Arc<AtomicBool>, // TCP 是否已连上（未连上则跳过图像拷发）
    gimbal_slot: Arc<Mutex<Option<GimbalCmd>>>, // 收到的最近云台指令
    threads: Mutex<Vec<JoinHandle<()>>>, // 持有的后台线程句柄（随 publisher 一起释放）
}

impl NetIpcPublisher {
    /// 绑定系统分配的 UDP 端口，启动 UDP 接收线程与 TCP 图像线程。
    pub fn connect(remote_ip: IpAddr) -> std::io::Result<Self> {
        // 绑定端口 0 = 让操作系统随机分配一个本地端口（作为收发源端口）。
        let udp = UdpSocket::bind(("0.0.0.0", 0))?;
        let remote = SocketAddr::new(remote_ip, NET_UDP_PORT);

        let publisher = Self {
            udp,
            remote,
            seq: AtomicU64::new(0),
            // 初值设为"已过期"（当前 - 一个心跳间隔），保证首次 update_heartbeat 就会发。
            last_packet: Mutex::new(Instant::now() - HEARTBEAT_INTERVAL),
            camera_info: Mutex::new(None),
            runtime_state: Mutex::new(None),
            image_slot: Arc::new(ImageSlot::new()),
            tcp_connected: Arc::new(AtomicBool::new(false)),
            gimbal_slot: Arc::new(Mutex::new(None)),
            threads: Mutex::new(Vec::new()),
        };

        // 两条后台线程：一条收云台指令，一条持续发图像。
        publisher.spawn_udp_recv_thread()?;
        publisher.spawn_tcp_thread();

        Ok(publisher)
    }

    fn spawn_udp_recv_thread(&self) -> std::io::Result<()> {
        // `try_clone`：复制一份 socket 句柄给接收线程用（原 socket 仍归 publisher 发送）。
        let socket = self.udp.try_clone()?;
        let gimbal_slot = self.gimbal_slot.clone();
        let handle = thread::Builder::new()
            .name("talos-net-udp-recv".into())
            .spawn(move || udp_recv_loop(socket, gimbal_slot))?;
        self.threads.lock().unwrap().push(handle);
        Ok(())
    }

    fn spawn_tcp_thread(&self) {
        let remote_ip = self.remote.ip();
        let image_slot = self.image_slot.clone();
        let tcp_connected = self.tcp_connected.clone();
        let handle = thread::Builder::new()
            .name("talos-net-image".into())
            .spawn(move || tcp_image_loop(remote_ip, image_slot, tcp_connected))
            .expect("spawn talos-net image thread");
        self.threads.lock().unwrap().push(handle);
    }

    /// 发送一个 UDP 包（fire-and-forget，失败静默忽略，绝不阻塞渲染循环）。
    fn send_packet(&self, pkt_type: u8, index: u8, payload: &[u8]) {
        debug_assert!(payload.len() <= u16::MAX as usize); // 包头用 u16 存长度
        let header = PacketHeader {
            magic: NET_MAGIC,
            version: NET_VERSION,
            payload_len: payload.len() as u16,
            pkt_type,
            index,
            _pad: 0,
            _pad2: 0,
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            timestamp_ns: now_ns(),
        };

        // 包头与载荷必须在同一个数据报内
        // `Vec::with_capacity` 预分配，避免两次 extend 触发扩容。
        let mut buf = Vec::with_capacity(size_of::<PacketHeader>() + payload.len());
        buf.extend_from_slice(bytes_of(&header));
        buf.extend_from_slice(payload);

        // `send_to` 失败（对端不可达等）只忽略；成功则刷新"最后发包时刻"供心跳判断。
        if self.udp.send_to(&buf, self.remote).is_ok() {
            *self.last_packet.lock().unwrap() = Instant::now();
        }
    }

    /// 发一个位姿包：`position` 单位米（ROS 系）、`quaternion` 为 `[w,x,y,z]`、`frame_seq`、`timestamp_ns`（纳秒）。
    pub fn publish_pose(
        &self,
        index: PoseIndex,
        position: [f32; 3],
        quaternion: [f32; 4],
        frame_seq: u64,
        timestamp_ns: u64,
    ) {
        self.publish_pose_with_aux(index, position, quaternion, [0.0; 4], frame_seq, timestamp_ns);
    }

    /// 带辅助数据的位姿包：`aux_f32` 塞进 `PoseMeta._pad`（见 publisher.rs 的 `aux_f32_to_bytes`）。
    pub fn publish_pose_with_aux(
        &self,
        index: PoseIndex,
        position: [f32; 3],
        quaternion: [f32; 4],
        aux_f32: [f32; 4],
        frame_seq: u64,
        timestamp_ns: u64,
    ) {
        let pose = PoseMeta {
            frame_seq,
            position,
            quaternion,
            timestamp_ns,
            _pad: aux_f32_to_bytes(aux_f32),
        };
        // `index as u8`：PoseIndex 是 `#[repr(u8)]`，可直接转成协议里的槽编号字节。
        self.send_packet(PKT_TYPE_POSE, index as u8, bytes_of(&pose));
    }

    /// 发一个底盘观测包（字段单位见 layout.rs 的 `ChassisObservation`）。
    pub fn publish_chassis_observation(&self, observation: ChassisObservation) {
        self.send_packet(PKT_TYPE_CHASSIS_OBSERVATION, 0, bytes_of(&observation));
    }

    /// 发一个真值批次包（战车/机关真值，见 layout.rs 的 `GroundTruthBatch`）。
    pub fn publish_ground_truth(&self, batch: &GroundTruthBatch) {
        self.send_packet(PKT_TYPE_GROUND_TRUTH, 0, bytes_of(batch));
    }

    /// 启动时发一次 + 变更时立即重发（调用方在参数变更时调用本方法）。
    pub fn set_camera_info(&self, info: CameraInfo) {
        self.send_packet(PKT_TYPE_CAMERA_INFO, 0, bytes_of(&info));
        *self.camera_info.lock().unwrap() = Some((info, Instant::now()));
    }

    /// 自瞄开关等运行状态：变更时立即发送。
    pub fn publish_runtime_state(&self, state: RuntimeState) {
        let changed = {
            let guard = self.runtime_state.lock().unwrap();
            // `is_none_or(谓词)`：以前没发过（None），或 `following` 字段变了 → 需要发送。
            // （作用域块让 `guard` 及早释放，避免后面再 `lock()` 时死锁。）
            guard.as_ref().is_none_or(|last| last.following != state.following)
        };
        if changed {
            self.send_packet(PKT_TYPE_RUNTIME_STATE, 0, bytes_of(&state));
        }
        *self.runtime_state.lock().unwrap() = Some(state);
    }

    /// 每渲染帧调用：距上次任意发包 >100ms 就补发心跳；顺带低频重发相机参数，
    /// 保证对端视觉程序或桥重启后无需人工干预即可恢复。
    pub fn update_heartbeat(&self) {
        if self.last_packet.lock().unwrap().elapsed() >= HEARTBEAT_INTERVAL {
            self.send_packet(PKT_TYPE_HEARTBEAT, 0, &[]); // 心跳载荷为空
        }

        // `is_some_and(谓词)`：已发送过相机参数、且距上次发送超过刷新周期。
        let due = {
            let guard = self.camera_info.lock().unwrap();
            guard
                .as_ref()
                .is_some_and(|(_, sent_at)| sent_at.elapsed() >= CAMERA_INFO_REFRESH)
        };
        if due {
            let info = self.camera_info.lock().unwrap().unwrap().0;
            self.send_packet(PKT_TYPE_CAMERA_INFO, 0, bytes_of(&info));
            *self.camera_info.lock().unwrap() = Some((info, Instant::now()));
        }
    }

    /// 渲染线程调用：把最新一帧原始 RGB 推给 TCP 线程（只保留最新，允许跳帧）。
    /// TCP 未连接时直接跳过：省掉每帧 1440x1080x3 ≈ 4.6MB 的无谓拷贝。
    pub fn publish_image_raw(&self, data: &[u8], seq: u64, timestamp_ns: u64) {
        if !self.tcp_connected.load(Ordering::Relaxed) {
            return;
        }
        self.image_slot
            .publish(data, IMAGE_WIDTH, IMAGE_HEIGHT, seq, timestamp_ns);
    }

    /// 取最新云台指令（与共享内存订阅端语义一致：返回最近一次收到的指令）。
    pub fn recv_gimbal_cmd(&self) -> Option<GimbalCmd> {
        self.gimbal_slot.lock().ok().and_then(|slot| *slot)
    }

    /// 云台指令槽位句柄（供独立的订阅封装复用，避免持有整个 publisher）。
    pub fn gimbal_slot(&self) -> Arc<Mutex<Option<GimbalCmd>>> {
        self.gimbal_slot.clone()
    }
}

/// UDP 接收线程主循环：阻塞收包，校验后把云台指令写进共享槽位。
fn udp_recv_loop(socket: UdpSocket, gimbal_slot: Arc<Mutex<Option<GimbalCmd>>>) {
    let mut buf = [0u8; 128]; // 足够容纳云台指令包（32 包头 + 32 载荷 = 64）
    loop {
        // `Ok((n, _src)) if n >= 64`：match 守卫——只有收到足够长的包才处理，
        // 满足要求的包按包头(32) + 载荷(32) 解析。
        match socket.recv_from(&mut buf) {
            Ok((n, _src)) if n >= 64 => {
                if !valid_packet_header(
                    &buf[..size_of::<PacketHeader>()],
                    PKT_TYPE_GIMBAL_CMD,
                    size_of::<GimbalCmd>() as u16,
                ) {
                    continue;
                }
                // `buf[32..64]` 即载荷，`try_into` 把它转成定长 [u8; 32]。
                let payload: [u8; 32] = buf[32..64].try_into().unwrap();
                let mut cmd = GimbalCmd::default();
                // 载荷字节直接 memcpy 到结构体（同为小端，且来自对齐的栈缓冲）
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        payload.as_ptr(),
                        &mut cmd as *mut GimbalCmd as *mut u8,
                        size_of::<GimbalCmd>(),
                    );
                }
                if let Ok(mut slot) = gimbal_slot.lock() {
                    *slot = Some(cmd); // 覆盖式写入：永远保留最近一条指令
                }
            }
            _ => continue, // 包太短/接收出错：忽略，继续收下一个
        }
    }
}

/// 校验包头：magic / version / type / payload_len。
/// 逐字段按小端从原始字节手动解析（不依赖结构体内存布局），确保脏包/异构包被丢弃。
fn valid_packet_header(header: &[u8], expect_type: u8, expect_payload_len: u16) -> bool {
    if header.len() < size_of::<PacketHeader>() {
        return false;
    }
    let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let version = u16::from_le_bytes(header[4..6].try_into().unwrap());
    let payload_len = u16::from_le_bytes(header[6..8].try_into().unwrap());
    let pkt_type = header[8];
    magic == NET_MAGIC
        && version == NET_VERSION
        && pkt_type == expect_type
        && payload_len == expect_payload_len
}

/// TCP 图像线程主循环：连上就发流，断开则按 `TCP_RECONNECT_DELAY` 重连，永不退出。
fn tcp_image_loop(remote_ip: IpAddr, image_slot: Arc<ImageSlot>, connected: Arc<AtomicBool>) {
    let addr = SocketAddr::new(remote_ip, NET_TCP_PORT);
    loop {
        // `connect_timeout`：超过 2 秒连不上就放弃本次，交给外层 sleep 后重试。
        match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
            Ok(mut stream) => {
                let _ = stream.set_nodelay(true); // 禁用 Nagle：低延迟小包更重要
                let _ = stream.set_write_timeout(Some(Duration::from_secs(2))); // 写阻塞上限
                connected.store(true, Ordering::Relaxed); // 通知发布端"可发图了"
                eprintln!("talos-net: image stream connected to {}", addr);
                if let Err(e) = image_stream_loop(&mut stream, &image_slot) {
                    eprintln!("talos-net: image stream error: {}, reconnecting", e);
                }
                connected.store(false, Ordering::Relaxed); // 断开：发布端跳过图像拷发
            }
            Err(e) => {
                eprintln!("talos-net: image connect failed: {}, retrying", e);
            }
        }
        thread::sleep(TCP_RECONNECT_DELAY);
    }
}

/// 单条 TCP 连接内的发送循环：不断取最新帧、编码、写出。
fn image_stream_loop(stream: &mut TcpStream, image_slot: &ImageSlot) -> std::io::Result<()> {
    loop {
        // 按值取走帧：缓冲所有权移交编码器，全链路零多余拷贝（无帧时在此阻塞等待）
        let raw = image_slot.take();
        let (seq, timestamp_ns) = (raw.seq, raw.timestamp_ns);
        let jpeg = encode_jpeg(raw)?;

        let header = ImageFrameHeader {
            magic: NET_MAGIC,
            pkt_type: PKT_TYPE_IMAGE,
            _pad: [0; 3],
            width: IMAGE_WIDTH,
            height: IMAGE_HEIGHT,
            jpeg_len: jpeg.len() as u32,
            _pad2: 0,
            seq,
            timestamp_ns,
        };

        // 帧头与 JPEG 数据分两次写、最后一次 flush 一起推送（保证对端读到完整一帧）。
        stream.write_all(bytes_of(&header))?;
        stream.write_all(&jpeg)?;
        stream.flush()?;
    }
}

/// JPEG 编码；分辨率必须严格 1440×1080，渲染分辨率不同时先 resize。
/// 按值接收帧：缓冲所有权移交（不 clone），编码后即释放。
fn encode_jpeg(mut raw: RawImageFrame) -> std::io::Result<Vec<u8>> {
    use image::codecs::jpeg::JpegEncoder;
    use image::imageops::FilterType;
    use image::{ExtendedColorType, ImageBuffer, ImageEncoder, RgbImage};

    // 通道序开关：捕获缓冲按 BGR 解释（与对端 imdecode→共享内存→talos 链路约定
    // 一致），先交换 R/B 再按 RGB 编码。若对端颜色反而更歪（说明缓冲本就是
    // RGB 序），删掉下面的 swap 循环即可。
    for pixel in raw.data.chunks_exact_mut(3) {
        pixel.swap(0, 2);
    }

    let rgb = RgbImage::from_raw(raw.width, raw.height, std::mem::take(&mut raw.data))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad image size"))?;

    let frame: ImageBuffer<image::Rgb<u8>, Vec<u8>> =
        if raw.width != IMAGE_WIDTH || raw.height != IMAGE_HEIGHT {
            image::imageops::resize(&rgb, IMAGE_WIDTH, IMAGE_HEIGHT, FilterType::Triangle)
        } else {
            rgb
        };

    let mut out = Vec::new();
    let encoder = JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
    encoder
        .write_image(
            frame.as_raw(),
            frame.width(),
            frame.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    Ok(out)
}

/// 辅助 f32 数组 → 16 字节小端序列（塞进 `PoseMeta._pad` 复用为数据通道）。
/// 每个 f32 占 4 字节，`to_le_bytes` 保证与协议端字节序一致。
fn aux_f32_to_bytes(aux_f32: [f32; 4]) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    for (i, value) in aux_f32.iter().enumerate() {
        bytes[i * 4..(i + 1) * 4].copy_from_slice(&value.to_le_bytes());
    }
    bytes
}
