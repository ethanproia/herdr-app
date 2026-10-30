#![cfg(target_os = "macos")]
use herdr_browser_protocol::PixelSize;
use herdr_browser_surface::FramePool;

const S: PixelSize = PixelSize { w: 8, h: 8 };

#[test]
fn one_in_flight_newest_pending_recycled() {
    let mut p = FramePool::new();
    let a = p.acquire(S).unwrap();
    assert_eq!(p.submit(a).map(|f| f.seq), Some(1), "first frame goes out at once");
    for _ in 0..5 {
        let s = p.acquire(S).unwrap();
        assert!(p.submit(s).is_none(), "held while seq 1 is in flight");
    }
    assert_eq!(p.pending_seq(), Some(6), "only the newest pending frame is kept");
    assert!(p.ack(99).is_none(), "stale ack ignored");
    assert_eq!(p.ack(1).map(|f| f.seq), Some(6));
    assert_eq!(p.displayed_seq(), Some(1), "acked frame stays on screen");
    let fresh = p.acquire(S).unwrap();
    assert_ne!(fresh.id(), p.displayed_id(), "never repaint the frame on screen");
    p.submit(fresh);
    assert_eq!(p.ack(6).map(|f| f.seq), Some(7));
    assert_eq!(p.displayed_seq(), Some(6));
    p.ack(7);
    assert!(p.created() <= 4, "recycled, created {}", p.created());
    for _ in 0..50 {
        let s = p.acquire(S).unwrap();
        let seq = p.submit(s).unwrap().seq;
        p.ack(seq);
    }
    assert!(p.created() <= 4, "steady state allocates nothing, created {}", p.created());
}

#[test]
fn resize_retires_old_slots() {
    let mut p = FramePool::new();
    let s = p.acquire(S).unwrap();
    let seq = p.submit(s).unwrap().seq;
    p.ack(seq);
    let big = p.acquire(PixelSize { w: 16, h: 16 }).unwrap();
    assert_eq!(big.size(), PixelSize { w: 16, h: 16 });
    assert_eq!(p.created(), 2);
}
