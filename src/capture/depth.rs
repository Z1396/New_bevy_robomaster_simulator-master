//! 深度图捕获：一台只用于产出深度图的离屏相机（供 ROS2 的 livox 点云等消费）。
//!
//! 与彩色捕获的区别：彩色相机把颜色直接渲染到 `ImageHandle` 纹理；深度相机这里用
//! `RenderTarget::None { size }` —— 它只"搭个渲染通道"生成深度缓冲，真正的深度纹理由
//! Bevy 的内部 prepass 深度纹理承载，随后由 `view_copy` 模块 blit 到可读回的目标纹理。
//!
//! 单位约定：`near`/`far` 与相机位置一样，单位是**米**（Bevy 世界单位即米）。深度写入前
//! 是"反向 Z"编码，消费方（见 ros2/livox.rs 的 `linearize_reverse_z`）需用 near 解码回真实米。

use crate::capture::{CaptureSource, copy_transform}; // 复用公共的捕获源标记与变换拷贝
use bevy::camera::RenderTarget;
use bevy::core_pipeline::prepass::DepthPrepass; // 预写深度缓冲，深度捕获必需
use bevy::prelude::*;

// 深度相机渲染顺序：比彩色捕获相机(-100)再靠前一位，确保深度先于彩色就绪。
pub const DEPTH_CAPTURE_CAMERA_ORDER: isize = -101;

/// 深度相机设置：图像尺寸（单位：像素）、垂直视场角（单位：弧度）、近/远裁剪面（单位：米）。
#[derive(Resource, Clone, Copy)]
pub struct DepthCameraSettings {
    pub width: u32,
    pub height: u32,
    pub fov_y: f32, // 垂直视场角（弧度）
    pub near: f32,  // 近裁剪面（单位：米）
    pub far: f32,   // 远裁剪面（单位：米）
}

// `DepthCaptureCamera`：标记这台相机是深度捕获相机，sync 系统据此区分目标。
#[derive(Component)]
pub struct DepthCaptureCamera;

/// 启动系统（Startup 阶段跑一次）：创建深度捕获用离屏相机。幂等——已存在则跳过。
/// `Projection::Perspective` 的 near/far 单位是米，决定深度可分辨的范围。
pub fn setup_depth_capture_camera(world: &mut World) {
    let depth_camera_exists = {
        let mut query = world.query_filtered::<Entity, With<DepthCaptureCamera>>();
        query.iter(world).next().is_some()
    };
    if depth_camera_exists {
        return;
    }

    // `*` 解引用后再复制（DepthCameraSettings 实现了 Copy，复制成本很低）。
    let settings = *world.resource::<DepthCameraSettings>();

    world.spawn((
        Camera3d::default(),
        Camera {
            order: DEPTH_CAPTURE_CAMERA_ORDER,
            ..default()
        },
        Projection::Perspective(PerspectiveProjection {
            fov: settings.fov_y, // 垂直视场角（弧度）
            near: settings.near, // 近裁剪面（米）
            far: settings.far,   // 远裁剪面（米）
            ..default()
        }),
        // 只指定渲染尺寸、不落到某张纹理：深度由 prepass 深度纹理承载（见文件头）。
        RenderTarget::None {
            size: UVec2::new(settings.width, settings.height),
        },
        Msaa::Off,    // 关多重采样，保证深度值不被混样污染
        DepthPrepass, // 深度捕获必需的预渲染通道
        DepthCaptureCamera,
    ));
}

/// 每帧 Update 系统：把主相机（`CaptureSource`）的变换同步给深度相机，使其视角与玩家一致。
/// 两侧 `Without<...>` 用于解除查询对 `Transform` 的借用冲突（同 capture.rs 的 sync_capture_camera）。
pub fn sync_depth_capture_camera(
    target: Single<&Transform, (With<CaptureSource>, Without<DepthCaptureCamera>)>,
    mut our: Single<&mut Transform, (With<DepthCaptureCamera>, Without<CaptureSource>)>,
) {
    copy_transform(&target, &mut our); // 复用公共的逐字段拷贝
}
