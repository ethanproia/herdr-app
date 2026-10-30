//! GPUI `surface()` paints a 32BGRA IOSurface-backed CVPixelBuffer (browser frames) and the
//! original 420f YUV path still works. Offscreen Metal headless renderer, no window.
//! Exercises the HERDR: patch in vendor/gpui-pre-apple/src/metal_renderer.rs.

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    imp::run();
}

#[cfg(target_os = "macos")]
mod imp {
    use core_foundation::base::TCFType;
    use core_video::pixel_buffer::{CVPixelBuffer, CVPixelBufferRef};
    use gpui::{prelude::*, *};
    use herdr_browser_protocol::PixelSize;
    use std::ffi::c_void;

    unsafe extern "C" {
        fn CVPixelBufferCreateWithIOSurface(a: *const c_void, s: *const c_void, attrs: *const c_void, out: *mut CVPixelBufferRef) -> i32;
    }

    struct Scene(CVPixelBuffer);
    impl Render for Scene {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().bg(rgb(0x000000)).child(
                div().absolute().left(px(10.)).top(px(10.)).w(px(40.)).h(px(20.))
                    .child(surface(self.0.clone()).size_full().object_fit(ObjectFit::Fill)),
            )
        }
    }

    /// Left half red, right half blue, 80x40 px (40x20 DIPs at 2x).
    fn bgra_buffer() -> (herdr_browser_surface::Surface, CVPixelBuffer) {
        let s = herdr_browser_surface::create_pixels(PixelSize { w: 80, h: 40 }).unwrap();
        let mut px = Vec::new();
        for _ in 0..40 {
            for x in 0..80 {
                px.extend_from_slice(if x < 40 { &[0, 0, 255, 255] } else { &[255, 0, 0, 255] });
            }
        }
        s.write_bgra(&px).unwrap();
        let mut out: CVPixelBufferRef = std::ptr::null_mut();
        let k = unsafe { CVPixelBufferCreateWithIOSurface(std::ptr::null(), s.as_ptr(), std::ptr::null(), &mut out) };
        assert_eq!(k, 0, "CVPixelBufferCreateWithIOSurface");
        (s, unsafe { CVPixelBuffer::wrap_under_create_rule(out) })
    }

    /// 420f buffer: Y=255, CbCr=128 everywhere -> white.
    fn yuv_buffer() -> CVPixelBuffer {
        use core_foundation::{base::CFType, dictionary::CFDictionary, string::CFString};
        use core_video::pixel_buffer::{kCVPixelBufferIOSurfacePropertiesKey, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange};
        let key = unsafe { CFString::wrap_under_get_rule(kCVPixelBufferIOSurfacePropertiesKey) };
        let empty: CFDictionary<CFString, CFType> = CFDictionary::from_CFType_pairs(&[]);
        let attrs = CFDictionary::from_CFType_pairs(&[(key, empty.as_CFType())]);
        let b = CVPixelBuffer::new(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange, 80, 40, Some(&attrs)).unwrap();
        assert_eq!(b.lock_base_address(0), 0);
        for (plane, v) in [(0, 255u8), (1, 128u8)] {
            let base = unsafe { b.get_base_address_of_plane(plane) } as *mut u8;
            let n = b.get_bytes_per_row_of_plane(plane) * b.get_height_of_plane(plane);
            unsafe { std::ptr::write_bytes(base, v, n) };
        }
        b.unlock_base_address(0);
        b
    }

    fn shoot(buf: CVPixelBuffer, expect: &[(f32, f32, [u8; 4])], label: &str) {
        let mut cx = HeadlessAppContext::with_platform(
            gpui_platform::current_platform(true).text_system(),
            std::sync::Arc::new(()),
            gpui_platform::current_headless_renderer,
        );
        let window = cx.open_window(size(px(60.), px(40.)), |_, cx| cx.new(|_| Scene(buf))).unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            let _ = window.draw(cx);
            let image = window.render_to_image().expect("headless render");
            let scale = window.scale_factor();
            for &(x, y, want) in expect {
                let got = image.get_pixel((x * scale) as u32, (y * scale) as u32).0;
                let close = got.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= 2);
                assert!(close, "{label}: pixel ({x},{y}) got {got:?} want {want:?}");
            }
            println!("{label}: {} pixels ok ({}x{} @{scale}x)", expect.len(), image.width(), image.height());
        })
        .unwrap();
    }

    pub fn run() {
        let (surface, buf) = bgra_buffer();
        shoot(buf, &[(5., 5., [0, 0, 0, 255]), (15., 20., [255, 0, 0, 255]), (45., 20., [0, 0, 255, 255]), (55., 35., [0, 0, 0, 255])], "bgra");
        drop(surface);
        shoot(yuv_buffer(), &[(5., 5., [0, 0, 0, 255]), (30., 20., [255, 255, 255, 255])], "yuv420f");
        println!("surface pixel test passed");
    }
}
