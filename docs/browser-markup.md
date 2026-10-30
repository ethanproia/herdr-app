# Browser block: markup (design slice S9)

`src/browser/markup.rs`: pen, rectangle, ellipse and arrow drawn by the app over the page (a GPUI canvas), with undo and redo, burned into a PNG for the agent. Reference style: red `#ff3b30`, 3 px.

- Input only through `BrowserInput` while the owner is `Owner::Markup` (docs/browser-input-owner.md). The view drains `input.take_overlay()` into `Markup::on_event`: pointer events draw, cmd-z / cmd-shift-z undo and redo, leaving markup (Esc, an agent lease) drops a stroke in progress. A click without a drag draws nothing.
- Shapes are DIPs from the page's (or the crop's) top-left. `burn(base, scale)` stamps solid pixels (no blending), so no shapes gives `base` byte for byte.

## Hooks for view.rs (part 1)
1. Hold `markup: Entity<Markup>` per block.
2. A toolbar toggle or chord calls `input.enter(Owner::Markup)`; tool buttons set `markup.tool`.
3. After input events, `for ev in input.take_overlay() { markup.on_event(&ev) }`, then `cx.notify()` if it returned true.
4. In render, add `markup::element(markup.clone())` as the last child of the page (above the frame) while the owner is Markup or shapes exist.
5. On "attach" or send: take the page screenshot (or crop), call `markup.burn(&image, scale)` and then `markup::png(..)`.

## Tests
`cargo test --locked --bin herdr-app browser::markup`: rect then undo gives the unmarked crop byte for byte (patterned fixture); red pixel count equals the outline geometry (960 at 1x, about 4x at 2x) and only those pixels change; negative regions inside and outside are untouched; ellipse against Ramanujan's perimeter; arrow head and pen placement; clicks and other owners draw nothing; driven through `BrowserInput` in Markup mode, the page gets none of it.

## Limits
- The on-screen canvas uses GPUI's antialiased stroke; the burn is aliased solid pixels, so they match in shape, not pixel for pixel.
- No pixel test of the on-screen canvas over a live page yet (needs the slot); no shift-constrained shapes or colour picker.
