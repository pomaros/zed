// SPDX-License-Identifier: MIT
// Copyright (c) 2026 John Knipper

//! Window rendering and native surface lifetime. Portable CPU conformance
//! remains in renderer.rs; GPU scenes use GPUI's existing WGPU renderer.
use crate::{glue, renderer::CpuRenderer};
use gpui::{DevicePixels, GpuSpecs, PlatformAtlas, Scene, Size};
use std::{ffi::c_void, sync::Arc};

pub(crate) enum WindowRenderer {
    Cpu(CpuRenderer),
    #[cfg(feature = "native-gpu")]
    Gpu {
        renderer: gpui_wgpu::WgpuRenderer,
        first_frame_submitted: bool,
    },
}

impl WindowRenderer {
    /// The owner must destroy this renderer before closing the native window.
    pub(crate) fn new(handle: *mut c_void, size: Size<DevicePixels>) -> anyhow::Result<Self> {
        #[cfg(feature = "native-gpu")]
        {
            let native = unsafe { glue::gpa_native_window(handle) };
            let native = std::ptr::NonNull::new(native)
                .ok_or_else(|| anyhow::anyhow!("Intuition window is unavailable"))?;
            let window = NativeWindowHandle(native);
            let renderer = gpui_wgpu::WgpuRenderer::new(
                Default::default(),
                &window,
                gpui_wgpu::WgpuSurfaceConfig {
                    size,
                    transparent: false,
                    preferred_present_mode: None,
                },
                None,
            )?;
            let specs = renderer
                .gpu_specs()
                .ok_or_else(|| anyhow::anyhow!("GPU adapter identity is unavailable"))?;
            if specs.is_software_emulated {
                anyhow::bail!(
                    "native-gpu selected a software adapter: {}",
                    specs.device_name
                );
            }
            log::info!(
                "AROS GPU renderer: {} ({})",
                specs.device_name,
                specs.driver_name
            );
            return Ok(Self::Gpu {
                renderer,
                first_frame_submitted: false,
            });
        }
        #[cfg(not(feature = "native-gpu"))]
        {
            let _ = handle;
            Ok(Self::Cpu(CpuRenderer::new(size)))
        }
    }

    pub(crate) fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        match self {
            Self::Cpu(cpu) => cpu.sprite_atlas(),
            #[cfg(feature = "native-gpu")]
            Self::Gpu { renderer: gpu, .. } => gpu.sprite_atlas().clone(),
        }
    }

    pub(crate) fn invalidate(&mut self) {
        match self {
            Self::Cpu(cpu) => cpu.invalidate(),
            // GPU frames render the complete scene, including refresh events.
            #[cfg(feature = "native-gpu")]
            Self::Gpu { .. } => (),
        }
    }

    pub(crate) fn update_drawable_size(&mut self, size: Size<DevicePixels>) {
        match self {
            Self::Cpu(cpu) => cpu.update_drawable_size(size),
            #[cfg(feature = "native-gpu")]
            Self::Gpu { renderer: gpu, .. } => gpu.update_drawable_size(size),
        }
    }

    pub(crate) fn gpu_specs(&self) -> Option<GpuSpecs> {
        match self {
            Self::Cpu(_) => Some(GpuSpecs {
                is_software_emulated: true,
                device_name: "AROS CPU".into(),
                driver_name: "tiny-skia".into(),
                driver_info: String::new(),
            }),
            #[cfg(feature = "native-gpu")]
            Self::Gpu { renderer: gpu, .. } => gpu.gpu_specs(),
        }
    }

    pub(crate) fn draw(&mut self, handle: *mut c_void, scene: &Scene) {
        match self {
            Self::Cpu(cpu) => {
                let started = std::time::Instant::now();
                let Some(damage) = cpu.draw(scene) else {
                    crate::stats::skipped();
                    return;
                };
                let raster = started.elapsed();
                let stride = cpu.width() as i32 * 4;
                let started = std::time::Instant::now();
                unsafe {
                    glue::gpa_blit(
                        handle,
                        cpu.framebuffer().as_ptr().cast(),
                        stride,
                        damage.x0,
                        damage.y0,
                        damage.x1 - damage.x0,
                        damage.y1 - damage.y0,
                    );
                }
                crate::stats::painted(raster, started.elapsed(), damage);
            }
            #[cfg(feature = "native-gpu")]
            Self::Gpu {
                renderer: gpu,
                first_frame_submitted,
            } => {
                if gpu.draw(scene) {
                    if !*first_frame_submitted {
                        log::info!("AROS GPU first scene submitted");
                        *first_frame_submitted = true;
                    }
                } else {
                    log::error!("AROS GPU scene was not presented");
                }
            }
        }
    }
}

#[cfg(feature = "native-gpu")]
#[derive(Clone, Debug)]
struct NativeWindowHandle(std::ptr::NonNull<c_void>);

// These wrappers only copy native handle values, never access window memory.
// Intuition owns synchronization of its windows. The UI owner keeps the window
// alive until WindowRenderer is dropped, before gpa_close_window (window.rs).
#[cfg(feature = "native-gpu")]
unsafe impl Send for NativeWindowHandle {}
#[cfg(feature = "native-gpu")]
unsafe impl Sync for NativeWindowHandle {}

#[cfg(feature = "native-gpu")]
impl raw_window_handle::HasWindowHandle for NativeWindowHandle {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        Ok(unsafe {
            raw_window_handle::WindowHandle::borrow_raw(
                raw_window_handle::ArosWindowHandle::new(self.0).into(),
            )
        })
    }
}
#[cfg(feature = "native-gpu")]
impl raw_window_handle::HasDisplayHandle for NativeWindowHandle {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(unsafe {
            raw_window_handle::DisplayHandle::borrow_raw(
                raw_window_handle::ArosDisplayHandle::new().into(),
            )
        })
    }
}
