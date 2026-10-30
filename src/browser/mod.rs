//! The browser block: a real Chromium (CEF, off-screen) painted inside a layout block.
//! Skeleton from the lead: the seams between the five parts. Read docs/browser-block-contract.md.
//!
//! One owner per file (see the ownership table in the contract):
//! - `view.rs`  part 1: `BrowserView`, the host connection (`HostLink`), state, block lifecycle
//! - `frame.rs` part 3: `FrameSink`: IOSurface frames painted by GPUI
//! - `input.rs` part 4: GPUI events and IME to protocol `InputEvent`s
//! - `agent.rs` part 5: `herdr-app do browser.*`, the CDP port file
//! Parts talk to each other only through the traits below and the protocol crate.
#![allow(dead_code)]

pub mod agent;
// HERDR: part 4, design slice S7.
pub mod annotate;
pub mod dialogs;
// HERDR: part 4, design slice S11.
pub mod edits;
pub mod frame;
pub mod geometry;
pub mod input;
// HERDR: part 4, design slice S9.
pub mod markup;
// HERDR: part 4, design slice S10 (panel model).
pub mod styles;
pub mod view;

pub use herdr_browser_protocol as protocol;
use herdr_browser_protocol::{AppToHost, BrowserId, PixelSize, Size, SurfaceRef};

/// The connection to `herdr-browser-host`. Part 1 implements it (socket, spawn, restart on crash);
/// parts 4 and 5 send through it and never open the socket themselves.
pub trait HostLink: Send + Sync {
    /// Queue a message for the host. Messages are delivered in order; sends never block the UI.
    fn send(&self, msg: AppToHost);
}

pub use frame::{FrameSink, FrameView};

/// What a browser block tells the rest of the app about itself (part 1 fills it from host events).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BrowserState {
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub can_back: bool,
    pub can_forward: bool,
    pub cursor: String,
}

/// Looks up a live browser by block id; part 1 provides it, part 5 uses it for `browser.*` commands.
pub trait BrowserRegistry {
    fn state(&self, id: &BrowserId) -> Option<BrowserState>;
    fn link(&self) -> &dyn HostLink;
}
