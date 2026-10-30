# Browser agent tools vs Cursor's (A2)

Cursor's set comes from `extensions/cursor-browser-automation/dist/extension.js`, as listed in the Hub note `browser-vs-cursor-t3`. "Ours" is `scripts/herdr-browser` (CLI, MCP `browser_*`). "Tested" names the check in `scripts/herdr-browser/selftest.agent.mjs`. Every tool also takes `viewId` (our `tab`) and `take_screenshot_afterwards`; there's a check for that too.

| Cursor tool (required*) | Ours | Same parameters | Tested |
|---|---|---|---|
| `browser_navigate(url*, viewId, position)` | yes | yes (`position`: `side` opens beside, `active` a tab) | open loads the page; toggle on lets the agent drive (navigate) |
| `browser_tabs(action*: list/new/close/select, index, position)` | yes | yes | tabs list marks the selected tab; close removes the agent tab |
| `browser_snapshot(viewId, interactive, maxDepth, compact, selector)` | yes | yes | snapshot has refs; compact snapshot |
| `browser_click(ref*, element, offsetX, offsetY, doubleClick, button)` | yes | yes (`element` is a description, not used to pick) | click the button by ref |
| `browser_mouse_click_xy(x*, y*, button)` | yes | yes | mouse_click_xy |
| `browser_type(ref*, text*, element, clear, submit, slowly)` | yes | yes | type into the textbox by ref |
| `browser_fill(ref*, value, element)` | yes | yes | fill sets the value and fires input |
| `browser_select_option(ref*, values*)` | yes | yes | select_option by label |
| `browser_press_key(key*)` | yes | yes | press_key; press Tab then Enter |
| `browser_scroll(ref, direction, amount, deltaX, deltaY, scrollIntoView)` | yes | yes | scroll by direction |
| `browser_drag(sourceRef, targetRef, targetX, targetY)` | yes | yes | drag between refs |
| `browser_get_bounding_box(ref*)` | yes | yes | get_bounding_box |
| `browser_highlight(ref*, durationMs)` | yes | yes | highlight (A4 adds a pixel check) |
| `browser_take_screenshot(type, filename, element, ref, fullPage)` | yes | yes | element screenshot by ref; full-res PNG |
| `browser_lock(action*: lock/unlock)` | yes | yes; on the app it holds the block's input lease (`browser.lock`) | lock sets the lock flag (app lease: unit test `lock_goes_to_the_window_as_a_lease`) |
| `browser_cdp(method, params)` | yes, brokered | yes; needs the person's `cdp` grant and an allowed method | X2 checks |
| `browser_hover` (seen as a string only) | yes | `ref*`, `element` (Cursor's shape is unverified) | hover reaches the element |
| `browser_fill_form` (seen as a string only) | yes | `fields*: [{ref, value}]` (Cursor's shape is unverified) | fill_form fills a textbox and a select |

Ours only: `text`, `wait`, `console`, `network`, `eval`, `close`, `open` (T3/Playwright-style extras), the `herdr-app do browser.*` commands, and the person's grants.
