//! GPU 异步快照管线（capture 模块的核心，文件最大）。
//!
//! 一句话：每帧把离屏相机渲染出的纹理"读回" CPU，再分发给 talos/ros2 的订阅者。
//!
//! 新手最常卡的三点，先讲清楚（本文件是唯一详解处，别处只引用）：
//! 1. **为什么异步？** `copy_texture_to_buffer` 只是"发起"一条 GPU 命令；GPU→CPU 的
//!    读回（map）要等 GPU 把这一帧真正画完才完成，是跨帧延迟的。若在渲染线程里同步
//!    等待，主循环帧率会被拖垮。所以这里发起后不等待，把"读映射内存→转码→回调订阅者"
//!    整段丢给异步任务池（AsyncComputeTaskPool），当前帧立刻返回。
//! 2. **为什么要帧缓冲池？** 每帧都新建 GPU buffer / CPU Vec 会反复向驱动申请与释放
//!    内存，成本高。于是用"用完即归还、需要时复用"的池子：`free_buffers`（GPU 读回缓冲）
//!    与 `free_frames`（CPU 帧字节 Vec）。
//! 3. **颜色序为何要翻转？** Windows/DX12 的纹理常是 BGRA 序，Vulkan/Linux 常是 RGBA。
//!    视觉算法统一要 RGB，于是 FrameLayout 在打包时按 ColorOrder 决定是否把首尾字节对调。
//!
//! 渲染阶段（Bevy 渲染世界的调度）：Extract（主世界相机等抽到渲染世界）→ Prepare/Queue →
//! RenderGraph 里的 `image_copy_driver`（发起 copy_texture_to_buffer）→ Render 阶段的
//! `receive_image_from_buffer`（发起异步 map，后台完成读回与分发）。

use bevy::asset::RenderAssetUsages;
use bevy::core_pipeline::schedule::camera_driver; // 相机驱动节点，拷贝系统排在其后
use bevy::ecs::world::DeferredWorld; // "延迟世界"：可安全读取资源、稍后应用变更
use bevy::render::texture::GpuImage; // 已在 GPU 上的图像资源
use bevy::tasks::AsyncComputeTaskPool; // 异步计算任务池（跑后台读回/转码）
use bevy::{
    image::TextureFormatPixelInfo,
    prelude::*,
    render::{
        Render, RenderApp, RenderSystems,
        render_asset::RenderAssets,
        render_resource::{
            Buffer, BufferDescriptor, BufferUsages, Extent3d, MapMode, Origin3d,
            TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, TextureAspect,
            TextureFormat, TextureUsages,
        },
        renderer::{RenderContext, RenderDevice, RenderGraph, RenderGraphSystems},
    },
};
// VecDeque：双端队列，用作"已发起拷贝、待读回"的 FIFO 队列（先进先出）。
use std::collections::VecDeque;
// Arc=多线程共享所有权（引用计数，克隆只是计数+1）；Mutex=互斥锁，保护共享数据的并发访问。
use std::sync::{Arc, Mutex};

/// 帧序号：给每帧捕获打上单调递增的编号，用于跨帧校验（防止把 A 帧的图像配到
/// B 帧的位姿上）。`CaptureFrameId(u64)` 是 Rust 的"新类型"惯用法——用独立类型
/// 包住 u64，避免与其它整数混用。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CaptureFrameId(u64);

impl CaptureFrameId {
    // `const fn`：可在编译期求值，因此能用在 const 上下文里。
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    // 取内部的 u64；`self` 按值传入（类型实现了 Copy，复制成本极低）。
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// 捕获通道位标记（bit flags）：用一个字节的位表示"本帧需要/已提交哪些通道"。
/// `1<<0`=RGB 彩色、`1<<1`=DEPTH 深度、`1<<2`=R32_UINT 无符号整数通道。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureChannels(u8);

impl CaptureChannels {
    pub const RGB: Self = Self(1 << 0);
    pub const DEPTH: Self = Self(1 << 1);
    pub const R32_UINT: Self = Self(1 << 2);
    // 三个通道按位或，合成"全都要"。
    pub const ALL: Self = Self(Self::RGB.0 | Self::DEPTH.0 | Self::R32_UINT.0);

    // 由帧类型反查它属于哪个通道。
    const fn for_kind(kind: CapturedFrameKind) -> Self {
        match kind {
            CapturedFrameKind::Rgb8 => Self::RGB,
            CapturedFrameKind::Depth32F => Self::DEPTH,
            CapturedFrameKind::R32Uint => Self::R32_UINT,
        }
    }

    // 判断 self 是否已包含 other 的全部位（用于"必需通道是否都已提交"）。
    const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    // 置位：把 other 的位并进来（标记"该通道已提交"）。
    fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

// 一帧正在进行的提交记录：需要哪些通道(required)、已提交哪些(submitted)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CaptureSubmission {
    id: CaptureFrameId,
    required: CaptureChannels,
    submitted: CaptureChannels,
}

/// 提交状态机的错误类型。Rust 的 `enum` 每个分支都可携带数据（代数数据类型），
/// 调用方用 `match` 或 `{:?}` 就能区分具体失败原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureSubmissionError {
    AlreadyActive {
        active: CaptureFrameId,
        requested: CaptureFrameId,
    },
    NoActiveSubmission,
    WrongFrame {
        active: CaptureFrameId,
        received: CaptureFrameId,
    },
    UnexpectedChannel {
        id: CaptureFrameId,
        kind: CapturedFrameKind,
    },
    DuplicateChannel {
        id: CaptureFrameId,
        kind: CapturedFrameKind,
    },
    MissingChannels {
        id: CaptureFrameId,
        required: CaptureChannels,
        submitted: CaptureChannels,
    },
}

/// 帧提交状态机（全局资源，装在渲染世界）。用 `Mutex<Option<...>>` 共享，因为渲染线程
/// 与异步任务可能并发访问；同一时刻只允许有一帧处于"进行中"。
#[derive(Resource, Default)]
pub struct CaptureFrameSubmission(Mutex<Option<CaptureSubmission>>);

impl CaptureFrameSubmission {
    /// 开启一帧。若已有进行中的帧，返回 `AlreadyActive`。
    pub fn begin(
        &self,
        id: CaptureFrameId,
        required: CaptureChannels,
    ) -> Result<(), CaptureSubmissionError> {
        let mut guard = self.0.lock().unwrap(); // `.unwrap()`：锁中毒（持锁线程 panic）时直接崩
        if let Some(active) = *guard {
            return Err(CaptureSubmissionError::AlreadyActive {
                active: active.id,
                requested: id,
            });
        }
        *guard = Some(CaptureSubmission {
            id,
            required,
            submitted: CaptureChannels::default(), // 初始一个通道都没提交
        });
        Ok(())
    }

    /// 读取当前进行中的帧号（无则返回 None）。
    pub fn active_id(&self) -> Option<CaptureFrameId> {
        self.0.lock().unwrap().as_ref().map(|active| active.id)
    }

    /// 认领某通道：依次校验"帧号对、该通道被需要、未被重复提交"。
    fn claim(
        &self,
        id: CaptureFrameId,
        kind: CapturedFrameKind,
    ) -> Result<(), CaptureSubmissionError> {
        let mut guard = self.0.lock().unwrap();
        // `.ok_or(...)?`：把 None 转成 Err 并立即返回（Rust 的 `?` 错误传播写法）。
        let active = guard
            .as_mut()
            .ok_or(CaptureSubmissionError::NoActiveSubmission)?;
        if active.id != id {
            return Err(CaptureSubmissionError::WrongFrame {
                active: active.id,
                received: id,
            });
        }
        let channel = CaptureChannels::for_kind(kind);
        if !active.required.contains(channel) {
            return Err(CaptureSubmissionError::UnexpectedChannel { id, kind });
        }
        if active.submitted.contains(channel) {
            return Err(CaptureSubmissionError::DuplicateChannel { id, kind });
        }
        active.submitted.insert(channel); // 置位：记录"该通道已提交"
        Ok(())
    }

    /// 结束一帧：校验必需通道是否已全部提交，然后清空状态。
    fn finish(&self, id: CaptureFrameId) -> Result<(), CaptureSubmissionError> {
        // `.take()`：取出 Option 内容并把槽位置为 None——一帧结束后状态即被清空。
        let active = self
            .0
            .lock()
            .unwrap()
            .take()
            .ok_or(CaptureSubmissionError::NoActiveSubmission)?;
        if active.id != id {
            return Err(CaptureSubmissionError::WrongFrame {
                active: active.id,
                received: id,
            });
        }
        if !active.submitted.contains(active.required) {
            return Err(CaptureSubmissionError::MissingChannels {
                id,
                required: active.required,
                submitted: active.submitted,
            });
        }
        Ok(())
    }
}

/// 捕获帧的数据类型：Rgb8=每像素 3 字节彩色、Depth32F=每像素 4 字节 f32 深度、
/// R32Uint=每像素 4 字节无符号整数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturedFrameKind {
    Rgb8,
    Depth32F,
    R32Uint,
}

/// 一次捕获的配置：尺寸（单位：像素）、源纹理格式、目标产物帧类型。
#[derive(Resource, Clone)]
pub struct CaptureConfig {
    pub width: u32,
    pub height: u32,
    pub texture_format: TextureFormat,
    pub frame_kind: CapturedFrameKind,
}

/// 交给订阅者回调的只读帧视图。`<'a>` 是生命周期标注：`data` 只是借用外部缓冲，
/// 不拥有、不复制（`CapturedFrame` 存在期间该缓冲不能被改写）。
pub struct CapturedFrame<'a> {
    pub frame_id: Option<CaptureFrameId>,
    pub kind: CapturedFrameKind,
    pub width: u32,
    pub height: u32,
    pub data: &'a [u8],
}

/// 在资产系统（Assets<Image>）里新建一张可作为渲染目标的图像，返回其句柄。
/// 深度格式用 `new_uninit`（不需要初始像素数据）；彩色格式用 `new_target_texture`
/// （直接作为渲染目标）。`asset_usages`/`texture_usages` 决定这张纹理的用途，
/// 例如 `COPY_SRC` 表示它可被拷贝读出（读回的前提）。
pub fn create_capture_image_handle(
    app: &mut App,
    width: u32,
    height: u32,
    texture_format: TextureFormat,
    asset_usages: RenderAssetUsages,
    texture_usages: TextureUsages,
) -> Handle<Image> {
    let extent = Extent3d {
        width,
        height,
        ..Default::default() // 其余字段（depth_or_array_layers 等）取默认
    };

    // `matches!` 宏：把值与一组模式比对并返回 bool——这里判断是否为深度格式。
    let mut image = if matches!(
        texture_format,
        TextureFormat::Depth16Unorm
            | TextureFormat::Depth24Plus
            | TextureFormat::Depth24PlusStencil8
            | TextureFormat::Depth32Float
            | TextureFormat::Depth32FloatStencil8
    ) {
        Image::new_uninit(
            extent,
            bevy::render::render_resource::TextureDimension::D2,
            texture_format,
            asset_usages,
        )
    } else {
        Image::new_target_texture(width, height, texture_format, Some(texture_format))
    };

    image.texture_descriptor.usage |= texture_usages; // 按位或叠加用途标志
    let mut images = app.world_mut().resource_mut::<Assets<Image>>();
    images.add(image) // 注册进资产表并返回句柄
}

// 一个 CaptureBundle 由若干子插件节点组成；用 enum 统一两种类型，才能放进同一个 Vec。
enum CapturePluginNode {
    Camera(CameraCapturePlugin),
    ViewCopy(crate::capture::view_copy::ViewTextureCopyPlugin),
}

/// 捕获插件的"套餐"：把彩色/深度相机捕获插件与 view_copy 插件打包成一个
/// 可直接 `add_plugins` 的单元。talos/ros2 通过 `CaptureBundle::color(...)` /
/// `color_and_depth(...)` 一步完成装配。
pub struct CaptureBundle {
    plugins: Vec<CapturePluginNode>,
    color_target: Option<Handle<Image>>,
    depth_target: Option<Handle<Image>>,
}

impl CaptureBundle {
    // 仅彩色：创建彩色捕获相机插件。
    pub fn color(
        app: &mut App,
        config: CaptureConfig,
        snapshots: Vec<Box<dyn GpuCaptureHandler>>,
    ) -> Self {
        let (plugin, color_target) = CameraCapturePlugin::new(app, config, snapshots);
        Self {
            plugins: vec![CapturePluginNode::Camera(plugin)],
            color_target: Some(color_target),
            depth_target: None,
        }
    }

    // 彩色+深度：在彩色基础上再挂一个深度捕获分支。
    pub fn color_and_depth(
        app: &mut App,
        color_config: CaptureConfig,
        color_snapshots: Vec<Box<dyn GpuCaptureHandler>>,
        depth_snapshots: Vec<Box<dyn GpuCaptureHandler>>,
    ) -> Self {
        Self::color(app, color_config.clone(), color_snapshots).with_depth_from_camera_order(
            app,
            CaptureConfig {
                width: color_config.width,
                height: color_config.height,
                texture_format: TextureFormat::Depth32Float, // 深度用 32 位浮点格式
                frame_kind: CapturedFrameKind::Depth32F,
            },
            crate::capture::CAPTURE_CAMERA_ORDER,
            depth_snapshots,
        )
    }

    // 仅深度（按指定相机 order 关联）。
    pub fn depth_from_camera_order(
        app: &mut App,
        config: CaptureConfig,
        camera_order: isize,
        snapshots: Vec<Box<dyn GpuCaptureHandler>>,
    ) -> Self {
        let mut bundle = Self {
            plugins: Vec::new(),
            color_target: None,
            depth_target: None,
        };
        bundle.push_depth_from_camera_order(app, config, camera_order, snapshots);
        bundle
    }

    // 在已有 bundle 上追加深度分支（链式构建：`mut self` 消费并返回自身）。
    pub fn with_depth_from_camera_order(
        mut self,
        app: &mut App,
        config: CaptureConfig,
        camera_order: isize,
        snapshots: Vec<Box<dyn GpuCaptureHandler>>,
    ) -> Self {
        self.push_depth_from_camera_order(app, config, camera_order, snapshots);
        self
    }

    // 取彩色目标纹理句柄（talos 用它做预览与图像发布）。
    pub fn color_target(&self) -> Option<&Handle<Image>> {
        self.color_target.as_ref()
    }

    // 取深度目标纹理句柄。
    pub fn depth_target(&self) -> Option<&Handle<Image>> {
        self.depth_target.as_ref()
    }

    fn push_depth_from_camera_order(
        &mut self,
        app: &mut App,
        config: CaptureConfig,
        camera_order: isize,
        snapshots: Vec<Box<dyn GpuCaptureHandler>>,
    ) {
        // 深度纹理由 view_copy 插件创建（它负责把渲染出的深度 blit 到这张纹理），
        // 再把该纹理交给相机捕获插件用于读回，两者共用同一句柄。
        let (view_copy, depth_target) =
            crate::capture::view_copy::ViewTextureCopyPlugin::new_depth_for_camera_order(
                app,
                config.width,
                config.height,
                camera_order,
            );
        let depth_capture =
            CameraCapturePlugin::from_existing_handle(config, depth_target.clone(), snapshots);

        self.plugins.push(CapturePluginNode::ViewCopy(view_copy));
        self.plugins.push(CapturePluginNode::Camera(depth_capture));
        self.depth_target = Some(depth_target);
    }
}

impl Plugin for CaptureBundle {
    // `is_unique=false`：允许同一个 App 里安装多个 CaptureBundle（如彩色、深度各一个）。
    fn is_unique(&self) -> bool {
        false
    }

    // 依次 build 内部子插件，完成它们的资源注册与系统安装。
    fn build(&self, app: &mut App) {
        for plugin in &self.plugins {
            match plugin {
                CapturePluginNode::Camera(plugin) => plugin.build(app),
                CapturePluginNode::ViewCopy(plugin) => plugin.build(app),
            }
        }
    }
}

// 类型别名：给又长又难读的类型起短名，提升可读性。
type ToSyncSnapshot = Box<dyn GpuCaptureHandler>;
type DynSnapshotSync = Box<dyn SnapshotSync>;

// 所有 copier 的集合（渲染世界资源）。`Deref/DerefMut` 让它能像 Vec 一样直接调用 len()/get()/push()。
#[derive(Resource, Default, Deref, DerefMut)]
struct ImageCopiers(Vec<ImageCopier>);

// 记录"读回驱动系统是否已安装"，避免多个插件重复 add_systems。
#[derive(Resource, Default)]
struct ImageCopyDriverInstalled(bool);

// 单个"纹理→内存"拷贝器：负责一类捕获目标（彩色或深度）。
struct ImageCopier {
    config: CaptureConfig,
    src_image: Handle<Image>, // 要读回的源纹理
    // 已发起拷贝、等待读回的队列：(GPU缓冲, 快照回调, 宽, 高, 纹理格式)。
    queue: Mutex<VecDeque<(Buffer, Vec<DynSnapshotSync>, u32, u32, TextureFormat)>>,
    // GPU 侧空闲缓冲池（复用，见文件头第 2 点）。
    free_buffers: Arc<Mutex<Vec<Buffer>>>,
    /// Pool of frame-sized output buffers, so the per-frame conversion allocates nothing.
    /// 中文：帧大小的输出缓冲池，使每帧的转码零分配（复用同一块内存，避免反复申请）。
    free_frames: Arc<Mutex<Vec<Vec<u8>>>>,
    snapshots: Arc<Vec<ToSyncSnapshot>>, // 订阅者列表
}

impl ImageCopier {
    pub fn new(
        config: CaptureConfig,
        src_image: Handle<Image>,
        snapshots: Arc<Vec<ToSyncSnapshot>>,
    ) -> ImageCopier {
        ImageCopier {
            config,
            src_image,
            queue: Mutex::new(VecDeque::new()),
            free_buffers: Arc::new(Mutex::new(Vec::new())),
            free_frames: Arc::new(Mutex::new(Vec::new())),
            snapshots,
        }
    }

    // 从池里取一个 GPU 缓冲；池空则新建。`size` 单位：字节。
    fn acquire_buffer(&self, render_device: &RenderDevice, size: u64) -> Buffer {
        if let Some(buf) = self.free_buffers.lock().unwrap().pop() {
            return buf;
        }
        render_device.create_buffer(&BufferDescriptor {
            label: None,
            size,
            // MAP_READ 允许 CPU 映射读取；COPY_DST 允许作为拷贝目标被写入。
            usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }
}

/// Byte order of a 4-byte color texel as it sits in the readback buffer.
/// 中文：4 字节颜色像素在读回缓冲里的字节顺序（Rgba 还是 Bgra）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColorOrder {
    Rgba, // 红绿蓝α（Vulkan/Linux 常见）
    Bgra, // 蓝绿红α（Windows/DX12 常见）——打包成 RGB 时需把首尾字节对调
}

/// How mapped readback bytes become the bytes handlers receive.
/// 中文：映射后的读回字节，如何转换成订阅者最终收到的字节。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrameEncoding {
    /// 4-byte color texels packed down to tight RGB triples.
    /// 中文：把 4 字节颜色像素压缩成紧凑的 RGB 三元组（丢 alpha、按需翻转字节序）。
    Rgb8 { source: ColorOrder },
    /// Rows passed through verbatim, minus the row padding wgpu requires.
    /// 中文：整行原样透传，仅去掉 wgpu 要求的行对齐填充（深度/整数帧用）。
    Raw,
}

/// Everything needed to turn one mapped readback into a frame, resolved once per frame instead of
/// per pixel. Constructing one is also the check that a texture format can produce the requested
/// frame kind at all, which [`CameraCapturePlugin`] runs at startup.
/// 中文：把一次读回变成一帧所需的全部信息，每帧只解析一次（而非逐像素判断）。构造成功本身
/// 也是一种校验——该纹理格式能否产出目标帧类型；`CameraCapturePlugin` 在启动时就跑此校验。
#[derive(Clone, Copy, Debug)]
struct FrameLayout {
    encoding: FrameEncoding,
    /// Meaningful bytes per source row, ignoring copy alignment padding.
    /// 中文：源数据每行有效字节数（不含为对齐而补的填充）。
    row_bytes: usize,
    padded_row_bytes: usize, // 含对齐填充的每行字节数
    output_row_bytes: usize, // 输出每行字节数（Rgb8 为 width*3）
    height: u32,
}

impl FrameLayout {
    fn new(
        kind: CapturedFrameKind,
        format: TextureFormat,
        width: u32,
        height: u32,
    ) -> Option<Self> {
        let pixel_size = format.pixel_size().ok()?; // `.ok()?`：是 Err 就整体返回 None（提前退出）
        let row_bytes = width as usize * pixel_size;
        // wgpu 要求每行字节数按 256 对齐（COPY_BYTES_PER_ROW_ALIGNMENT），这里算出对齐后的值。
        let padded_row_bytes = RenderDevice::align_copy_bytes_per_row(row_bytes);

        let (encoding, output_row_bytes) = match kind {
            CapturedFrameKind::Rgb8 => {
                // 由源纹理格式判断字节序；不认识的格式返回 None（不支持该转换）。
                let source = match format {
                    TextureFormat::Rgba8UnormSrgb | TextureFormat::Rgba8Unorm => ColorOrder::Rgba,
                    TextureFormat::Bgra8UnormSrgb | TextureFormat::Bgra8Unorm => ColorOrder::Bgra,
                    _ => return None,
                };
                (FrameEncoding::Rgb8 { source }, width as usize * 3) // 输出 3 字节/像素
            }
            // 深度与 R32Uint 都是每像素 4 字节、原样透传。
            CapturedFrameKind::Depth32F | CapturedFrameKind::R32Uint => {
                (FrameEncoding::Raw, row_bytes)
            }
        };

        Some(Self {
            encoding,
            row_bytes,
            padded_row_bytes,
            output_row_bytes,
            height,
        })
    }

    fn output_len(&self) -> usize {
        self.output_row_bytes * self.height as usize // 输出总字节数
    }

    /// Writes straight from the mapped range into `out`, which must be [`Self::output_len`] long.
    /// The encoding is matched per row, never per pixel.
    /// 中文：从映射区间直接写入 `out`（长度须为 `Self::output_len`）。编码按"行"匹配、
    /// 不逐像素判断，避免每像素的分支开销。
    fn write(&self, mapped: &[u8], out: &mut [u8]) {
        // `chunks` 按 padded_row_bytes 切片，`take(height)` 只取有效的前 height 行（跳过尾部填充）。
        let rows = mapped
            .chunks(self.padded_row_bytes)
            .take(self.height as usize);

        // `zip` 把源行与输出行配对；`chunks_exact_mut` 按输出行宽切分。
        for (row, out_row) in rows.zip(out.chunks_exact_mut(self.output_row_bytes)) {
            let row = &row[..self.row_bytes.min(row.len())]; // 只取该行有效字节（去掉行尾填充）
            match self.encoding {
                FrameEncoding::Raw => {
                    // 整行拷贝（长度取两者较小值，防御性处理）。
                    let len = row.len().min(out_row.len());
                    out_row[..len].copy_from_slice(&row[..len]);
                }
                FrameEncoding::Rgb8 {
                    source: ColorOrder::Rgba,
                } => {
                    // 每 4 字节取前 3 字节（丢弃 alpha）。
                    for (texel, pixel) in row.chunks_exact(4).zip(out_row.chunks_exact_mut(3)) {
                        pixel.copy_from_slice(&texel[..3]);
                    }
                }
                FrameEncoding::Rgb8 {
                    source: ColorOrder::Bgra,
                } => {
                    // BGRA→RGB：按 [2],[1],[0] 取，等价于把首尾字节对调（这就是"颜色序翻转"）。
                    for (texel, pixel) in row.chunks_exact(4).zip(out_row.chunks_exact_mut(3)) {
                        pixel.copy_from_slice(&[texel[2], texel[1], texel[0]]);
                    }
                }
            }
        }
    }
}

/// Takes a pooled output buffer, sized without ever re-zeroing a buffer that already fits.
/// 中文：从池中取一个输出缓冲并保证其长度为 `len`；若已够长只截断、绝不重新清零
///（清零是浪费——后面会整块写满）。
fn acquire_frame_buffer(pool: &Mutex<Vec<Vec<u8>>>, len: usize) -> Vec<u8> {
    let mut buffer = pool.lock().unwrap().pop().unwrap_or_default(); // 池空则用空 Vec
    if buffer.len() < len {
        buffer.resize(len, 0); // 不够长才扩容（新增部分会被清零）
    } else {
        buffer.truncate(len); // 够长则截断到目标长度，复用原有内存
    }
    buffer
}

// 选择纹理的读取切面：深度格式只能按"仅深度"切面读，颜色/整数格式取全切面。
fn capture_texture_aspect(format: TextureFormat) -> TextureAspect {
    if matches!(
        format,
        TextureFormat::Depth16Unorm
            | TextureFormat::Depth24Plus
            | TextureFormat::Depth24PlusStencil8
            | TextureFormat::Depth32Float
            | TextureFormat::Depth32FloatStencil8
    ) {
        TextureAspect::DepthOnly
    } else {
        TextureAspect::All
    }
}

/// 同步阶段快照：在渲染世界的同步点被调用，产出一个可在后台处理的异步快照对象。
/// `Send` 约束保证它可跨线程移动（稍后要扔进异步任务池）。
pub trait SnapshotSync: Send {
    // 本快照对应的帧号；默认 None 表示不参与帧号校验。
    fn frame_id(&self) -> Option<CaptureFrameId> {
        None
    }

    // 读取渲染世界里的资源、构造出后续异步处理的载体。
    // `self: Box<Self>`：消费式方法，按装箱值取走自身；返回装箱的异步快照。
    fn captured(
        self: Box<Self>,
        world: &mut DeferredWorld,
        config: &CaptureConfig,
    ) -> Box<dyn SnapshotAsync>;
}

/// 异步阶段快照：在后台任务里拿到已读回的帧字节，做最终处理（如发布到 Talos/ROS）。
pub trait SnapshotAsync: Send {
    fn captured(&mut self, frame: CapturedFrame<'_>);
}

/// 订阅者（处理器）总接口：被 copier 长期持有；每个捕获点询问它"这帧要不要，要就给我快照"。
/// `Send + Sync + 'static`：既要在多线程间共享，又要能长期存活（不借用外部数据）。
pub trait GpuCaptureHandler: Send + Sync + 'static {
    fn captured(
        &self,
        world: &World,
        frame_id: Option<CaptureFrameId>,
    ) -> Option<Box<dyn SnapshotSync>>;
}

/// 渲染图节点（RenderGraph 阶段，紧跟相机驱动 `camera_driver` 之后执行）：
/// 对每个 copier，把源纹理拷贝到一块 GPU 缓冲，并把"待读回"任务压入队列。
/// 这里只"发起"拷贝命令，真正的 CPU 读回由下一个系统 `receive_image_from_buffer` 异步完成。
fn image_copy_driver(world: &World, mut render_context: RenderContext) {
    let Some(copiers) = world.get_resource::<ImageCopiers>() else {
        return; // 没有捕获器就什么都不做
    };
    let Some(gpu_images) = world.get_resource::<RenderAssets<GpuImage>>() else {
        return; // 源图还没上传到 GPU
    };

    let submission = world.get_resource::<CaptureFrameSubmission>();
    let active_frame_id = submission.and_then(CaptureFrameSubmission::active_id);

    for copier in copiers.iter() {
        let Some(src_image) = gpu_images.get(&copier.src_image) else {
            continue; // 该 copier 的源纹理不在 GPU 上，跳过
        };

        // 向每个订阅者询问：本帧你要不要？要的把手里的快照对象给我。
        // `filter_map`：同时过滤（None 丢弃）与映射（Some 解包），一步完成。
        let snapshots: Vec<DynSnapshotSync> = copier
            .snapshots
            .iter()
            .filter_map(|handler| handler.captured(world, active_frame_id))
            .collect();
        if let Some(id) = active_frame_id {
            for snapshot in &snapshots {
                // `if let ... && ...` 是 let-chains：先把 Option 解构成 id，再判不相等。
                if let Some(snapshot_id) = snapshot.frame_id()
                    && snapshot_id != id
                {
                    // 帧号错配说明管线契约被破坏（图像与位姿对不上帧），直接崩以暴露问题。
                    panic!(
                        "capture snapshot frame mismatch: active={id:?}, snapshot={snapshot_id:?}"
                    );
                }
            }
            if snapshots
                .iter()
                .any(|snapshot| snapshot.frame_id() == Some(id))
            {
                // 有订阅者认领本帧 → 在提交状态机里登记该通道（claim）。
                submission
                    .unwrap()
                    .claim(id, copier.config.frame_kind)
                    .unwrap_or_else(|error| panic!("capture channel submission failed: {error:?}"));
            }
        }
        if snapshots.is_empty() {
            continue; // 没人要这帧，不必拷贝
        }

        let size = src_image.texture_descriptor.size; // 纹理尺寸（单位：像素）
        let format = src_image.texture_descriptor.format;
        let block_dimensions = format.block_dimensions(); // 压缩格式的块尺寸（普通格式为 1×1）
        let block_size = format.block_copy_size(None).unwrap(); // 每块字节数
        // 每行字节数按 256 对齐（与 FrameLayout 的对齐规则一致）。
        let padded_bytes_per_row = RenderDevice::align_copy_bytes_per_row(
            (size.width as usize / block_dimensions.0 as usize) * block_size as usize,
        );
        let buffer_size = padded_bytes_per_row as u64 * size.height as u64; // 缓冲总字节数
        let buffer = copier.acquire_buffer(render_context.render_device(), buffer_size);

        // 记录一条 GPU 拷贝命令：纹理 → 缓冲（命令异步执行，此刻尚未真正发生）。
        render_context.command_encoder().copy_texture_to_buffer(
            TexelCopyTextureInfo {
                texture: &src_image.texture,
                mip_level: 0,
                origin: Origin3d::ZERO,
                aspect: capture_texture_aspect(format),
            },
            TexelCopyBufferInfo {
                buffer: &buffer,
                layout: TexelCopyBufferLayout {
                    offset: 0,
                    // `bytes_per_row` 要求非零 u32；用 NonZero 包装来表达这个不变量。
                    bytes_per_row: Some(
                        std::num::NonZero::<u32>::new(padded_bytes_per_row as u32)
                            .unwrap()
                            .into(),
                    ),
                    rows_per_image: None,
                },
            },
            size,
        );

        // 把 (缓冲, 快照, 尺寸, 格式) 入队，留给 receive_image_from_buffer 发起读回。
        let mut queue = copier.queue.lock().unwrap();
        queue.push_back((buffer, snapshots, size.width, size.height, format));
    }

    if let Some(id) = active_frame_id {
        // 所有 copier 处理完 → 结束本帧提交，校验必需通道是否齐全。
        submission
            .unwrap()
            .finish(id)
            .unwrap_or_else(|error| panic!("incomplete capture frame submission: {error:?}"));
    }
}

/// 主渲染阶段的系统（`Render` 调度，在 `RenderSystems::Render` 之后执行）：发起异步读回。
///
/// 关键点（为什么这么写）：`map_async` 只是"登记回调"，真正完成要等 GPU 把这一帧画完。
/// 因此这里绝不阻塞等待——发一个 oneshot 通道让回调完成时通知，然后把
/// "等待完成 → 读映射内存 → 转码 → 回调订阅者"整段逻辑丢进异步任务池后台执行（详见文件头）。
fn receive_image_from_buffer(mut world: DeferredWorld) {
    let copier_count = world.resource::<ImageCopiers>().len();
    if copier_count == 0 {
        return;
    }

    for idx in 0..copier_count {
        // 先把该 copier 队列里最早的一条取出（连同池/配置的 Arc 克隆）。
        // `Arc` 克隆只增加引用计数、不深拷贝数据，成本极低。
        let next = {
            let copiers = world.resource::<ImageCopiers>();
            let Some(copier) = copiers.get(idx) else {
                continue;
            };
            let mut guard = copier.queue.lock().unwrap();
            guard
                .pop_front()
                .map(|(buffer, snapshots, width, height, texture_format)| {
                    (
                        buffer,
                        snapshots,
                        width,
                        height,
                        texture_format,
                        copier.free_buffers.clone(),
                        copier.free_frames.clone(),
                        copier.config.clone(),
                    )
                })
        };

        let Some((
            buffer,
            snapshots,
            width,
            height,
            texture_format,
            free_buffers,
            free_frames,
            config,
        )) = next
        else {
            continue; // 队列为空，本 copier 本轮无事可做
        };

        // The callback only signals completion; the conversion happens on the async pool, reading
        // the mapped range in place. That keeps the heavy work off the polling thread without the
        // full-frame copy an intermediate `Vec` would cost.
        // 中文：回调只负责"通知完成"；真正的转码在异步池里原地读取映射区间完成，既不让重活
        // 占用轮询线程，又省掉了用中间 Vec 做整帧拷贝的开销。
        // oneshot=单次发送的异步通道：回调完成时 send，后台任务 await 接收结果。
        let (s, r) = futures::channel::oneshot::channel();
        buffer.slice(..).map_async(MapMode::Read, move |result| {
            let _ = s.send(result); // 忽略发送失败（接收端可能已丢弃）
        });

        // 在同步点把 SnapshotSync 转成 SnapshotAsync（此刻还能访问渲染世界资源）。
        let snapshots: Vec<(Option<CaptureFrameId>, Box<dyn SnapshotAsync>)> = snapshots
            .into_iter()
            .map(|snapshot| {
                let frame_id = snapshot.frame_id();
                (frame_id, snapshot.captured(&mut world, &config))
            })
            .collect();
        let frame_kind = config.frame_kind;

        // 丢进异步计算任务池；`.detach()` 表示不等它、让它自行跑完（fire-and-forget）。
        AsyncComputeTaskPool::get()
            .spawn(async move {
                r.await
                    .expect("capture buffer map channel dropped")
                    .expect("Failed to map buffer"); // 等待 GPU 完成并成功映射

                // 依据纹理格式构建布局，并从池里取一个帧缓冲（复用已有内存，零分配）。
                let layout = FrameLayout::new(frame_kind, texture_format, width, height)
                    .expect("Unsupported capture texture format");
                let mut frame_bytes = acquire_frame_buffer(&free_frames, layout.output_len());

                {
                    // `get_mapped_range` 拿到 CPU 可读的映射切片；用完必须 `unmap`。
                    let mapped = buffer.slice(..).get_mapped_range();
                    layout.write(&mapped, &mut frame_bytes); // 按颜色序把数据转码进帧缓冲
                }
                buffer.unmap(); // 解除映射，之后才能安全复用/回收
                free_buffers.lock().unwrap().push(buffer); // GPU 缓冲归还池

                // 逐个订阅者回调最终帧数据。
                for (frame_id, mut snapshot) in snapshots {
                    snapshot.captured(CapturedFrame {
                        frame_id,
                        kind: frame_kind,
                        width,
                        height,
                        data: frame_bytes.as_slice(),
                    });
                }

                free_frames.lock().unwrap().push(frame_bytes); // CPU 帧缓冲归还池
            })
            .detach();
    }
}

/// 单个捕获相机插件：把一张源纹理包装成"每帧读回并分发给订阅者"的能力。
/// talos 用 `expose_config_resource=true` 的新建路径；深度分支用 `from_existing_handle`
/// 复用 view_copy 插件创建的纹理。
pub struct CameraCapturePlugin {
    config: CaptureConfig,
    snapshots: Arc<Vec<ToSyncSnapshot>>,
    handle: Handle<Image>,
    expose_config_resource: bool, // 是否把 config 作为资源插入主世界/渲染世界
}

impl CameraCapturePlugin {
    // 新建源纹理，返回 (插件, 纹理句柄)。
    pub fn new(
        app: &mut App,
        config: CaptureConfig,
        snapshots: Vec<ToSyncSnapshot>,
    ) -> (Self, Handle<Image>) {
        let handle = create_capture_image_handle(
            app,
            config.width,
            config.height,
            config.texture_format,
            RenderAssetUsages::default(),
            TextureUsages::COPY_SRC, // 该纹理要被拷贝读出 → 必须有 COPY_SRC
        );

        (
            Self {
                config,
                snapshots: Arc::new(snapshots),
                handle: handle.clone(),
                expose_config_resource: true,
            },
            handle,
        )
    }

    // 复用外部已有的纹理句柄（深度分支：纹理由 view_copy 创建）。
    pub fn from_existing_handle(
        config: CaptureConfig,
        handle: Handle<Image>,
        snapshots: Vec<ToSyncSnapshot>,
    ) -> Self {
        Self {
            config,
            snapshots: Arc::new(snapshots),
            handle,
            expose_config_resource: false,
        }
    }
}

impl Plugin for CameraCapturePlugin {
    // `is_unique=false`：允许装多个（彩色、深度各一个）。
    fn is_unique(&self) -> bool {
        false
    }

    fn build(&self, app: &mut App) {
        // 启动即断言"该纹理格式能产出目标帧类型"，把错误提前到装配期而非运行期。
        assert!(
            FrameLayout::new(
                self.config.frame_kind,
                self.config.texture_format,
                self.config.width,
                self.config.height,
            )
            .is_some(),
            "capture texture format {:?} cannot produce {:?} frames",
            self.config.texture_format,
            self.config.frame_kind,
        );

        if self.expose_config_resource {
            app.insert_resource(self.config.clone()); // 主世界暴露 config，供外部读取尺寸
        }

        // 渲染世界（RenderApp）是独立于主世界的第二个 App；所有 GPU 相关系统都注册到它。
        let render_app = app.sub_app_mut(RenderApp);
        render_app.world_mut().init_resource::<ImageCopiers>();
        render_app
            .world_mut()
            .init_resource::<CaptureFrameSubmission>();
        render_app
            .world_mut()
            .init_resource::<ImageCopyDriverInstalled>();

        {
            // 注册本插件的 copier（每插件一个，负责一张源纹理）。
            let mut copiers = render_app.world_mut().resource_mut::<ImageCopiers>();
            copiers.push(ImageCopier::new(
                self.config.clone(),
                self.handle.clone(),
                self.snapshots.clone(),
            ));
        }

        let installed = render_app.world().resource::<ImageCopyDriverInstalled>().0;
        if !installed {
            // 两个核心系统只在第一个相机插件里安装一次（用标志位守护）：
            // 1) image_copy_driver 进渲染图，紧跟相机驱动之后执行；
            render_app.add_systems(
                RenderGraph,
                image_copy_driver
                    .after(camera_driver)
                    .in_set(RenderGraphSystems::Render),
            );

            render_app
                .world_mut()
                .resource_mut::<ImageCopyDriverInstalled>()
                .0 = true;
            // 2) receive_image_from_buffer 在主渲染阶段之后发起异步读回。
            render_app.add_systems(
                Render,
                receive_image_from_buffer.after(RenderSystems::Render),
            );
        }

        if self.expose_config_resource {
            render_app.insert_resource(self.config.clone()); // 渲染世界也放一份 config
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 验证：BGRA 源在打包成 RGB8 时会丢掉行对齐填充和 alpha，并把字节序翻转为 RGB。
    #[test]
    fn rgb8_layout_drops_row_padding_and_alpha() {
        // 2x2 BGRA, rows padded out to wgpu's copy alignment.
        // 中文：2×2 的 BGRA 数据，每行按 wgpu 拷贝对齐要求填充到 padded_row_bytes。
        let layout =
            FrameLayout::new(CapturedFrameKind::Rgb8, TextureFormat::Bgra8UnormSrgb, 2, 2).unwrap();
        assert_eq!(layout.output_len(), 2 * 2 * 3); // 输出应为 2×2 个 RGB 像素

        let mut mapped = vec![0u8; layout.padded_row_bytes * 2];
        mapped[..8].copy_from_slice(&[1, 2, 3, 255, 4, 5, 6, 255]);
        mapped[layout.padded_row_bytes..][..8].copy_from_slice(&[7, 8, 9, 255, 10, 11, 12, 255]);

        let mut out = vec![0u8; layout.output_len()];
        layout.write(&mapped, &mut out);

        assert_eq!(out, vec![3, 2, 1, 6, 5, 4, 9, 8, 7, 12, 11, 10]); // BGR→RGB 已翻转
    }

    // 验证：无法打包成 RGB8 的格式（如 Rgba16Float）在构造布局阶段就被拒绝。
    #[test]
    fn rgb8_layout_rejects_formats_it_cannot_pack() {
        assert!(
            FrameLayout::new(CapturedFrameKind::Rgb8, TextureFormat::Rgba16Float, 4, 4).is_none()
        );
    }

    // 验证：Raw 编码（深度）整行透传，仅去掉行对齐填充。
    #[test]
    fn raw_layout_passes_rows_through() {
        let layout = FrameLayout::new(
            CapturedFrameKind::Depth32F,
            TextureFormat::Depth32Float,
            2,
            2,
        )
        .unwrap();
        assert_eq!(layout.output_len(), 2 * 2 * 4); // 每像素 4 字节 f32

        let mut mapped = vec![0u8; layout.padded_row_bytes * 2];
        mapped[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        mapped[layout.padded_row_bytes..][..8].copy_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16]);

        let mut out = vec![0u8; layout.output_len()];
        layout.write(&mapped, &mut out);

        assert_eq!(out, (1..=16).collect::<Vec<u8>>());
    }

    // 验证：帧缓冲池确实复用内存（复用后的 capacity 与首次相同，且池被清空）。
    #[test]
    fn frame_buffer_pool_reuses_allocations() {
        let pool = Mutex::new(Vec::new());

        let buffer = acquire_frame_buffer(&pool, 64);
        assert_eq!(buffer.len(), 64);
        let capacity = buffer.capacity(); // 记录首次分配的内存容量
        pool.lock().unwrap().push(buffer); // 归还池

        let reused = acquire_frame_buffer(&pool, 64);
        assert_eq!(reused.len(), 64);
        assert_eq!(reused.capacity(), capacity); // 容量一致 = 复用了同一块内存
        assert!(pool.lock().unwrap().is_empty());
    }

    // 验证提交状态机：三个通道以任意顺序提交都应被接受，finish 后状态清空。
    #[test]
    fn capture_submission_accepts_all_channels_in_any_order() {
        let submission = CaptureFrameSubmission::default();
        let id = CaptureFrameId::new(7);

        submission.begin(id, CaptureChannels::ALL).unwrap();
        submission.claim(id, CapturedFrameKind::Depth32F).unwrap();
        submission.claim(id, CapturedFrameKind::R32Uint).unwrap();
        submission.claim(id, CapturedFrameKind::Rgb8).unwrap();

        assert_eq!(submission.finish(id), Ok(()));
        assert_eq!(submission.active_id(), None);
    }

    // 验证：重复提交同一通道报 DuplicateChannel；缺少必需通道时 finish 报 MissingChannels。
    #[test]
    fn capture_submission_rejects_missing_and_duplicate_channels() {
        let submission = CaptureFrameSubmission::default();
        let id = CaptureFrameId::new(11);

        submission.begin(id, CaptureChannels::ALL).unwrap();
        submission.claim(id, CapturedFrameKind::Rgb8).unwrap();
        assert!(matches!(
            submission.claim(id, CapturedFrameKind::Rgb8),
            Err(CaptureSubmissionError::DuplicateChannel { .. }) // `..` 忽略其它字段
        ));
        assert!(matches!(
            submission.finish(id),
            Err(CaptureSubmissionError::MissingChannels { .. })
        ));
    }

    // 验证：认领的帧号与当前进行中的帧号不一致时报 WrongFrame（防止跨帧串数据）。
    #[test]
    fn capture_submission_rejects_cross_frame_claims() {
        let submission = CaptureFrameSubmission::default();
        let active = CaptureFrameId::new(21);
        let wrong = CaptureFrameId::new(22);

        submission.begin(active, CaptureChannels::ALL).unwrap();

        assert!(matches!(
            submission.claim(wrong, CapturedFrameKind::Rgb8),
            Err(CaptureSubmissionError::WrongFrame {
                active: CaptureFrameId(21),
                received: CaptureFrameId(22),
            })
        ));
    }
}
