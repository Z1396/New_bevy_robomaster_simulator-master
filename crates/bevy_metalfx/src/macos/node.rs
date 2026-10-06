//! Encodes MetalFX into the frame's Metal command buffer.

use bevy::camera::MainPassResolutionOverride;
use bevy::core_pipeline::prepass::ViewPrepassTextures;
use bevy::math::UVec2;
use bevy::prelude::*;
use bevy::render::{
    camera::TemporalJitter,
    render_resource::{
        CommandEncoderDescriptor, Extent3d, Origin3d, TexelCopyTextureInfo, Texture, TextureAspect,
    },
    renderer::{RenderContext, ViewQuery},
    view::ViewTarget,
};
use objc2::runtime::ProtocolObject;
use objc2_metal::MTLTexture;
use objc2_metal_fx::{
    MTLFXFrameInterpolator, MTLFXFrameInterpolatorBase, MTLFXTemporalScaler,
    MTLFXTemporalScalerBase,
};
use std::sync::atomic::Ordering;

use crate::MetalFxTemporalUpscaling;

use super::context::{FrameGeneration, MetalFxContext};
use super::extract::MetalFxFrameDelta;
use super::metal::{retain_metal_texture, texture_size};

pub(super) fn metalfx_upscale(
    view: ViewQuery<(
        &MetalFxTemporalUpscaling,
        &MetalFxContext,
        &MainPassResolutionOverride,
        &TemporalJitter,
        &ViewTarget,
        &ViewPrepassTextures,
        &Projection,
    )>,
    frame_delta: Res<MetalFxFrameDelta>,
    mut ctx: RenderContext,
) {
    let (metalfx, context, resolution_override, jitter, view_target, prepass_textures, projection) =
        view.into_inner();

    let (Some(depth), Some(motion_vectors)) =
        (&prepass_textures.depth, &prepass_textures.motion_vectors)
    else {
        return;
    };
    let depth = &depth.texture.texture;
    let motion_vectors = &motion_vectors.texture.texture;

    let view_target = view_target.post_process_write();
    let render_resolution = resolution_override.0;

    // The scaler was built for the texture dimensions Bevy allocated. If anything reallocated at a
    // different size this frame, feeding it through would be undefined rather than merely wrong.
    let expected = context.output_resolution();
    if texture_size(view_target.source_texture) != expected
        || texture_size(view_target.destination_texture) != expected
        || texture_size(depth) != expected
        || texture_size(motion_vectors) != expected
    {
        return;
    }

    let (Some(color_texture), Some(depth_texture), Some(motion_texture), Some(destination_texture)) = (
        retain_metal_texture(view_target.source_texture),
        retain_metal_texture(depth),
        retain_metal_texture(motion_vectors),
        retain_metal_texture(view_target.destination_texture),
    ) else {
        return;
    };

    let reset = context.take_reset();
    let frame_generation = context
        .frame_generation()
        .filter(|_| metalfx.frame_generation);

    let Some(frame_generation) = frame_generation else {
        encode_temporal_scaler(
            context,
            render_resolution,
            jitter,
            reset,
            &color_texture,
            &depth_texture,
            &motion_texture,
            &destination_texture,
            &mut ctx,
        );
        return;
    };

    let (Some(current_color_texture), Some(previous_color_texture)) = (
        retain_metal_texture(&frame_generation.current_color),
        retain_metal_texture(&frame_generation.previous_color),
    ) else {
        return;
    };

    // Upscale into our own history texture so the interpolator has both the current and previous
    // reconstructed frames to work from.
    encode_temporal_scaler(
        context,
        render_resolution,
        jitter,
        reset,
        &color_texture,
        &depth_texture,
        &motion_texture,
        &current_color_texture,
        &mut ctx,
    );

    let has_history = frame_generation.history_valid.load(Ordering::Relaxed);
    if has_history {
        encode_frame_interpolator(
            frame_generation,
            render_resolution,
            expected,
            jitter,
            projection,
            frame_delta.0,
            &current_color_texture,
            &previous_color_texture,
            &depth_texture,
            &motion_texture,
            &destination_texture,
            &mut ctx,
        );
    }

    // All wgpu-side copies are batched after the raw encoding above: without history the upscaled
    // frame still has to reach the view target, and either way it becomes the next frame's history.
    let history_copy = (
        frame_generation.current_color.clone(),
        frame_generation.previous_color.clone(),
        expected,
    );
    let present_copy = (!has_history).then(|| {
        (
            frame_generation.current_color.clone(),
            view_target.destination_texture.clone(),
            expected,
        )
    });
    copy_textures(&mut ctx, present_copy.into_iter().chain([history_copy]));
    frame_generation
        .history_valid
        .store(true, Ordering::Relaxed);
}

#[allow(clippy::too_many_arguments)]
fn encode_temporal_scaler(
    context: &MetalFxContext,
    render_resolution: UVec2,
    jitter: &TemporalJitter,
    reset: bool,
    color: &ProtocolObject<dyn MTLTexture>,
    depth: &ProtocolObject<dyn MTLTexture>,
    motion: &ProtocolObject<dyn MTLTexture>,
    output: &ProtocolObject<dyn MTLTexture>,
    ctx: &mut RenderContext,
) {
    let scaler = context.scaler();
    unsafe {
        // The main pass drew into the top-left of full-size textures; this is the sub-rect.
        scaler.setInputContentWidth(render_resolution.x as _);
        scaler.setInputContentHeight(render_resolution.y as _);
        scaler.setColorTexture(Some(color));
        scaler.setDepthTexture(Some(depth));
        scaler.setMotionTexture(Some(motion));
        scaler.setOutputTexture(Some(output));
        scaler.setJitterOffsetX(-jitter.offset.x);
        scaler.setJitterOffsetY(-jitter.offset.y);
        // Bevy's motion vectors are `current_uv - previous_uv`; MetalFX wants pixels pointing back
        // to the previous frame, at the resolution the main pass rendered at.
        scaler.setMotionVectorScaleX(-(render_resolution.x as f32));
        scaler.setMotionVectorScaleY(-(render_resolution.y as f32));
        scaler.setReset(reset);
        scaler.setDepthReversed(true);
    }

    encode_to_command_buffer(ctx, |command_buffer| unsafe {
        scaler.encodeToCommandBuffer(command_buffer)
    });
}

#[allow(clippy::too_many_arguments)]
fn encode_frame_interpolator(
    frame_generation: &FrameGeneration,
    render_resolution: UVec2,
    output_resolution: UVec2,
    jitter: &TemporalJitter,
    projection: &Projection,
    delta_seconds: f32,
    current_color: &ProtocolObject<dyn MTLTexture>,
    previous_color: &ProtocolObject<dyn MTLTexture>,
    depth: &ProtocolObject<dyn MTLTexture>,
    motion: &ProtocolObject<dyn MTLTexture>,
    output: &ProtocolObject<dyn MTLTexture>,
    ctx: &mut RenderContext,
) {
    let interpolator = &frame_generation.interpolator;
    let (near, far, fov_degrees, aspect_ratio) =
        perspective_parameters(projection, output_resolution);

    unsafe {
        interpolator.setColorTexture(Some(current_color));
        interpolator.setPrevColorTexture(Some(previous_color));
        interpolator.setDepthTexture(Some(depth));
        interpolator.setMotionTexture(Some(motion));
        interpolator.setOutputTexture(Some(output));
        interpolator.setMotionVectorScaleX(-(render_resolution.x as f32));
        interpolator.setMotionVectorScaleY(-(render_resolution.y as f32));
        interpolator.setDeltaTime(delta_seconds);
        interpolator.setNearPlane(near);
        interpolator.setFarPlane(far);
        interpolator.setFieldOfView(fov_degrees);
        interpolator.setAspectRatio(aspect_ratio);
        interpolator.setJitterOffsetX(-jitter.offset.x);
        interpolator.setJitterOffsetY(-jitter.offset.y);
        interpolator.setShouldResetHistory(false);
        interpolator.setDepthReversed(true);
    }

    encode_to_command_buffer(ctx, |command_buffer| unsafe {
        interpolator.encodeToCommandBuffer(command_buffer)
    });
}

/// Hands MetalFX the raw `MTLCommandBuffer` behind this system's encoder.
///
/// `RenderContext`'s encoder is per-system, and wgpu panics if one encoder sees both the wgpu and
/// the raw encoding API. So this system may only ever touch its encoder through here; anything that
/// needs wgpu commands has to go through [`copy_textures`], which uses an encoder of its own.
fn encode_to_command_buffer(
    ctx: &mut RenderContext,
    encode: impl FnOnce(&ProtocolObject<dyn objc2_metal::MTLCommandBuffer>),
) {
    // SAFETY: the callback only encodes into the command buffer wgpu already owns for this frame.
    unsafe {
        ctx.command_encoder()
            .as_hal_mut::<wgpu::hal::api::Metal, _, _>(|encoder| {
                let Some(command_buffer) = encoder.and_then(|encoder| encoder.raw_command_buffer())
                else {
                    return;
                };
                encode(command_buffer);
            });
    }
}

/// Records texture copies onto a dedicated encoder and queues it behind the raw MetalFX work.
///
/// `add_command_buffer` flushes the raw encoder first, so the copies still land after everything
/// MetalFX encoded in this system.
fn copy_textures(
    ctx: &mut RenderContext,
    copies: impl IntoIterator<Item = (Texture, Texture, UVec2)>,
) {
    let mut encoder = ctx
        .render_device()
        .create_command_encoder(&CommandEncoderDescriptor {
            label: Some("metalfx_frame_generation_history"),
        });
    for (source, destination, resolution) in copies {
        encoder.copy_texture_to_texture(
            texture_copy_info(&source),
            texture_copy_info(&destination),
            Extent3d {
                width: resolution.x,
                height: resolution.y,
                depth_or_array_layers: 1,
            },
        );
    }
    let command_buffer = encoder.finish();
    ctx.add_command_buffer(command_buffer);
}

fn texture_copy_info(texture: &Texture) -> TexelCopyTextureInfo<'_> {
    TexelCopyTextureInfo {
        texture,
        mip_level: 0,
        origin: Origin3d::ZERO,
        aspect: TextureAspect::All,
    }
}

fn perspective_parameters(projection: &Projection, resolution: UVec2) -> (f32, f32, f32, f32) {
    let aspect_ratio = resolution.x as f32 / resolution.y as f32;
    match projection {
        Projection::Perspective(projection) => (
            projection.near,
            projection.far,
            projection.fov.to_degrees(),
            aspect_ratio,
        ),
        _ => (0.1, 10000.0, 60.0, aspect_ratio),
    }
}
