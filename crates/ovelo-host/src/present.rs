//! Guest→display presentation path (design doc §02 + G1): bridges
//! [`ovelo_ui::BufferQueue`] into the [`SurfaceRegistry`] frame contract.
//!
//! The guest renders through an [`ANativeWindow`] (EGL `eglSwapBuffers` or
//! Vulkan `vkQueuePresentKHR`); each vsync [`Presenter::display`] latches the
//! newest queued frame, wraps its pixels as a [`Frame`], runs the registry's
//! contract gate, then releases the buffer back to the producer. Generations
//! stay in lockstep: [`Presenter::resize`] bumps both the queue and the
//! registry so stale frames die at whichever side sees them first.

use std::collections::HashMap;

use ovelo_error::{ErrorCategory, OveloError, Phase, Result};
use ovelo_ui::ANativeWindow;

use crate::frame::{ColorSpace, Frame, Presented, SurfaceRegistry};

fn err(cause: &str) -> OveloError {
    OveloError::new(ErrorCategory::GpuFeature, Phase::Gpu, cause)
}

/// One composited layer (a `SurfaceControl` position + z-order entry).
#[derive(Debug, Clone, Copy)]
pub struct Layer {
    /// Guest surface to read.
    pub surface_id: u64,
    /// Left offset in output px.
    pub x: i32,
    /// Top offset in output px.
    pub y: i32,
}

/// Compositor-side owner of every guest surface.
#[derive(Default)]
pub struct Presenter {
    registry: SurfaceRegistry,
    /// surface_id → the window the guest produces into.
    windows: HashMap<u64, ANativeWindow>,
}

impl Presenter {
    /// Empty presenter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a guest surface: creates the window (queue of `slots`
    /// buffers at `w`×`h`) and opens the registry entry. Returns the
    /// surface id the guest uses.
    pub fn attach(&mut self, slots: usize, w: usize, h: usize) -> Result<u64> {
        let id = self.windows.keys().max().map(|m| m + 1).unwrap_or(1);
        let q = ovelo_ui::BufferQueue::new(slots, w, h)?;
        self.windows.insert(id, ANativeWindow::new(q));
        self.registry.create(id);
        Ok(id)
    }

    /// Producer side for the guest (`eglCreateWindowSurface` /
    /// `vkCreateAndroidSurfaceKHR` take this handle).
    pub fn producer(&mut self, surface_id: u64) -> Result<&mut ANativeWindow> {
        self.windows
            .get_mut(&surface_id)
            .ok_or_else(|| err("host.surface.unknown"))
    }

    /// Geometry change: bump both generations — queued stale frames drop in
    /// the queue, in-flight frames drop at the registry gate.
    pub fn resize(&mut self, surface_id: u64, w: usize, h: usize) -> Result<u64> {
        let win = self
            .windows
            .get_mut(&surface_id)
            .ok_or_else(|| err("host.surface.unknown"))?;
        let gen = win.queue_mut().resize(w, h)?;
        self.registry.bump_generation(surface_id)?;
        Ok(gen)
    }

    /// One vsync: latch the newest frame the guest posted, gate it through
    /// the registry, recycle the buffer. `None` when nothing was posted.
    pub fn display(&mut self, surface_id: u64) -> Result<Option<Presented>> {
        let win = self
            .windows
            .get_mut(&surface_id)
            .ok_or_else(|| err("host.surface.unknown"))?;
        let Some(id) = win.queue_mut().acquire() else {
            return Ok(None);
        };
        let frame = {
            let px = win
                .queue()
                .read(id)
                .ok_or_else(|| err("host.frame.missing"))?;
            Frame {
                surface_id,
                generation: id.generation,
                size: (px.width() as u32, px.height() as u32),
                color_space: ColorSpace::SrgbPremul,
                premultiplied: true,
                buffer: px.pixels().to_vec(),
            }
        };
        win.queue_mut().release(id)?;
        Ok(Some(self.registry.present(frame)?))
    }

    /// Multi-layer composition (P3): composite the newest frame of each
    /// `layers` entry — bottom→top order, each at its (x,y) offset — onto
    /// the output surface's geometry, then run it through the registry
    /// like [`Presenter::display`]. Layers with nothing queued contribute
    /// nothing; unreleased buffers always recycle.
    pub fn display_layers(
        &mut self,
        output_id: u64,
        layers: &[Layer],
    ) -> Result<Option<Presented>> {
        let (w, h) = {
            let win = self
                .windows
                .get(&output_id)
                .ok_or_else(|| err("host.surface.unknown"))?;
            (win.queue().width(), win.queue().height())
        };
        let mut out = ovelo_ui::Surface::new(w, h)?;
        let mut any = false;
        for l in layers {
            // A dead worker's layer contributes nothing — the registry gate
            // covers the output surface, each layer must check its own.
            if !self.registry.is_alive(l.surface_id) {
                continue;
            }
            let win = self
                .windows
                .get_mut(&l.surface_id)
                .ok_or_else(|| err("host.surface.unknown"))?;
            let Some(id) = win.queue_mut().acquire() else {
                continue;
            };
            {
                let px = win
                    .queue()
                    .read(id)
                    .ok_or_else(|| err("host.frame.missing"))?;
                out.blit(l.x, l.y, px.width(), px.height(), px.pixels())?;
            }
            win.queue_mut().release(id)?;
            any = true;
        }
        if !any {
            return Ok(None);
        }
        Ok(Some(self.registry.present(Frame {
            surface_id: output_id,
            generation: self
                .windows
                .get(&output_id)
                .unwrap()
                .queue()
                .generation(),
            size: (w as u32, h as u32),
            color_space: ColorSpace::SrgbPremul,
            premultiplied: true,
            buffer: out.pixels().to_vec(),
        })?))
    }

    /// The guest app died: no frame of its surface may display again.
    pub fn stop_worker(&mut self, surface_id: u64) {
        self.registry.stop_worker(surface_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_posts_host_displays() {
        let mut p = Presenter::new();
        let s = p.attach(2, 4, 4).unwrap();
        assert!(p.display(s).unwrap().is_none());
        {
            let w = p.producer(s).unwrap();
            let buf = w.lock().unwrap();
            buf.fill_rect(
                ovelo_ui::Rect::new(0, 0, 4, 4),
                ovelo_ui::Color::srgb(9, 9, 9),
            );
            w.unlock_and_post().unwrap();
        }
        let pr = p.display(s).unwrap().unwrap();
        assert_eq!(pr.frame.size, (4, 4));
        assert_eq!(pr.frame.buffer[0], 9); // opaque premultiplied sRGB
        // Buffer recycled: producer can draw again.
        p.producer(s).unwrap().lock().unwrap();
    }

    #[test]
    fn resize_kills_inflight() {
        let mut p = Presenter::new();
        let s = p.attach(2, 4, 4).unwrap();
        p.producer(s).unwrap().lock().unwrap();
        p.producer(s).unwrap().unlock_and_post().unwrap();
        p.resize(s, 8, 8).unwrap();
        assert!(p.display(s).unwrap().is_none()); // stale frame dropped
    }

    #[test]
    fn layers_compose_in_order() {
        let mut p = Presenter::new();
        let out = p.attach(2, 4, 4).unwrap();
        let top = p.attach(2, 2, 2).unwrap();
        {
            let w = p.producer(top).unwrap();
            w.lock()
                .unwrap()
                .fill_rect(ovelo_ui::Rect::new(0, 0, 2, 2), ovelo_ui::Color::srgb(200, 0, 0));
            w.unlock_and_post().unwrap();
        }
        let pr = p
            .display_layers(
                out,
                &[Layer {
                    surface_id: top,
                    x: 1,
                    y: 1,
                }],
            )
            .unwrap()
            .unwrap();
        // top layer's red pixel landed at (1,1) of the 4x4 output.
        assert_eq!(pr.frame.buffer[(4 + 1) * 4], 200);
    }

    #[test]
    fn dead_worker_shows_nothing() {
        let mut p = Presenter::new();
        let s = p.attach(2, 4, 4).unwrap();
        p.producer(s).unwrap().lock().unwrap();
        p.producer(s).unwrap().unlock_and_post().unwrap();
        p.stop_worker(s);
        assert!(p.display(s).is_err());
    }
}
