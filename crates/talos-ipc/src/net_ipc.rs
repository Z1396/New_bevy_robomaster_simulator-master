//! 网络转发 IPC (对接 Linux 侧 talos_ipc_bridge)
//!
//! - 一个 UDP socket：向 `NUC:9571` 发送所有小数据包（位姿/底盘观测/真值/相机参数/
//!   运行状态/心跳），并在同一 socket 上接收云台指令回传（桥按收包源地址回发）。
//! - 一条 TCP 连接（独立线程）：向 `NUC:9572` 发送 JPEG 图像流，断线每 500ms 重连。
//! - 字节序：双方均为小端，结构体字节直接 memcpy，不做网络字节序转换。
//!
//! UDP 包格式：32 字节包头 + 结构体载荷；TCP 图像帧：40 字节帧头 + JPEG 字节。

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

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// 将 `#[repr(C)]` POD 结构体按字节读取（双方均为小端，直接 memcpy 语义）。
fn bytes_of<T: Copy>(value: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(value as *const T as *const u8, size_of::<T>()) }
}

// ---------------------------------------------------------------------------
// 最新帧槽位（渲染线程写入，TCP 线程消费，只保留最新一帧，允许跳帧）
// ---------------------------------------------------------------------------

struct RawImageFrame {
    data: Vec<u8>,
    width: u32,
    height: u32,
    seq: u64,
    timestamp_ns: u64,
}

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
        self.signal.notify_one();
    }

    /// 取走最新一帧；槽位为空时阻塞等待。
    fn take(&self) -> RawImageFrame {
        let mut guard = self.frame.lock().unwrap();
        loop {
            if let Some(frame) = guard.take() {
                return frame;
            }
            guard = self.signal.wait(guard).unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// NetIpcPublisher
// ---------------------------------------------------------------------------

/// 网络发布端：UDP 元数据 + TCP JPEG 图像流。
pub struct NetIpcPublisher {
    udp: UdpSocket,
    remote: SocketAddr,
    seq: AtomicU64,
    last_packet: Mutex<Instant>,
    camera_info: Mutex<Option<(CameraInfo, Instant)>>,
    runtime_state: Mutex<Option<RuntimeState>>,
    image_slot: Arc<ImageSlot>,
    tcp_connected: Arc<AtomicBool>,
    gimbal_slot: Arc<Mutex<Option<GimbalCmd>>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl NetIpcPublisher {
    /// 绑定系统分配的 UDP 端口，启动 UDP 接收线程与 TCP 图像线程。
    pub fn connect(remote_ip: IpAddr) -> std::io::Result<Self> {
        let udp = UdpSocket::bind(("0.0.0.0", 0))?;
        let remote = SocketAddr::new(remote_ip, NET_UDP_PORT);

        let publisher = Self {
            udp,
            remote,
            seq: AtomicU64::new(0),
            last_packet: Mutex::new(Instant::now() - HEARTBEAT_INTERVAL),
            camera_info: Mutex::new(None),
            runtime_state: Mutex::new(None),
            image_slot: Arc::new(ImageSlot::new()),
            tcp_connected: Arc::new(AtomicBool::new(false)),
            gimbal_slot: Arc::new(Mutex::new(None)),
            threads: Mutex::new(Vec::new()),
        };

        publisher.spawn_udp_recv_thread()?;
        publisher.spawn_tcp_thread();

        Ok(publisher)
    }

    fn spawn_udp_recv_thread(&self) -> std::io::Result<()> {
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
        debug_assert!(payload.len() <= u16::MAX as usize);
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
        let mut buf = Vec::with_capacity(size_of::<PacketHeader>() + payload.len());
        buf.extend_from_slice(bytes_of(&header));
        buf.extend_from_slice(payload);

        if self.udp.send_to(&buf, self.remote).is_ok() {
            *self.last_packet.lock().unwrap() = Instant::now();
        }
    }

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
        self.send_packet(PKT_TYPE_POSE, index as u8, bytes_of(&pose));
    }

    pub fn publish_chassis_observation(&self, observation: ChassisObservation) {
        self.send_packet(PKT_TYPE_CHASSIS_OBSERVATION, 0, bytes_of(&observation));
    }

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
            self.send_packet(PKT_TYPE_HEARTBEAT, 0, &[]);
        }

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

fn udp_recv_loop(socket: UdpSocket, gimbal_slot: Arc<Mutex<Option<GimbalCmd>>>) {
    let mut buf = [0u8; 128];
    loop {
        match socket.recv_from(&mut buf) {
            Ok((n, _src)) if n >= 64 => {
                if !valid_packet_header(
                    &buf[..size_of::<PacketHeader>()],
                    PKT_TYPE_GIMBAL_CMD,
                    size_of::<GimbalCmd>() as u16,
                ) {
                    continue;
                }
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
                    *slot = Some(cmd);
                }
            }
            _ => continue,
        }
    }
}

/// 校验包头：magic / version / type / payload_len。
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

fn tcp_image_loop(remote_ip: IpAddr, image_slot: Arc<ImageSlot>, connected: Arc<AtomicBool>) {
    let addr = SocketAddr::new(remote_ip, NET_TCP_PORT);
    loop {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
            Ok(mut stream) => {
                let _ = stream.set_nodelay(true);
                let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                connected.store(true, Ordering::Relaxed);
                eprintln!("talos-net: image stream connected to {}", addr);
                if let Err(e) = image_stream_loop(&mut stream, &image_slot) {
                    eprintln!("talos-net: image stream error: {}, reconnecting", e);
                }
                connected.store(false, Ordering::Relaxed);
            }
            Err(e) => {
                eprintln!("talos-net: image connect failed: {}, retrying", e);
            }
        }
        thread::sleep(TCP_RECONNECT_DELAY);
    }
}

fn image_stream_loop(stream: &mut TcpStream, image_slot: &ImageSlot) -> std::io::Result<()> {
    loop {
        // 按值取走帧：缓冲所有权移交编码器，全链路零多余拷贝
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

fn aux_f32_to_bytes(aux_f32: [f32; 4]) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    for (i, value) in aux_f32.iter().enumerate() {
        bytes[i * 4..(i + 1) * 4].copy_from_slice(&value.to_le_bytes());
    }
    bytes
}
