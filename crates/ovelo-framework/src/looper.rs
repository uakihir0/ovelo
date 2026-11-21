//! `Looper`/`Handler`: a time-ordered message queue.
//!
//! Messages dispatch in `(when_ms, seq)` order — `MessageQueue.next()` takes
//! the earliest due message; `post_delayed` schedules by `when = now +
//! delay`. One `Looper` per thread (here: one per owner), handlers enqueue
//! into it.

use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicU64, Ordering};

/// A queued message — `Handler.obtainMessage` fields.
#[derive(Debug)]
pub struct Message {
    /// `msg.what`.
    pub what: i32,
    /// `msg.arg1`.
    pub arg1: i32,
    /// `msg.arg2`.
    pub arg2: i32,
    /// `msg.obj` — opaque payload slot (index/handle, not a heap ref).
    pub obj: u64,
    /// Dispatch time in milliseconds (queue's clock).
    pub when_ms: u64,
    seq: u64,
    /// Originating handler tag (lets a driver route `handleMessage`).
    pub handler: u64,
}

static SEQ: AtomicU64 = AtomicU64::new(0);

impl Message {
    /// New message for `handler` due at `when_ms`.
    pub fn new(handler: u64, what: i32, when_ms: u64) -> Self {
        Self {
            what,
            arg1: 0,
            arg2: 0,
            obj: 0,
            when_ms,
            seq: SEQ.fetch_add(1, Ordering::Relaxed),
            handler,
        }
    }
}

/// Heap entry ordered by (when, seq) — BinaryHeap is a max-heap, so invert.
struct Entry(u64, u64, Message);
impl PartialEq for Entry {
    fn eq(&self, o: &Self) -> bool {
        self.0 == o.0 && self.1 == o.1
    }
}
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Entry {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        // Reverse: earliest (when, seq) pops first.
        (o.0, o.1).cmp(&(self.0, self.1))
    }
}

/// `MessageQueue` — priority queue of pending messages.
#[derive(Default)]
pub struct MessageQueue {
    heap: BinaryHeap<Entry>,
}

impl MessageQueue {
    /// Enqueue `m`.
    pub fn push(&mut self, m: Message) {
        self.heap.push(Entry(m.when_ms, m.seq, m));
    }

    /// Remove the earliest due message (when <= now). `None` if none due.
    pub fn next_due(&mut self, now_ms: u64) -> Option<Message> {
        match self.heap.peek() {
            Some(e) if e.0 <= now_ms => Some(self.heap.pop().unwrap().2),
            _ => None,
        }
    }

    /// Time of the next pending message (`poll` without removing).
    pub fn next_when(&self) -> Option<u64> {
        self.heap.peek().map(|e| e.0)
    }

    /// Pending message count.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }
}

/// `Handler` — posts messages to its looper's queue.
pub struct Handler {
    /// Stable id used for `msg.handler` routing.
    pub id: u64,
}

impl Handler {
    /// New handler with the given routing id.
    pub fn new(id: u64) -> Self {
        Self { id }
    }

    /// `sendMessage(m)` — enqueue now.
    pub fn send(&self, q: &mut MessageQueue, m: Message) {
        q.push(m);
    }

    /// `sendEmptyMessage(what)` — bare message due now.
    pub fn send_empty(&self, q: &mut MessageQueue, what: i32, now_ms: u64) {
        q.push(Message::new(self.id, what, now_ms));
    }

    /// `sendEmptyMessageDelayed(what, delay)` — due at `now + delay`.
    pub fn send_empty_delayed(&self, q: &mut MessageQueue, what: i32, delay_ms: u64, now_ms: u64) {
        q.push(Message::new(self.id, what, now_ms + delay_ms));
    }

    /// `post(runnable)` — modeled as a message whose `what` is the
    /// runnable's tag; the driver maps it back to a callable.
    pub fn post(&self, q: &mut MessageQueue, what: i32, now_ms: u64) {
        self.send_empty(q, what, now_ms);
    }
}

/// `Looper` — owns a queue; `loop_once` dispatches one due message.
#[derive(Default)]
pub struct Looper {
    queue: MessageQueue,
}

impl Looper {
    /// The looper's message queue.
    pub fn queue(&mut self) -> &mut MessageQueue {
        &mut self.queue
    }

    /// `loopOnce`: take the earliest message due at `now`; returns it for
    /// the driver to call `handler.dispatchMessage` with.
    pub fn loop_once(&mut self, now_ms: u64) -> Option<Message> {
        self.queue.next_due(now_ms)
    }

    /// Drain every message due at `now`, in order.
    pub fn drain(&mut self, now_ms: u64) -> Vec<Message> {
        let mut out = Vec::new();
        while let Some(m) = self.queue.next_due(now_ms) {
            out.push(m);
        }
        out
    }
}
