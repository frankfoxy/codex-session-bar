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
`%TEMP%/codex-session-bar` (tests redirect TEMP into workspace `tmp`). It verifies the home-page overview, fixed recent-list
order, emoji catalog/search, bound random-icon actions and separate drag area.
Inspect its PNGs before changing curated README previews. The other two probes
are read-only: `--inspect` checks real recent-chat and quota RPC responses;
`--observe` checks window identity, titlebar bounds and local usage. Neither
navigates chats, renames them, submits prompts or resumes work.

## Repository layout and releases

- `src/`: native Rust application.
- `src/guard/`: integrated Git enforcement, process attribution, warnings and compressed event logs.
- `companion/memory-watch/`: retired source reference; its build entry point is disabled and it is excluded from packages.
- `assets/images/`: sanitized README screenshots and docking illustration.
- `emoji_catalog.json`: runtime emoji catalog.
- `Build.ps1`: build/test; `-NoInstall` keeps the installed executable untouched.
- `Package.ps1`: build/test and save an allowlisted Windows x64 ZIP, SHA-256 and extracted portable folder under `releases/v<version>/`; temporary staging remains under workspace `tmp` and is removed after success.
- `RELEASING.md`: version, tag, publishing and upgrade steps.

`--version` prints the Cargo package version without opening the UI or creating runtime files.
Build output and release staging live under workspace `tmp`, not in Git.

## Automatic updates

Settings can request an immediate check through the same worker channel. Requests are coalesced while a check or installation is in progress, and manual checks can reoffer a postponed version. The last completed check time/result persist across restarts and are published to shared UI state; rendering never polls the status file. Download/confirmation phases do not change the last check timestamp.

`update.rs` checks this repository's latest stable GitHub release ten seconds after startup and every 30 minutes thereafter. A channel deadline keeps the worker asleep between checks; HTTPS uses native WinHTTP with ETags, bounded responses and timeouts, without an extra Git process or account credentials. Drafts, prereleases, equal versions and downgrades are ignored. Background and manual checks only update status and a compact green update icon in the bar; they never open the confirmation prompt. Clicking that icon sends a separate review request, refreshes release metadata and opens a Chinese/English Yes/No prompt with up to five brief highlights from the release body. Markdown is converted to plain text, each item is limited to 90 characters and missing notes have an explicit fallback. No additional release-notes request is needed. Declining keeps the icon available for later review. The prompt blocks neither the UI nor the Git guard, and concurrent review requests are coalesced.

Only an installed `bin/codex-session-bar.exe` starts the updater. Downloads, staging and backups stay under a verified workspace `tmp` child. The ZIP must match `SHA256SUMS.txt` and any GitHub asset digest. An embedded Windows PowerShell helper rejects unsafe ZIP paths, linked or duplicate files, unexpected files and oversized expansion, and checks the staged executable's version before the bar exits. It waits on the original process identity, backs up the allowlisted application files, verifies replacement and restarts. Partial failures restore originals; settings, tags and caches are outside the copy list. Diagnostics are in `update-status.json` and the update's temporary directory.

`--check-update` is a read-only network probe. `pwsh -NoProfile -File tests/Update-Installer.ps1` runs isolated installer fixtures under workspace `tmp`, covering replacement/restart, rollback after a locked-file failure, settings preservation, bad checksums, ZIP traversal and wrong executable versions. It does not replace or restart the real installation. PowerShell 7 is required only for this developer test; runtime installation uses built-in Windows PowerShell 5.1.

## Reset announcement sources

`reset_notice.rs` reads public announcement data, independently of account quota and banked-credit expiry. The default source is Codex Resets (`https://codex-resets.com/api/v1/status`, plus its public localized `/api/resets?locale=zh-CN` feed for translations/history); AI Sentinel (`https://divin.cc/api/events`) remains selectable. Both are third-party trackers of public team posts. Attribution and source links remain in the tooltip and clickable badge; no account credentials, private social APIs or translation-model calls are used.

The bell is orange within three hours before a future explicit timestamp, green for three hours after a reported reset on the same local calendar day, and gray otherwise. Known later forecasts may retain a muted countdown. Deadline/approximation qualifiers remain visible; unknown assumed timezones, probability watches, unrelated credit grants and completed records cannot become a precise future reset. Reaching a forecast timestamp does not prove execution or turn the bell green. Six-hour stale cache entries cannot produce an active badge. Selected post text and up to three historical notices are bounded; observed records are labelled as site observations.

The worker waits on its configuration channel between checks (five minutes by default, configurable 1–60). Source switches immediately clear shared state and queued reminders; stale in-flight results are discarded. Primary `latest_reset` records remain usable even when the translation endpoint fails. `reminder.rs` detects fresh forecasts and newly reported completions (24-hour catch-up limit), queues nonblocking Windows tray notifications and persists deduplication after successful submission. Windows quiet-hours settings can still suppress a submitted notification; the same-day green badge remains independent of notification delivery. The UI clones an `Arc<View>` and performs no announcement-file polling. `--inspect-reset-notice` (Codex Resets) and `--inspect-reset-notice --divin` are read-only network probes. Native self-tests use synthetic notices without opening a browser or starting live tracking.

Scanning diagnostics are retained under system `%TEMP%/codex-session-bar/reset-notice-checks.jsonl`: request start/end, selected source/API, timestamps, result/error, discovered forecast/completion IDs and notification submission outcomes. At 128 KiB the log rotates into four gzip archives (`reset-notice-checks.1.gz` through `.4.gz`); no response bodies, credentials or chat contents are logged. Version 0.3.0 only retained an overwritten latest cache, so historical per-check timing cannot be reconstructed retroactively.

## Capacity-error continuation extension

`capacity_retry.rs` consumes the existing live `Recent` metadata projection. It does not poll conversation bodies or start another Codex server. The extension is off by default; its delay defaults to five seconds and is configurable from 1–300 seconds in Settings → Extensions. The metadata worker checks bounded `thread_turns.error_json` only for failed chats, using the existing per-thread index. Cached projections omit these failures, so an old cache cannot start a chat.

A candidate must be the latest failed turn and have structured `serverOverloaded` information (or the exact legacy capacity message without another error code). A currently loaded chat explicitly reporting `systemError` receives a fresh observation timestamp even if its original failure is older than 15 minutes; inactive historical failures keep the 15-minute cutoff. The read-only history query uses turn timestamps before rollout ordinals, because ordinals can restart. When a live error has no usable local failure record, a bounded latest-turn App Tools read classifies it instead: results are cached for 30 seconds, metadata changes invalidate them and failed reads retry after five seconds. Normal chats add no such probes.

Immediately before submission, the action worker calls `read_thread` for only the latest turn with tool outputs omitted, and checks the chat ID, turn ID, failed status and capacity error. Active, recovered, interrupted or changed turns cancel the request. A transport failure schedules another verification after five seconds without claiming delivery. Disabled settings and outdated configuration epochs also cancel requests. The scheduler waits on the existing source signal; its one-second deadline checks memory only. Actual submission can be later than the configured delay while metadata or the desktop connection is unavailable.

App Tools does not expose an in-place retry operation. With the user's consent, the extension calls `send_message_to_thread` once with `继续上次任务`, the original thread ID and local host, omitting model and reasoning overrides. This adds a user message and uses the chat's current configuration. A bounded journal claims the failed turn before the call; corrupt or unwritable journals block submission. The latest claimed turn per chat remains durable without expiring old unconfirmed submissions. Transport failures and ambiguous acknowledgements after submission are not replayed automatically for that failed turn, even across restarts. A subsequent failed turn is a new candidate.

The extension row includes a retry-count badge and log button, both opening the shared native log viewer without toggling the extension. Counts represent actual submission attempts, once per failed turn; cancelled waits are excluded. Counters and the start time are saved with the dedup journal before sending, then results are saved after acknowledgement. Restarting retains totals; claims without a confirmed outcome remain unconfirmed. Legacy journals retain their dedup identities but start the new counters at the first retry after upgrading; older logs are visible without claiming a complete historical total.

Diagnostics are in system `%TEMP%/codex-session-bar/capacity-retry-events.jsonl`, recording queueing, skips, preflight failures, retry numbers, configured delays, submission and unconfirmed outcomes with chat/turn identifiers. A mutex serializes writes and rotation. At 128 KiB the log rotates into four gzip archives. The viewer reads at most a 64 KiB tail on open or explicit refresh, ignores partial records, shows up to 64 recent events and exposes full IDs/reasons in hover text; it performs no periodic log polling. Prompt bodies, authentication and conversation contents are not logged. Unit tests use isolated databases and mock submission callbacks; native self-tests exercise the settings and viewer without starting this extension or sending live messages.

## Codex Git guard

Git protection runs inside the Session Bar process. `guard.rs` owns settings and the status view; `guard/worker.rs` runs enforcement on a dedicated worker. Warnings use a separate thread, so an unanswered dialog or a full notification queue cannot pause enforcement. There is no standalone Memory Watch executable, monitor window, second tray icon, whole-system sampler or periodic sample log. The lightweight collector retains only process identities, ancestry and private bytes needed for Git checks; it performs no CPU history, grouping or memory-history recording.

Configuration, episode counters and event logs use `%TEMP%/codex-session-bar/git-guard`. Initial startup copies small settings/counter/status files from the legacy workspace `tmp/memory-watch-rust` if the new store is uninitialized; the source is preserved. Deleted directories self-heal using the last valid in-memory settings and totals. Storage failures do not stop process enforcement, retry at 1 second, and produce at most one popup per minute. Status must match the current Session Bar PID and have a fresh heartbeat; old companion status is rejected. Default limits remain 16 processes / 1024 MiB / five seconds, while saved user limits are preserved. Event logs retain compressed rotation. Protection remains active when the bar is hidden; exiting Session Bar stops its protection worker.

Enforcement normally runs every five seconds, switches to one second after a breach or failure, and returns to the configured interval after 60 stable seconds. Two snapshots confirm a breach; only verified read-query Git handles are terminated, with an intent record before termination. Creation-time validation prevents PID reuse errors. An executable name alone is insufficient; installed Codex path, account and Windows session are verified. Write/unknown commands are never killed. Shutdown/cancellation does not stop unrelated applications.

The default guard tests are read-only or synthetic. The ignored controlled native test requires a deliberately built sleeping `git.exe` fixture under workspace `tmp`, supplied as `CODEX_GUARD_FIXTURE_EXE`; test-only PID/birth scoping confines termination to children the test owns. It checks disabling, real termination and log records, while leaving its `commit` fixture alive. No production scope override is compiled into release builds.

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

Recent user previews are handled by `prompts.rs` on the existing metadata worker.
A resumed rollout filename can bind a visible chat ID to a different physical
history ID (`...-<chat-id>_<history-id>.jsonl`); that alias is accepted only when
the corresponding history projection or user records exist. Navigation, titles
and tags keep the visible ID. No rollout scan or per-chat API request is needed.
The existing partial user-message index supplies at most 64 candidates, bounded
by the projection cursor to avoid high ordinals left over during a rebuild.
The latest meaningful prompt is selected, falling back to an earlier prompt
longer than ten characters when the newest one is short. Transport/context
envelopes remain excluded. Cache keys include history source, item identity,
ordinal and revision for the examined candidate prefix. A warm refresh compares
that prefix without fetching or parsing message JSON, so edits to fallback
candidates also invalidate the preview. Each message stays capped at 64 KiB.
`--inspect-prompts` is a read-only probe showing previews, physical history IDs
and cold/warm metadata timings; it never sends a continuation or navigates chats.

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
restore a user-hidden bar. Quit stops its integrated guard worker and its own
adapter children. It never stops Codex or another task.

Preferences remain in ignored `settings.json`. Runtime files stay in the
system `%TEMP%/codex-session-bar` directory, resolved by the OS without a fixed user or drive path. Runtime discovery leaves TEMP/TMP/TMPDIR unchanged; build/test launchers redirect them into workspace `tmp` for isolated verification. Small retry-claim/reminder caches migrate once without overwriting existing files; source files remain intact. The named
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
The private cache is `%TEMP%/codex-session-bar/body-search.sqlite`: FTS5 trigram,
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

Internal Guardian approval reviews are excluded by `thread_source=guardian_review`
or `source.subagent.other=guardian`, including older restored list caches. An empty
`agent_path` alone is insufficient to identify a user chat. No title-based filter
or database mutation is used; missing source columns remain compatible.

Reset announcement tooltips lead with forecast/status, show original/translated
notice excerpts once, and place source/check metadata last. Bell color rules and
raw links are omitted; pending/observed/stale/error distinctions remain explicit.

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

## Latest-reply hover previews

Recent/fixed chat row tooltips retain status/model/effort/Fast and add the latest
visible model output, without repeating the row prompt. A lazy local-only worker
opens read-only SQLite connections only after a hover request. The latest user
item selects the exact turn and ordinal floor; a new unanswered user message never
falls back to an older turn's answer. Restored history aliases and projection
bounds match recent-prompt identity handling. Analysis and tool output are excluded.

Only the hovered chat is refreshed using existing UI ticks and file notifications,
at most once per second with a five-second metadata fallback while hovering. No
queries run while the mouse is away. At most 16 candidate metadata rows and 901 text
characters per changed candidate are returned; unchanged revision keys skip JSON
extraction. The excerpt is capped at 900 characters / 14 lines, caches retain eight
chats, and idle database handles close after 30 seconds. No replies are written to
runtime caches or sent over the network. `--inspect-reply <chat-id>` is a read-only
source/performance diagnostic. Native self-test verifies registered tooltip text
includes the model reply and excludes the user prompt.
