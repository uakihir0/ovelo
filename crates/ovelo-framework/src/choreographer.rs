//! `Choreographer` (design doc §06/Ch.08): frame callbacks driven by the
//! host's vsync. The host calls [`Choreographer::tick`] once per display
//! refresh with the frame timestamp; callbacks posted via
//! [`Choreographer::post_frame_callback`] run on the *next* tick (posted
//! during a tick run the tick after, matching Android's "next frame"
//! semantics). Callback types run in Android's order: input → animation
//! → traversal.

use std::collections::VecDeque;

/// Callback type ordering inside one frame (Android's CALLBACK_* order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CallbackType {
    /// `CALLBACK_INPUT` — handle input first.
    Input = 0,
    /// `CALLBACK_ANIMATION`.
    Animation = 1,
    /// `CALLBACK_TRAVERSAL` — layout/draw.
    Traversal = 2,
}

/// A posted `FrameCallback` (`doFrame(frame_time_nanos)`).
#[derive(Debug, Clone)]
pub struct FrameCallback {
    /// Which pipeline stage it belongs to.
    pub kind: CallbackType,
    /// Opaque id the driver uses to route the callback.
    pub id: u64,
    /// Frames to skip before running (`postFrameCallbackDelayed`).
    pub delay_frames: u32,
}

/// One per UI thread. Not Sync — driven from the host's display link.
#[derive(Debug, Default)]
pub struct Choreographer {
    pending: VecDeque<FrameCallback>,
    /// Vsync timestamp of the last tick (nanoseconds).
    last_vsync: u64,
    /// Ticks since the previous tick's pending queue emptied — the
    /// skipped-frames signal for jank diagnosis.
    frames_skipped: u64,
    /// Total ticks seen.
    ticks: u64,
}

impl Choreographer {
    /// `Choreographer.getInstance()`.
    pub fn new() -> Self {
        Self::default()
    }

    /// `postFrameCallback` — runs on the next vsync.
    pub fn post_frame_callback(&mut self, kind: CallbackType, id: u64) {
        self.post_frame_callback_delayed(kind, id, 0);
    }

    /// `postFrameCallbackDelayed(delayMillis)` — expressed in frames at
    /// this level (host converts ms → vsync counts).
    pub fn post_frame_callback_delayed(&mut self, kind: CallbackType, id: u64, delay_frames: u32) {
        self.pending.push_back(FrameCallback {
            kind,
            id,
            delay_frames,
        });
    }

    /// `removeFrameCallback` — cancel a posted callback by id.
    pub fn remove_frame_callback(&mut self, id: u64) {
        self.pending.retain(|c| c.id != id);
    }

    /// Host vsync: returns the callbacks due this frame, in
    /// input → animation → traversal order, each with its
    /// `frameTimeNanos` equal to `vsync_nanos`. Callbacks posted during
    /// the tick wait for the next one; delayed callbacks count down.
    pub fn tick(&mut self, vsync_nanos: u64) -> Vec<(FrameCallback, u64)> {
        self.ticks += 1;
        if self.pending.is_empty() {
            self.frames_skipped += 1;
        }
        self.last_vsync = vsync_nanos;
        let mut due: Vec<FrameCallback> = Vec::new();
        let mut keep: VecDeque<FrameCallback> = VecDeque::new();
        // Snapshot: callbacks posted during delivery are NOT in `pending`
        // mutation here because the driver appends via post_frame_callback
        // after tick returns — a post inside a handler lands for next tick.
        while let Some(mut c) = self.pending.pop_front() {
            if c.delay_frames == 0 {
                due.push(c);
            } else {
                c.delay_frames -= 1;
                keep.push_back(c);
            }
        }
        self.pending = keep;
        due.sort_by_key(|c| c.kind);
        due.into_iter().map(|c| (c, vsync_nanos)).collect()
    }

    /// Consecutive ticks with nothing to do (jank signal for `dumpsys`).
    pub fn frames_skipped(&self) -> u64 {
        self.frames_skipped
    }

    /// Last vsync timestamp handed out (`getFrameTimeNanos`).
    pub fn last_vsync_nanos(&self) -> u64 {
        self.last_vsync
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_and_delay() {
        let mut c = Choreographer::new();
        c.post_frame_callback(CallbackType::Traversal, 3);
        c.post_frame_callback(CallbackType::Input, 1);
        c.post_frame_callback(CallbackType::Animation, 2);
        c.post_frame_callback_delayed(CallbackType::Input, 9, 2);
        let due = c.tick(16_000_000);
        let ids: Vec<u64> = due.iter().map(|(c, _)| c.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert!(due.iter().all(|(_, t)| *t == 16_000_000));
        assert!(c.tick(32_000_000).is_empty()); // countdown 2→1
        let due = c.tick(48_000_000);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0.id, 9);
    }

    #[test]
    fn remove_cancels() {
        let mut c = Choreographer::new();
        c.post_frame_callback(CallbackType::Input, 7);
        c.remove_frame_callback(7);
        assert!(c.tick(1).is_empty());
    }
}
