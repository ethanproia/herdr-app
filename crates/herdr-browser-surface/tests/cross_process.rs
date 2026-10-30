//! OQ1: a surface made in this process is read back by a child process, per transport.
//! The child is this test binary re-run with HERDR_SURFACE_CHILD set.
#![cfg(target_os = "macos")]

use herdr_browser_protocol::{PixelSize, SurfaceRef};
use herdr_browser_surface::{Transport, create_pixels, import};
use std::process::Command;

const W: u32 = 64;
const H: u32 = 32;

/// Each pixel encodes its own coordinates: B=x, G=y, R=0x5a, A=0xff.
fn pattern() -> Vec<u8> {
    let mut v = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            v.extend_from_slice(&[x as u8, y as u8, 0x5a, 0xff]);
        }
    }
    v
}

fn run(transport: Transport) {
    let mut s = create_pixels(PixelSize { w: W, h: H }).expect("create");
    s.write_bgra(&pattern()).expect("write");
    let r = s.export(transport).expect("export");
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child", "--nocapture"])
        .env("HERDR_SURFACE_CHILD", serde_ref(&r))
        .output()
        .expect("spawn child");
    let log = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{transport:?} child failed:\n{log}");
    assert!(log.contains("CHILD_OK"), "{transport:?}: child never ran:\n{log}");
    // The child wrote a marker pixel back into the shared memory: proves it is the same surface.
    assert_eq!(s.pixel(0, 0).unwrap(), [1, 2, 3, 4], "{transport:?}: child write not visible to parent");
}

fn serde_ref(r: &SurfaceRef) -> String {
    match r {
        SurfaceRef::GlobalId { id } => format!("g:{id}"),
        SurfaceRef::MachPort { name } => format!("m:{name}"),
    }
}

#[test]
fn child() {
    let Ok(arg) = std::env::var("HERDR_SURFACE_CHILD") else { return };
    let r = match arg.split_once(':').unwrap() {
        ("g", id) => SurfaceRef::GlobalId { id: id.parse().unwrap() },
        (_, name) => SurfaceRef::MachPort { name: name.into() },
    };
    let s = import(&r).expect("import in child");
    assert_eq!(s.size(), PixelSize { w: W, h: H });
    assert_eq!(s.read_bgra().unwrap(), pattern(), "pixels differ across processes");
    assert_eq!(s.pixel(17, 9).unwrap(), [17, 9, 0x5a, 0xff]);
    s.write_bgra(&{
        let mut p = pattern();
        p[..4].copy_from_slice(&[1, 2, 3, 4]);
        p
    })
    .unwrap();
    println!("CHILD_OK pid={} id={}", std::process::id(), s.id());
}

/// The fence the host pool relies on: a use count taken in another process is visible here.
#[test]
fn use_count_crosses_processes() {
    use std::io::{BufRead, Write};
    if let Ok(id) = std::env::var("HERDR_SURFACE_USE_CHILD") {
        let s = import(&SurfaceRef::GlobalId { id: id.parse().unwrap() }).unwrap();
        let guard = s.use_guard();
        let mut line = String::new();
        println!("HELD");
        std::io::stdout().flush().unwrap();
        std::io::stdin().read_line(&mut line).unwrap();
        drop(guard);
        println!("RELEASED");
        std::io::stdout().flush().unwrap();
        std::io::stdin().read_line(&mut line).unwrap();
        return;
    }
    let s = create_pixels(PixelSize { w: 4, h: 4 }).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "use_count_crosses_processes", "--nocapture"])
        .env("HERDR_SURFACE_USE_CHILD", s.id().to_string())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut inp = child.stdin.take().unwrap();
    let mut wait_for = |word: &str| {
        let mut line = String::new();
        while !line.contains(word) {
            line.clear();
            assert!(out.read_line(&mut line).unwrap() > 0, "child exited before {word}");
        }
    };
    assert!(!s.in_use());
    wait_for("HELD");
    assert!(s.in_use(), "child's use count not visible to the parent");
    writeln!(inp).unwrap();
    wait_for("RELEASED");
    assert!(!s.in_use());
    writeln!(inp).unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn cross_process_global_id() {
    run(Transport::GlobalId);
}

#[test]
fn cross_process_mach_port() {
    run(Transport::MachPort);
}
