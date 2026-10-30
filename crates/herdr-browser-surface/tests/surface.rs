#![cfg(target_os = "macos")]
use herdr_browser_protocol::{PixelSize, Size, SurfaceRef};
use herdr_browser_surface::{Transport, create, import};

#[test]
fn create_scales_and_roundtrips_in_process() {
    let mut s = create(Size { w: 10.5, h: 4.0 }, 2.0).unwrap();
    assert_eq!(s.size(), PixelSize { w: 21, h: 8 });
    s.fill([10, 20, 30, 255]).unwrap();
    let r = s.export(Transport::GlobalId).unwrap();
    assert_eq!(r, SurfaceRef::GlobalId { id: s.id() });
    let t = import(&r).unwrap();
    assert_eq!(t.pixel(20, 7).unwrap(), [10, 20, 30, 255]);
    assert_eq!(t.id(), s.id());
}

#[test]
fn import_missing_id_errors() {
    assert!(import(&SurfaceRef::GlobalId { id: 0x7fff_fff0 }).is_err());
}
