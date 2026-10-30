// The browser parity harness, shared by the browser test files (each is its own crate, run on
// the main thread with `harness = false`). Include it at the top of a test file:
//
//     include!("common/browser_harness.rs");
//
// It mounts the app's browser code (src/browser, src/layout.rs) with small stand-ins for the
// rest of the app, and adds `harness` (the page driver) and `fixtures` (a local HTTP server for
// tests/fixtures/browser). See docs/browser-parity-harness.md.

#[path = "../../src/layout.rs"]
#[allow(dead_code)]
mod layout;
#[path = "../../src/browser/mod.rs"]
#[allow(dead_code)]
mod browser;
#[path = "harness.rs"]
#[allow(dead_code)]
mod harness;
#[path = "fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

/// The app's settings.rs needs the whole app; the block only reads the search engine.
#[allow(dead_code)]
mod settings {
    #[derive(Clone, Copy)]
    pub enum SearchEngine {
        Google,
    }
    impl SearchEngine {
        pub fn url(self, q: &str) -> String {
            format!("https://www.google.com/search?q={q}")
        }
    }
    pub struct Settings {
        pub search_engine: SearchEngine,
    }
    pub fn get() -> Settings {
        Settings { search_engine: SearchEngine::Google }
    }
}

#[allow(dead_code)]
mod mods {
    pub fn app_dir() -> std::path::PathBuf {
        std::env::temp_dir()
    }
}
