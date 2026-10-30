#![cfg(target_os = "macos")]
use herdr_browser_protocol::PixelSize;
use herdr_browser_surface::FramePool;

const S: PixelSize = PixelSize { w: 8, h: 8 };

#[test]
fn two_in_flight_newest_pending_recycled() {
    let mut p = FramePool::new();
    for want in [1, 2] {
        let s = p.acquire(S).unwrap();
        assert_eq!(p.submit(s).map(|f| f.seq), Some(want), "up to two go out at once");
    }
    for _ in 0..5 {
        let s = p.acquire(S).unwrap();
        assert!(p.submit(s).is_none(), "held while two are in flight");
    }
    assert_eq!(p.pending_seq(), Some(7), "only the newest pending frame is kept");
    assert!(p.ack(99).is_none(), "stale ack ignored");
    assert_eq!(p.ack(1).map(|f| f.seq), Some(7), "ack frees a slot: pending goes out");
    assert_eq!(p.displayed_seq(), Some(1), "acked frame stays on screen");
    let fresh = p.acquire(S).unwrap();
    assert_ne!(fresh.id(), p.displayed_id(), "never repaint the frame on screen");
    assert!(p.submit(fresh).is_none(), "2 and 7 in flight");
    // Acking 7 covers 2 as well (the app skipped it).
    assert_eq!(p.ack(7).map(|f| f.seq), Some(8));
    assert_eq!(p.displayed_seq(), Some(7));
    assert_eq!(p.in_flight_len(), 1);
    p.ack(8);
    for _ in 0..50 {
        let s = p.acquire(S).unwrap();
        let seq = p.submit(s).unwrap().seq;
        p.ack(seq);
    }
    assert!(p.created() <= 6, "steady state allocates nothing new, created {}", p.created());
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

#[test]
fn never_hands_out_a_surface_the_app_still_holds() {
    let mut p = FramePool::new();
    let s = p.acquire(S).unwrap();
    let held = s.use_guard(); // the app's CVPixelBuffer / in-flight GPU work
    let held_id = s.id();
    let seq = p.submit(s).unwrap().seq;
    p.ack(seq);
    let t = p.acquire(S).unwrap();
    let seq = p.submit(t).unwrap().seq;
    p.ack(seq); // frame 1 leaves "displayed" for the free list, still held
    for _ in 0..3 {
        let u = p.acquire(S).unwrap();
        assert_ne!(u.id(), held_id, "recycled a surface still in use");
        let seq = p.submit(u).unwrap().seq;
        p.ack(seq);
    }
    drop(held);
    let mut ids = Vec::new();
    for _ in 0..4 {
        let u = p.acquire(S).unwrap();
        ids.push(u.id());
        let seq = p.submit(u).unwrap().seq;
        p.ack(seq);
    }
    assert!(ids.contains(&held_id), "released surface is reused again");
}

#[test]
fn drop_pending_on_hide() {
    let mut p = FramePool::new();
    for _ in 0..3 {
        let s = p.acquire(S).unwrap();
        p.submit(s);
    }
    assert_eq!(p.pending_seq(), Some(3));
    p.drop_pending();
    assert_eq!(p.pending_seq(), None);
    assert_eq!(p.ack(2).map(|f| f.seq), None, "nothing stale is sent after a hide");
}
