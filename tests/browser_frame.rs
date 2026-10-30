//! src/browser/frame.rs end to end, offscreen: a host-side surface goes through
//! `FrameSink::on_frame` (export -> import -> CVPixelBuffer) and GPUI paints it into a block.
//! Asserts pixels, stretching during a pending resize, ack-once, and that replaced frames are
//! released (the host's surface is no longer in use). No window.

#[cfg(target_os = "macos")]
#[path = "../src/browser/frame.rs"]
mod frame;

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    use frame::{FrameSink, FrameView};
    use gpui::{prelude::*, *};
    use herdr_browser_protocol::{PixelSize, Size as Dips};
    use herdr_browser_surface::{Transport, create_pixels};

    struct Block(FrameView);
    impl Render for Block {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let inner = div().absolute().left(px(10.)).top(px(10.)).w(px(40.)).h(px(20.));
            div().size_full().bg(rgb(0x000000)).child(match self.0.element() {
                Some(e) => inner.child(e),
                None => inner,
            })
        }
    }

    // Host side: a 2x surface for a 40x20 DIP block, left half red, right half blue.
    let host = |w: u32, h: u32| {
        let s = create_pixels(PixelSize { w, h }).unwrap();
        let mut px = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                px.extend_from_slice(if x < w / 2 { &[0, 0, 255, 255] } else { &[255, 0, 0, 255] });
            }
        }
        s.write_bgra(&px).unwrap();
        s
    };

    let mut cx = HeadlessAppContext::with_platform(
        gpui_platform::current_platform(true).text_system(),
        std::sync::Arc::new(()),
        gpui_platform::current_headless_renderer,
    );
    let window = cx.open_window(size(px(60.), px(40.)), |_, cx| cx.new(|_| Block(FrameView::new()))).unwrap();
    let mut a = host(80, 40);
    let mut b = host(40, 20); // a smaller frame, stretched to fill the block
    let ref_a = a.export(Transport::GlobalId).unwrap();
    let ref_b = b.export(Transport::GlobalId).unwrap();

    let shoot = |cx: &mut HeadlessAppContext, label: &str, expect: &[(f32, f32, [u8; 4])]| {
        cx.update_window(window.into(), |_, window, cx| {
            let _ = window.draw(cx);
            let image = window.render_to_image().expect("headless render");
            let s = window.scale_factor();
            for &(x, y, want) in expect {
                let got = image.get_pixel((x * s) as u32, (y * s) as u32).0;
                assert!(got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 2), "{label}: ({x},{y}) got {got:?} want {want:?}");
            }
            println!("{label}: {} pixels ok", expect.len());
        })
        .unwrap();
    };
    let view = |cx: &mut HeadlessAppContext, f: &mut dyn FnMut(&mut FrameView)| {
        cx.update_window(window.into(), |root, _, cx| {
            let block = root.downcast::<Block>().unwrap();
            block.update(cx, |block, cx| { f(&mut block.0); cx.notify(); });
        })
        .unwrap();
    };
    const RED: [u8; 4] = [255, 0, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const BLACK: [u8; 4] = [0, 0, 0, 255];

    shoot(&mut cx, "before first frame", &[(30., 20., BLACK)]);
    view(&mut cx, &mut |v| assert_eq!(v.ack(), None, "nothing painted, nothing to ack"));

    view(&mut cx, &mut |v| v.on_frame(1, ref_a.clone(), a.size(), 2.0).unwrap());
    shoot(&mut cx, "frame 1", &[(5., 5., BLACK), (15., 20., RED), (45., 20., BLUE), (55., 35., BLACK)]);
    view(&mut cx, &mut |v| {
        assert_eq!(v.ack(), None, "drawn into a scene is not presented");
        v.presented(false);
        assert_eq!(v.ack(), None, "presented while the window is not visible: no ack");
    });
    shoot(&mut cx, "frame 1 again", &[(15., 20., RED)]);
    view(&mut cx, &mut |v| {
        v.presented(true);
        assert_eq!(v.ack(), Some(1));
        assert_eq!(v.ack(), None, "ack is taken once");
        // Hidden: undelivered acks are forgotten and nothing acks until visible again.
        assert!(v.set_visible(false));
        assert!(!v.set_visible(false), "no change, nothing to send");
    });
    shoot(&mut cx, "hidden (last frame kept)", &[(15., 20., RED)]);
    view(&mut cx, &mut |v| {
        v.presented(true);
        assert_eq!(v.ack(), None, "a hidden block never acks");
        assert!(v.set_visible(true));
    });
    shoot(&mut cx, "visible again", &[(15., 20., RED)]);
    view(&mut cx, &mut |v| {
        v.presented(true);
        assert_eq!(v.ack(), Some(1), "acks resume");
    });
    assert!(a.in_use(), "app holds frame 1");

    // Resize pending: the 1x frame is stretched over the same block bounds.
    view(&mut cx, &mut |v| {
        v.on_resize_pending(Dips { w: 40., h: 20. });
        v.on_frame(2, ref_b.clone(), b.size(), 1.0).unwrap();
        assert!(!v.stretching(), "40x20 at 1x matches the asked size");
        v.on_resize_pending(Dips { w: 80., h: 40. });
        assert!(v.stretching());
        v.on_frame(1, ref_a.clone(), a.size(), 2.0).unwrap(); // stale seq ignored
        assert_eq!(v.seq(), Some(2));
    });
    shoot(&mut cx, "frame 2 stretched", &[(12., 20., RED), (28., 20., RED), (32., 20., BLUE), (48., 20., BLUE)]);
    assert!(!a.in_use(), "frame 1 released after it was replaced (no leak)");

    view(&mut cx, &mut |v| v.clear());
    shoot(&mut cx, "cleared", &[(30., 20., BLACK)]);
    assert!(!b.in_use(), "frame 2 released after clear");
    let _ = (&mut a, &mut b);
    // Stats mode: a paint line per new frame, the host's coalesced seqs counted as dropped.
    let home = std::env::temp_dir().join(format!("herdr-frame-stats-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    unsafe {
        std::env::set_var("HERDR_APP_HOME", &home);
        std::env::set_var("HERDR_BROWSER_STATS", "1");
    }
    let mut v = FrameView::new();
    for seq in [1, 2, 5] {
        v.on_frame(seq, ref_a.clone(), a.size(), 2.0).unwrap();
        assert!(v.element().is_some());
        v.presented(true);
        assert!(v.element().is_some());
        v.presented(true); // a repaint of the same frame is not counted
    }
    let log = std::fs::read_to_string(home.join("browser/stats.log")).unwrap();
    let paints: Vec<_> = log.lines().filter(|l| l.contains(" paint ")).collect();
    assert_eq!(paints.len(), 3, "{log}");
    assert!(paints[2].contains("seq=5") && paints[2].ends_with("dropped=2"), "{log}");
    let _ = std::fs::remove_dir_all(&home);
    println!("stats: {} paint lines ok", paints.len());
    println!("browser frame test passed");
}
