use std::path::PathBuf;

fn main() {
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
        eprintln!("usage: herdr-browser-host --socket <path> --stub");
        std::process::exit(2);
    };
    if !stub {
        eprintln!("herdr-browser-host: the CEF host is not built yet; pass --stub");
        std::process::exit(2);
    }
    if let Err(e) = herdr_browser_host::serve(&socket, herdr_browser_host::IoSurfaces) {
        eprintln!("herdr-browser-host: {e}");
        std::process::exit(1);
    }
}
