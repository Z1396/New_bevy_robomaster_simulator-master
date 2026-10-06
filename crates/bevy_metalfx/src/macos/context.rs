//! Ownership of the `MTLFXTemporalScaler` (and its optional frame interpolator) for one view.

use bevy::core_pipeline::{core_3d::CORE_3D_DEPTH_FORMAT, prepass::MOTION_VECTOR_PREPASS_FORMAT};
use bevy::math::UVec2;
use bevy::prelude::*;
use bevy::render::{
    render_resource::{Texture, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages},
    renderer::RenderDevice,
};
use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal_fx::{
    MTLFXFrameInterpolatableScaler, MTLFXFrameInterpolator, MTLFXFrameInterpolatorDescriptor,
    MTLFXTemporalScaler, MTLFXTemporalScalerDescriptor,
};
use std::sync::atomic::{AtomicBool, Ordering};

use super::metal::{metal_pixel_format, objc_class_exists};

/// The resolutions a scaler is built around.
///
/// MetalFX validates the textures it is handed against the *descriptor's* dimensions, so `texture`
/// must be the allocation size of Bevy's view and prepass textures — not the reduced size the main
/// pass draws into. The reduced size is communicated per frame through `inputContentWidth`/
/// `inputContentHeight`, which is what `inputContentPropertiesEnabled` turns on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct ScalerResolutions {
    /// Allocation size of the color, depth and motion textures.
    pub texture: UVec2,
    /// Size of the reconstructed image.
    pub output: UVec2,
}

/// Everything the scaler was configured with. Any change means a new scaler.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct ScalerSetup {
    pub resolutions: ScalerResolutions,
    pub color_format: TextureFormat,
    pub frame_generation: bool,
}

#[derive(Component)]
pub(super) struct MetalFxContext {
    scaler: Retained<ProtocolObject<dyn MTLFXTemporalScaler>>,
    /// `Some` only when the interpolator *and* its history textures exist, so the node can never
    /// see one without the other.
    frame_generation: Option<FrameGeneration>,
    /// Consumed by the node; starts set so a freshly created scaler never reconstructs from
    /// uninitialized history.
    pending_reset: AtomicBool,
    setup: ScalerSetup,
    /// The scale range baked into the descriptor. Factors outside it need a new scaler.
    supported_scale: (f32, f32),
}

pub(super) struct FrameGeneration {
    pub interpolator: Retained<ProtocolObject<dyn MTLFXFrameInterpolator>>,
    pub current_color: Texture,
    pub previous_color: Texture,
    pub history_valid: AtomicBool,
}

// SAFETY: the Metal objects are only touched from the render world, which owns the component and
// never accesses it from two threads at once.
unsafe impl Send for MetalFxContext {}
unsafe impl Sync for MetalFxContext {}

impl MetalFxContext {
    pub(super) fn new(render_device: &RenderDevice, setup: ScalerSetup) -> Option<Self> {
        let color_format = metal_pixel_format(setup.color_format)?;
        let motion_format = metal_pixel_format(MOTION_VECTOR_PREPASS_FORMAT)?;
        let depth_format = metal_pixel_format(CORE_3D_DEPTH_FORMAT)?;
        let ScalerResolutions { texture, output } = setup.resolutions;

        let (scaler, frame_generation, supported_scale) = unsafe {
            let device_guard = render_device
                .wgpu_device()
                .as_hal::<wgpu::hal::api::Metal>()?;
            let device = device_guard.raw_device().as_ref();

            if !objc_class_exists(b"MTLFXTemporalScalerDescriptor\0")
                || !MTLFXTemporalScalerDescriptor::supportsDevice(device)
            {
                return None;
            }

            let min_scale =
                MTLFXTemporalScalerDescriptor::supportedInputContentMinScaleForDevice(device);
            let max_scale =
                MTLFXTemporalScalerDescriptor::supportedInputContentMaxScaleForDevice(device);

            let descriptor = MTLFXTemporalScalerDescriptor::new();
            descriptor.setColorTextureFormat(color_format);
            descriptor.setOutputTextureFormat(color_format);
            descriptor.setDepthTextureFormat(depth_format);
            descriptor.setMotionTextureFormat(motion_format);
            // Describe the *textures* Bevy hands us, then narrow to the rendered sub-rect per frame.
            descriptor.setInputWidth(texture.x as _);
            descriptor.setInputHeight(texture.y as _);
            descriptor.setOutputWidth(output.x as _);
            descriptor.setOutputHeight(output.y as _);
            descriptor.setInputContentPropertiesEnabled(true);
            descriptor.setInputContentMinScale(min_scale);
            descriptor.setInputContentMaxScale(max_scale);
            descriptor.setAutoExposureEnabled(true);
            descriptor.setRequiresSynchronousInitialization(false);

            let scaler = descriptor.newTemporalScalerWithDevice(device)?;

            let interpolator = setup
                .frame_generation
                .then(|| {
                    (objc_class_exists(b"MTLFXFrameInterpolatorDescriptor\0")
                        && MTLFXFrameInterpolatorDescriptor::supportsDevice(device))
                    .then(|| {
                        let descriptor = MTLFXFrameInterpolatorDescriptor::new();
                        descriptor.setColorTextureFormat(color_format);
                        descriptor.setOutputTextureFormat(color_format);
                        descriptor.setDepthTextureFormat(depth_format);
                        descriptor.setMotionTextureFormat(motion_format);
                        descriptor.setInputWidth(output.x as _);
                        descriptor.setInputHeight(output.y as _);
                        descriptor.setOutputWidth(output.x as _);
                        descriptor.setOutputHeight(output.y as _);
                        // Telling the interpolator about the scaler is what lets it accept depth
                        // and motion at the scaler's (lower) input resolution.
                        let temporal: &ProtocolObject<dyn MTLFXTemporalScaler> = scaler.as_ref();
                        let interpolatable: &ProtocolObject<dyn MTLFXFrameInterpolatableScaler> =
                            temporal.as_ref();
                        descriptor.setScaler(Some(interpolatable));
                        descriptor.newFrameInterpolatorWithDevice(device)
                    })
                    .flatten()
                })
                .flatten();

            (scaler, interpolator, (min_scale, max_scale))
        };

        if setup.frame_generation && frame_generation.is_none() {
            warn_once!(
                "MetalFX frame interpolation is unavailable on this system (needs macOS 26 or \
                 newer); using temporal upscaling only"
            );
        }

        let frame_generation = frame_generation.map(|interpolator| FrameGeneration {
            interpolator,
            current_color: history_texture(
                render_device,
                output,
                setup.color_format,
                "metalfx_frame_generation_current_color",
            ),
            previous_color: history_texture(
                render_device,
                output,
                setup.color_format,
                "metalfx_frame_generation_previous_color",
            ),
            history_valid: AtomicBool::new(false),
        });

        Some(Self {
            scaler,
            frame_generation,
            pending_reset: AtomicBool::new(true),
            setup,
            supported_scale,
        })
    }

    pub(super) fn scaler(&self) -> &ProtocolObject<dyn MTLFXTemporalScaler> {
        &self.scaler
    }

    pub(super) fn frame_generation(&self) -> Option<&FrameGeneration> {
        self.frame_generation.as_ref()
    }

    pub(super) fn output_resolution(&self) -> UVec2 {
        self.setup.resolutions.output
    }

    /// True when the current setup still matches, i.e. the scaler can be reused.
    pub(super) fn matches(&self, setup: &ScalerSetup, factor: f32) -> bool {
        self.setup == *setup && (self.supported_scale.0..=self.supported_scale.1).contains(&factor)
    }

    pub(super) fn request_reset(&self) {
        self.pending_reset.store(true, Ordering::Relaxed);
    }

    /// Reads and clears the pending reset. Also invalidates frame-generation history, which is
    /// meaningless once the reconstruction restarts.
    pub(super) fn take_reset(&self) -> bool {
        let reset = self.pending_reset.swap(false, Ordering::Relaxed);
        if reset && let Some(frame_generation) = &self.frame_generation {
            frame_generation
                .history_valid
                .store(false, Ordering::Relaxed);
        }
        reset
    }
}

fn history_texture(
    render_device: &RenderDevice,
    resolution: UVec2,
    format: TextureFormat,
    label: &'static str,
) -> Texture {
    render_device.create_texture(&TextureDescriptor {
        label: Some(label),
        size: bevy::render::render_resource::Extent3d {
            width: resolution.x,
            height: resolution.y,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::STORAGE_BINDING
            | TextureUsages::TEXTURE_BINDING
            | TextureUsages::COPY_SRC
            | TextureUsages::COPY_DST,
        view_formats: &[],
    })
}
