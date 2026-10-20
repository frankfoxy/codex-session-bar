# Development and troubleshooting

The default runtime is the Rust executable in `bin/codex-session-bar.exe`.
Only the Rust runtime is maintained. Launchers and the scheduled task do not
start Python, Qt or a WebView. Status polling does not start an independent
Codex app-server.

## Build and verification

Use Windows, the Rust MSVC toolchain and Visual Studio C++ Build Tools.
`Build.ps1` places Cargo caches, targets and temporary files in the enclosing
workspace's direct `tmp/session-bar-rust-build` directory. It uses the installed
compiler directly, without asking rustup to update a toolchain.

```powershell
.\Build.ps1 -Check
.\bin\codex-session-bar.exe --self-test
.\bin\codex-session-bar.exe --inspect
.\bin\codex-session-bar.exe --observe
```

`-Check` runs the unit suite before producing the release binary. `--self-test`
uses sanitized fixtures, hidden windows and software-rendered captures under
`tmp/codex-session-bar`. It verifies the home-page overview, fixed recent-list
order, emoji catalog/search, bound random-icon actions and separate drag area.
Inspect its PNGs before changing curated README previews. The other two probes
are read-only: `--inspect` checks real recent-chat and quota RPC responses;
`--observe` checks window identity, titlebar bounds and local usage. Neither
navigates chats, renames them, submits prompts or resumes work.

## Repository layout and releases

- `src/`: native Rust application.
- `companion/memory-watch/`: independent native monitor and Git enforcement backend.
- `assets/images/`: sanitized README screenshots and docking illustration.
- `emoji_catalog.json`: runtime emoji catalog.
- `Build.ps1`: build/test; `-NoInstall` keeps the installed executable untouched.
- `Package.ps1`: build/test and stage an allowlisted Windows x64 ZIP plus SHA-256.
- `RELEASING.md`: version, tag, publishing and upgrade steps.

`--version` prints the Cargo package version without opening the UI or creating runtime files.
Build output and release staging live under workspace `tmp`, not in Git.

## Codex Git guard

The Session Bar `guard.rs` only reads fresh, PID-matched Memory Watch state and writes its enable preference, preserving advanced limits. It can start a bundled companion hidden. The native monitor owns enumeration, user/session and ancestry checks, command classification, logging, termination and warning windows. Shared files live under workspace `tmp/memory-watch-rust`; heartbeat expiry is 15 seconds. A missing backend is shown as unavailable.

Enforcement runs every five seconds even when ordinary 30-second recording is paused. Two snapshots confirm a breach; only verified read-query Git handles are terminated, after a durable intent record. Creation-time validation prevents PID reuse errors. An executable name alone is insufficient; installed Codex path, account and Windows session are verified. Write/unknown commands are never killed. Shutdown/cancellation does not stop unrelated applications.

The default companion tests are read-only or synthetic. Its ignored controlled native test requires a deliberately built sleeping `git.exe` fixture under workspace `tmp`, supplied as `CODEX_GUARD_FIXTURE_EXE`; test-only PID/birth scoping confines termination to children the test owns. It checks disabling, real termination and log records, while leaving its `commit` fixture alive. No production scope override is compiled into release builds.

## Data and desktop integration

`data.rs` opens the local SQLite databases read-only with in-memory temporary
storage. Archived chats and subagents are excluded. Token records are parsed
from at most a 256 KiB tail, followed by bounded increments. Partial lines remain
unconsumed until completed; TPS samples must belong to the current turn and
expire after 30 seconds. Cached input is included in input totals.

`host.rs` uses a dedicated COM/UI Automation observer. Only window metadata,
the outer document title and header-control bounds are read; chat bodies are
not traversed. Exact titles must map uniquely to a local root chat. Home and
ambiguous pages display an account overview rather than guessing a background
chat. The titlebar observer excludes embedded browser documents and protects
native window buttons. A separate observer keeps header scans off the fast
chat-follow path.

`desktop.rs` reuses the desktop's bundled App Tools MCP adapter for recent
status, quota, navigation and title updates. Quota comes from `get_usage_limits`
on the existing desktop connection; no account-server or extra MCP tool servers
are started for it. Context metadata anchors transport without identifying the
visible page. Icon updates fetch the original chat title, preserve its text,
and require explicit desktop confirmation. Pending navigation is coalesced;
queued icon changes retain their original chat ID. Application errors are not
retried as transport errors.

Current usage, recent scans, account limits and explicit actions have separate
workers. Newly identified chats bypass the normal 500 ms usage refresh delay.
An open recent list freezes row IDs and order; updates merge live metrics by ID.
Reopening applies the latest Busy-first order. Busy totals cover all scanned
local roots, including those outside the ten displayed rows. Missing usage is
shown as unavailable, not zero. Failed refreshes retain previous data as stale.

## Rendering and window lifecycle

`ui.rs` uses native Win32 windows, software Direct2D and DirectWrite. Chinese
text uses Microsoft YaHei UI (Microsoft YaHei fallback); Latin text uses Segoe
UI and color emoji use Segoe UI Emoji. ClearType and per-monitor DPI awareness
avoid bitmap-font and whole-window scaling artifacts. Search and category
controls use native Windows widgets with a ClearType Chinese font.

The bar follows the Codex window without activation. Only the center grip
starts dragging. Saved offsets are host-relative and cannot cover header menu
controls. If the blank header is too narrow, docking falls below the menu.
Closing hides to tray and sets an explicit hidden flag; host movement cannot
restore a user-hidden bar. Quit terminates the companion and its own adapter
children. It never stops Codex or another task.

Preferences remain in ignored `settings.json`. Runtime files stay in the
workspace's direct `tmp/codex-session-bar` directory. Set
`CODEX_SESSION_BAR_WORKSPACE` to an existing workspace when needed. The named
Windows show event keeps the installed opening skill compatible with both
versions. The native mutex prevents duplicate Rust instances. The scheduled
task remains named `Codex Session Bar`; the launcher, tray and chat skill can
restore a hidden instance or start an exited one.

## Diagnostics and disclosure

Body matching is opt-in, while indexing prewarms automatically after startup
and checks for incremental updates 60 seconds after completion. Closing panels
or typing a title-only query does not cancel prewarming. One low-priority worker
handles both indexing and queries, with 200 ms debounce and latest-query
cancellation (including SQLite interrupts). An empty query indexes without
matching content; interactive Body queries take priority. The footer reports
index progress even with Body off. It prefers Codex's existing `thread_items` cache, selects only
user/agent messages, and resumes through the existing per-thread updated-ordinal
index. Changed messages replace their old indexed chunks; checkpoints keep tied
ordinals together. Local rollouts are the fallback, streamed through a 64 KiB
reader with complete-line offsets committed every 8 MiB. Only visible
user/assistant message text is retained.
Append-only updates resume at those offsets; truncation, replacement and same-size
rewrites invalidate the affected chat. Incomplete trailing events are retried.
The private cache is `tmp/codex-session-bar/body-search.sqlite`: FTS5 trigram,
`detail=none`, indexed LIKE for substrings and cached-text fallback for short words
or literal wildcard characters. The page cache is 2 MiB; the database cap is
256 MiB. Visible events above 4 MiB are counted as partial coverage; unavailable
local rollouts are reflected in the footer's local-body count. No body-pinyin
expansion is performed. Completed query results are published together; background
status refreshes keep the chosen result order stable. Cache data is never published.

Idle rows use `threads.tokens_used` and sum valid completed-minus-started durations
from `thread_turns`; gaps, missing endpoints and negative durations are excluded.
Both chat-list icon hit targets are separate from row navigation. The icon editor
captures the target ID and label on opening; choosing an icon never changes the
follow/fixed selection. Successful renames update all three row caches immediately.
List editing uses a separate owned icon window with independent search/category/
scroll and hit targets. The list HWND and child edit remain alive. Native combo
activation is inside the popover; Escape closes the popover before its parent.

List eligibility is the union of readable local rollout files and cached visible
user/agent messages. Metadata-only records are excluded from both lists and the
index denominator; percentages use indexed/local-chat count. The FTS cache keeps
normalized search text plus unindexed original text for snippets. Old normalized-
only caches rebuild once. One grouped indexed query returns the latest matching
chunk per chat, clipped to 150 characters around the first term. Highlight ranges
map Unicode case folds back to original UTF-8, then DirectWrite UTF-16 ranges.
Body summaries remain stable during background index-only refreshes.

Ctrl+Alt+J is a nonrepeating global hotkey registered against the bar window.
It restores Codex when needed and toggles the list. Registration errors are
recorded in `shortcut-status.json`; tests do not register a competing hotkey.

`live-status.json` records runtime, PID, visible identity, quota, caches, font
selection and docking bounds. Read bounded stderr excerpts for desktop errors.
Never commit runtime diagnostics, credentials, settings, local databases,
rollouts or live screenshots. README images must use sanitized fixtures.

`emoji_catalog.json` preserves the Unicode Emoji 18.0 entries and their license.
The first page is the original 90-item 默认集合. The full catalog is filtered and
virtualized as a 90-cell viewport with scroll, category and search controls.
Random selection excludes the current icon. Prefix matching is longest-first
so flags, skin tones, keycaps and ZWJ sequences remain intact during renames.

## UI interaction notes · 2026-10-20

- Recent panel: 560 × 436 logical pixels, compact icon switches and native Win32 tooltips. Tool rectangles and text are compared before updating, so the timer does not restart the hover delay on every paint.
- Icon popovers prefer the left side of the list, then the right side, then a monitor-bounded position. Title suggestions run locally using deterministic keyword rules and share the captured target ID used by random/clear actions.
- Native self-test verifies tooltip registration survives repeated refreshes and suggested icons remain bound to the selected list row.
