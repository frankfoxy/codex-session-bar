"""Focused tests for real-data parsing and icon identity preservation."""
import json
from pathlib import Path
import unittest
from unittest.mock import MagicMock, patch
import time

import session_bar as sb


class DataTests(unittest.TestCase):
    def test_recent_cache_load_is_bounded_and_marks_stale(self):
        runtime = (sb.RUNTIME / 'cache-fixture').resolve()
        self.assertTrue(runtime.is_relative_to(sb.TMP_ROOT))
        runtime.mkdir(exist_ok=True)
        path = runtime / 'recent-cache.json'
        row = {'id': 'fixture', 'label': 'Fixture', 'state': 'Busy', 'state_icon': '●', 'color': '#fb923c'}
        path.write_text(json.dumps({'schema': 1, 'rows': [row], 'busy_count': 4, 'loaded_at': 123}), encoding='utf-8')
        with patch.object(sb, 'RUNTIME', runtime):
            self.assertTrue(sb.load_recent_cache()['stale'])
            self.assertEqual(sb.load_recent_cache()['busy_count'], 4)
            path.write_text('x'*65537, encoding='utf-8')
            self.assertEqual(sb.load_recent_cache(), {})

    def test_metrics_use_current_turn_and_expire_old_tps(self):
        snapshot = {'turn': {'started_at': 100}, 'usage': {'total_token_usage': {'total_tokens': 27190000}},
                    'output_rate': {'start': 110, 'end': 120, 'value': 42.3}}
        row = sb.session_metrics(snapshot)
        self.assertEqual(sb.metrics_text(row, now=125), '42.3 t/s   ·   27.2M TOK   ·   00:25')
        self.assertTrue(sb.metrics_text(row, now=151).startswith('— t/s'))
        snapshot['turn']['started_at'] = 115
        self.assertIsNone(sb.session_metrics(snapshot)['tps'])
        self.assertIsNone(sb.session_metrics({})['tokens'])

    def test_refresh_failure_retains_rows_count_and_age(self):
        old = {'rows': [{'id': 'one'}], 'busy_count': 5, 'loaded_at': 123}
        stale = sb.retain_recent(old, 'offline')
        self.assertEqual(stale['rows'], old['rows'])
        self.assertEqual(stale['busy_count'], 5)
        self.assertEqual(stale['loaded_at'], 123)
        self.assertTrue(stale['stale'])
        self.assertNotIn('error', old)

    def test_busy_count_includes_chats_outside_the_ten_rows(self):
        rows = [{'id': str(i), 'kind': 'codex', 'status': 'active', 'updatedAt': 100-i, 'title': str(i)} for i in range(12)]
        store = MagicMock()
        store.recent_turns.return_value = {str(i): 'inProgress' for i in range(12)}
        store.enrich_recent.side_effect = lambda rows: rows
        response = {'content': [{'type': 'text', 'text': json.dumps({'threads': rows})}]}
        with patch.object(sb.DesktopTools, 'call', return_value=response):
            result = sb.DesktopTools().recent(store)
        self.assertEqual(len(result['rows']), 10)
        self.assertEqual(result['busy_count'], 12)

    def test_one_missing_busy_rollout_does_not_hide_others(self):
        store = sb.SessionStore()
        rows = [{'id': 'bad', 'state': 'Busy'}, {'id': 'ok', 'state': 'Busy'}, {'id': 'done', 'state': 'Done'}]
        with patch.object(store, 'read', side_effect=[FileNotFoundError(), {'tokens_used': 1234}]):
            enriched = store.enrich_recent(rows)
        self.assertEqual(len(enriched), 3)
        self.assertNotIn('tokens', enriched[0])
        self.assertEqual(enriched[1]['tokens'], 1234)

    def test_default_thread_uses_local_configuration(self):
        with patch.dict(sb.os.environ, {'CODEX_THREAD_ID': ''}), patch.object(sb, 'load_settings', return_value={}):
            with patch.object(sb.SessionStore, 'sessions', return_value=[{'id': 'recent-local'}]):
                self.assertEqual(sb.default_thread(), 'recent-local')
            with patch.object(sb.SessionStore, 'sessions', return_value=[]):
                self.assertEqual(sb.default_thread(), '')
        with patch.dict(sb.os.environ, {'CODEX_THREAD_ID': 'caller-thread'}):
            self.assertEqual(sb.default_thread(), 'caller-thread')
        with patch.dict(sb.os.environ, {'CODEX_THREAD_ID': ''}), patch.object(sb, 'load_settings', return_value={'thread': 'saved-thread'}):
            self.assertEqual(sb.default_thread(), 'saved-thread')

    def test_recent_ten_busy_priority_and_root_filter(self):
        rows = [{'id': str(i), 'kind': 'codex', 'hostId': 'local', 'title': f'Chat {i}',
                 'status': 'active' if i in (3, 9, 11) else 'idle', 'updatedAt': 100-i} for i in range(12)]
        rows += [{'id': 'child', 'kind': 'codex', 'status': 'active', 'updatedAt': 999},
                 {'id': 'cloud', 'kind': 'chatgpt', 'status': 'active', 'updatedAt': 999}]
        payload = {'threads': rows, 'pinnedThreads': [rows[0]]}
        roots = {str(i): 'completed' for i in range(12)}
        roots['cloud'] = 'completed'
        recent = sb.recent_threads(payload, roots)
        self.assertEqual(len(recent), 10)
        self.assertEqual([r['id'] for r in recent[:2]], ['3', '9'])
        self.assertEqual({r['id'] for r in recent}, {str(i) for i in range(10)})
        self.assertTrue(all(r['state'] == 'Busy' for r in recent[:2]))
        self.assertTrue(all(r['state'] == 'Done' for r in recent[2:]))

    def test_recent_state_uses_live_busy_not_old_turn(self):
        self.assertEqual(sb.recent_state('active', 'completed')[0], 'Busy')
        self.assertEqual(sb.recent_state('idle', 'inProgress')[0], 'Ready')
        self.assertEqual(sb.recent_state('idle', 'interrupted')[0], 'Stopped')
        self.assertEqual(sb.recent_state('idle', 'failed')[0], 'Failed')
        self.assertEqual(sb.recent_state(None, 'inProgress')[0], 'Unknown')

    def test_navigation_requires_real_desktop_confirmation(self):
        desktop = sb.DesktopTools()
        result = {'content': [{'type': 'text', 'text': '{"navigated":true}'}]}
        with patch.object(desktop, 'close'), patch.object(desktop, 'call', return_value=result) as call:
            desktop.navigate('target-id')
            self.assertEqual(call.call_args.args[1]['arguments'], {'threadId': 'target-id'})
            self.assertEqual(call.call_args.args[1]['name'], 'navigate_to_codex_page')
        for bad in ({'isError': True}, {'content': [{'type': 'text', 'text': '{"navigated":false}'}]}):
            with patch.object(desktop, 'close'), patch.object(desktop, 'call', return_value=bad):
                with self.assertRaises(RuntimeError):
                    desktop.navigate('target-id')

    def test_dock_in_blank_titlebar_and_follow_resize(self):
        screens = [(0, 0, 1920, 1080)]
        self.assertEqual(sb.dock_position((200, 200, 1400, 1000), (600, 40), screens),
                         (595, 202, 'titlebar-blank'))
        self.assertEqual(sb.dock_position((300, 250, 1700, 1050), (600, 40), screens),
                         (795, 252, 'titlebar-blank'))

    def test_dock_stays_inside_host_with_upper_monitor(self):
        screens = [(0, 0, 1920, 1080), (0, -1080, 1920, 0)]
        self.assertEqual(sb.dock_position((0, 0, 1920, 1080), (600, 40), screens),
                         (755, 2, 'titlebar-blank'))

    def test_dock_no_space_does_not_cover_menu_or_taskbar(self):
        screens = [(0, 0, 1920, 1040)]
        for host, expected in [((0, 0, 1920, 1040), 'titlebar-blank'),
                               ((200, 5, 1000, 900), 'below-menu'),
                               ((-8, -8, 1928, 1048), 'below-menu')]:
            x, y, placement = sb.dock_position(host, (600, 40), screens)
            self.assertEqual(placement, expected)
            self.assertFalse(sb.overlaps_host_menu((x, y), (600, 40), host))
            self.assertGreaterEqual(x, 0)
            self.assertGreaterEqual(y, 0)
            self.assertLessEqual(x+600, 1920)
            self.assertLessEqual(y+40, 1040)

    def test_titlebar_gap_protects_menu_and_caption_controls(self):
        controls = [(6, 4, 34, 32), (40, 4, 68, 32), (74, 4, 102, 32),
                    (108, 6, 317, 30), (1233, 1, 1373, 36)]
        slot = sb.titlebar_slot(1373, controls)
        self.assertEqual(slot, [325, 2, 1225, 42])
        host = (2829, -192, 4202, 1968)
        x, y, mode = sb.dock_position(host, (591, 40), [(2560, -192, 6400, 1968)], slot)
        self.assertEqual(mode, 'titlebar-blank')
        self.assertEqual(y, -190)
        self.assertFalse(sb.overlaps_host_menu((x, y), (591, 40), host, slot))
        for l, t, r, b in controls:
            self.assertFalse(x < host[0]+r and x+591 > host[0]+l and
                             y < host[1]+b and y+40 > host[1]+t)
        self.assertTrue(sb.overlaps_host_menu((host[0]+200, y), (591, 40), host, slot))
        self.assertTrue(sb.overlaps_host_menu((host[0]+1000, y), (591, 40), host, slot))

    def test_startup_null_uia_pointer_is_reacquired(self):
        import ctypes
        tracker = sb.CurrentPageTracker.__new__(sb.CurrentPageTracker)
        tracker.document = ctypes.c_void_p(None)  # COM null is false, but not None.
        tracker.hwnd, tracker.last_windows, tracker.latest = 55, time.monotonic(), None
        tracker.ui, tracker.api, tracker.condition = MagicMock(), MagicMock(), MagicMock()
        tracker.ui.ElementFromHandle.return_value.FindFirst.return_value.CurrentName = 'Loaded chat'
        db = MagicMock()
        db.__enter__.return_value.execute.return_value = [{'id': 'new', 'label': 'Loaded chat'}]
        with patch.object(tracker, 'visible_windows', return_value=[55]), patch.object(sb, 'readonly_db', return_value=db):
            self.assertEqual(tracker.poll()['thread'], 'new')
        tracker.ui.ElementFromHandle.return_value.FindFirst.assert_called_once()

    def test_output_tps_uses_output_delta_and_elapsed_time(self):
        before = {'total_token_usage': {'output_tokens': 100, 'total_tokens': 10000}}
        after = {'total_token_usage': {'output_tokens': 220, 'total_tokens': 90000}}
        rate = sb.output_rate(before, '2026-10-18T06:00:00Z', after, '2026-10-18T06:00:04Z')
        self.assertEqual(rate['value'], 30)
        self.assertEqual(rate['output_tokens'], 120)
        self.assertIsNone(sb.output_rate(before, None, after, None))
        self.assertIsNone(sb.output_rate(before, '2026-10-18T06:00:00Z', before, '2026-10-18T06:00:04Z'))
        self.assertIsNone(sb.output_rate(after, '2026-10-18T06:00:00Z', before, '2026-10-18T06:00:04Z'))

    def test_page_identity_requires_exact_unique_title(self):
        rows = [{"id": "a", "label": "🧪 FPGA"}, {"id": "b", "label": "FPGA"}]
        self.assertEqual(sb.CurrentPageTracker.resolve_title("🧪 FPGA", rows), "a")
        self.assertIsNone(sb.CurrentPageTracker.resolve_title("fpga", rows))
        self.assertIsNone(sb.CurrentPageTracker.resolve_title("🧪 FPG", rows))
        self.assertIsNone(sb.CurrentPageTracker.resolve_title("🧪 FPGA", rows + [{"id": "c", "label": "🧪 FPGA"}]))
        self.assertIsNone(sb.CurrentPageTracker.resolve_title("", []))

    def test_rename_requires_desktop_confirmation(self):
        result = {"content": [{"type": "text", "text": json.dumps({"threadId": "t1", "title": "🔀 FPGA"})}]}
        self.assertEqual(sb.DesktopTools.verify_rename(result, "t1", "🔀 FPGA"), "🔀 FPGA")
        with self.assertRaises(RuntimeError):
            sb.DesktopTools.verify_rename({"isError": True, "content": []}, "t1", "🔀 FPGA")
        with self.assertRaises(RuntimeError):
            sb.DesktopTools.verify_rename(result, "other-thread", "🔀 FPGA")
        with self.assertRaises(RuntimeError):
            sb.DesktopTools.verify_rename({"content": []}, "t1", "🔀 FPGA")

    def test_icons_preserve_titles(self):
        for emoji in sb.ICONS:
            self.assertEqual(sb.with_icon(emoji + " USB31 调试", "🟧"), "🟧 USB31 调试")
            self.assertEqual(sb.with_icon(emoji + " USB31 调试", ""), "USB31 调试")
        self.assertEqual(sb.with_icon("FPGA 🧠 实验", "🔬"), "🔬 FPGA 🧠 实验")
        self.assertEqual(sb.with_icon("👩‍💻 工程", "🔬"), "🔬 工程")

    def test_unknown_quota_is_not_zero(self):
        self.assertEqual(sb.limit_windows({"rateLimits": {"primary": {"usedPercent": None}}}), [])
        limits = {"rateLimitsByLimitId": {
            "codex": {"primary": {"usedPercent": 25, "windowDurationMins": 300, "resetsAt": 123}},
            "other": {"primary": {"usedPercent": 110, "windowDurationMins": 10080}}}}
        windows = sb.limit_windows(limits)
        self.assertEqual(windows[0]["remaining"], 75)
        self.assertEqual(windows[0]["label"], "5h")
        self.assertEqual(windows[1]["remaining"], 0)
        self.assertEqual(windows[1]["label"], "7d")

    def test_bounded_tail_and_partial_append(self):
        path = sb.RUNTIME / "parser-test.jsonl"
        def record(total):
            return (json.dumps({"type": "event_msg", "payload": {"type": "token_count", "info": {
                "total_token_usage": {"total_tokens": total}, "last_token_usage": {"total_tokens": 10}}}}) + "\n").encode()
        # This fixture exceeds the initial read bound; only the final complete
        # token event is relevant, regardless of the preceding oversized record.
        with path.open("wb") as stream:
            stream.write(b'{"padding":"' + b'x' * 280000 + b'"}\n')
            stream.write(record(100))
        store = sb.SessionStore()
        self.assertEqual(store.read_tokens(path)["usage"]["total_token_usage"]["total_tokens"], 100)
        line = record(250)
        with path.open("ab") as stream:
            stream.write(line[:40])
        self.assertEqual(store.read_tokens(path)["usage"]["total_token_usage"]["total_tokens"], 100)
        with path.open("ab") as stream:
            stream.write(line[40:])
        self.assertEqual(store.read_tokens(path)["usage"]["total_token_usage"]["total_tokens"], 250)
        with path.open("wb") as stream:
            stream.write(record(5))
        self.assertEqual(store.read_tokens(path)["usage"]["total_token_usage"]["total_tokens"], 5)


if __name__ == "__main__":
    unittest.main()
