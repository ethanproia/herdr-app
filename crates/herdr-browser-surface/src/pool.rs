//! Host-side frame pool: at most `MAX_IN_FLIGHT` (2) unacked frames per browser, only the newest
//! pending frame kept, surfaces recycled on `FrameAck` (contract D3). Two in flight because the
//! app acks after GPUI presented a frame, one vsync after drawing it: with one, delivery halves to
//! 30 fps; with two the app always has the next frame when it draws (triple buffering: one on
//! screen, one in flight, one being painted). The app keeps showing the last acked frame
//! until the next one arrives, so an ack for seq N recycles the frame shown before N, never N.
//!
//! Safe reuse is fenced on the IOSurface use count, which is kernel-wide (`tests/cross_process.rs`
//! checks a child's count from the parent): while the app holds a frame as a CVPixelBuffer, its
//! GPUI scene references it, or its Metal renderer has a CVMetalTexture of it in a command buffer
//! not yet completed (the HERDR: patch in vendor/gpui-pre-apple releases them on completion), the
//! surface is in use and `acquire` passes it over. An ack paces delivery; it never licenses reuse.

use crate::{Result, Surface, create_pixels};
use herdr_browser_protocol::PixelSize;
use std::collections::VecDeque;

/// A surface the pool handed out, tagged with the frame `seq` it carries.
pub struct PoolFrame {
    pub seq: u64,
    pub surface: Surface,
}

#[derive(Default)]
pub struct FramePool {
    free: Vec<Surface>,
    /// Sent, not yet acked, oldest first.
    in_flight: VecDeque<PoolFrame>,
    /// Acked and on screen in the app: not reusable until a newer frame is acked.
    displayed: Option<PoolFrame>,
    pending: Option<PoolFrame>,
    next_seq: u64,
    created: usize,
}

impl FramePool {
    pub const MAX_IN_FLIGHT: usize = 2;
    const MAX_FREE: usize = 4;

    pub fn new() -> Self {
        Self::default()
    }

    /// A surface of `size` to paint the next frame into (recycled when one fits, else new).
    /// Surfaces of another size are dropped: a resize retires the old slots.
    /// Surfaces still held by the app are skipped (see the module docs).
    pub fn acquire(&mut self, size: PixelSize) -> Result<Surface> {
        self.free.retain(|s| s.size() == size);
        if let Some(i) = self.free.iter().position(|s| !s.in_use()) {
            return Ok(self.free.swap_remove(i));
        }
        // Every free slot is still held: grow, but keep at most MAX_FREE held slots around.
        if self.free.len() > Self::MAX_FREE {
            self.free.remove(0);
        }
        self.created += 1;
        create_pixels(size)
    }

    /// Submit a painted surface. Returns the frame to send now if fewer than `MAX_IN_FLIGHT`
    /// are unacked; otherwise it becomes the pending frame (replacing and recycling an older one).
    pub fn submit(&mut self, surface: Surface) -> Option<&PoolFrame> {
        self.next_seq += 1;
        let frame = PoolFrame { seq: self.next_seq, surface };
        if self.in_flight.len() < Self::MAX_IN_FLIGHT {
            self.in_flight.push_back(frame);
            return self.in_flight.back();
        }
        if let Some(old) = self.pending.replace(frame) {
            self.free.push(old.surface);
        }
        None
    }

    /// The app presented `seq` (and so is done with every older frame): `seq` is on screen, the
    /// frames it replaced are recycled, and the pending frame (if any) is promoted for the caller
    /// to send. Acks for seqs not in flight are ignored.
    pub fn ack(&mut self, seq: u64) -> Option<&PoolFrame> {
        if !self.in_flight.iter().any(|f| f.seq == seq) {
            return None;
        }
        while let Some(f) = self.in_flight.pop_front() {
            let done = f.seq == seq;
            if let Some(old) = self.displayed.replace(f) {
                self.free.push(old.surface);
            }
            if done {
                break;
            }
        }
        let next = self.pending.take()?;
        self.in_flight.push_back(next);
        self.in_flight.back()
    }

    /// The page was hidden: forget the pending frame (a fresh one comes on resume).
    pub fn drop_pending(&mut self) {
        if let Some(old) = self.pending.take() {
            self.free.push(old.surface);
        }
    }

    /// Newest unacked seq.
    pub fn in_flight_seq(&self) -> Option<u64> {
        self.in_flight.back().map(|f| f.seq)
    }

    pub fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }

    pub fn displayed_seq(&self) -> Option<u64> {
        self.displayed.as_ref().map(|f| f.seq)
    }

    pub fn pending_seq(&self) -> Option<u64> {
        self.pending.as_ref().map(|f| f.seq)
    }

    /// Surfaces this pool ever allocated (tests check recycling keeps this small).
    pub fn created(&self) -> usize {
        self.created
    }

    /// Id of the surface on screen in the app (0 if none). For tests.
    pub fn displayed_id(&self) -> u32 {
        self.displayed.as_ref().map_or(0, |f| f.surface.id())
    }
}
