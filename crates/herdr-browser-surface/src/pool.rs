//! Host-side frame pool: one frame in flight per browser, only the newest pending frame kept,
//! surfaces recycled on `FrameAck` (contract D3). The app keeps showing the last acked frame
//! until the next one arrives, so an ack for seq N recycles the frame shown before N, never N.

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
    /// Acked and on screen in the app: not reusable until a newer frame is acked.
    displayed: Option<PoolFrame>,
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

    /// The app acked `seq`: it is now on screen, the frame it replaced is recycled, and the
    /// pending frame (if any) is promoted for the caller to send. Other seqs are ignored.
    pub fn ack(&mut self, seq: u64) -> Option<&PoolFrame> {
        if self.in_flight.as_ref().map(|f| f.seq) != Some(seq) {
            return None;
        }
        if let Some(old) = self.displayed.replace(self.in_flight.take().expect("checked")) {
            self.free.push(old.surface);
        }
        self.in_flight = self.pending.take();
        self.in_flight.as_ref()
    }

    pub fn in_flight_seq(&self) -> Option<u64> {
        self.in_flight.as_ref().map(|f| f.seq)
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
}

impl FramePool {
    /// Id of the surface on screen in the app (0 if none). For tests.
    pub fn displayed_id(&self) -> u32 {
        self.displayed.as_ref().map_or(0, |f| f.surface.id())
    }
}
