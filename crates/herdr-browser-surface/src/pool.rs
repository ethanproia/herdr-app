//! Host-side frame pool: one frame in flight per browser, only the newest pending frame kept,
//! surfaces recycled on `FrameAck` (contract D3).

use crate::{Result, Surface, create_pixels};
use herdr_browser_protocol::PixelSize;

/// A surface the pool handed out, tagged with the frame `seq` it carries.
pub struct PoolFrame {
    pub seq: u64,
    pub surface: Surface,
}

#[derive(Default)]
pub struct FramePool {
    free: Vec<Surface>,
    in_flight: Option<PoolFrame>,
    pending: Option<PoolFrame>,
    next_seq: u64,
    created: usize,
}

impl FramePool {
    pub fn new() -> Self {
        Self::default()
    }

    /// A surface of `size` to paint the next frame into (recycled when one fits, else new).
    /// Surfaces of another size are dropped: a resize retires the old slots.
    pub fn acquire(&mut self, size: PixelSize) -> Result<Surface> {
        self.free.retain(|s| s.size() == size);
        if let Some(s) = self.free.pop() {
            return Ok(s);
        }
        self.created += 1;
        create_pixels(size)
    }

    /// Submit a painted surface. Returns the frame to send now if nothing is in flight;
    /// otherwise it becomes the pending frame (replacing and recycling an older pending one).
    pub fn submit(&mut self, surface: Surface) -> Option<&PoolFrame> {
        self.next_seq += 1;
        let frame = PoolFrame { seq: self.next_seq, surface };
        if self.in_flight.is_none() {
            self.in_flight = Some(frame);
            return self.in_flight.as_ref();
        }
        if let Some(old) = self.pending.replace(frame) {
            self.free.push(old.surface);
        }
        None
    }

    /// The app acked `seq`: recycle the in-flight surface and promote the pending frame, which
    /// the caller then sends. Acks for anything but the in-flight seq are ignored.
    pub fn ack(&mut self, seq: u64) -> Option<&PoolFrame> {
        if self.in_flight.as_ref().map(|f| f.seq) != Some(seq) {
            return None;
        }
        let done = self.in_flight.take().expect("checked");
        self.free.push(done.surface);
        self.in_flight = self.pending.take();
        self.in_flight.as_ref()
    }

    pub fn in_flight_seq(&self) -> Option<u64> {
        self.in_flight.as_ref().map(|f| f.seq)
    }

    pub fn pending_seq(&self) -> Option<u64> {
        self.pending.as_ref().map(|f| f.seq)
    }

    /// Surfaces this pool ever allocated (tests check recycling keeps this small).
    pub fn created(&self) -> usize {
        self.created
    }
}
