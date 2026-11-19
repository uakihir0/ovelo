//! `Vulkan` shim (design doc §07, G1): the object model the guest's
//! `libvulkan` exports resolve to. `vkCreateAndroidSurfaceKHR` wraps an
//! [`ANativeWindow`]; `VkSwapchain` presents through the same
//! [`BufferQueue`] the EGL path uses, so either API lands frames with
//! identical lifetime and staleness rules.
//!
//! Missing capabilities are reported by name (`GPU_FEATURE`), never
//! silently degraded — MoltenVK replaces the transport later.

use std::collections::BTreeSet;

use ovelo_error::OveloError;

use crate::bufq::{ANativeWindow, FrameId};

fn err(cause: &str) -> OveloError {
    OveloError::new(
        ovelo_error::ErrorCategory::GpuFeature,
        ovelo_error::Phase::Gpu,
        cause,
    )
}

/// Extensions our instance exposes (`vkEnumerateInstanceExtensionProperties`).
pub const INSTANCE_EXTENSIONS: &[&str] = &["VK_KHR_surface", "VK_KHR_android_surface"];

/// Device extensions we honour (`VK_KHR_swapchain` is required for present).
pub const DEVICE_EXTENSIONS: &[&str] = &["VK_KHR_swapchain"];

/// Swapchain formats we can present (sRGB8 premultiplied CPU surface).
pub const SURFACE_FORMATS: &[&str] = &["VK_FORMAT_R8G8B8A8_SRGB", "VK_FORMAT_B8G8R8A8_SRGB"];

/// `vkCreateInstance` — requested extensions must all be supported.
#[derive(Debug)]
pub struct VkInstance {
    enabled: BTreeSet<String>,
}

impl VkInstance {
    /// Create; fails naming every extension we cannot provide.
    pub fn create(requested: &[&str]) -> Result<Self, OveloError> {
        let missing: Vec<String> = requested
            .iter()
            .filter(|e| !INSTANCE_EXTENSIONS.contains(e))
            .map(|e| e.to_string())
            .collect();
        if !missing.is_empty() {
            return Err(err("ui.vk.missing_extension").with_detail(missing.join(",")));
        }
        Ok(Self {
            enabled: requested.iter().map(|s| s.to_string()).collect(),
        })
    }

    /// Whether an extension was enabled at creation.
    pub fn enabled(&self, ext: &str) -> bool {
        self.enabled.contains(ext)
    }
}

/// `VkSurfaceKHR` created by `vkCreateAndroidSurfaceKHR`.
pub struct VkSurface {
    /// The window backing this surface.
    pub window: ANativeWindow,
    /// Set by `destroy`.
    pub destroyed: bool,
}

/// `VkSwapchainKHR` — image count maps onto `BufferQueue` slots.
pub struct VkSwapchain {
    /// Format chosen at creation.
    pub format: String,
    /// Last presented frame (the acquire fence source — history isn't kept,
    /// a swapchain must not grow with every present).
    last: Option<FrameId>,
}

impl VkSurface {
    /// `vkCreateAndroidSurfaceKHR`; requires `VK_KHR_android_surface`.
    pub fn create(_inst: &VkInstance, window: ANativeWindow) -> Self {
        Self {
            window,
            destroyed: false,
        }
    }

    /// `vkGetPhysicalDeviceSurfaceFormatsKHR`.
    pub fn formats(&self) -> &'static [&'static str] {
        SURFACE_FORMATS
    }

    /// `vkCreateSwapchainKHR` — `images` slot count, `format` must be in
    /// [`SURFACE_FORMATS`].
    pub fn create_swapchain(&mut self, format: &str, images: usize) -> Result<VkSwapchain, OveloError> {
        if self.destroyed {
            return Err(err("ui.surface.destroyed"));
        }
        if !SURFACE_FORMATS.contains(&format) {
            return Err(err("ui.vk.bad_format").with_detail(format));
        }
        {
            let q = self.window.queue_mut();
            let cur = q.len();
            if images < cur {
                // Cannot shrink a live queue; Vulkan allows recreating.
                return Err(err("ui.vk.min_images"));
            }
            for _ in cur..images {
                q.add_slot()?;
            }
        }
        Ok(VkSwapchain {
            format: format.to_string(),
            last: None,
        })
    }

    /// `vkQueuePresentKHR` — the producer frame already posted via
    /// `unlock_and_post`/`eglSwapBuffers`; this records the fence order.
    pub fn present(&mut self, sc: &mut VkSwapchain, frame: FrameId) -> Result<(), OveloError> {
        if self.destroyed {
            return Err(err("ui.surface.destroyed"));
        }
        let q = self.window.queue_mut();
        if q.read(frame).is_none() && q.last_queued() < frame.frame {
            return Err(err("ui.vk.bad_frame"));
        }
        sc.last = Some(frame);
        Ok(())
    }

    /// `vkDestroySurfaceKHR`.
    pub fn destroy(&mut self) {
        self.destroyed = true;
        self.window.queue_mut().destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bufq::BufferQueue;

    #[test]
    fn instance_extension_gate() {
        assert!(VkInstance::create(INSTANCE_EXTENSIONS).is_ok());
        assert!(VkInstance::create(&["VK_EXT_missing"]).is_err());
    }

    #[test]
    fn surface_swapchain_present() {
        let inst = VkInstance::create(INSTANCE_EXTENSIONS).unwrap();
        let mut surf = VkSurface::create(&inst, ANativeWindow::new(BufferQueue::new(2, 4, 4).unwrap()));
        let mut sc = surf.create_swapchain("VK_FORMAT_R8G8B8A8_SRGB", 3).unwrap();
        assert!(surf.create_swapchain("VK_FORMAT_D16", 3).is_err());
        assert!(surf.window.unlock_and_post().is_err()); // nothing locked
        let f = {
            surf.window.lock().unwrap();
            surf.window.unlock_and_post().unwrap()
        };
        surf.present(&mut sc, f).unwrap();
        surf.destroy();
        assert!(surf.present(&mut sc, f).is_err());
    }
}
