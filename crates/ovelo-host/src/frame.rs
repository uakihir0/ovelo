//! Worker→Display frame contract (design doc §02).
//!
//! A worker submits `Frame`s tagged with a surface id and a monotonically
//! increasing generation. The registry displays a frame only when the worker
//! that owns the surface is alive AND the frame is complete (size and buffer
//! consistent); a resize bumps the current generation so stale frames are
//! dropped instead of being presented late.

use ovelo_error::{ErrorCategory, OveloError, Phase, Result};
use std::collections::HashMap;

fn err(cause: &str) -> OveloError {
    OveloError::new(ErrorCategory::GpuFeature, Phase::Gpu, cause)
}

/// Frame buffer color space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpace {
    /// Premultiplied sRGB (the surface's native space).
    SrgbPremul,
    /// Display P3, premultiplied.
    DisplayP3Premul,
}

/// A complete frame submitted by a worker for presentation.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Surface this frame targets.
    pub surface_id: u64,
    /// Worker generation when the frame was produced.
    pub generation: u64,
    /// Pixel size (px).
    pub size: (u32, u32),
    /// Buffer color space.
    pub color_space: ColorSpace,
    /// Whether alpha is premultiplied (must match `color_space`).
    pub premultiplied: bool,
    /// Premultiplied RGBA8 pixels, `w*h*4` bytes.
    pub buffer: Vec<u8>,
}

impl Frame {
    /// Buffer consistent with declared size?
    pub fn complete(&self) -> bool {
        self.buffer.len() == self.size.0 as usize * self.size.1 as usize * 4
    }
}

/// Per-surface state: owning worker liveness + accepted generation.
struct SurfaceState {
    alive: bool,
    generation: u64,
}

/// Tracks surfaces and gates presentation (§02 contract).
#[derive(Default)]
pub struct SurfaceRegistry {
    surfaces: HashMap<u64, SurfaceState>,
}

impl SurfaceRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a surface owned by a live worker; returns its first generation.
    pub fn create(&mut self, surface_id: u64) -> u64 {
        self.surfaces.insert(
            surface_id,
            SurfaceState {
                alive: true,
                generation: 0,
            },
        );
        0
    }

    /// Whether the surface's worker is alive (layers gate on this too).
    pub fn is_alive(&self, surface_id: u64) -> bool {
        self.surfaces
            .get(&surface_id)
            .map(|s| s.alive)
            .unwrap_or(false)
    }

    /// Worker stopped/died: its frames must never display again.
    pub fn stop_worker(&mut self, surface_id: u64) {
        if let Some(s) = self.surfaces.get_mut(&surface_id) {
            s.alive = false;
        }
    }

    /// Resize/orientation change: bump the generation so in-flight frames
    /// produced for the old geometry are dropped.
    pub fn bump_generation(&mut self, surface_id: u64) -> Result<u64> {
        let s = self
            .surfaces
            .get_mut(&surface_id)
            .ok_or_else(|| err("host.surface.unknown"))?;
        s.generation += 1;
        Ok(s.generation)
    }

    /// Gate check + accept: only live workers' complete, current-generation
    /// frames are handed to presentation.
    pub fn present(&mut self, frame: Frame) -> Result<Presented> {
        let s = self
            .surfaces
            .get(&frame.surface_id)
            .ok_or_else(|| err("host.surface.unknown"))?;
        if !s.alive {
            return Err(err("host.worker.stopped"));
        }
        if !frame.complete() {
            return Err(err("host.frame.incomplete"));
        }
        if frame.generation != s.generation {
            return Err(err("host.frame.stale"));
        }
        // TODO(Gpu): hand `frame` to the CAMetalLayer drawable pipeline
        // (§02 Worker→Display). For now we accept and surface the pixels.
        Ok(Presented { frame })
    }
}

/// A frame that passed the contract gate.
#[derive(Debug)]
pub struct Presented {
    /// The accepted frame (pixels + metadata).
    pub frame: Frame,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(gen: u64, w: u32, h: u32) -> Frame {
        Frame {
            surface_id: 1,
            generation: gen,
            size: (w, h),
            color_space: ColorSpace::SrgbPremul,
            premultiplied: true,
            buffer: vec![7; (w * h * 4) as usize],
        }
    }

    #[test]
    fn contract_gates() {
        let mut r = SurfaceRegistry::new();
        let gen = r.create(1);
        assert_eq!(r.present(frame(gen, 2, 2)).unwrap().frame.size, (2, 2));
        // resize bumps generation → old gen dropped as stale
        r.bump_generation(1).unwrap();
        assert_eq!(
            r.present(frame(gen, 2, 2)).unwrap_err().cause(),
            "host.frame.stale"
        );
        // incomplete frame rejected
        let mut bad = frame(1, 4, 4);
        bad.buffer.truncate(10);
        assert_eq!(r.present(bad).unwrap_err().cause(), "host.frame.incomplete");
        // stopped worker never displays
        r.stop_worker(1);
        assert_eq!(
            r.present(frame(1, 2, 2)).unwrap_err().cause(),
            "host.worker.stopped"
        );
        // unknown surface rejected
        assert_eq!(
            r.present(Frame {
                surface_id: 99,
                ..frame(0, 1, 1)
            })
            .unwrap_err()
            .cause(),
            "host.surface.unknown"
        );
    }
}
