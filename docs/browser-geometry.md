# Browser block geometry (finding C2)

Everything the app draws over a page (element highlight, pick, comment pins, markup, find
matches, the IME caret) converts coordinates through `src/browser/geometry.rs` and nothing else.

## Spaces
| space | origin | unit | from the previous |
|---|---|---|---|
| frame CSS px | an (i)frame's viewport | CSS px | what `getBoundingClientRect()` returns in that frame |
| viewport CSS px | the top frame's viewport | CSS px | + each enclosing iframe's content-box offset, outermost first (offsets sum) |
| page DIPs | the page area's top-left (under the 28 DIP bar) | DIP | x zoom (Chrome level L: 1.2^L) |
| window points | GPUI window | point | + the page area's origin in the window |
| backing pixels | window or frame | pixel | x scale factor (2 on Retina); a frame is page size x scale |

The page's `devicePixelRatio` is zoom x scale. Input events (`InputEvent`) are in page DIPs.
A page rect is clipped to the page area; a rect scrolled out of view is empty and not drawn.

## Staleness: epochs
The app numbers content-moving changes (resize, scroll, zoom, navigation) with an epoch
(`OverlayState::changed`). Each overlay request carries the epoch it was made in; an answer from
an older epoch is dropped, and anything shown from an older epoch hides at the change, until a
fresh answer arrives. Scroll and zoom changes reach the app as host events (below).

## Hover
One hit test in flight per block; pointer moves during it replace a single waiting point, sent
when the answer arrives (`OverlayState::hover` / `next_hover`). A lost answer (host restart)
releases the slot (`hover_lost`).

## Protocol (proposed; lead owns the crate)
- `AppToHost::Overlay { id, req, epoch, query }`, `query`: `HitTest { x, y }` (viewport CSS px),
  `Selector { selector }`, `Selection`.
- `HostToApp::OverlayRects { id, req, epoch, rects, frames }`: rects in the answering frame's CSS px,
  `frames` the enclosing iframe offsets outermost first; `epoch` echoed unchanged.
- `HostToApp::Viewport { id, scroll_x, scroll_y, zoom_level }`: sent on scroll and zoom changes
  (coalesced to at most one per frame); the app bumps the epoch on it.

## Tests
`cargo test --bin herdr-app browser::geometry`: zoom levels, CSS to window through zoom and
origin, backing and frame pixels, iframe offsets, clipping, window to CSS round trip, stale
answers dropped and a stale selection hidden, hover coalescing.
