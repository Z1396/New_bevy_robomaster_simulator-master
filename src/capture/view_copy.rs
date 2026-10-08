//! 纹理拷贝（blit）：把某台相机渲染出的"视图深度纹理"复制到一张可供 CPU 读回的图像。
//!
//! 为什么需要它：深度相机的深度缓冲住在 Bevy 内部的 prepass 深度纹理里，名字/生命周期由
//! 引擎管理，外部不能直接把它当普通纹理读回。于是安排这个渲染图系统，在渲染流程的合适时机
//! （Prepass 之后、MainPass 之前）把深度纹理 blit 到我们自己创建的 `Image` 纹理，之后
//! driver 的异步回读（见 driver.rs）就能照常从这张纹理取数据。
//!
//! 流程归属：这是 capture 管线"深度分支"的一环；彩色分支直接把颜色渲到目标纹理，不需要它。

use crate::capture::driver::create_capture_image_handle; // 复用 driver 的图像创建助手
use bevy::asset::RenderAssetUsages;
use bevy::core_pipeline::{Core3dSystems, schedule::Core3d}; // 3D 核心渲染阶段与系统集
use bevy::prelude::*;
use bevy::render::RenderApp; // 渲染世界（GPU 相关注册都在这里）
use bevy::render::camera::ExtractedCamera; // 从主世界抽取到渲染世界的相机
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::{
    Extent3d, Origin3d, TexelCopyTextureInfo, TextureAspect, TextureFormat, TextureUsages,
};
use bevy::render::renderer::{RenderContext, ViewQuery}; // 渲染上下文 + 按视图查询
use bevy::render::texture::GpuImage;
use bevy::render::view::ViewDepthTexture; // 某视图的深度纹理（blit 的源）

// 拷贝目标：我们自己创建、可被拷贝读出的图像纹理句柄。
#[derive(Resource, Clone, Deref, DerefMut)]
struct CopyViewTextureTarget(Handle<Image>);

// 目标相机 order：只有与它匹配的相机视图才会被拷贝（其余相机直接跳过）。
#[derive(Resource, Clone, Copy)]
struct CopyViewTextureCameraOrder(isize);

// 记录"拷贝系统是否已安装"，避免多个插件重复注册。
#[derive(Resource, Default)]
struct CopyViewTextureSystemInstalled(bool);

// 拷贝源类型。目前只有 Depth；用 enum 预留将来可扩展的其它源。
#[derive(Resource, Clone, Copy)]
enum ViewTextureCopySource {
    Depth,
}

/// 渲染图系统（Core3d 调度，排在 Prepass 之后、MainPass 之前）。
/// `ViewQuery` 针对"当前正在渲染的那台相机视图"取参数，因此对每台相机各执行一次；
/// 只有 order 匹配的相机才真正做拷贝（其余相机立即 return）。
fn copy_view_texture_system(
    view: ViewQuery<(&ExtractedCamera, &ViewDepthTexture)>, // 本视图的相机 + 深度纹理
    target_order: Res<CopyViewTextureCameraOrder>,
    target: Res<CopyViewTextureTarget>,
    source: Res<ViewTextureCopySource>,
    image_assets: Res<RenderAssets<GpuImage>>,
    mut render_context: RenderContext,
) {
    let (camera, depth_texture) = view.into_inner();
    if camera.order != target_order.0 {
        return; // 不是我们要拷贝的那台相机，跳过
    }

    // 目标纹理此刻必须已在 GPU 上；没有就跳过（下一帧再说）。
    let Some(output_image) = image_assets.get(target.0.id()) else {
        return;
    };

    // 深度纹理只能按"仅深度"切面拷贝。
    let aspect = match *source {
        ViewTextureCopySource::Depth => TextureAspect::DepthOnly,
    };
    let encoder = render_context.command_encoder();
    encoder.push_debug_group("copy capture view texture to image"); // 调试分组，便于抓帧定位
    encoder.copy_texture_to_texture(
        TexelCopyTextureInfo {
            texture: &depth_texture.texture, // 源：引擎管理的 prepass 深度纹理
            mip_level: 0,
            origin: Origin3d::ZERO,
            aspect,
        },
        TexelCopyTextureInfo {
            texture: &output_image.texture, // 目标：我们可读回的图像纹理
            mip_level: 0,
            origin: Origin3d::ZERO,
            aspect,
        },
        Extent3d {
            width: output_image.texture_descriptor.size.width, // 拷贝尺寸（单位：像素）
            height: output_image.texture_descriptor.size.height,
            depth_or_array_layers: 1, // 单层
        },
    );
    encoder.pop_debug_group();
}

/// 深度纹理拷贝插件：创建一个可读回的深度目标纹理，并安装上面的拷贝系统。
/// 深度分支的 `CaptureBundle` 会构造它，并把返回的纹理句柄交给相机捕获插件用于读回。
pub struct ViewTextureCopyPlugin {
    target_texture: Handle<Image>,
    camera_order: isize,
    source: ViewTextureCopySource,
}

impl ViewTextureCopyPlugin {
    // 使用默认深度相机 order 创建（见 depth::DEPTH_CAPTURE_CAMERA_ORDER）。
    pub fn new_depth(app: &mut App, width: u32, height: u32) -> (Self, Handle<Image>) {
        Self::new_depth_for_camera_order(
            app,
            width,
            height,
            crate::capture::depth::DEPTH_CAPTURE_CAMERA_ORDER,
        )
    }

    // 按指定相机 order 创建深度目标纹理，返回 (插件, 纹理句柄)。
    pub fn new_depth_for_camera_order(
        app: &mut App,
        width: u32,
        height: u32,
        camera_order: isize,
    ) -> (Self, Handle<Image>) {
        let depth_texture = create_capture_image_handle(
            app,
            width,
            height,
            TextureFormat::Depth32Float, // 与相机深度缓冲一致的 32 位浮点格式
            RenderAssetUsages::default(),
            // COPY_DST：作为 blit 的目标被写入；COPY_SRC：之后能被读回；
            // TEXTURE_BINDING：可作为纹理被采样/显示。
            TextureUsages::COPY_DST | TextureUsages::COPY_SRC | TextureUsages::TEXTURE_BINDING,
        );

        (
            Self {
                target_texture: depth_texture.clone(),
                camera_order,
                source: ViewTextureCopySource::Depth,
            },
            depth_texture,
        )
    }
}

impl Plugin for ViewTextureCopyPlugin {
    fn build(&self, app: &mut App) {
        // 全部注册到渲染世界：拷贝是纯 GPU 操作。
        let render_app = app.sub_app_mut(RenderApp);
        render_app
            .world_mut()
            .init_resource::<CopyViewTextureSystemInstalled>();
        render_app
            .world_mut()
            .insert_resource(CopyViewTextureTarget(self.target_texture.clone()));
        render_app
            .world_mut()
            .insert_resource(CopyViewTextureCameraOrder(self.camera_order));
        render_app.world_mut().insert_resource(self.source);

        let installed = render_app
            .world()
            .resource::<CopyViewTextureSystemInstalled>()
            .0;
        if installed {
            return; // 系统只需安装一次
        }

        // 拷贝系统插在 Prepass 之后（深度已就绪）、MainPass 之前（主渲染尚未覆盖）。
        render_app.add_systems(
            Core3d,
            copy_view_texture_system
                .after(Core3dSystems::Prepass)
                .before(Core3dSystems::MainPass),
        );
        render_app
            .world_mut()
            .resource_mut::<CopyViewTextureSystemInstalled>()
            .0 = true;
    }
}
