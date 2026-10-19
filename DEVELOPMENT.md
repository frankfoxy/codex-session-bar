# Development and troubleshooting

User-facing features and setup are in [README](README.md). This document covers integration details and verification.

## Data and identity

- Open `state_5.sqlite` and `thread_history_1.sqlite` read-only. Exclude archived chats and subagents from the recent picker.
- Identify the visible chat using Windows UI Automation and a unique local title match. Do not infer the active page from background activity. Clear old data during identity changes and reject late snapshots.
- Read JSONL usage from a bounded tail and subsequent increments. TPS must use samples from the current turn and expires after 30 seconds. Cached input is part of input totals; last-call usage is not whole-turn usage.
- Read account limits through `account/rateLimits/read` every 60 seconds. Keep previous values on failure and label them stale. Unknown is distinct from zero.

## Desktop actions and responsiveness

Use the desktop's bundled `codex-app-tools` MCP adapter for `set_thread_title` and `navigate_to_codex_page`. An independent app-server does not notify the running desktop renderer of title changes. Require explicit desktop confirmation; never silently fall back to a local title write.

The control worker warms and reuses its own adapter. Missing desktop pipes and transport failures trigger reconnection, with at most one retry for an explicit, idempotent target action. Application errors are surfaced without retries. Coalesce pending navigation commands to the latest target; preserve icon writes bound to their original chat. Icon changes fetch only the current title, without reading usage logs.

Quota, control actions, recent scans and current-view tracking have separate workers. The recent scanner uses its own adapter, approximately every 3 seconds while closed and 1.5 seconds while open. Reopening a cached picker reuses its widgets. An open picker preserves its displayed IDs and order, including rows that leave the latest top ten; updates merge by ID. Reopening applies the latest Busy-first order. An initially empty menu can populate when its first result arrives.

Busy totals cover all scanned Busy local root chats, including those outside the ten visible rows. Sum available cumulative tokens and fresh TPS samples; expose partial coverage in tooltips. A missing rollout affects only that chat's metrics.

The bar watches the host window every 80 ms. A separate read-only titlebar observer supplies safe blank bounds. Too little space falls back below the menu. Current-view detection runs incrementally every 0.1 seconds and wakes data reads; session data normally refreshes every 0.5 seconds.

## Runtime and configuration

- Preferences: ignored `settings.json` beside the script.
- Runtime: the workspace's direct `tmp/codex-session-bar/` child. When installed under `tools/codex-session-bar`, use that enclosing workspace; otherwise use the repository root.
- Override the workspace with `CODEX_SESSION_BAR_WORKSPACE`, pointing to an existing directory. Temporary content must remain in its direct `tmp` child.
- Override the launcher interpreter with `CODEX_SESSION_BAR_PYTHON` if needed.
- If there are multiple desktop pipes, restart from the intended Codex instance rather than guessing the destination.
- Autostart installs the current-user `Codex Session Bar` scheduled task, with up to three recovery restarts. It waits for the desktop to appear.

No credentials or personal session IDs belong in the repository. Do not submit prompts, resume tasks, interrupt work or write Codex databases for verification.

## Visibility and emoji

The bar always uses `WindowStaysOnTopHint`. Closing hides it with an explicit `user_hidden` flag, which host-follow updates must respect. Tray Open, the existing single-instance local socket, and the named auto-reset Windows show event clear that flag; Quit stops the process. The event name is `Local\\codex-session-bar-<home-hash>-show`, matching the first 12 SHA-256 hex characters of the case-folded Codex home path. The installed opening skill starts the existing scheduled task if needed, signals the event, then verifies fresh diagnostics. It does not restart Codex or write runtime files.

`emoji_catalog.json` contains Unicode Emoji 18.0 fully-qualified sequences plus components, generated from the official `emoji-test.txt`; preserve `UNICODE-LICENSE.txt`. The picker uses a batched, uniform-size Qt list model instead of thousands of buttons. Search covers names, group, subgroup and literal emoji. Longest-first prefix buckets preserve full flag, skin-tone, keycap and ZWJ sequences during title changes without scanning the entire catalog for each title.

## Verification

```powershell
python -B -m unittest discover -s . -p test_session_bar.py -v
$env:QT_QPA_PLATFORM = 'offscreen'
python -B session_bar.py --test-ui
```

These checks use fixtures and do not navigate user chats. They cover transport reuse/recovery, queued navigation, fixed picker order, live metric updates, icon identity, dock bounds, window following and stale snapshot rejection. UI screenshots go to workspace `tmp`, not the repository. Curated README previews are sanitized fixture captures.

`python -B session_bar.py --probe` is a live diagnostic: it resets the current icon to its existing value to verify desktop synchronization. It does not submit a prompt. Do not run it unnecessarily against unrelated chats.

Inspect `live-status.json` and bounded stderr excerpts in the runtime directory for identity, connection errors, picker timings and quota availability. Do not publish these files. Compatibility depends on Codex's local database and desktop tool formats, which may change across app versions.
