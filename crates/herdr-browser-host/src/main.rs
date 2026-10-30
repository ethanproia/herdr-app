use std::path::PathBuf;

fn main() {
    // CEF helper processes are this same binary started with `--type=...`; they take CEF's
    // arguments, so hand them over before parsing ours.
    #[cfg(all(feature = "cef", target_os = "macos"))]
    if std::env::args().any(|a| a.starts_with("--type=")) {
        std::process::exit(herdr_browser_host::cef::main(None));
    }
    let mut socket = None;
    let mut stub = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = args.next().map(PathBuf::from),
            "--stub" => stub = true,
            other => {
                eprintln!("herdr-browser-host: unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    let Some(socket) = socket else {
        eprintln!("usage: herdr-browser-host --socket <path> [--stub]");
        std::process::exit(2);
    };
    if stub {
        if let Err(e) = herdr_browser_host::serve(&socket, herdr_browser_host::IoSurfaces) {
            eprintln!("herdr-browser-host: {e}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(all(feature = "cef", target_os = "macos"))]
    std::process::exit(herdr_browser_host::cef::main(Some(&socket)));
    #[cfg(not(all(feature = "cef", target_os = "macos")))]
    {
        eprintln!("herdr-browser-host: built without the `cef` feature; pass --stub");
        std::process::exit(2);
    }
}
