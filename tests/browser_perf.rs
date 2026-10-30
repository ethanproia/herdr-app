//! Section R (responsiveness) of the parity test plan, against the real CEF host, offscreen.
//! Every number is repeated `HERDR_PERF_RUNS` times (default 5) and reported as mean, sd, min,
//! max. Labels follow the review's measurement rules:
//! - `delivery`: frames the host delivered to the app (arrival in the app).
//! - `drawn`: frames the app drew into a GPUI scene (the harness pumps at 60 Hz, a vsync
//!   stand-in). Offscreen there is no display, so neither is an on-screen presentation metric:
//!   that comes from `HERDR_BROWSER_STATS=1` in the one visible session.
//! Run through the Chromium slot: FORK_WORKER=frames ~/work.noindex/herdr-fork/bin/chromium-slot
//!   cargo test --locked --test browser_perf
//! `HERDR_PERF_ONLY=R1,R4` runs a subset.
include!("common/browser_harness.rs");

use harness::{Host, Page, metric};
use serde_json::json;
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_micros(16_667);

/// Pump at 60 Hz for `d`, calling `each` once per tick before the draw.
fn paced(p: &mut Page, d: Duration, mut each: impl FnMut(&mut Page, u32)) {
    let start = Instant::now();
    let mut n = 0;
    while start.elapsed() < d {
        each(p, n);
        p.pump();
        n += 1;
        let next = start + TICK * n;
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
}

fn runs() -> usize {
    std::env::var("HERDR_PERF_RUNS").ok().and_then(|v| v.parse().ok()).unwrap_or(5)
}

fn wanted(id: &str) -> bool {
    std::env::var("HERDR_PERF_ONLY").map_or(true, |v| v.split(',').any(|x| x.trim() == id))
}

/// One row of the R table: mean, sd, min, max over runs.
fn report(id: &str, label: &str, unit: &str, target: &str, pass: impl Fn(f64) -> bool, v: &[f64]) {
    let n = v.len() as f64;
    let mean = v.iter().sum::<f64>() / n;
    let sd = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0).max(1.0)).sqrt();
    let (min, max) = v.iter().fold((f64::MAX, f64::MIN), |(a, b), x| (a.min(*x), b.max(*x)));
    let ok = if target.is_empty() { "-" } else if pass(mean) { "PASS" } else { "MISS" };
    let mut sorted = v.to_vec();
    let p50 = pct(&mut sorted, 0.5);
    println!("RESULT {id} [{label}] mean={mean:.2} sd={sd:.2} p50={p50:.2} min={min:.2} max={max:.2} {unit} n={} target={target} {ok}", v.len());
    metric(id, mean, unit, target);
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.is_empty() { 0.0 } else { v[((v.len() - 1) as f64 * p).round() as usize] }
}

/// R1: 60 Hz trackpad wheel for 5 s on a long page.
fn r1(server: &fixtures::Server) {
    let (mut fps, mut drawn, mut acked, mut gap95) = (vec![], vec![], vec![], vec![]);
    let (mut cpu, mut blit) = (vec![], vec![]);
    for run in 0..runs() {
        let mut p = Page::open(Host::Cef, &server.url("scroll.html")).expect("CEF host");
        p.run_for(Duration::from_millis(500));
        p.clear_frames();
        let t0 = cpu_secs(&p);
        let start = Instant::now();
        paced(&mut p, Duration::from_secs(5), |p, _| p.wheel(400.0, 300.0, 0.0, -8.0, true));
        let secs = start.elapsed().as_secs_f64();
        let t1 = cpu_secs(&p);
        let f = p.frames();
        let s = p.frame_stats(start);
        let y = p.eval("scrollY").unwrap().as_f64().unwrap_or(0.0);
        assert!(y > 1000.0, "the page did not scroll (scrollY {y})");
        fps.push(s.fps);
        gap95.push(s.p95_gap_ms);
        drawn.push(f.iter().filter(|r| r.painted.is_some()).count() as f64 / secs);
        acked.push(f.iter().filter(|r| r.acked.is_some()).count() as f64 / secs);
        cpu.push((t1 - t0) / secs * 100.0);
        if let Some(b) = blit_p95() {
            blit.push(b);
        }
        println!("R1 run {run}: delivery {:.1} fps, p95 gap {:.1} ms, scrollY {y}", s.fps, s.p95_gap_ms);
    }
    report("R1-scroll-fps", "delivery", "fps", ">= 55", |m| m >= 55.0, &fps);
    report("R1-scroll-drawn-fps", "drawn", "fps", ">= 55", |m| m >= 55.0, &drawn);
    report("R1-scroll-acked-fps", "delivery", "fps", "", |_| true, &acked);
    report("R1-scroll-p95-gap", "delivery", "ms", "<= 20", |m| m <= 20.0, &gap95);
    report("R6-cpu-scrolling", "host+helpers", "% of a core", "", |_| true, &cpu);
    if !blit.is_empty() {
        report("A4-blit-p95", "host copy incl. GPU wait", "ms", "", |_| true, &blit);
    }
}

/// R2: video playback per clip: delivered fps vs source fps, and decode success.
fn r2(server: &fixtures::Server) {
    for clip in ["test-30-vp9.webm", "test-60-vp9.webm", "test-30-h264.mp4", "test-60-h264.mp4"] {
        let source: f64 = if clip.contains("-60-") { 60.0 } else { 30.0 };
        let (mut ratio, mut decoded_fps) = (vec![], vec![]);
        let mut error = None;
        for _ in 0..runs().min(3) {
            let mut p = Page::open(Host::Cef, &server.url(&format!("video.html?src={clip}"))).expect("CEF host");
            p.run_for(Duration::from_millis(800));
            let before = p.eval("videoStats()").unwrap();
            if !before["error"].is_null() {
                error = Some(before["error"].to_string());
                break;
            }
            p.clear_frames();
            let start = Instant::now();
            paced(&mut p, Duration::from_secs(4), |_, _| {});
            let s = p.frame_stats(start);
            let after = p.eval("videoStats()").unwrap();
            let d = |v: &serde_json::Value| v["decoded"].as_f64().unwrap_or(0.0);
            let dt = after["time"].as_f64().unwrap_or(0.0) - before["time"].as_f64().unwrap_or(0.0);
            if dt <= 0.1 {
                error = Some(format!("not playing: {after}"));
                break;
            }
            ratio.push(s.fps / source * 100.0);
            decoded_fps.push((d(&after) - d(&before)) / start.elapsed().as_secs_f64());
        }
        match error {
            Some(e) => println!("RESULT R2-{clip} [decode] FAILS: {e}"),
            None => {
                report(&format!("R2-{clip}-delivery"), "delivery vs source", "% of source fps", ">= 95", |m| m >= 95.0, &ratio);
                report(&format!("R2-{clip}-decoded"), "page decoded", "fps", "", |_| true, &decoded_fps);
            }
        }
    }
}

/// R3: CSS animation and requestAnimationFrame vs frames delivered.
fn r3(server: &fixtures::Server) {
    let (mut anim, mut raf, mut raf_delivery) = (vec![], vec![], vec![]);
    for _ in 0..runs() {
        let mut p = Page::open(Host::Cef, &server.url("animation.html")).expect("CEF host");
        p.run_for(Duration::from_millis(500));
        p.clear_frames();
        let start = Instant::now();
        paced(&mut p, Duration::from_secs(3), |_, _| {});
        anim.push(p.frame_stats(start).fps);
        drop(p);

        let mut p = Page::open(Host::Cef, &server.url("raf.html")).expect("CEF host");
        p.run_for(Duration::from_millis(500));
        p.clear_frames();
        let c0 = p.eval("rafCount").unwrap().as_f64().unwrap_or(0.0);
        let start = Instant::now();
        paced(&mut p, Duration::from_secs(3), |_, _| {});
        let c1 = p.eval("rafCount").unwrap().as_f64().unwrap_or(0.0);
        raf.push((c1 - c0) / start.elapsed().as_secs_f64());
        raf_delivery.push(p.frame_stats(start).fps);
    }
    report("R3-css-animation-fps", "delivery", "fps", ">= 55", |m| m >= 55.0, &anim);
    report("R3-raf-page-fps", "page rAF", "fps", ">= 55", |m| m >= 55.0, &raf);
    report("R3-raf-delivery-fps", "delivery", "fps", ">= 55", |m| m >= 55.0, &raf_delivery);
}

/// A page that changes colour on every mousedown and keydown (input-specific visual marker).
const MARKER: &str = "data:text/html,<style>html,body{margin:0;height:100%25;background:%23000}</style><script>let n=0;const c=['%23f00','%230f0','%2300f','%23ff0'];const f=()=>document.body.style.background=c[n++%254];addEventListener('mousedown',f);addEventListener('keydown',f)</script>";

/// R4: click and key to the first delivered frame whose pixels changed (hashing on).
fn r4() {
    let (mut click, mut key, mut click_drawn) = (vec![], vec![], vec![]);
    for _ in 0..runs() {
        let mut p = Page::open(Host::Cef, MARKER).expect("CEF host");
        p.hash_frames(true);
        p.click(50.0, 50.0); // focus the page
        p.run_for(Duration::from_millis(500));
        for i in 0..20 {
            let last = p.frames().last().and_then(|f| f.hash);
            let t = Instant::now();
            if i % 2 == 0 { p.click(100.0, 100.0) } else { p.key("a") }
            let mut hit = None;
            p.wait(Duration::from_secs(2), |p| {
                hit = p.frames().into_iter().find(|f| f.arrived >= t && f.hash.is_some() && f.hash != last);
                hit.as_ref().is_some_and(|f| f.painted.is_some())
            });
            let f = hit.expect("no changed frame within 2 s");
            let ms = (f.arrived - t).as_secs_f64() * 1e3;
            if i % 2 == 0 {
                click.push(ms);
                click_drawn.push((f.painted.unwrap() - t).as_secs_f64() * 1e3);
            } else {
                key.push(ms);
            }
            p.run_for(Duration::from_millis(100));
        }
    }
    let row = |id: &str, label: &str, v: &mut Vec<f64>| {
        let (p50, p95) = (pct(v, 0.5), pct(v, 0.95));
        println!("RESULT {id} [{label}] p50={p50:.1} p95={p95:.1} max={:.1} ms n={} target=p95 <= 50 {}", v.last().unwrap(), v.len(), if p95 <= 50.0 { "PASS" } else { "MISS" });
        metric(&format!("{id}-p95"), p95, "ms", "<= 50");
    };
    row("R4-click-latency", "delivery", &mut click);
    row("R4-click-latency-drawn", "drawn", &mut click_drawn);
    row("R4-key-latency", "delivery", &mut key);
}

/// R5: resize to the first frame at the exact new size, and 2 s of continuous resize.
fn r5(server: &fixtures::Server) {
    let (mut first, mut during, mut blanks) = (vec![], vec![], 0usize);
    for _ in 0..runs() {
        let mut p = Page::open(Host::Cef, &server.url("scroll.html")).expect("CEF host");
        p.resize_block(Some((800.0, 600.0)));
        p.run_for(Duration::from_millis(500));
        // Backing scale from the frame the 800 DIP wide block got.
        let scale = p.frames().last().map_or(2.0, |f| f.size.w as f32 / 800.0);
        for (w, h) in [(700.0, 500.0), (900.0, 650.0), (600.0, 450.0)] {
            let t = Instant::now();
            p.resize_block(Some((w, h)));
            let (pw, ph) = ((w * scale).ceil() as u32, ((h - crate::browser::view::CHROME_HEIGHT) * scale).ceil() as u32);
            let f = p.wait_frame_size(pw, ph, Duration::from_secs(2)).unwrap_or_else(|| panic!("no {pw}x{ph} frame"));
            first.push((f.arrived - t).as_secs_f64() * 1e3);
        }
        p.clear_frames();
        let start = Instant::now();
        paced(&mut p, Duration::from_secs(2), |p, n| {
            let w = 600.0 + (n % 60) as f32 * 5.0;
            p.resize_block(Some((w, 500.0)));
            if n % 30 == 15 {
                let img = p.shot();
                if harness::colours(&img) < 3 {
                    blanks += 1;
                }
            }
        });
        during.push(p.frame_stats(start).fps);
    }
    report("R5-resize-first-frame", "delivery", "ms", "<= 100", |m| m <= 100.0, &first);
    let host_side = host_stat_values("resize_to_paint_ms=");
    if !host_side.is_empty() {
        report("R5-host-resize-to-paint", "host: Resize received to first paint at that size", "ms", "", |_| true, &host_side);
    }
    let mut f = first.clone();
    println!("RESULT R5-resize-first-frame-p95 [delivery] p95={:.1} ms", pct(&mut f, 0.95));
    report("R5-continuous-resize-fps", "delivery", "fps", "", |_| true, &during);
    println!("RESULT R5-blank-shots [drawn] blank={blanks} of {} shots during resize {}", runs() * 4, if blanks == 0 { "PASS" } else { "MISS" });
}

/// R6: idle CPU on a static page, and RSS.
fn r6(server: &fixtures::Server) {
    let (mut idle, mut rss, mut procs) = (vec![], vec![], vec![]);
    for _ in 0..runs().min(3) {
        let mut p = Page::open(Host::Cef, &server.url("form.html")).expect("CEF host");
        p.run_for(Duration::from_secs(2));
        let u = p.usage_over(Duration::from_secs(5));
        idle.push(u.cpu);
        rss.push(u.rss_mb);
        procs.push(u.processes as f64);
    }
    report("R6-cpu-idle-1-tab", "host+helpers", "% of a core", "<= 2", |m| m <= 2.0, &idle);
    report("R6-rss-1-tab", "host+helpers", "MB", "", |_| true, &rss);
    report("R6-processes-1-tab", "host+helpers", "count", "", |_| true, &procs);
    // More tabs: more browsers in the same host, opened the way agents open unshown pages.
    for tabs in [3usize, 5] {
        let mut p = Page::open(Host::Cef, &server.url("form.html")).expect("CEF host");
        let mut opened = 1;
        for _ in 1..tabs {
            match p.command("browser.open", json!({ "url": server.url("scroll.html") })) {
                Ok(_) => opened += 1,
                Err(e) => {
                    println!("RESULT R6-{tabs}-tabs not measured: browser.open in the harness: {e}");
                    break;
                }
            }
        }
        if opened < tabs {
            continue;
        }
        p.run_for(Duration::from_secs(3));
        let u = p.usage_over(Duration::from_secs(5));
        println!("RESULT R6-{tabs}-tabs [host+helpers] cpu_idle={:.1}% rss={:.0} MB processes={}", u.cpu, u.rss_mb, u.processes);
    }
}

/// CPU seconds (user + system) so far of the host serving `p` and every process under it.
fn cpu_secs(p: &Page) -> f64 {
    let out = std::process::Command::new("ps").args(["-Ao", "pid=,ppid=,time=,command="]).output().unwrap();
    let rows: Vec<(u32, u32, f64, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let (pid, ppid, t) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?);
            // [[dd-]hh:]mm:ss.cc
            let (days, rest) = t.split_once('-').map_or((0.0, t), |(d, r)| (d.parse().unwrap_or(0.0), r));
            let secs = days * 86400.0 + rest.split(':').filter_map(|x| x.parse::<f64>().ok()).fold(0.0, |a, x| a * 60.0 + x);
            Some((pid, ppid, secs, it.collect::<Vec<_>>().join(" ")))
        })
        .collect();
    let sock = p.dir.join("host.sock").to_string_lossy().into_owned();
    let mut pids: Vec<u32> = rows.iter().filter(|r| r.3.contains(&sock)).map(|r| r.0).collect();
    let mut i = 0;
    while i < pids.len() {
        let parent = pids[i];
        pids.extend(rows.iter().filter(|r| r.1 == parent).map(|r| r.0));
        i += 1;
    }
    rows.iter().filter(|r| pids.contains(&r.0)).map(|r| r.2).sum()
}

/// p95 of the host's copy cost from the newest `host-stats.log` summary of this process's pages.
fn blit_p95() -> Option<f64> {
    let pid = std::process::id();
    let mut best: Option<(std::time::SystemTime, f64)> = None;
    for e in std::fs::read_dir("/tmp").ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&format!("hp-{pid}-")) {
            continue;
        }
        let path = e.path().join("host-stats.log");
        let (Ok(meta), Ok(text)) = (std::fs::metadata(&path), std::fs::read_to_string(&path)) else { continue };
        let Some(v) = text.lines().rev().find_map(|l| l.split_whitespace().find_map(|w| w.strip_prefix("p95_ms=")?.parse::<f64>().ok())) else { continue };
        let t = meta.modified().ok()?;
        if best.is_none_or(|(b, _)| t > b) {
            best = Some((t, v));
        }
    }
    best.map(|(_, v)| v)
}

/// Every `key=value` sample in this process's pages' host-stats.log files.
fn host_stat_values(key: &str) -> Vec<f64> {
    let pid = std::process::id();
    let mut out = vec![];
    for e in std::fs::read_dir("/tmp").into_iter().flatten().flatten() {
        if !e.file_name().to_string_lossy().starts_with(&format!("hp-{pid}-")) {
            continue;
        }
        let text = std::fs::read_to_string(e.path().join("host-stats.log")).unwrap_or_default();
        out.extend(text.lines().flat_map(|l| l.split_whitespace()).filter_map(|w| w.strip_prefix(key)?.parse::<f64>().ok()));
    }
    out
}

fn main() {
    // SAFETY: before any thread is started.
    unsafe { std::env::set_var("HERDR_BROWSER_STATS", "1") };
    let server = fixtures::Server::start();
    println!("R runs: {} per number; offscreen, harness pumping at 60 Hz", runs());
    if wanted("R1") { r1(&server) }
    if wanted("R2") { r2(&server) }
    if wanted("R3") { r3(&server) }
    if wanted("R4") { r4() }
    if wanted("R5") { r5(&server) }
    if wanted("R6") { r6(&server) }
    println!("browser perf done");
}
