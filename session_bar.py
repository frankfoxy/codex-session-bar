"""Small Windows companion for Codex. Never submits prompts or resumes turns."""
from __future__ import annotations

import argparse
import ctypes
from ctypes import wintypes
from datetime import datetime
import hashlib
import json
import os
from pathlib import Path
import queue
import re
import shutil
import sqlite3
import subprocess
import sys
import threading
import time

APP_DIR = Path(__file__).resolve().parent
DEFAULT_WORKSPACE = APP_DIR.parent.parent if APP_DIR.parent.name == 'tools' else APP_DIR
WORKSPACE = Path(os.environ.get('CODEX_SESSION_BAR_WORKSPACE', str(DEFAULT_WORKSPACE))).resolve()
TMP_ROOT = (WORKSPACE / "tmp").resolve()
RUNTIME = (TMP_ROOT / "codex-session-bar").resolve()
if TMP_ROOT.parent != WORKSPACE or not RUNTIME.is_relative_to(TMP_ROOT):
    raise RuntimeError("Temporary directory must be inside workspace/tmp")
RUNTIME.mkdir(parents=True, exist_ok=True)
for _env in ("TEMP", "TMP", "TMPDIR"):
    os.environ[_env] = str(RUNTIME)
os.environ["PYTHONDONTWRITEBYTECODE"] = "1"
sys.dont_write_bytecode = True
HOME = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))).resolve()
INSTANCE_NAME = 'codex-session-bar-' + hashlib.sha256(str(HOME).casefold().encode()).hexdigest()[:12]
SETTINGS = APP_DIR / "settings.json"
ICON_ROWS = [
    ["🔴", "🟠", "🟡", "🟢", "🔵", "🟣", "🟤", "⚫", "⚪", "🟥", "🟧", "🟨", "🟩", "🟦", "🟪", "🟫", "⬛", "⬜"],
    ["🚧", "✅", "⏸️", "⏳", "❗", "📌", "⭐", "🔥", "🆕", "👀", "🔁", "🧊", "🔒", "❌", "🎯", "🏁", "💯", "⚠️"],
    ["🐛", "🔍", "🩺", "🚨", "💥", "🛠️", "🩹", "🧹", "🔬", "⏱️", "🔌", "📦", "🧩", "⚡", "📡", "🎛️", "🧮", "🔋"],
    ["💻", "🧪", "📝", "🚀", "♻️", "🔀", "🌿", "⚙️", "🐍", "🌐", "🤖", "🔐", "💡", "📚", "📊", "📈", "🧠", "❓"],
    ["💬", "📧", "📅", "👥", "📋", "💰", "🧾", "📞", "🌟", "🎉", "❤️", "🍀", "☕", "🌙", "🏠", "✈️", "🎮", "🐱"],
]
ICONS = {i for row in ICON_ROWS for i in row}
COM_INIT_LOCK = threading.Lock()


def strip_icon(title):
    """Remove one leading emoji/grapheme, retaining ordinary Unicode text."""
    title = title.strip()
    for icon in sorted(ICONS, key=len, reverse=True):
        if title.startswith(icon):
            return title[len(icon):].lstrip()
    # Also support an existing emoji outside the picker, including ZWJ sequences.
    return re.sub(r"^(?:[\U0001F000-\U0001FAFF\u2600-\u27BF](?:\ufe0f|[\U0001F3FB-\U0001F3FF])?(?:\u200d[\U0001F000-\U0001FAFF\u2600-\u27BF]\ufe0f?)*)\s*", "", title, count=1)


def with_icon(title, icon):
    return f"{icon} {strip_icon(title)}".strip()


def leading_icon(title):
    base = strip_icon(title)
    return title[:len(title) - len(base)].strip() or "◈"


def recent_state(live_status, turn_status):
    if live_status in ('active', 'busy', 'running', 'inProgress'):
        return 'Busy', '●', '#fb923c'
    if live_status in ('needsAttention', 'needs_attention', 'waiting', 'blocked'):
        return 'Waiting', '◷', '#facc15'
    if live_status in ('idle', 'completed'):
        return {'completed': ('Done', '✓', '#65d69b'),
                'failed': ('Failed', '!', '#f87171'),
                'interrupted': ('Stopped', 'Ⅱ', '#a8b3bf')}.get(turn_status, ('Ready', '○', '#a8b3bf'))
    return 'Unknown', '◌', '#87929d'


def recent_threads(payload, local_turns, limit=10):
    """Choose the latest ten root chats, then prioritize Busy within that set."""
    unique = {}
    for row in payload.get('pinnedThreads', []) + payload.get('threads', []):
        tid = row.get('id')
        if row.get('kind') != 'codex' or row.get('hostId', 'local') != 'local' or tid not in local_turns:
            continue
        state, icon, color = recent_state(row.get('status'), local_turns[tid])
        updated = row.get('updatedAt')
        if not isinstance(updated, (int, float)):
            updated = 0
        unique[tid] = {'id': tid, 'label': row.get('title') or 'Untitled chat',
                       'state': state, 'state_icon': icon, 'color': color, 'updated_at': updated}
    rows = sorted(unique.values(), key=lambda r: (-r['updated_at'], r['id']))[:limit]
    return sorted(rows, key=lambda r: r['state'] != 'Busy')


def compact(value):
    if value is None:
        return "—"
    if value >= 1_000_000:
        return f"{value / 1_000_000:.1f}M"
    if value >= 1000:
        return f"{value / 1000:.1f}k"
    return str(value)


def output_rate(previous_usage, previous_at, usage, at):
    """Observed output throughput between two persisted usage samples."""
    try:
        start = datetime.fromisoformat(previous_at.replace('Z', '+00:00')).timestamp()
        end = datetime.fromisoformat(at.replace('Z', '+00:00')).timestamp()
        delta = usage['total_token_usage']['output_tokens'] - previous_usage['total_token_usage']['output_tokens']
        if end > start and delta > 0:
            return {"value": delta / (end - start), "start": start, "end": end, "output_tokens": delta}
    except (TypeError, ValueError, KeyError, AttributeError):
        pass
    return None


def duration(seconds):
    seconds = max(0, int(seconds))
    hours, rest = divmod(seconds, 3600)
    minutes, secs = divmod(rest, 60)
    return f"{hours}:{minutes:02}:{secs:02}" if hours else f"{minutes:02}:{secs:02}"


def reset_text(epoch):
    if not epoch:
        return "Reset time unavailable"
    left = int(epoch - time.time())
    if left <= 0:
        return "Waiting for quota refresh"
    days, rest = divmod(left, 86400)
    hours, rest = divmod(rest, 3600)
    minutes = rest // 60
    return "Resets in " + (f"{days}d " if days else "") + f"{hours}h {minutes:02}m"


def quota_svg(remaining, reset_share=None):
    """Vector battery and reset ring, matching the Claude bar's quota colors."""
    known = remaining is not None
    left = max(0, min(100, remaining)) if known else 0
    color = ("#22c55e" if left >= 75 else "#3b82f6" if left >= 50
             else "#f97316" if left >= 25 else "#ef4444") if known else "#87929d"
    ring = ''
    if reset_share is not None:
        arc = max(0, min(1, reset_share)) * 28.2743
        ring = (f'<circle cx="36" cy="8" r="4.5" fill="none" stroke="#414954" stroke-width="2"/>'
                f'<circle cx="36" cy="8" r="4.5" fill="none" stroke="#7ea7ef" stroke-width="2" '
                f'stroke-dasharray="{arc:.2f} 28.2743" transform="rotate(-90 36 8)"/>')
    fill = f'<rect x="3" y="5" width="{left * .2:.2f}" height="6" rx="1.5" fill="{color}"/>' if known else ''
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="43" height="16" viewBox="0 0 43 16">'
            f'<rect x=".75" y="2.75" width="24.5" height="10.5" rx="3" fill="none" stroke="{color}" stroke-width="1.5"/>'
            f'<rect x="26.5" y="6" width="2" height="4" rx="1" fill="{color}"/>'
            f'{fill}{ring}</svg>')


def readonly_db(name):
    conn = sqlite3.connect((HOME / name).as_uri() + "?mode=ro", uri=True, timeout=0.5)
    conn.execute("PRAGMA query_only=ON")
    conn.execute("PRAGMA temp_store=MEMORY")
    conn.row_factory = sqlite3.Row
    return conn


class SessionStore:
    def __init__(self):
        self.cache = {}

    def sessions(self):
        with readonly_db("state_5.sqlite") as db:
            return [dict(row) for row in db.execute(
                "SELECT id, coalesce(nullif(name,''),title) AS label, cwd "
                "FROM threads WHERE archived=0 AND agent_path IS NULL "
                "ORDER BY updated_at DESC LIMIT 40")]

    def read(self, tid):
        with readonly_db("state_5.sqlite") as db:
            row = db.execute(
                "SELECT id,coalesce(nullif(name,''),title) AS label,rollout_path,"
                "tokens_used,model,cwd FROM threads WHERE id=?", (tid,)).fetchone()
        if row is None:
            raise ValueError("Chat not found; choose another chat")
        result = dict(row)
        with readonly_db("thread_history_1.sqlite") as db:
            row = db.execute(
                "SELECT turn_id,status,started_at,completed_at,duration_ms "
                "FROM thread_turns WHERE thread_id=? ORDER BY rollout_ordinal DESC LIMIT 1", (tid,)).fetchone()
        result["turn"] = dict(row) if row else {}
        result.update(self.read_tokens(result["rollout_path"]))
        return result

    def recent_turns(self, ids):
        if not ids:
            return {}
        placeholders = ','.join('?' for _ in ids)
        with readonly_db('state_5.sqlite') as db:
            roots = [row['id'] for row in db.execute(
                f'SELECT id FROM threads WHERE archived=0 AND agent_path IS NULL AND id IN ({placeholders})', ids)]
        with readonly_db('thread_history_1.sqlite') as db:
            return {tid: (row['status'] if (row := db.execute(
                'SELECT status FROM thread_turns WHERE thread_id=? ORDER BY rollout_ordinal DESC LIMIT 1',
                (tid,)).fetchone()) else None) for tid in roots}

    def read_tokens(self, path):
        """Read only a bounded tail and subsequent appended bytes; never full logs."""
        p = Path(path)
        stat = p.stat()
        previous = self.cache.get(path)
        if previous and previous["size"] == stat.st_size and previous["mtime"] == stat.st_mtime_ns:
            return previous["data"]
        start = max(0, stat.st_size - 262144)
        data = dict(previous["data"]) if previous and stat.st_size >= previous["size"] else {}
        if previous and 0 <= stat.st_size - previous["size"] <= 262144:
            start = previous["offset"]
        with p.open("rb") as stream:
            stream.seek(start)
            chunk = stream.read(262144)
        # Offset points after the last complete newline, preserving partial records.
        end = chunk.rfind(b"\n") + 1
        lines = chunk[:end].splitlines()
        for raw in lines:
            try:
                event = json.loads(raw)
            except (ValueError, UnicodeError):
                continue
            payload = event.get("payload", {})
            if event.get("type") == "event_msg" and payload.get("type") == "token_count":
                if payload.get("info"):
                    rate = output_rate(data.get('usage'), data.get('token_at'), payload['info'], event.get('timestamp'))
                    if rate:
                        data['output_rate'] = rate
                    data["usage"] = payload["info"]
                    data["token_at"] = event.get("timestamp")
                if payload.get("rate_limits"):
                    data["log_limits"] = payload["rate_limits"]
        self.cache[path] = {"size": stat.st_size, "mtime": stat.st_mtime_ns, "offset": start + end, "data": data}
        return data


def default_thread():
    """Resolve local context without shipping a private session identifier."""
    existing = os.environ.get('CODEX_THREAD_ID') or load_settings().get('thread')
    if existing:
        return existing
    try:
        rows = SessionStore().sessions()
        return rows[0]['id'] if rows else ''
    except (OSError, sqlite3.Error):
        return ''


class Rpc:
    def __init__(self):
        self.proc = None
        self.responses = queue.Queue()
        self.next_id = 0
        self.stderr = None
        self.close_lock = threading.RLock()

    def start(self):
        cli = shutil.which("codex")
        if not cli:
            candidates = list((Path.home() / "AppData/Local/OpenAI/Codex/bin").glob("*/codex.exe"))
            if not candidates:
                raise RuntimeError("Codex CLI not found")
            cli = str(max(candidates, key=lambda p: p.stat().st_mtime))
        self.open_process([cli, "app-server", "--stdio"], "app-server-stderr.log")
        self.call("initialize", {"clientInfo": {"name": "codex_session_bar", "version": "0.1.1"}})
        self.proc.stdin.write(b'{"method":"initialized"}\n')
        self.proc.stdin.flush()

    def open_process(self, command, log_name, env=None):
        self.close()
        self.responses = queue.Queue()
        self.stderr = (RUNTIME / log_name).open("ab")
        self.proc = subprocess.Popen(command, stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=self.stderr,
                                     creationflags=subprocess.CREATE_NO_WINDOW, env=env or os.environ.copy())
        proc, responses = self.proc, self.responses

        def reader():
            try:
                for raw in proc.stdout:
                    try:
                        response = json.loads(raw)
                    except ValueError:
                        continue
                    if "id" in response:
                        responses.put(response)
            except (OSError, ValueError):
                pass
            finally:
                responses.put({"error": {"message": "Codex connection closed"}, "id": -1})

        threading.Thread(target=reader, daemon=True).start()

    def call(self, method, params=None):
        if not self.proc or self.proc.poll() is not None:
            if method == "initialize":
                raise RuntimeError("Codex connection failed")
            self.start()
        self.next_id += 1
        request_id = self.next_id
        raw = (json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params or {}}, ensure_ascii=False) + "\n").encode()
        self.proc.stdin.write(raw)
        self.proc.stdin.flush()
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            try:
                response = self.responses.get(timeout=max(.01, deadline - time.monotonic()))
            except queue.Empty:
                break
            if response.get("id") not in (request_id, -1):
                continue
            if "error" in response:
                raise RuntimeError(response["error"].get("message", "Codex API error"))
            return response.get("result", {})
        self.close()
        raise TimeoutError("Codex request timed out; retry later")

    def close(self):
        with self.close_lock:
            proc, stderr = self.proc, self.stderr
            self.proc, self.stderr = None, None
            if proc:
                if proc.poll() is None:
                    proc.terminate()
                    try:
                        proc.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait(timeout=2)
                for pipe in (proc.stdin, proc.stdout):
                    if pipe:
                        pipe.close()
            if stderr:
                stderr.close()


class DesktopTools(Rpc):
    """Use the app's bundled MCP adapter so rename also updates its live UI.

    A separate app-server only persists the name; its notifications do not
    reach the already-running desktop renderer. Never use that as a fallback.
    """
    def start(self):
        node = shutil.which("node") or os.environ.get("CODEX_MCP_NODE_PATH")
        adapters = list((HOME / "plugins/cache/openai-bundled/codex-app-tools").glob("*/server.mjs"))
        if not node or not adapters:
            raise RuntimeError("Desktop App Tools unavailable; icon unchanged")
        env = os.environ.copy()
        pipe = env.get("CODEX_APP_TOOLS_PIPE_PATH")
        if not pipe or not os.path.exists(pipe):
            endpoints = ["\\\\.\\pipe\\" + p for p in os.listdir("\\\\.\\pipe\\") if p.startswith("codex-app-tools-")]
            if len(endpoints) != 1:
                raise RuntimeError("Cannot identify Codex desktop connection; restart the bar from Codex")
            env["CODEX_APP_TOOLS_PIPE_PATH"] = endpoints[0]
        adapter = max(adapters, key=lambda p: p.stat().st_mtime)
        self.open_process([node, str(adapter)], "desktop-tools-stderr.log", env)
        self.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                 "clientInfo": {"name": "codex-session-bar", "version": "0.1.1"}})
        self.proc.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
        self.proc.stdin.flush()

    def rename(self, tid, name):
        # The bar outlives desktop restarts; reconnect to the current app pipe
        # for each explicit icon change rather than retaining a dead adapter.
        self.close()
        result = self.call("tools/call", {"name": "set_thread_title",
            "arguments": {"threadId": tid, "title": name},
            "_meta": {"codexThreadId": default_thread() or tid}})
        return self.verify_rename(result, tid, name)

    @staticmethod
    def tool_payload(result):
        if result.get('isError'):
            message = next((c.get('text', '') for c in result.get('content', []) if c.get('type') == 'text'), 'Desktop request failed')
            raise RuntimeError(message[:200])
        for content in result.get('content', []):
            if content.get('type') == 'text':
                try:
                    value = json.loads(content['text'])
                    if isinstance(value, dict):
                        return value
                except (ValueError, KeyError):
                    pass
        raise RuntimeError('Desktop response unavailable')

    def recent(self, store):
        payload = self.tool_payload(self.call('tools/call', {'name': 'list_threads', 'arguments': {'limit': 50},
            '_meta': {'codexThreadId': default_thread()}}))
        ids = list({r['id'] for r in payload.get('pinnedThreads', []) + payload.get('threads', []) if r.get('id')})
        return recent_threads(payload, store.recent_turns(ids))

    def navigate(self, tid):
        self.close()  # Discover the current desktop pipe after app restarts.
        payload = self.tool_payload(self.call('tools/call', {'name': 'navigate_to_codex_page',
            'arguments': {'threadId': tid}, '_meta': {'codexThreadId': default_thread() or tid}}))
        if payload.get('navigated') is not True:
            raise RuntimeError('Codex did not confirm navigation')

    @staticmethod
    def verify_rename(result, tid, name):
        if result.get("isError"):
            message = next((c.get("text", "") for c in result.get("content", []) if c.get("type") == "text"), "Desktop rename failed")
            raise RuntimeError(message[:200])
        for content in result.get("content", []):
            if content.get("type") != "text":
                continue
            try:
                response = json.loads(content["text"])
            except (ValueError, KeyError):
                continue
            if response.get("threadId") == tid and response.get("title") == name:
                return name
        raise RuntimeError("Desktop did not confirm the title update; retry")


class CurrentPageTracker:
    """Read the visible app document's title, not stream-owner/activity logs.

    Only window metadata and the document title are requested. No conversation
    bodies are traversed. A title must map to exactly one local root thread.
    """
    def __init__(self):
        # The two observers share comtypes' generated-module cache. Serialize
        # its initialization while keeping each COM object on its own thread.
        with COM_INIT_LOCK:
            self.initialize_com()

    def initialize_com(self):
        sys.coinit_flags = 0  # Dedicated observer thread; use COM's MTA.
        import comtypes
        import comtypes.client
        comtypes.CoInitializeEx(0)
        cache = RUNTIME / "comtypes-gen"
        cache.mkdir(parents=True, exist_ok=True)
        comtypes.client.gen_dir = str(cache)
        self.api = comtypes.client.GetModule("UIAutomationCore.dll")
        self.ui = comtypes.client.CreateObject(self.api.CUIAutomation, interface=self.api.IUIAutomation)
        self.condition = self.ui.CreatePropertyCondition(self.api.UIA_ControlTypePropertyId,
                                                        self.api.UIA_DocumentControlTypeId)
        self.document = None
        self.hwnd = None
        self.latest = None
        self.last_windows = 0

    @staticmethod
    def visible_windows():
        import psutil
        user = ctypes.windll.user32
        user.IsWindowVisible.argtypes = [wintypes.HWND]
        user.GetWindowTextW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
        user.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
        user.GetForegroundWindow.restype = wintypes.HWND
        foreground = user.GetForegroundWindow()
        found = []
        @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
        def callback(hwnd, unused):
            if not user.IsWindowVisible(hwnd) or user.IsIconic(hwnd):
                return True
            caption = ctypes.create_unicode_buffer(256)
            user.GetWindowTextW(hwnd, caption, 256)
            if not any(name in caption.value.lower() for name in ("chatgpt", "codex")):
                return True
            pid = wintypes.DWORD()
            user.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
            try:
                exe = psutil.Process(pid.value).exe().lower()
                if "openai.codex" in exe or "openai\\codex" in exe:
                    found.append((hwnd == foreground, hwnd))
            except psutil.Error:
                pass
            return True
        user.EnumWindows(callback, 0)
        found.sort(reverse=True)
        return [hwnd for _, hwnd in found]

    @staticmethod
    def resolve_title(title, rows):
        if not title:
            return None
        matches = [row for row in rows if row.get("label") == title]
        return matches[0]["id"] if len(matches) == 1 else None

    def poll(self):
        if not self.document or time.monotonic() - self.last_windows > 1:
            windows = self.visible_windows()
            self.last_windows = time.monotonic()
            if not windows:
                self.document, self.hwnd = None, None
                return {"thread": None, "available": False, "source": "uia-current-page",
                        "message": "No visible Codex chat window"}
            # Keep the last window when the user focuses the floating bar.
            hwnd = self.hwnd if self.hwnd in windows else windows[0]
            foreground = ctypes.windll.user32.GetForegroundWindow()
            if foreground in windows:
                hwnd = foreground
            if hwnd != self.hwnd or not self.document:
                self.hwnd = hwnd
                root = self.ui.ElementFromHandle(hwnd)
                self.document = root.FindFirst(self.api.TreeScope_Descendants, self.condition)
        try:
            title = self.document.CurrentName if self.document else ""
            if not title:
                self.document = None
                return {"thread": None, "available": False, "source": "uia-current-page",
                        "message": "Waiting for Codex page to load"}
            # Query only a title and identity, with an exact match. Duplicate
            # names are deliberately ambiguous; never pick the most recent.
            with readonly_db("state_5.sqlite") as db:
                rows = [dict(row) for row in db.execute(
                    "SELECT id, coalesce(nullif(name,''),title) AS label FROM threads "
                    "WHERE coalesce(nullif(name,''),title)=? AND agent_path IS NULL LIMIT 3", (title,))]
            if not self.document or self.document.CurrentName != title:
                return {"thread": None, "available": False, "source": "uia-current-page",
                        "message": "Switching…"}
        except Exception:
            self.document = None
            raise
        tid = self.resolve_title(title, rows)
        if not tid:
            self.latest = None
            return {"thread": None, "available": False, "source": "uia-current-page", "label": title,
                    "message": "Multiple chats match this title; use fixed mode to choose" if len(rows) > 1
                               else "No local chat matches this page"}
        if not self.latest or self.latest.get("thread") != tid or self.latest.get("label") != title:
            now = time.time()
            self.latest = {"thread": tid, "label": title, "window": self.hwnd, "available": True,
                           "source": "uia-current-page", "timestamp": datetime.fromtimestamp(now).astimezone().isoformat(),
                           "detected_at": now}
        return dict(self.latest)

    def close(self):
        self.document, self.ui = None, None
        import comtypes
        comtypes.CoUninitialize()

    def read_titlebar(self, hwnd):
        """Read only header control rectangles; never descend into chat content."""
        state = host_window_state(hwnd)
        if not state.get('visible'):
            return None
        left, top, right, bottom = state['rect']
        dpi = ctypes.windll.user32.GetDpiForWindow
        dpi.argtypes = [wintypes.HWND]
        scale = (dpi(hwnd) or 96) / 96
        controls, visited = [], 0
        menu_found = False
        condition = self.ui.CreateTrueCondition()

        def visit(element, depth):
            nonlocal visited, menu_found
            if depth > 22 or visited >= 160:
                return
            rect = element.CurrentBoundingRectangle
            if rect.bottom <= top or rect.top >= top+40*scale:
                return
            visited += 1
            kind = element.CurrentControlType
            if kind in (50000, 50005, 50010, 50011, 50020) and rect.bottom <= top+44*scale:
                controls.append(tuple(round(v/scale) for v in
                                      (rect.left-left, rect.top-top, rect.right-left, rect.bottom-top)))
                menu_found = menu_found or kind in (50010, 50011)
            children = element.FindAll(self.api.TreeScope_Children, condition)
            for index in range(min(children.Length, 60)):
                visit(children.GetElement(index), depth+1)

        visit(self.ui.ElementFromHandle(hwnd), 0)
        if not menu_found:
            return None
        width = round((right-left)/scale)
        controls.append((width-140, 0, width, 36))  # Also protect native caption buttons while loading.
        return {"window": hwnd, "width": width, "controls": controls,
                "slot": titlebar_slot(width, controls), "source": "uia-titlebar"}


def limit_windows(result):
    buckets = result.get("rateLimitsByLimitId") or {"codex": result.get("rateLimits")}
    windows = []
    for key, bucket in buckets.items():
        if not bucket:
            continue
        for slot in ("primary", "secondary"):
            window = bucket.get(slot)
            if not window:
                continue
            used = window.get("usedPercent")
            minutes = window.get("windowDurationMins")
            if used is None:
                continue
            label = f"{minutes // 1440}d" if minutes and minutes % 1440 == 0 else f"{minutes // 60}h" if minutes and minutes % 60 == 0 else f"{minutes}m" if minutes else slot
            windows.append({"bucket": bucket.get("limitName") or key, "label": label,
                            "remaining": max(0, min(100, 100 - used)), "reset": window.get("resetsAt"),
                            "minutes": minutes})
    return windows


def codex_windows():
    """Read only visible Codex window bounds for optional position following."""
    user = ctypes.windll.user32
    user.GetWindowRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
    results = []
    for hwnd in CurrentPageTracker.visible_windows():
        rect = wintypes.RECT()
        if user.GetWindowRect(hwnd, ctypes.byref(rect)):
            if rect.right - rect.left > 300 and rect.bottom - rect.top > 300:
                results.append((rect.left, rect.top, rect.right, rect.bottom))
    return results


def host_window_state(hwnd):
    """Read native window lifecycle without activating or changing Codex."""
    user = ctypes.windll.user32
    for name in ("IsWindow", "IsWindowVisible", "IsIconic"):
        getattr(user, name).argtypes = [wintypes.HWND]
    user.GetWindowRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
    if not hwnd or not user.IsWindow(hwnd):
        return {"exists": False, "visible": False}
    rect = wintypes.RECT()
    user.GetWindowRect(hwnd, ctypes.byref(rect))
    cloaked = wintypes.DWORD()
    try:
        dwm = ctypes.windll.dwmapi.DwmGetWindowAttribute
        dwm.argtypes = [wintypes.HWND, wintypes.DWORD, ctypes.c_void_p, wintypes.DWORD]
        dwm(hwnd, 14, ctypes.byref(cloaked), ctypes.sizeof(cloaked))
    except OSError:
        pass
    return {"exists": True,
            "visible": bool(user.IsWindowVisible(hwnd) and not user.IsIconic(hwnd) and not cloaked.value),
            "rect": [rect.left, rect.top, rect.right, rect.bottom]}


def titlebar_slot(width, controls):
    """Largest empty header interval, leaving padding around all controls."""
    intervals = sorted((max(0, l-8), min(width, r+8)) for l, t, r, b in controls
                       if r > l and b > 2 and t < 42)
    if not intervals:
        return [340, 2, max(340, width-150), 42]
    gaps, cursor = [], 8
    for left, right in intervals:
        if left > cursor:
            gaps.append((left-cursor, cursor, left))
        cursor = max(cursor, right)
    if cursor < width-8:
        gaps.append((width-8-cursor, cursor, width-8))
    if not gaps:
        return [0, 2, 0, 42]
    _, left, right = max(gaps)
    return [left, 2, right, 42]


def dock_position(host, size, screens, slot=None):
    """Dock inside the blank title bar; use below-menu only if it cannot fit."""
    left, top, right, bottom = host
    width, height = size
    slot = slot or titlebar_slot(right-left, [])
    sl, st, sr, sb = slot
    if sr-sl >= width and sb-st >= height:
        x, y = round(left+(sl+sr-width)/2), round(top+(st+sb-height)/2)
        if any(l <= x and t <= y and x+width <= r and y+height <= b for l, t, r, b in screens):
            return x, y, "titlebar-blank"
    center_x = (left + right - width) / 2
    sl, st, sr, sb = max(screens, key=lambda s:
        max(0, min(right, s[2])-max(left, s[0])) * max(0, min(bottom, s[3])-max(top, s[1])))
    x = round(max(sl, min(center_x, sr-width)))
    y = max(st, top+64)
    y = min(y, sb-height)
    return x, y, "below-menu"


def overlaps_host_menu(position, size, host, slot=None):
    x, y = position
    width, height = size
    left, top, right, _ = host
    intersects = x < right and x+width > left and y < top+44 and y+height > top
    slot = slot or titlebar_slot(right-left, [])
    sl, st, sr, sb = slot
    in_blank = left+sl <= x and x+width <= left+sr and top+st <= y and y+height <= top+sb
    return intersects and not in_blank


def load_settings():
    try:
        if SETTINGS.stat().st_size < 16384:
            return json.loads(SETTINGS.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        pass
    return {}


def write_json_atomic(path, value):
    """Keep all staging in workspace/tmp and avoid partial diagnostic reads."""
    pending = RUNTIME / f"{path.name}.{os.getpid()}.pending"
    try:
        pending.write_text(json.dumps(value, ensure_ascii=False, indent=2), encoding="utf-8")
        pending.replace(path)
        return True
    except OSError:
        # Another short-lived reader or shutdown must not crash a Qt slot.
        return False


def run_gui(args):
    from PyQt6.QtCore import Qt, QTimer, QPoint, QObject, pyqtSignal, QByteArray
    from PyQt6.QtGui import QFont, QFontDatabase, QIcon, QPixmap, QPainter
    from PyQt6.QtSvg import QSvgRenderer
    from PyQt6.QtSvgWidgets import QSvgWidget
    from PyQt6.QtNetwork import QLocalServer, QLocalSocket
    from PyQt6.QtWidgets import (QApplication, QWidget, QFrame, QLabel, QPushButton,
                                QVBoxLayout, QHBoxLayout, QGridLayout, QComboBox, QCheckBox)

    app = QApplication(sys.argv[:1])
    # The offscreen Windows Qt plugin needs explicit fonts; native Windows also
    # benefits from deterministic Chinese and color-emoji font selection.
    for font_file in ("msyh.ttc", "seguiemj.ttf", "segoeui.ttf"):
        font_path = Path(os.environ.get("WINDIR", "C:/Windows")) / "Fonts" / font_file
        if font_path.exists():
            QFontDatabase.addApplicationFont(str(font_path))
    app.setApplicationName("Codex Session Bar")
    app.setQuitOnLastWindowClosed(False)
    server = QLocalServer()
    if not args.test_ui:
        socket = QLocalSocket()
        socket.connectToServer(INSTANCE_NAME)
        if socket.waitForConnected(500):
            socket.write(b"show")
            socket.flush()
            socket.waitForBytesWritten(500)
            return 0
        if not server.listen(INSTANCE_NAME):
            raise RuntimeError("Session bar instance service failed")

    class Bridge(QObject):
        data = pyqtSignal(dict)
        error = pyqtSignal(str)
        renamed = pyqtSignal(str, str)
        rename_failed = pyqtSignal(str)
        active_changed = pyqtSignal(dict)
        recents = pyqtSignal(dict)
        navigated = pyqtSignal(str)
        navigation_failed = pyqtSignal(str, str)

    bridge = Bridge()
    commands = queue.Queue()
    stop = threading.Event()
    data_wake = threading.Event()
    quota_wake = threading.Event()
    recent_wake = threading.Event()
    recent_visible = threading.Event()
    selected = [args.thread or default_thread()]
    binding = {"auto": load_settings().get("auto_thread", True), "view": None}
    rpc = Rpc()
    desktop = DesktopTools()
    recent_client = DesktopTools()
    account = [{"limits": {}, "limits_at": 0, "limits_error": ""}]

    def track_active():
        tracker = None
        signature = None
        while not stop.is_set():
            try:
                if tracker is None:
                    tracker = CurrentPageTracker()
                observed = tracker.poll()
            except Exception as exc:
                observed = {"thread": None, "available": False, "source": "unavailable",
                            "message": "Chat follow failed: " + str(exc)[:100]}
            binding["view"] = observed
            current = (observed.get("thread"), observed.get("window"), observed.get("source"), observed.get("label"))
            if current != signature:
                signature = current
                bridge.active_changed.emit(observed)
            stop.wait(.10 if tracker is not None else 1)
        if tracker:
            tracker.close()

    def account_worker():
        while not stop.is_set():
            previous = account[0]
            try:
                result = rpc.call("account/rateLimits/read")
                account[0] = {"limits": result, "limits_at": time.time(), "limits_error": ""}
                retry = 60
            except Exception as exc:
                account[0] = {**previous, "limits_error": str(exc)[:160]}
                retry = 15
            data_wake.set()
            quota_wake.wait(retry)
            quota_wake.clear()

    def titlebar_worker():
        # Keep header inspection off the fast chat-follow and GUI threads.
        tracker = None
        try:
            while not stop.is_set():
                try:
                    if tracker is None:
                        tracker = CurrentPageTracker()
                    hwnd = binding.get("host_window")
                    if hwnd:
                        layout = tracker.read_titlebar(hwnd)
                        if layout:
                            binding["titlebar"] = layout
                            binding["titlebar_error"] = ""
                except Exception as exc:
                    binding["titlebar_error"] = str(exc)[:160]
                stop.wait(1)
        finally:
            if tracker:
                tracker.close()

    def control_worker():
        store = SessionStore()
        while not stop.is_set():
            try:
                command = commands.get(timeout=.5)
            except queue.Empty:
                continue
            try:
                if command[0] == "icon":
                    tid, icon = command[1:]
                    current = store.read(tid)["label"]
                    name = with_icon(current, icon)
                    desktop.rename(tid, name)
                    bridge.renamed.emit(tid, name)
                    data_wake.set()
                elif command[0] == "refresh":
                    quota_wake.set()
                elif command[0] == 'navigate':
                    desktop.navigate(command[1])
                    bridge.navigated.emit(command[1])
            except Exception as exc:
                if command[0] == 'navigate':
                    bridge.navigation_failed.emit(command[1], str(exc)[:220])
                else:
                    bridge.rename_failed.emit(str(exc)[:220])

    def recent_worker():
        client, store = recent_client, SessionStore()
        try:
            while not stop.is_set():
                recent_wake.wait(2 if recent_visible.is_set() else None)
                recent_wake.clear()
                if stop.is_set():
                    break
                if not recent_visible.is_set():
                    continue
                try:
                    rows = client.recent(store)
                    bridge.recents.emit({'rows': rows, 'loaded_at': time.time()})
                except Exception as exc:
                    client.close()
                    bridge.recents.emit({'rows': [], 'error': str(exc)[:200]})
        finally:
            client.close()

    def worker():
        store = SessionStore()
        sessions, sessions_at = [], 0
        while not stop.is_set():
            data_wake.clear()
            tid = selected[0]
            try:
                if binding["auto"] and not (binding["view"] or {}).get("available"):
                    data_wake.wait(.2)
                    continue
                snapshot = store.read(tid)
                if not sessions or time.monotonic() - sessions_at >= 3:
                    sessions, sessions_at = store.sessions(), time.monotonic()
                snapshot["sessions"] = sessions
                snapshot.update(account[0])
                # Coalesce fast navigation: discard work for superseded chats
                # before enqueueing a GUI update, as well as checking in the GUI.
                view = binding["view"] or {}
                if tid == selected[0] and (not binding["auto"] or tid == view.get("thread")):
                    bridge.data.emit(snapshot)
            except Exception as exc:
                if tid == selected[0]:
                    bridge.error.emit(str(exc)[:220])
            data_wake.wait(.5)

    class TitleLabel(QLabel):
        def mouseReleaseEvent(self, event):
            if event.button() == Qt.MouseButton.LeftButton and self.rect().contains(event.position().toPoint()):
                bar.show_recent()

    class DragHandle(QLabel):
        def mousePressEvent(self, event):
            if event.button() == Qt.MouseButton.LeftButton:
                self.press_point = event.globalPosition().toPoint()
                self.dragged = False
                bar.drag_delta = event.globalPosition().toPoint() - bar.pos()

        def mouseMoveEvent(self, event):
            if event.buttons() & Qt.MouseButton.LeftButton and bar.drag_delta is not None:
                if (event.globalPosition().toPoint()-self.press_point).manhattanLength() >= QApplication.startDragDistance():
                    self.dragged = True
                if self.dragged:
                    bar.move(event.globalPosition().toPoint() - bar.drag_delta)

        def mouseReleaseEvent(self, event):
            if event.button() == Qt.MouseButton.LeftButton:
                bar.drag_delta = None
                if getattr(self, 'dragged', False):
                    bar.remember_host_offset()
                    bar.write_settings()

    class RecentPopup(QFrame):
        def hideEvent(self, event):
            recent_visible.clear()
            super().hideEvent(event)

    class DetailsPopup(QFrame):
        def hideEvent(self, event):
            bar.expanded = False
            super().hideEvent(event)

    class QuotaView(QWidget):
        def __init__(self):
            super().__init__()
            self.row = QHBoxLayout(self)
            self.row.setContentsMargins(0, 0, 0, 0)
            self.row.setSpacing(10)
            self.cells = []
            self.set_windows([])

        def set_windows(self, windows, stale=False):
            shown = windows[:2] or [None]
            while len(self.cells) < len(shown):
                cell = QWidget()
                row = QHBoxLayout(cell)
                row.setContentsMargins(0, 0, 0, 0)
                row.setSpacing(4)
                label, svg, percent = QLabel(), QSvgWidget(), QLabel()
                label.setObjectName("muted")
                svg.setFixedSize(43, 16)
                percent.setFixedWidth(36)
                row.addWidget(label)
                row.addWidget(svg)
                row.addWidget(percent)
                self.row.addWidget(cell)
                self.cells.append((cell, label, svg, percent))
            for index, (cell, label, svg, percent) in enumerate(self.cells):
                cell.setVisible(index < len(shown))
                if index >= len(shown):
                    continue
                window = shown[index]
                value = window['remaining'] if window else None
                share = None
                if window and window.get('reset') and window.get('minutes'):
                    share = (window['reset'] - time.time()) / (window['minutes'] * 60)
                label.setText(window['label'] if window else 'Limit')
                svg.load(QByteArray(quota_svg(value, share).encode()))
                percent.setText((f"{value:g}%" if value is not None else '—') + ('*' if stale else ''))
                cell.layout().invalidate()
            self.row.invalidate()
            self.updateGeometry()
            self.setAccessibleName('Account quota remaining')

    def svg_icon(paths):
        svg = ('<svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" '
               'fill="none" stroke="#b7c2cf" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">'
               + paths + '</svg>')
        pixmap = QPixmap(36, 36)
        pixmap.fill(Qt.GlobalColor.transparent)
        painter = QPainter(pixmap)
        QSvgRenderer(QByteArray(svg.encode())).render(painter)
        painter.end()
        pixmap.setDevicePixelRatio(2)
        return QIcon(pixmap)

    class Bar(QWidget):
        def __init__(self):
            super().__init__()
            settings = load_settings()
            self.topmost = settings.get("topmost", True)
            self.host_hwnd = None
            self.host_rect = None
            # Reset old exterior/bottom-relative docking once on upgrade.
            self.position_version = 3
            self.host_offset = settings.get("host_offset") if settings.get("position_version") == 3 else None
            if not isinstance(self.host_offset, list) or len(self.host_offset) != 2:
                self.host_offset = None
            self.placement = "titlebar-blank"
            self.host_hidden = False
            self.last_host_search = 0
            self.expanded = False
            self.drag_delta = None
            self.snapshot = {}
            self.data_error = ""
            self.last_rename = None
            self.last_icon_action = None
            self.last_navigation = None
            self.recent_result = {}
            self.recent_signature = None
            self.popup_kind = None
            self.follow_latency = None
            self.tracker_ready = False
            self.view = None
            self.popup = None
            self.setWindowTitle("Codex Session Bar")
            self.setObjectName("SessionBar")
            self.setWindowFlags(Qt.WindowType.FramelessWindowHint | Qt.WindowType.Tool |
                                (Qt.WindowType.WindowStaysOnTopHint if self.topmost else Qt.WindowType.Widget))
            self.setAttribute(Qt.WidgetAttribute.WA_TranslucentBackground)
            self.setFont(QFont("Segoe UI", 9))
            self.setFixedHeight(40)
            self.setStyleSheet("""
                QFrame#shell, QFrame#details {background:#23272b; border:1px solid #41494e; border-radius:10px;}
                QLabel {color:#eef2f2; background:transparent; border:0;}
                QLabel#muted {color:#aeb8bf;} QLabel#status {color:#7cd7ae;}
                QPushButton {background:transparent;color:#b7c2cf;border:0;border-radius:6px;padding:3px 5px;}
                QPushButton:hover {background:#45515a;} QPushButton:checked {background:#315c4a;}
                QComboBox {background:#30373d;color:#eaf0f1;border:1px solid #46515a;border-radius:6px;padding:5px;}
                QComboBox QAbstractItemView {background:#262d32;color:#eef2f2;selection-background-color:#315c4a;}
            """)
            outer = QVBoxLayout(self)
            self.outer = outer
            outer.setContentsMargins(2, 2, 2, 2)
            shell = QFrame()
            shell.setObjectName("shell")
            outer.addWidget(shell)
            row = QHBoxLayout(shell)
            self.row = row
            row.setContentsMargins(6, 2, 6, 2)
            row.setSpacing(6)
            self.icon = QPushButton("◈")
            self.icon.setFont(QFont("Segoe UI Emoji", 14))
            self.icon.setFixedSize(28, 28)
            self.icon.setToolTip("Session icon")
            self.icon.setEnabled(not binding["auto"])
            self.icon.clicked.connect(self.show_icons)
            row.addWidget(self.icon)
            self.title = TitleLabel("Connecting…")
            self.title.setFixedWidth(85)
            self.title.setCursor(Qt.CursorShape.PointingHandCursor)
            self.title.setToolTip("Click for recent chats")
            row.addWidget(self.title)
            self.status = QLabel("Loading")
            self.status.setObjectName("status")
            row.addWidget(self.status)
            self.elapsed = QLabel("—")
            self.elapsed.setObjectName("muted")
            self.elapsed.setMinimumWidth(40)
            row.addWidget(self.elapsed)
            self.grip = DragHandle()
            self.grip.setFixedSize(14, 28)
            dots = ''.join(f'<circle cx="{x}" cy="{y}" r="1"/>' for x in (9, 15) for y in (6, 12, 18))
            self.grip.setPixmap(svg_icon(dots).pixmap(14, 22))
            self.grip.setAlignment(Qt.AlignmentFlag.AlignCenter)
            self.grip.setCursor(Qt.CursorShape.SizeAllCursor)
            self.grip.setToolTip('Drag to move')
            self.grip.setAccessibleName('Move session bar')
            row.addWidget(self.grip)
            self.tokens = QLabel("—")
            self.tokens.setObjectName("muted")
            row.addWidget(self.tokens)
            self.tps = QLabel("— t/s")
            self.tps.setObjectName("muted")
            row.addWidget(self.tps)
            self.quota = QuotaView()
            row.addWidget(self.quota)
            self.pin = QPushButton()
            self.pin.setIcon(svg_icon('<path d="m16 3 5 5-4 2-3 5-2-2-7 7 7-7-2-2 5-3z"/>'))
            self.pin.setToolTip("Always on top")
            self.pin.setFixedSize(28, 28)
            self.pin.setCheckable(True)
            self.pin.setChecked(self.topmost)
            self.pin.clicked.connect(self.toggle_pin)
            row.addWidget(self.pin)
            self.expand = QPushButton()
            self.expand.setIcon(svg_icon('<path d="m6 9 6 6 6-6"/>'))
            self.expand.setFixedSize(26, 28)
            self.expand.setToolTip("Session details")
            self.expand.clicked.connect(self.toggle_details)
            row.addWidget(self.expand)
            close = QPushButton()
            close.setIcon(svg_icon('<path d="m6 6 12 12M18 6 6 18"/>'))
            close.setFixedSize(26, 28)
            close.setToolTip("Quit session bar")
            close.clicked.connect(self.close)
            row.addWidget(close)
            self.details = DetailsPopup(self, Qt.WindowType.Popup | Qt.WindowType.FramelessWindowHint)
            self.details.setObjectName("details")
            self.details.setFixedWidth(560)
            detail = QVBoxLayout(self.details)
            detail.setContentsMargins(16, 14, 16, 14)
            detail.setSpacing(10)
            self.auto = QCheckBox("Follow current chat")
            self.auto.setStyleSheet("QCheckBox{color:#eef2f2;spacing:8px;}")
            self.auto.setChecked(binding["auto"])
            self.auto.setToolTip("Turn off to keep a fixed chat")
            self.auto.toggled.connect(self.set_auto_thread)
            detail.addWidget(self.auto)
            self.selector = QComboBox()
            self.selector.setToolTip("Choose a chat to switch to fixed mode")
            self.selector.currentIndexChanged.connect(self.select_thread)
            detail.addWidget(self.selector)
            self.token_detail = QLabel("Loading token details…")
            self.token_detail.setWordWrap(True)
            detail.addWidget(self.token_detail)
            self.limit_detail = QLabel("Loading account limits…")
            self.limit_detail.setWordWrap(True)
            detail.addWidget(self.limit_detail)
            self.path = QLabel("")
            self.path.setObjectName("muted")
            self.path.setWordWrap(True)
            detail.addWidget(self.path)
            actions = QHBoxLayout()
            dock = QPushButton("Dock in title bar")
            dock.clicked.connect(self.dock)
            actions.addWidget(dock)
            refresh = QPushButton("Refresh limits")
            refresh.clicked.connect(lambda: commands.put(("refresh",)))
            actions.addWidget(refresh)
            actions.addStretch()
            detail.addLayout(actions)
            self.note = QLabel("Chat detection 0.1s · Status 0.5s · Account limits 60s")
            self.note.setObjectName("muted")
            self.note.setWordWrap(True)
            detail.addWidget(self.note)
            self.details.hide()
            self.clock = QTimer(self)
            self.clock.timeout.connect(self.tick)
            self.clock.start(1000)
            bridge.data.connect(self.update_data)
            bridge.error.connect(self.show_error)
            bridge.renamed.connect(self.rename_done)
            bridge.rename_failed.connect(self.rename_error)
            bridge.active_changed.connect(self.active_changed)
            bridge.recents.connect(self.update_recent)
            bridge.navigated.connect(self.navigation_done)
            bridge.navigation_failed.connect(self.navigation_error)
            self.host_clock = QTimer(self)
            self.host_clock.timeout.connect(self.sync_host)
            if not args.test_ui:
                self.host_clock.start(80)
            self.fit_content()

        def fit_content(self):
            # No stretch: short titles should bring status directly alongside.
            text = strip_icon(self.snapshot.get("label", self.title.text()))
            width = min(180, max(55, self.title.fontMetrics().horizontalAdvance(text) + 2))
            self.title.setFixedWidth(width)
            self.title.setText(self.title.fontMetrics().elidedText(text, Qt.TextElideMode.ElideRight, width))
            widgets = [self.row.itemAt(i).widget() for i in range(self.row.count())]
            content_width = sum(max(w.minimumWidth(), w.minimumSizeHint().width(), w.sizeHint().width()) for w in widgets)
            margins = self.row.contentsMargins()
            outer_margins = self.outer.contentsMargins()
            self.setFixedWidth(content_width + self.row.spacing() * (len(widgets)-1)
                               + margins.left()+margins.right()+outer_margins.left()+outer_margins.right()+2)
            self.layout().activate()

        def toggle_pin(self):
            point = self.pos()
            self.topmost = self.pin.isChecked()
            self.setWindowFlag(Qt.WindowType.WindowStaysOnTopHint, self.topmost)
            self.show()
            self.move(point)
            self.write_settings()

        def toggle_details(self):
            if self.details.isVisible():
                self.details.hide()
                return
            if self.popup:
                self.popup.close()
            self.details.adjustSize()
            self.place_popup(self.details)
            self.expanded = True
            self.details.show()

        def place_popup(self, popup):
            anchor = self.mapToGlobal(QPoint(0, 0))
            bounds = (QApplication.screenAt(anchor) or self.screen()).availableGeometry()
            x = max(bounds.left(), min(anchor.x(), bounds.right() - popup.width()))
            y = anchor.y() - popup.height() - 8
            if y < bounds.top():
                y = anchor.y() + self.height() + 8
            popup.move(x, max(bounds.top(), min(y, bounds.bottom() - popup.height())))

        def select_thread(self, index):
            tid = self.selector.itemData(index)
            if tid and tid != selected[0]:
                self.set_auto_thread(False)
                selected[0] = tid
                data_wake.set()
                self.snapshot = {}
                self.title.setText("Switching…")
                self.status.setText("Loading")
                self.tokens.setText("—")
                self.tps.setText("— t/s")
                self.write_settings()

        def set_auto_thread(self, enabled):
            binding["auto"] = enabled
            self.auto.blockSignals(True)
            self.auto.setChecked(enabled)
            self.auto.blockSignals(False)
            if enabled:
                self.active_changed(binding["view"] or {"available": False, "message": "Detecting current chat…"})
            else:
                self.icon.setEnabled(bool(self.snapshot))
            data_wake.set()
            self.write_settings()

        def active_changed(self, view):
            self.view = view
            if isinstance(view.get("window"), int):
                self.host_hwnd = view["window"]
            if not binding["auto"]:
                return
            tid = view.get("thread")
            valid = view.get("available") and bool(tid)
            self.tracker_ready = bool(valid)
            if valid and tid == selected[0] and self.snapshot:
                self.icon.setEnabled(True)
                return
            if self.popup:
                self.popup.close()
            self.snapshot = {}
            self.data_error = ""
            self.icon.setEnabled(False)
            self.icon.setText("◈")
            self.tokens.setText("—")
            self.tps.setText("— t/s")
            self.token_detail.setText("Loading current chat…" if valid else "No identifiable chat")
            self.path.setText("")
            self.elapsed.setText("—")
            if valid:
                selected[0] = tid
                data_wake.set()
                self.title.setText("Switching…")
                self.note.setText("Following current Codex chat")
                self.status.setText("Loading")
                self.write_settings()
            else:
                self.title.setText("Waiting for chat")
                self.status.setText("Waiting")
                self.note.setText(view.get("message") or "No chat page selected")
            self.write_diagnostic()

        def update_data(self, data):
            if data.get("id") != selected[0]:
                return
            if binding["auto"] and (not self.tracker_ready or data.get("id") != (self.view or {}).get("thread")):
                return
            if not self.snapshot and binding["auto"]:
                detected = (self.view or {}).get("detected_at")
                if detected:
                    event_time = datetime.fromisoformat(self.view["timestamp"].replace("Z", "+00:00")).timestamp()
                    event_ms = (time.time() - event_time) * 1000
                    self.follow_latency = {"detection_to_data_ms": round((time.time() - detected) * 1000, 1),
                                           "event_to_data_ms": round(event_ms, 1) if 0 <= event_ms < 10000 else None}
            self.snapshot = data
            self.data_error = ""
            label = data.get("label", "Untitled chat")
            self.title.setText(self.title.fontMetrics().elidedText(strip_icon(label), Qt.TextElideMode.ElideRight, self.title.width()))
            self.title.setToolTip(label + "\nClick for recent chats")
            self.icon.setText(leading_icon(label))
            self.icon.setEnabled(True)
            sessions = data.get("sessions", [])
            new_items = [(s["id"], s["label"]) for s in sessions]
            old_items = [(self.selector.itemData(i), self.selector.itemText(i)) for i in range(self.selector.count())]
            if new_items != old_items:
                self.selector.blockSignals(True)
                self.selector.clear()
                for tid, name in new_items:
                    self.selector.addItem(name, tid)
                if self.selector.findData(selected[0]) < 0:
                    self.selector.addItem(label, selected[0])
                self.selector.setCurrentIndex(self.selector.findData(selected[0]))
                self.selector.blockSignals(False)
            usage = data.get("usage", {})
            total = usage.get("total_token_usage", {})
            recent = usage.get("last_token_usage", {})
            value = total.get("total_tokens", data.get("tokens_used"))
            self.tokens.setText(compact(value))
            self.tokens.setToolTip(f"Session tokens: {compact(value)}\nLast call: {compact(recent.get('total_tokens'))}")
            self.token_detail.setText("Session tokens: {:,}\nInput: {:,} · Output: {:,} · Cached input: {:,}\nLast call: {}".format(
                value or 0, total.get("input_tokens", 0), total.get("output_tokens", 0),
                total.get("cached_input_tokens", 0), compact(recent.get('total_tokens'))) if total else "Token details unavailable")
            self.token_detail.setToolTip("Session totals include repeated context. Cached input is included in input. Last call is not the whole turn.")
            self.path.setText(f"{data.get('model') or 'Model unavailable'}  ·  {data.get('cwd', '')}")
            self.tick()
            self.write_diagnostic()

        def tick(self):
            turn = self.snapshot.get("turn", {})
            code = turn.get("status")
            labels = {"inProgress": "● Working", "completed": "✓ Done", "interrupted": "Ⅱ Paused", "failed": "! Error"}
            self.status.setText(labels.get(code, code or "Loading"))
            if binding["auto"] and not self.tracker_ready:
                self.status.setText("Waiting")
            if self.data_error:
                self.status.setText("! Unavailable")
            self.status.setStyleSheet("color:#f5a34b" if code == "inProgress" else "color:#7cd7ae" if code == "completed" else "color:#e9bd77")
            start = turn.get("started_at")
            end = turn.get("completed_at")
            self.elapsed.setText(duration((end or time.time()) - start) if start else "—")
            rate = self.snapshot.get('output_rate')
            if rate and start and rate['start'] >= start:
                self.tps.setText(f"{rate['value']:.1f} t/s")
                self.tps.setToolTip(f"Observed output TPS: {rate['output_tokens']:,} output tokens / {rate['end']-rate['start']:.2f}s.\n"
                                    "Based on the latest saved usage samples, including tool/network wait; not instantaneous model decode speed.")
            else:
                self.tps.setText("— t/s")
                self.tps.setToolTip("Output TPS: waiting for two usage samples in this turn")
            limits = self.snapshot.get("limits", {})
            windows = limit_windows(limits)
            error = self.snapshot.get("limits_error", "")
            if windows:
                stale = bool(error) or time.time() - self.snapshot.get("limits_at", 0) > 180
                self.quota.set_windows(windows, stale)
                text = "\n".join(f"{w['bucket']} · {w['label']}: {w['remaining']:g}% left · {reset_text(w['reset'])}" for w in windows)
                if stale:
                    text += "\n* Last known quota; refresh pending"
                self.limit_detail.setText(text)
                self.quota.setToolTip(text + ("\nRefresh failed: " + error if error else "") + "\nBattery = remaining quota · Ring = time until reset\nLimits are shared across the account.")
            elif error:
                self.quota.set_windows([])
                self.limit_detail.setText("Account limits unavailable: " + error)
                self.quota.setToolTip("Account limits unavailable: " + error)
            self.fit_content()

        def rename_done(self, tid, name):
            self.last_rename = {"thread": tid, "title": name, "desktop_confirmed": True}
            self.last_icon_action = {"phase": "confirmed", "thread": tid, "at": time.time()}
            self.note.setText("Synced to Codex: " + name)
            if tid == selected[0]:
                self.snapshot["label"] = name
                self.icon.setText(leading_icon(name))
                self.title.setToolTip(name + "\nClick for recent chats")
            self.icon.setToolTip("Synced to Codex: " + name)
            self.icon.setEnabled(bool(self.snapshot) and (not binding["auto"] or self.tracker_ready))
            self.write_diagnostic()

        def rename_error(self, error):
            self.last_rename = {"thread": selected[0], "desktop_confirmed": False, "error": error}
            self.last_icon_action = {"phase": "failed", "thread": selected[0], "at": time.time(), "error": error}
            self.note.setText("Icon sync failed: " + error)
            self.icon.setEnabled(True)
            self.icon.setToolTip("Icon sync failed: " + error)
            if not self.expanded:
                self.toggle_details()
            self.write_diagnostic()

        def show_error(self, error):
            self.data_error = error
            self.note.setText("Read failed: " + error)
            self.status.setText("! Unavailable")
            self.icon.setEnabled(True)

        def show_recent(self):
            if self.popup and self.popup.isVisible():
                self.popup.close()
                if self.popup_kind == 'recent':
                    return
            self.details.hide()
            popup = RecentPopup(self, Qt.WindowType.Popup | Qt.WindowType.FramelessWindowHint)
            popup.setObjectName('recentPicker')
            popup.setStyleSheet('''
                QFrame#recentPicker{background:#242a2e;border:1px solid #47535b;border-radius:12px;}
                QLabel{color:#dce5ea;border:0;background:transparent;}
                QPushButton{background:transparent;border:1px solid transparent;border-radius:6px;color:#c8d2db;}
                QPushButton:hover{background:#35434a;border-color:#52636e;}
                QPushButton:focus{border-color:#6aa891;}
            ''')
            bounds = (QApplication.screenAt(self.pos()) or self.screen()).availableGeometry()
            popup.setFixedWidth(min(560, bounds.width()-24))
            layout = QVBoxLayout(popup)
            layout.setContentsMargins(12, 10, 12, 12)
            layout.setSpacing(8)
            heading = QHBoxLayout()
            title = QLabel('Recent chats')
            title.setStyleSheet('font-weight:600;font-size:13px;')
            heading.addWidget(title)
            heading.addStretch()
            refresh = QPushButton('Refresh')
            refresh.setFixedSize(66, 26)
            refresh.clicked.connect(lambda: recent_wake.set())
            heading.addWidget(refresh)
            layout.addLayout(heading)
            self.recent_area = QWidget(popup)
            self.recent_layout = QVBoxLayout(self.recent_area)
            self.recent_layout.setContentsMargins(0, 0, 0, 0)
            self.recent_layout.setSpacing(4)
            layout.addWidget(self.recent_area)
            self.recent_note = QLabel('Latest 10 · Busy first · Updates every 2s')
            self.recent_note.setStyleSheet('color:#8f9da9;font-size:11px;')
            self.recent_note.setWordWrap(True)
            layout.addWidget(self.recent_note)
            self.popup, self.popup_kind = popup, 'recent'
            self.recent_signature = None
            cached = self.recent_result if time.time()-self.recent_result.get('loaded_at', 0) < 3 else {}
            self.render_recent(cached)
            self.place_popup(popup)
            popup.show()
            recent_visible.set()
            recent_wake.set()

        def update_recent(self, result):
            self.recent_result = result
            if self.popup_kind == 'recent' and self.popup and self.popup.isVisible():
                self.render_recent(result)

        def render_recent(self, result):
            rows = result.get('rows', [])
            signature = (tuple((r['id'], r['label'], r['state']) for r in rows), result.get('error'),
                         bool(result.get('loaded_at')), selected[0])
            if signature == self.recent_signature:
                return
            self.recent_signature = signature
            while self.recent_layout.count():
                item = self.recent_layout.takeAt(0)
                if item.widget():
                    item.widget().deleteLater()
            self.recent_buttons = []
            for row in rows:
                button = QPushButton(self.recent_area)
                button.setProperty('threadId', row['id'])
                button.setFixedHeight(38)
                button.setCursor(Qt.CursorShape.PointingHandCursor)
                if row['id'] == selected[0]:
                    button.setStyleSheet('QPushButton{background:#293d36;border:1px solid #527b69;} QPushButton:hover{background:#354b42;}')
                elif row['state'] == 'Busy':
                    button.setStyleSheet('QPushButton{background:#342c23;} QPushButton:hover{background:#483a2a;}')
                line = QHBoxLayout(button)
                line.setContentsMargins(10, 0, 10, 0)
                label = QLabel(row['label'])
                label.setFont(QFont('Segoe UI', 10))
                label.setText(label.fontMetrics().elidedText(row['label'], Qt.TextElideMode.ElideRight, self.popup.width()-160))
                status = QLabel(row['state_icon']+' '+row['state'])
                status.setAlignment(Qt.AlignmentFlag.AlignRight | Qt.AlignmentFlag.AlignVCenter)
                status.setFixedWidth(92)
                status.setStyleSheet('color:'+row['color']+';font-weight:600;')
                for widget in (label, status):
                    widget.setAttribute(Qt.WidgetAttribute.WA_TransparentForMouseEvents)
                line.addWidget(label, 1)
                line.addWidget(status)
                button.setToolTip(row['label']+'\n'+row['state']+' · Click to open in Codex')
                button.setAccessibleName(row['label']+' · '+row['state'])
                button.clicked.connect(lambda unused=False, tid=row['id']: self.open_thread(tid))
                self.recent_layout.addWidget(button)
                self.recent_buttons.append(button)
            if not rows:
                message = QLabel('Could not load recent chats' if result.get('error') else
                                 'No recent local chats' if result.get('loaded_at') else 'Loading recent chats…')
                message.setMinimumHeight(38)
                self.recent_layout.addWidget(message)
            self.recent_note.setText(('Load failed: '+result['error']) if result.get('error') else
                                     'Latest 10 · Busy first · Updates every 2s')
            self.popup.layout().activate()
            self.popup.adjustSize()
            self.place_popup(self.popup)

        def open_thread(self, tid):
            if self.popup:
                self.popup.close()
            self.set_auto_thread(True)
            self.last_navigation = {'thread': tid, 'phase': 'queued', 'at': time.time()}
            commands.put(('navigate', tid))
            self.write_diagnostic()

        def navigation_done(self, tid):
            self.last_navigation = {'thread': tid, 'phase': 'opened', 'at': time.time()}
            recent_wake.set()
            self.write_diagnostic()

        def navigation_error(self, tid, error):
            self.last_navigation = {'thread': tid, 'phase': 'failed', 'error': error, 'at': time.time()}
            self.note.setText('Could not open chat: '+error)
            if not self.details.isVisible():
                self.toggle_details()
            self.write_diagnostic()

        def show_icons(self):
            if not self.snapshot or (binding["auto"] and not self.tracker_ready):
                return
            if binding["auto"] and (self.view or {}).get("source") != "uia-current-page":
                self.note.setText("Waiting to identify the current chat")
                return
            if self.popup and self.popup.isVisible():
                self.popup.close()
                return
            self.details.hide()
            popup = QFrame(self, Qt.WindowType.Popup | Qt.WindowType.FramelessWindowHint)
            popup.setObjectName("iconPicker")
            popup.setStyleSheet("QFrame#iconPicker{background:#242a2e;border:1px solid #47535b;border-radius:12px;} QLabel{color:#d9e2e6;} QPushButton{background:transparent;border:0;border-radius:6px;color:#fff;padding:0px;} QPushButton:hover{background:#425149;}")
            layout = QVBoxLayout(popup)
            layout.setContentsMargins(12, 10, 12, 12)
            layout.addWidget(QLabel("Session icon"))
            grid = QGridLayout()
            grid.setSpacing(2)
            for row, icons in enumerate(ICON_ROWS):
                for column, icon in enumerate(icons):
                    button = QPushButton(icon)
                    button.setFont(QFont("Segoe UI Emoji", 15))
                    button.setFixedSize(32, 34)
                    button.setToolTip(icon)
                    button.clicked.connect(lambda unused=False, value=icon: self.choose_icon(value))
                    grid.addWidget(button, row, column)
            layout.addLayout(grid)
            clear = QPushButton("Clear icon")
            clear.setMinimumHeight(28)
            clear.clicked.connect(lambda: self.choose_icon(""))
            layout.addWidget(clear)
            popup.adjustSize()
            anchor = self.mapToGlobal(QPoint(0, 0))
            screen = QApplication.screenAt(anchor) or self.screen()
            bounds = screen.availableGeometry()
            x = max(bounds.left(), min(anchor.x(), bounds.right() - popup.width()))
            y = anchor.y() - popup.height() - 8
            if y < bounds.top():
                y = anchor.y() + self.height() + 8
            popup.move(x, max(bounds.top(), min(y, bounds.bottom() - popup.height())))
            self.popup = popup
            self.popup_kind = 'icons'
            self.popup_thread = selected[0]
            popup.show()
            self.last_icon_action = {"phase": "picker_open", "thread": selected[0], "at": time.time()}
            self.write_diagnostic()

        def choose_icon(self, icon):
            self.popup.close()
            if self.popup_thread != selected[0] or not self.snapshot:
                return
            self.icon.setEnabled(False)
            self.note.setText("Updating chat title…")
            self.last_icon_action = {"phase": "queued", "thread": selected[0], "icon": icon, "at": time.time()}
            self.write_diagnostic()
            commands.put(("icon", selected[0], icon))

        def logical_host_rect(self, rect):
            left, top, right, bottom = rect
            screen = QApplication.screenAt(QPoint((left + right)//2, (top + bottom)//2)) or self.screen()
            scale = screen.devicePixelRatio()
            if scale != 1:
                left, top, right, bottom = [round(v / scale) for v in (left, top, right, bottom)]
            return left, top, right, bottom

        def remember_host_offset(self):
            if self.host_rect:
                left, top, right, bottom = self.host_rect
                self.host_offset = [self.x() + self.width()/2 - (left+right)/2, self.y() - top]

        def dock_target(self):
            screens = []
            for screen in QApplication.screens():
                bounds = screen.availableGeometry()
                screens.append((bounds.x(), bounds.y(), bounds.x()+bounds.width(), bounds.y()+bounds.height()))
            return dock_position(self.host_rect, (self.width(), self.height()), screens, self.header_slot())

        def header_slot(self):
            layout = binding.get("titlebar", {})
            width = self.host_rect[2]-self.host_rect[0]
            if layout.get("window") == self.host_hwnd and abs(layout.get("width", 0)-width) <= 2:
                return layout['slot']
            return titlebar_slot(width, [])

        def sync_host(self):
            if not self.host_hwnd or not host_window_state(self.host_hwnd)['exists']:
                if time.monotonic() - self.last_host_search >= 1:
                    self.last_host_search = time.monotonic()
                    windows = CurrentPageTracker.visible_windows()
                    self.host_hwnd = windows[0] if windows else None
            state = host_window_state(self.host_hwnd)
            binding['host_window'] = self.host_hwnd
            if state.get('rect'):
                state['rect'] = self.logical_host_rect(state['rect'])
            self.sync_host_state(state)

        def sync_host_state(self, state):
            if not state.get('visible'):
                self.host_hidden = True
                if self.popup:
                    self.popup.close()
                self.details.hide()
                self.hide()
                return
            self.host_rect = state['rect']
            left, top, right, bottom = self.host_rect
            if self.drag_delta is None:
                if isinstance(self.host_offset, list) and len(self.host_offset) == 2:
                    self.move(round((left+right-self.width())/2 + self.host_offset[0]), round(top+self.host_offset[1]))
                    self.clamp()
                    self.placement = "custom"
                    if overlaps_host_menu((self.x(), self.y()), (self.width(), self.height()), self.host_rect, self.header_slot()):
                        self.host_offset = None
                if self.host_offset is None:
                    x, y, self.placement = self.dock_target()
                    self.move(x, y)
            if self.host_hidden or not self.isVisible():
                self.show()
            self.host_hidden = False
            if self.details.isVisible():
                self.place_popup(self.details)
            if self.popup and self.popup.isVisible():
                self.place_popup(self.popup)

        def dock(self):
            self.host_offset = None
            self.sync_host()
            self.write_settings()

        def clamp(self):
            screen = QApplication.screenAt(self.pos()) or self.screen()
            bounds = screen.availableGeometry()
            self.move(max(bounds.left(), min(self.x(), bounds.right() - self.width())),
                      max(bounds.top(), min(self.y(), bounds.bottom() - self.height())))

        def write_settings(self):
            write_json_atomic(SETTINGS, {"thread": selected[0], "topmost": self.topmost,
                                        "auto_thread": binding["auto"], "x": self.x(), "y": self.y(),
                                        "host_offset": self.host_offset, "position_version": self.position_version})

        def write_diagnostic(self):
            d = self.snapshot
            safe = {"pid": os.getpid(), "thread": selected[0], "status": d.get("turn", {}).get("status"),
                    "tokens": d.get("usage", {}).get("total_token_usage", {}).get("total_tokens", d.get("tokens_used")),
                    "token_timestamp": d.get("token_at"), "quota": limit_windows(d.get("limits", {})),
                    "quota_error": d.get("limits_error", ""), "updated_at": time.time(),
                    "topmost": self.topmost, "geometry": self.geometry().getRect(),
                    "last_rename": self.last_rename, "auto_thread": binding["auto"],
                    "last_icon_action": self.last_icon_action,
                    "last_navigation": self.last_navigation,
                    "active_view": self.view, "follow_latency": self.follow_latency,
                    "host_window": self.host_hwnd, "host_hidden": self.host_hidden,
                    "host_offset": self.host_offset, "host_rect": self.host_rect,
                    "titlebar_slot": self.header_slot() if self.host_rect else None,
                    "titlebar_source": (binding.get('titlebar') or {}).get('source', 'conservative'),
                    "titlebar_error": binding.get('titlebar_error', ''),
                    "placement": self.placement, "output_rate": d.get('output_rate')}
            diagnostic_name = "test-live-status.json" if args.test_ui else "live-status.json"
            write_json_atomic(RUNTIME / diagnostic_name, safe)

        def closeEvent(self, event):
            self.write_settings()
            stop.set()
            data_wake.set()
            quota_wake.set()
            recent_wake.set()
            if self.popup:
                self.popup.close()
            self.details.close()
            event.accept()
            app.quit()

    bar = Bar()
    server.newConnection.connect(lambda: (server.nextPendingConnection().deleteLater(), bar.show(), bar.raise_()))
    bar.show()
    saved = load_settings()
    if isinstance(saved.get("x"), int) and isinstance(saved.get("y"), int):
        bar.move(saved["x"], saved["y"])
        bar.clamp()
    if not args.test_ui:
        bar.sync_host()
    if args.test_ui:
        # Deterministic local UI checks do not read or navigate the desktop.
        binding["auto"] = False
        bar.set_auto_thread(False)
        from PyQt6.QtTest import QTest
        def checks():
            bar.update_data({"id": selected[0], "label": "🧪 查找 Codex 的 Mods 功能", "model": "gpt-6.1-sol",
                             "turn": {"status": "inProgress", "started_at": time.time() - 1738},
                             "usage": {"total_token_usage": {"total_tokens": 24840000,
                                       "input_tokens": 24000000, "output_tokens": 840000, "cached_input_tokens": 21000000},
                                       "last_token_usage": {"total_tokens": 319300}},
                             "limits": {"rateLimits": {"primary": {"usedPercent": 3,
                                        "windowDurationMins": 10080, "resetsAt": time.time() + 350000}}},
                             "limits_at": time.time(), "sessions": [],
                             "output_rate": {"value": 42.3, "start": time.time()-20, "end": time.time()-10,
                                             "output_tokens": 423}})
            app.processEvents()
            compact_height = bar.height()
            assert compact_height == 40
            assert bar.status.x() - (bar.title.x()+bar.title.width()) <= 8
            for index in range(bar.row.count()-1):
                a, b = bar.row.itemAt(index).widget(), bar.row.itemAt(index+1).widget()
                assert a.x()+a.width() <= b.x(), 'Toolbar items must not overlap'
            assert bar.auto.parent() == bar.details
            assert bar.tps.text() == '42.3 t/s'
            # Window movement and visibility are deterministic simulations; no
            # minimize/move of the user's real Codex window is needed.
            real_clamp = bar.clamp
            bar.clamp = lambda: None  # Offscreen platform has an unrelated 800px screen.
            real_dock_target = bar.dock_target
            bar.dock_target = lambda: dock_position(bar.host_rect, (bar.width(), bar.height()),
                                                    [(-1000, -1000, 3000, 3000)])
            bar.host_offset = None
            bar.sync_host_state({'visible': True, 'rect': [0, 0, 1200, 1000]})
            assert bar.y() == 2 and bar.placement == 'titlebar-blank'
            initial = bar.pos()
            bar.sync_host_state({'visible': True, 'rect': [100, 50, 1300, 1050]})
            assert bar.pos() == initial + QPoint(100, 50)
            # Old/manual menu-overlapping positions are corrected on follow.
            bar.host_offset = [0, 10]
            bar.sync_host_state({'visible': True, 'rect': [100, 50, 1300, 1050]})
            assert bar.placement == 'titlebar-blank' and bar.host_offset is None
            bar.dock_target = real_dock_target
            bar.host_offset = [0, 850]
            bar.sync_host_state({'visible': True, 'rect': [0, 0, 1200, 1000]})
            initial = bar.pos()
            bar.sync_host_state({'visible': True, 'rect': [100, 50, 1300, 1050]})
            assert bar.pos() == initial + QPoint(100, 50)
            bar.sync_host_state({'visible': False})
            assert not bar.isVisible() and bar.host_hidden
            bar.sync_host_state({'visible': True, 'rect': [100, 50, 1300, 1050]})
            assert bar.isVisible() and not bar.host_hidden
            for _ in range(3):
                bar.toggle_details()
                assert bar.details.isVisible() and bar.height() == compact_height
                bar.details.grab().save(str(RUNTIME / "details-verification.png"))
                bar.toggle_details()
                assert not bar.details.isVisible() and bar.height() == compact_height
            bar.show_icons()
            assert bar.popup.isVisible()
            assert len(bar.popup.findChildren(QPushButton)) == 91
            bar.popup.grab().save(str(RUNTIME / "icon-picker-verification.png"))
            bar.popup.close()
            before = bar.pos()
            QTest.mousePress(bar.grip, Qt.MouseButton.LeftButton, pos=QPoint(6, 8))
            QTest.mouseMove(bar.grip, QPoint(46, 28), delay=30)
            QTest.mouseRelease(bar.grip, Qt.MouseButton.LeftButton, pos=QPoint(46, 28))
            assert bar.pos() != before
            dragged = bar.pos()
            bar.sync_host_state({'visible': True, 'rect': [150, 75, 1350, 1075]})
            assert bar.pos() == dragged + QPoint(50, 25)
            bar.clamp = real_clamp
            QTest.mouseClick(bar.pin, Qt.MouseButton.LeftButton)
            assert not bar.topmost
            assert not bar.windowFlags() & Qt.WindowType.WindowStaysOnTopHint
            QTest.mouseClick(bar.pin, Qt.MouseButton.LeftButton)
            assert bar.topmost
            assert bar.windowFlags() & Qt.WindowType.WindowStaysOnTopHint
            app.processEvents()
            bar.grab().save(str(RUNTIME / "ui-verification.png"))
            # Clicking a title opens ten real-shaped rows, while a drag above
            # did not open a popup. The test consumes navigation locally.
            offset_before = bar.host_offset
            QTest.mouseClick(bar.title, Qt.MouseButton.LeftButton)
            assert bar.popup_kind == 'recent' and bar.popup.isVisible()
            assert bar.host_offset == offset_before
            fake = {'threads': [{'id': f'recent-{i}', 'title': f'{"🧪" if i%2 else "💬"} Session {i} 会话',
                                'kind': 'codex', 'status': 'active' if i in (2, 7) else 'idle',
                                'updatedAt': 100-i} for i in range(10)]}
            rows = recent_threads(fake, {f'recent-{i}': 'completed' for i in range(10)})
            bar.update_recent({'rows': rows, 'loaded_at': time.time()})
            app.processEvents()
            assert len(bar.recent_buttons) == 10
            assert bar.recent_buttons[0].property('threadId') == 'recent-2'
            assert bar.recent_buttons[1].property('threadId') == 'recent-7'
            assert all(button.isVisible() for button in bar.recent_buttons)
            bar.popup.grab().save(str(RUNTIME / 'recent-chats-verification.png'))
            target = bar.recent_buttons[-1].property('threadId')
            QTest.mouseClick(bar.recent_buttons[-1], Qt.MouseButton.LeftButton)
            assert commands.get_nowait() == ('navigate', target)
            assert binding['auto'] and not bar.popup.isVisible()
            assert bar.last_navigation['phase'] == 'queued'
            # A newly viewed session immediately clears old data and disables
            # icon writes until its own snapshot arrives. Late old snapshots
            # must not change the new identity.
            old_id = selected[0]
            other_id = ("22222222-2222-2222-2222-222222222222"
                        if old_id == "11111111-1111-1111-1111-111111111111"
                        else "11111111-1111-1111-1111-111111111111")
            bar.set_auto_thread(True)
            bar.active_changed({"thread": other_id, "available": True, "window": "test"})
            assert selected[0] == other_id and not bar.icon.isEnabled()
            bar.update_data({"id": old_id, "label": "旧会话"})
            assert not bar.snapshot
            bar.update_data({"id": other_id, "label": "🔬 当前会话", "tokens_used": 123,
                             "turn": {"status": "completed"}, "sessions": []})
            assert bar.snapshot["id"] == other_id and bar.icon.isEnabled()
            assert bar.icon.text() == "🔬"
            print(json.dumps({"ui": "pass", "drag": True, "pin": True, "icons": 90, "compact_height": compact_height,
                              "details_preserve_height": True,
                              "host_move_hide_restore": True, "drag_offset_preserved": True, "tps": True,
                              "session_follow": True, "late_snapshot_rejected": True,
                              "recent_click_drag": True, "recent_rows": 10, "navigation_queued": True,
                              "screenshot": str(RUNTIME / "ui-verification.png")}))
            bar.close()
        def report_checks():
            try:
                checks()
            except Exception:
                import traceback
                traceback.print_exc()
                app.exit(1)
        QTimer.singleShot(200, report_checks)
    else:
        threading.Thread(target=track_active, daemon=True).start()
        threading.Thread(target=worker, daemon=True).start()
        threading.Thread(target=account_worker, daemon=True).start()
        threading.Thread(target=control_worker, daemon=True).start()
        threading.Thread(target=titlebar_worker, daemon=True).start()
        threading.Thread(target=recent_worker, daemon=True).start()
        if args.verify_current_icon:
            def verify_current_icon():
                if not bar.snapshot:
                    QTimer.singleShot(1000, verify_current_icon)
                    return
                from PyQt6.QtTest import QTest
                current = leading_icon(bar.snapshot.get("label", ""))
                QTest.mouseClick(bar.icon, Qt.MouseButton.LeftButton)
                for button in bar.popup.findChildren(QPushButton):
                    if button.text() == current or (current == "◈" and button.text() == "Clear icon"):
                        QTest.mouseClick(button, Qt.MouseButton.LeftButton)
                        break
                else:
                    bar.popup.close()
            QTimer.singleShot(3000, verify_current_icon)
        if args.capture_after:
            QTimer.singleShot(args.capture_after * 1000,
                              lambda: bar.grab().save(str(RUNTIME / "live-bar.png")))
    result = app.exec()
    stop.set()
    recent_wake.set()
    recent_client.close()
    rpc.close()
    desktop.close()
    return result


def main():
    global SETTINGS
    parser = argparse.ArgumentParser()
    parser.add_argument("--thread")
    parser.add_argument("--probe", action="store_true")
    parser.add_argument("--test-ui", action="store_true")
    parser.add_argument("--capture-after", type=int, default=0)
    parser.add_argument("--verify-current-icon", action="store_true", help="Re-select current icon to verify the click chain")
    args = parser.parse_args()
    if args.test_ui:
        SETTINGS = RUNTIME / "test-settings.json"
    if args.probe:
        snapshot = SessionStore().read(args.thread or default_thread())
        rpc = Rpc()
        try:
            limits = rpc.call("account/rateLimits/read")
            desktop = DesktopTools()
            try:
                desktop.rename(snapshot["id"], snapshot["label"])
            finally:
                desktop.close()
        finally:
            rpc.close()
        print(json.dumps({"thread": snapshot["id"], "status": snapshot["turn"].get("status"),
                          "tokens": snapshot.get("usage", {}).get("total_token_usage", {}).get("total_tokens"),
                          "quota": limit_windows(limits), "title_api": "pass"}, ensure_ascii=False))
        return 0
    return run_gui(args)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception:
        import traceback
        with (RUNTIME / "error.log").open("a", encoding="utf-8") as stream:
            traceback.print_exc(file=stream)
        raise
