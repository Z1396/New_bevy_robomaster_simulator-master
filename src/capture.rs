//! 离屏捕获管线的公共入口模块（talos / ros2 两套功能共用）。
//!
//! 职责分工：
//! - 本文件：定义"捕获源相机"与"离屏相机"的公共类型、启动系统与相机内参计算；
//! - `driver`：GPU 异步快照核心——把离屏纹理读回 CPU 再分发给订阅者；
//! - `depth`：只输出深度图的离屏相机；
//! - `view_copy`：把离屏纹理拷贝（blit）到屏幕预览图。
//!
//! 数据流（新手先抓这条主线）：setup.rs 的主相机挂上 `CaptureSource` 标记 →
//! `sync_capture_camera` 每帧把主相机变换抄给离屏 `CaptureCamera` →
//! 离屏相机渲染进 `ImageHandle` 指向的纹理 → driver 把纹理异步读回并交给 Snapshots。

// 声明子模块；`pub` 让 talos/ros2 能通过 `crate::capture::driver::...` 访问其内部类型。
pub mod depth;
pub mod driver;
pub mod view_copy;

use bevy::camera::RenderTarget; // 相机渲染目标：窗口 or 内存纹理
use bevy::core_pipeline::prepass::DepthPrepass; // 预写深度缓冲，深度相关处理需要
use bevy::core_pipeline::tonemapping::Tonemapping; // 色调映射开关
use bevy::prelude::*;

use crate::config::SimulationConfig;
use bevy_metalfx::{MetalFxTemporalUpscaling, UpscaleFactor};

// 把 driver 里的 CaptureBundle 提到本模块顶层，外部可直接写 `crate::capture::CaptureBundle`。
pub use driver::CaptureBundle;

// `CaptureSource`：空标记组件，由 setup.rs 挂在玩家主相机上，语义是"这台相机是捕获源"。
// 各 sync_* 系统靠它找到要跟随的相机（见下方 sync_capture_camera）。
#[derive(Component)]
pub struct CaptureSource;

// `CaptureCamera`：挂在本模块创建的离屏相机上，语义是"这台相机专门用于捕获"。
#[derive(Component)]
pub struct CaptureCamera;

// `Deref` 让 ImageHandle 能像 Handle 一样直接用（`*handle`、自动解引用到 `handle.0`）。
// 它保存离屏相机渲染目标纹理的句柄，由 talos/ros2 插件在装配时插入。
#[derive(Resource, Deref, Clone)]
pub struct ImageHandle(pub Handle<Image>);

// 离屏相机的垂直视场角（单位：弧度）；talos 用它反推相机内参，必须与主相机一致。
#[derive(Resource, Clone, Copy)]
pub struct CameraFov(pub f32);

// 相机渲染顺序（order）：Bevy 按 order 升序渲染相机。取负值让捕获相机最先渲染，
// 把本帧画面先写进离屏纹理，再轮到主相机（order 0）与预览相机（order 1）。
pub const CAPTURE_CAMERA_ORDER: isize = -100;

/// 启动系统（Startup 阶段跑一次）：创建离屏捕获相机。
/// 该相机不显示到窗口，而是把画面渲染进 `ImageHandle` 指向的纹理；
/// talos/ros2 插件随后从这张纹理异步读回图像。函数幂等——已存在则直接返回。
pub fn setup_capture_camera(world: &mut World) {
    let capture_camera_exists = {
        // `query_filtered`：只查实体 ID、不取组件数据，专用于"存不存在"判断。
        let mut query = world.query_filtered::<Entity, With<CaptureCamera>>();
        query.iter(world).next().is_some()
    };
    if capture_camera_exists {
        return;
    }

    let render_target_handle = world.resource::<ImageHandle>().0.clone();
    let fov = world.resource::<CameraFov>().0;
    // MetalFX 是 macOS 专有的时空上采样；仅当编译目标为 macOS 且配置开启时才启用。
    // `.filter(...)` 条件不满足时返回 None，后面按需把组件插入相机。
    let metalfx = world
        .get_resource::<SimulationConfig>()
        .filter(|config| cfg!(target_os = "macos") && config.render.metalfx_temporal)
        .map(|config| MetalFxTemporalUpscaling {
            factor: UpscaleFactor::clamped(config.render.metalfx_scale),
            frame_generation: config.render.metalfx_frame_generation,
        });

    let mut capture_camera = world.spawn((
        Camera3d::default(),
        Tonemapping::None, // 关闭色调映射：输出原始颜色给视觉算法，避免被"美化"
        RenderTarget::Image(render_target_handle.into()), // 渲染到内存纹理而非窗口
        Camera {
            order: CAPTURE_CAMERA_ORDER, // 渲染顺序，含义见常量注释
            // clear_color: ClearColorConfig::Custom(Color::BLACK),
            ..default()
        },
        Projection::Perspective(PerspectiveProjection {
            fov,          // 垂直视场角（弧度），与主相机一致才能画面/内参对齐
            near: 0.1,    // 近裁剪面（单位：米）
            far: 10000.0, // 远裁剪面（单位：米）
            ..default()
        }),
        Msaa::Off,     // 关多重采样：视觉算法要原始像素，抗锯齿会引入混色
        DepthPrepass,  // 添加深度预渲染通道
        CaptureCamera, // 标记：本相机用于捕获
    ));
    if let Some(metalfx) = metalfx {
        capture_camera.insert(metalfx);
    }
}

// 预览相机（2D）：把捕获纹理当作一张全屏图片显示出来。
#[derive(Component)]
pub struct PreviewCamera;

// 预览图片节点：承载 `ImageNode` 的 UI 实体，铺满窗口。
#[derive(Component)]
pub struct PreviewImageNode;

/// 启动系统：当 `config.preview.enabled` 为真时创建屏幕预览——
/// 一台 2D 相机 + 一个铺满窗口的图片节点，显示捕获纹理（即 talos 看到的那张图）。
/// 同样幂等：相机与图片节点各自"不存在才创建"。
pub fn setup_preview_window(world: &mut World) {
    let preview_enabled = world
        .resource::<crate::config::SimulationConfig>()
        .preview
        .enabled;
    if !preview_enabled {
        return;
    }

    let render_target_handle = world.resource::<ImageHandle>().0.clone();
    let preview_camera_exists = {
        let mut query = world.query_filtered::<Entity, With<PreviewCamera>>();
        query.iter(world).next().is_some()
    };
    if !preview_camera_exists {
        world.spawn((
            Camera2d::default(),
            Camera {
                order: 1, // 排在捕获相机(-100)、主相机(0)之后，叠在最上层
                ..default()
            },
            PreviewCamera,
        ));
    }

    let preview_node_exists = {
        let mut query = world.query_filtered::<Entity, With<PreviewImageNode>>();
        query.iter(world).next().is_some()
    };
    if !preview_node_exists {
        world.spawn((
            Node {
                width: Val::Percent(100.0),  // 铺满父节点宽度
                height: Val::Percent(100.0), // 铺满父节点高度
                ..default()
            },
            // Render as a background; help text UI remains on top.
            // 中文：作为背景渲染，左上角帮助文字 UI 仍覆盖在其上方。
            GlobalZIndex(-1), // z 序置底，确保不遮挡其它 UI
            ImageNode::new(render_target_handle), // 用捕获纹理作为 2D 图片显示
            PreviewImageNode,
        ));
    }
}

/// 把源相机的局部变换（平移/缩放/旋转）逐字段抄给目标相机。
/// 这里没有直接 `*our = *target`，因为目标相机可能还带有别的分量需保留。
pub fn copy_transform(target: &Transform, our: &mut Transform) {
    our.translation = target.translation;
    our.scale = target.scale;
    our.rotation = target.rotation;
}

/// 每帧 Update 系统：把主相机（`CaptureSource`）的变换同步给离屏相机（`CaptureCamera`），
/// 让捕获画面与玩家视角保持一致。
/// `Single<...>` 保证查询"恰好命中一个"实体，否则本帧系统不运行；
/// 两侧都加 `Without<...>` 是为了解除两个查询对 `Transform` 的借用别名冲突。
pub fn sync_capture_camera(
    target: Single<&Transform, (With<CaptureSource>, Without<CaptureCamera>)>,
    mut our: Single<&mut Transform, (With<CaptureCamera>, Without<CaptureSource>)>,
) {
    copy_transform(&target, &mut our);
}

/// 针孔相机内参（与 OpenCV 约定一致，供 Talos 做像素↔射线换算）：
/// `fx`/`fy` 为焦距（单位：像素），`cx`/`cy` 为主点（单位：像素，通常为图像中心）。
#[derive(Clone, Copy, Debug)]
pub struct CameraIntrinsics {
    pub fx: f64,
    pub fy: f64,
    pub cx: f64,
    pub cy: f64,
    pub width: u32,
    pub height: u32,
}

/// 由图像尺寸与垂直视场角推导针孔内参。
/// 入参 `fov_y` 单位：弧度；返回的 fx/fy/cx/cy 单位：像素。
pub fn compute_camera_intrinsics(width: u32, height: u32, fov_y: f32) -> CameraIntrinsics {
    let fov_y = fov_y as f64; // 内部统一用 f64，减小大像素分辨率下的精度误差
    let aspect = width as f64 / height as f64; // 宽高比
    let fov_x = 2.0 * ((fov_y / 2.0).tan() * aspect).atan(); // 由垂直视场角换算水平视场角（弧度）

    let fx = width as f64 / (2.0 * (fov_x / 2.0).tan()); // 焦距_x = 宽 / (2·tan(水平半角))
    let fy = height as f64 / (2.0 * (fov_y / 2.0).tan());

    let cx = width as f64 / 2.0; // 主点=图像中心
    let cy = height as f64 / 2.0;

    CameraIntrinsics {
        fx,
        fy,
        cx,
        cy,
        width,
        height,
    }
}

// 捕获图像默认分辨率（单位：像素）。1440×1080（4:3）是 Talos 契约里的期望尺寸
//（见 talos/capture.rs 对 IMAGE_WIDTH/IMAGE_HEIGHT 的校验）。
pub const IMAGE_WIDTH: u32 = 1440;
pub const IMAGE_HEIGHT: u32 = 1080;
