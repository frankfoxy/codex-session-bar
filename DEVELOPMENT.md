# Development and troubleshooting

The default runtime is the Rust executable in `bin/codex-session-bar.exe`.
The earlier Python sources remain as migration reference; launchers and the
scheduled task use Rust and do not start Python, Qt, a WebView or an independent
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

`live-status.json` records runtime, PID, visible identity, quota, caches, font
selection and docking bounds. Read bounded stderr excerpts for desktop errors.
Never commit runtime diagnostics, credentials, settings, local databases,
rollouts or live screenshots. README images must use sanitized fixtures.

`emoji_catalog.json` preserves the Unicode Emoji 18.0 entries and their license.
The first page is the original 90-item 默认集合. The full catalog is filtered and
virtualized as a 90-cell viewport with scroll, category and search controls.
Random selection excludes the current icon. Prefix matching is longest-first
so flags, skin tones, keycaps and ZWJ sequences remain intact during renames.
