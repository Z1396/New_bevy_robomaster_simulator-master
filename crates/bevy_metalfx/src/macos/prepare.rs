//! Per-frame view setup: texture usages, jitter, mip bias, and scaler lifetime.

use bevy::camera::{Camera3d, CameraMainTextureUsages, MainPassResolutionOverride};
use bevy::core_pipeline::prepass::{DepthPrepass, MotionVectorPrepass};
use bevy::diagnostic::FrameCount;
use bevy::math::Vec4Swizzles;
use bevy::prelude::*;
use bevy::render::{
    camera::{ExtractedCamera, MipBias, TemporalJitter},
    render_resource::TextureUsages,
    renderer::RenderDevice,
    view::ExtractedView,
};

use crate::{MetalFxResetHistory, MetalFxTemporalUpscaling, UpscaleFactor};

use super::context::{MetalFxContext, ScalerResolutions, ScalerSetup};

/// The 8-tap Halton(2, 3) sequence Bevy's TAA uses, in pixels of the render resolution.
const JITTER_SEQUENCE: [Vec2; 8] = [
    Vec2::ZERO,
    Vec2::new(0.0, -0.16666666),
    Vec2::new(-0.25, 0.16666669),
    Vec2::new(0.25, -0.3888889),
    Vec2::new(-0.375, -0.055555552),
    Vec2::new(0.125, 0.2777778),
    Vec2::new(-0.125, -0.2777778),
    Vec2::new(0.375, 0.055555582),
];

pub(super) fn prepare_metalfx(
    mut query: Query<
        (
            Entity,
            &ExtractedCamera,
            &ExtractedView,
            &MetalFxTemporalUpscaling,
            &mut Camera3d,
            &mut CameraMainTextureUsages,
            &mut TemporalJitter,
            &mut MipBias,
            Option<&MetalFxContext>,
            Has<MetalFxResetHistory>,
        ),
        (
            With<Camera3d>,
            With<TemporalJitter>,
            With<DepthPrepass>,
            With<MotionVectorPrepass>,
        ),
    >,
    render_device: Res<RenderDevice>,
    frame_count: Res<FrameCount>,
    mut commands: Commands,
) {
    for (
        entity,
        camera,
        view,
        metalfx,
        mut camera_3d,
        mut camera_main_texture_usages,
        mut temporal_jitter,
        mut mip_bias,
        context,
        reset_requested,
    ) in &mut query
    {
        let Some(texture_resolution) = camera.physical_target_size else {
            continue;
        };
        let output_resolution = view.viewport.zw();

        // MetalFX reads its inputs from the target's origin, which is also where a resolution
        // overridden main pass draws. A camera that doesn't own its whole target would have the
        // two disagree, so refuse rather than reconstruct from the wrong texels.
        if view.viewport.xy() != UVec2::ZERO || output_resolution != texture_resolution {
            warn_once!(
                "MetalFX needs a camera that covers its entire render target; disabling upscaling \
                 for a camera with viewport {:?} on a {:?} target",
                view.viewport,
                texture_resolution
            );
            commands
                .entity(entity)
                .remove::<(MetalFxContext, MainPassResolutionOverride)>();
            continue;
        }

        let render_resolution = render_resolution(output_resolution, metalfx.factor);

        // The scaler writes the reconstructed image with a compute shader.
        camera_main_texture_usages.0 |= TextureUsages::STORAGE_BINDING;
        if metalfx.frame_generation {
            camera_main_texture_usages.0 |= TextureUsages::COPY_DST;
        }

        let mut depth_texture_usages = TextureUsages::from(camera_3d.depth_texture_usages);
        depth_texture_usages |= TextureUsages::TEXTURE_BINDING;
        camera_3d.depth_texture_usages = depth_texture_usages.into();

        temporal_jitter.offset = JITTER_SEQUENCE[(frame_count.0 as usize) % JITTER_SEQUENCE.len()];
        mip_bias.0 = -metalfx.factor.get().log2();

        let setup = ScalerSetup {
            resolutions: ScalerResolutions {
                texture: texture_resolution,
                output: output_resolution,
            },
            color_format: view.target_format,
            frame_generation: metalfx.frame_generation,
        };

        match context {
            Some(context) if context.matches(&setup, metalfx.factor.get()) => {
                if reset_requested {
                    context.request_reset();
                }
                commands
                    .entity(entity)
                    .insert(MainPassResolutionOverride(render_resolution));
            }
            _ => match MetalFxContext::new(&render_device, setup) {
                // A new scaler always starts from a clean history, so `reset_requested` needs no
                // extra handling here.
                Some(context) => {
                    commands
                        .entity(entity)
                        .insert((context, MainPassResolutionOverride(render_resolution)));
                }
                None => {
                    warn_once!(
                        "MetalFX temporal scaling is unavailable for {:?}; rendering at full \
                         resolution",
                        view.target_format
                    );
                    commands
                        .entity(entity)
                        .remove::<(MetalFxContext, MainPassResolutionOverride)>();
                }
            },
        }
    }
}

/// Rounds the reduced main-pass resolution, keeping both axes on the same ratio: MetalFX assumes
/// the input and output aspect ratios match.
fn render_resolution(output: UVec2, factor: UpscaleFactor) -> UVec2 {
    (output.as_vec2() / factor.get())
        .round()
        .as_uvec2()
        .max(UVec2::ONE)
        .min(output)
}
