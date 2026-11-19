//! `BufferQueue` + `ANativeWindow` (design doc §07/§08 surface lifetime).
//!
//! Producer (the guest/GLES) dequeues a free slot, renders into it, and
//! queues it; the consumer (host compositor) acquires it, displays it, and
//! releases it back. Rules enforced here mirror BufferQueue semantics:
//! display only after writes complete (`queue` moves a slot out of
//! `FREE`), and buffers are reusable only after reads complete
//! (`release` returns a slot to `FREE`). Surface generation mirrors the
//! host's registry: a resize bumps it and stale queued frames are
//! discarded instead of shown.

use crate::surface::Surface;
use ovelo_error::OveloError;

fn err(cause: &str) -> OveloError {
    OveloError::new(
        ovelo_error::ErrorCategory::GpuFeature,
        ovelo_error::Phase::Gpu,
        cause,
    )
}

const MAX_SLOTS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// Writable by the producer.
    Free,
    /// Checked out by the producer (writes in flight).
    Dequeued,
    /// Written; waiting for the consumer.
    Queued,
    /// Checked out by the consumer (display/read in flight).
    Acquired,
}

/// One buffer slot's lifecycle record.
struct Entry {
    state: Slot,
    buf: Surface,
    /// Generation the queued frame was produced in.
    generation: u64,
    /// Frame counter for staleness ordering.
    frame: u64,
}

/// Producer/consumer queue for one surface.
pub struct BufferQueue {
    slots: Vec<Entry>,
    /// Current accepted generation; resize/orientation bumps it.
    generation: u64,
    /// Monotonic frame counter.
    frame_counter: u64,
    /// Last queued frame's number (for `last_queued`).
    queued_frame: u64,
    /// Set when the owning surface is destroyed — everything fails.
    destroyed: bool,
}

/// A queued frame's identity (what the consumer picks up).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameId {
    /// Slot index.
    pub slot: usize,
    /// Frame sequence number.
    pub frame: u64,
    /// Producer generation.
    pub generation: u64,
}

impl BufferQueue {
    /// A queue with `n` slots of `w`×`h` pixels (2–8).
    pub fn new(n: usize, w: usize, h: usize) -> Result<Self, OveloError> {
        if !(1..=MAX_SLOTS).contains(&n) {
            return Err(err("ui.bufq.slots"));
        }
        let mut slots = Vec::with_capacity(n);
        for _ in 0..n {
            slots.push(Entry {
                state: Slot::Free,
                buf: Surface::new(w, h)?,
                generation: 0,
                frame: 0,
            });
        }
        Ok(Self {
            slots,
            generation: 0,
            frame_counter: 0,
            queued_frame: 0,
            destroyed: false,
        })
    }

    /// Current generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Resize/orientation change: bump the generation and drop queued
    /// frames produced for the old geometry (stale frames never show).
    /// Returns the new generation.
    pub fn resize(&mut self, w: usize, h: usize) -> Result<u64, OveloError> {
        if self.destroyed {
            return Err(err("ui.surface.destroyed"));
        }
        self.generation += 1;
        for e in &mut self.slots {
            match e.state {
                Slot::Queued => e.state = Slot::Free, // drop stale frame
                Slot::Acquired => {}                  // consumer keeps it until release
                _ => {}
            }
            if e.state == Slot::Free || e.state == Slot::Dequeued {
                e.buf = Surface::new(w, h)?;
            }
        }
        Ok(self.generation)
    }

    /// Producer: check out a free slot for writing. Returns the slot index.
    pub fn dequeue(&mut self) -> Result<usize, OveloError> {
        if self.destroyed {
            return Err(err("ui.surface.destroyed"));
        }
        let i = self
            .slots
            .iter()
            .position(|e| e.state == Slot::Free)
            .ok_or_else(|| err("ui.bufq.no_free"))?;
        self.slots[i].state = Slot::Dequeued;
        Ok(i)
    }

    /// Producer: borrow the slot's buffer for rendering.
    pub fn buffer_mut(&mut self, slot: usize) -> Result<&mut Surface, OveloError> {
        let e = self.slots.get_mut(slot).ok_or_else(|| err("ui.bufq.slot"))?;
        if e.state != Slot::Dequeued {
            return Err(err("ui.bufq.not_dequeued"));
        }
        Ok(&mut e.buf)
    }

    /// Producer: writing finished — hand the frame to the consumer.
    /// Frames produced in an older generation are discarded instead.
    pub fn queue(&mut self, slot: usize) -> Result<FrameId, OveloError> {
        let e = self.slots.get_mut(slot).ok_or_else(|| err("ui.bufq.slot"))?;
        if e.state != Slot::Dequeued {
            return Err(err("ui.bufq.not_dequeued"));
        }
        self.frame_counter += 1;
        let id = FrameId {
            slot,
            frame: self.frame_counter,
            generation: self.generation,
        };
        e.generation = self.generation;
        e.frame = self.frame_counter;
        e.state = Slot::Queued;
        self.queued_frame = self.frame_counter;
        Ok(id)
    }

    /// Producer: cancel a checked-out slot (no frame produced).
    pub fn cancel(&mut self, slot: usize) -> Result<(), OveloError> {
        let e = self.slots.get_mut(slot).ok_or_else(|| err("ui.bufq.slot"))?;
        if e.state != Slot::Dequeued {
            return Err(err("ui.bufq.not_dequeued"));
        }
        e.state = Slot::Free;
        Ok(())
    }

    /// Consumer: take the newest queued frame for display. Older queued
    /// frames are dropped (only the latest is shown, like the compositor's
    /// latch). Returns `None` when nothing is queued.
    pub fn acquire(&mut self) -> Option<FrameId> {
        if self.destroyed {
            return None;
        }
        let mut best: Option<usize> = None;
        for (i, e) in self.slots.iter().enumerate() {
            if e.state == Slot::Queued && best.is_none_or(|b| e.frame > self.slots[b].frame) {
                best = Some(i);
            }
        }
        let i = best?;
        // Free every older queued frame — it is stale by definition.
        for (j, e) in self.slots.iter_mut().enumerate() {
            if e.state == Slot::Queued && j != i {
                e.state = Slot::Free;
            }
        }
        let e = &mut self.slots[i];
        e.state = Slot::Acquired;
        Some(FrameId {
            slot: i,
            frame: e.frame,
            generation: e.generation,
        })
    }

    /// Consumer: read the acquired slot's pixels.
    pub fn read(&self, id: FrameId) -> Option<&Surface> {
        let e = self.slots.get(id.slot)?;
        (e.state == Slot::Acquired && e.frame == id.frame).then_some(&e.buf)
    }

    /// Consumer: reading finished — the slot is reusable by the producer.
    pub fn release(&mut self, id: FrameId) -> Result<(), OveloError> {
        let e = self.slots.get_mut(id.slot).ok_or_else(|| err("ui.bufq.slot"))?;
        if e.state != Slot::Acquired || e.frame != id.frame {
            return Err(err("ui.bufq.not_acquired"));
        }
        e.state = Slot::Free;
        Ok(())
    }

    /// The last queued frame number (frame callback ordering).
    pub fn last_queued(&self) -> u64 {
        self.queued_frame
    }

    /// `Surface.destroy()` — all further calls fail.
    pub fn destroy(&mut self) {
        self.destroyed = true;
    }

    /// Whether the surface is destroyed.
    pub fn is_destroyed(&self) -> bool {
        self.destroyed
    }
}

/// `ANativeWindow` — the producer-side handle the guest renders through.
/// Locks a buffer (`ANativeWindow_lock`), writes, and unlocks+posts
/// (`ANativeWindow_unlockAndPost`).
pub struct ANativeWindow {
    queue: BufferQueue,
    /// Slot held between lock and unlockAndPost.
    held: Option<usize>,
}

impl ANativeWindow {
    /// Wrap a queue.
    pub fn new(queue: BufferQueue) -> Self {
        Self {
            queue,
            held: None,
        }
    }

    /// The underlying queue (consumer side uses it too).
    pub fn queue_mut(&mut self) -> &mut BufferQueue {
        &mut self.queue
    }

    /// `ANativeWindow_lock` — dequeue a slot for writing.
    pub fn lock(&mut self) -> Result<&mut Surface, OveloError> {
        if self.held.is_some() {
            return Err(err("ui.nwindow.locked"));
        }
        let s = self.queue.dequeue()?;
        self.held = Some(s);
        self.queue.buffer_mut(s)
    }

    /// `ANativeWindow_unlockAndPost` — queue the written frame.
    pub fn unlock_and_post(&mut self) -> Result<FrameId, OveloError> {
        let s = self.held.take().ok_or_else(|| err("ui.nwindow.not_locked"))?;
        self.queue.queue(s)
    }

    /// `ANativeWindow_cancelBuffer`.
    pub fn cancel_lock(&mut self) -> Result<(), OveloError> {
        let s = self.held.take().ok_or_else(|| err("ui.nwindow.not_locked"))?;
        self.queue.cancel(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Color;

    const C: Color = Color {
        r: 10,
        g: 20,
        b: 30,
        a: 255,
    };

    #[test]
    fn produce_consume_cycle() {
        let mut w = ANativeWindow::new(BufferQueue::new(3, 4, 4).unwrap());
        let buf = w.lock().unwrap();
        buf.fill_rect(crate::Rect::new(0, 0, 4, 4), C);
        let id = w.unlock_and_post().unwrap();
        let q = w.queue_mut();
        let a = q.acquire().unwrap();
        assert_eq!(a.slot, id.slot);
        assert_eq!(q.read(a).unwrap().pixel(0, 0), C);
        q.release(a).unwrap();
        // Released slot is reusable.
        assert_eq!(q.dequeue().unwrap(), id.slot);
    }

    #[test]
    fn latest_frame_wins() {
        let mut w = ANativeWindow::new(BufferQueue::new(3, 2, 2).unwrap());
        w.lock().unwrap();
        w.unlock_and_post().unwrap();
        w.lock().unwrap();
        let second = w.unlock_and_post().unwrap();
        let a = w.queue_mut().acquire().unwrap();
        assert_eq!(a.frame, second.frame);
        assert!(w.queue_mut().acquire().is_none());
    }

    #[test]
    fn resize_discards_queued() {
        let mut q = BufferQueue::new(2, 2, 2).unwrap();
        let s = q.dequeue().unwrap();
        q.queue(s).unwrap();
        q.resize(8, 8).unwrap();
        assert!(q.acquire().is_none());
    }

    #[test]
    fn destroyed_fails() {
        let mut q = BufferQueue::new(1, 2, 2).unwrap();
        q.destroy();
        assert!(q.dequeue().is_err());
        assert!(q.resize(4, 4).is_err());
    }
}
