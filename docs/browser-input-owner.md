# Browser block: who owns input (OWN)

Review finding C3 (Astra, 2026-10-30): "swallow `Input.dispatch*` while picking" gates the wrong path. The user's input never goes through CDP; it goes `GPUI event -> BrowserInput -> AppToHost::Input`. So the gate lives in `src/browser/input.rs`, before anything is sent to the host.

## Owners
One stack per browser block, top owns input, `Browse` is the floor.

| Owner | Pointer | Wheel | Keys | Text / IME | Esc | Undo, redo, clipboard |
|---|---|---|---|---|---|---|
| Browse | page | page | page | page | page | page (`AppToHost::Edit`) |
| Pick | pick overlay; page while Space is held | page | dropped (Space, Esc, chords handled) | dropped | closes Pick | overlay `Edit` |
| Comment | comment overlay | dropped | comment field (Backspace) | comment field, its own composition | closes Comment | undo and redo in the field; others to overlay `Edit` |
| Markup | markup overlay; page while Space is held | page only with Space | dropped | dropped | closes Markup | overlay `Edit` (stroke undo) |
| Agent lease | refused, cue | refused, cue | refused, cue | refused, cue | refused, cue (does not end the lease) | refused, cue |

Transitions (`BrowserInput::enter`): Browse to Pick or Markup; Pick or Markup to Comment; Pick and Markup swap in place. Everything else is refused. `escape` pops one layer. `grant_lease(agent)` pushes the lease on top of whatever is open; `end_lease` (Take Control, or the agent finishing) returns to the layer it covered.

## Invariants
- Leaving Browse releases every key and button the page holds and cancels the page's composition, so nothing started in the page continues under another owner.
- Leaving Comment drops its open composition; a comment's text, marked text and undo history never reach the page.
- Releasing Space releases any button the page got while Space was held.
- A button-up for a press the page saw always reaches the page, whoever owns input then.
- Herdr keybindings still match before any of this runs (GPUI dispatch order).

## Hooks (view.rs, owned by part 1)
- The mode chord or toolbar button calls `input.enter(Owner::Pick)` (and later Markup, Comment).
- `input.take_overlay()` after each event batch: `Pointer` goes to the pick or markup slice, `Refused` flashes the Take Control strip, `Owner` re-renders the mode chrome.
- When `input.lease()` is `Some(agent)`, the block shows a visible strip "<agent> is driving · Take Control"; the button calls `input.end_lease()`.
- Agent commands that drive the page (agent.rs) call `grant_lease(agent)` before acting and `end_lease()` when done or revoked.

## Tests
`cargo test --locked --bin herdr-app browser::input`: the transition table, Esc layering, pick routing with Space-to-browse, IME in each owner, and lease refusal, all through the GPUI listeners and the text input handler with a recording `HostLink`.

## Limits
- Copy, paste and select-all inside the comment field are passed to the overlay (`Edit`) for the comment slice to implement; only undo and redo are handled here.
- The pointer in Comment goes to the comment overlay; clicking the page to leave the field is the comment slice's call (it can `escape`).

## Known limits (input)
- Key repeat: the app sends `repeat: true` and the host sets `EVENTFLAG_IS_REPEAT`, but CEF off-screen on macOS drops it: held keys still repeat characters, only pages that read `KeyboardEvent.repeat` see `false`. `tests/browser_input.rs` prints `KNOWN-LIMIT I8`; confirm by hand in the visible session.
- Native key codes are derived from the key name for a US layout (GPUI does not expose them).
