"""Tier 1: Happy-path black-box contract coverage across F1-F5."""
from __future__ import annotations

import ctypes
from ctypes import wintypes
import os
import sys
import time
import unittest
import xml.etree.ElementTree as ET

from ._helpers import (
    NAV_EXTENDED_STATUS_KEYS,
    SYNC_STATUS_CORE_KEYS,
    assert_cors_pna_headers,
    assert_lan_security_headers,
    bearer_headers,
    build_hoyolab_export_payload,
    build_sample_nearby_pins,
)
from .clients import HttpClient, NativeHostClient, WsClient
from .config import get_config
from .sandbox import BackendSandbox, FORBIDDEN_DEFAULT_PORTS, ServerHandle, allocate_free_port


def _assert_pid_visible_windows_offscreen(
    tc: unittest.TestCase,
    server: ServerHandle,
    *,
    expect_at_least_one: bool = False,
    timeout_s: float = 1.5,
) -> int:
    """Asserts every visible top-level Win32 window for `server` is outside the virtual desktop."""
    if sys.platform != "win32":
        return 0

    pid = server.process.pid
    user32 = ctypes.windll.user32
    user32.IsWindowVisible.argtypes = [wintypes.HWND]
    user32.IsWindowVisible.restype = wintypes.BOOL
    user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
    user32.GetWindowThreadProcessId.restype = wintypes.DWORD
    user32.GetWindowRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
    user32.GetWindowRect.restype = wintypes.BOOL
    enum_proc_type = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    user32.EnumWindows.argtypes = [enum_proc_type, wintypes.LPARAM]
    user32.EnumWindows.restype = wintypes.BOOL

    sm_xvirtualscreen = 76
    sm_yvirtualscreen = 77
    sm_cxvirtualscreen = 78
    sm_cyvirtualscreen = 79

    def _collect_visible_rects() -> tuple[bool, int, list[tuple[int, tuple[int, int, int, int]]]]:
        found: list[tuple[int, tuple[int, int, int, int]]] = []
        total_desktop_windows = 0

        @enum_proc_type
        def _cb(hwnd: int, _lp: int) -> bool:
            nonlocal total_desktop_windows
            total_desktop_windows += 1
            if not user32.IsWindowVisible(hwnd):
                return True
            win_pid = wintypes.DWORD(0)
            user32.GetWindowThreadProcessId(hwnd, ctypes.byref(win_pid))
            if int(win_pid.value) != int(pid):
                return True
            rc = wintypes.RECT()
            if user32.GetWindowRect(hwnd, ctypes.byref(rc)):
                found.append((int(hwnd), (int(rc.left), int(rc.top), int(rc.right), int(rc.bottom))))
            return True

        enum_ok = bool(user32.EnumWindows(_cb, 0))
        return enum_ok, total_desktop_windows, found

    deadline = time.monotonic() + timeout_s
    enum_ok, total_desktop_windows, rects = _collect_visible_rects()
    interactive_desktop = enum_ok and total_desktop_windows > 0
    while expect_at_least_one and interactive_desktop and not rects and time.monotonic() < deadline:
        time.sleep(0.05)
        enum_ok, total_desktop_windows, rects = _collect_visible_rects()
        interactive_desktop = enum_ok and total_desktop_windows > 0

    if expect_at_least_one:
        if interactive_desktop:
            tc.assertGreaterEqual(
                len(rects),
                1,
                f"Expected at least one visible offscreen HUD window for pid={pid}",
            )
        else:
            tc.assertIn(
                "[Startup] HUD offscreen mode active (offset=(-20000, -20000))",
                server.read_log_tail(),
            )

    vx = int(user32.GetSystemMetrics(sm_xvirtualscreen))
    vy = int(user32.GetSystemMetrics(sm_yvirtualscreen))
    vw = int(user32.GetSystemMetrics(sm_cxvirtualscreen))
    vh = int(user32.GetSystemMetrics(sm_cyvirtualscreen))
    tc.assertGreater(vw, 0)
    tc.assertGreater(vh, 0)

    for hwnd, (left, top, right, bottom) in rects:
        no_overlap = right <= vx or left >= vx + vw or bottom <= vy or top >= vy + vh
        tc.assertTrue(
            no_overlap,
            f"Window hwnd={hwnd} rect={(left, top, right, bottom)} overlaps virtual desktop {(vx, vy, vx + vw, vy + vh)}",
        )
    return len(rects)


class TestTier1SyncRoutes(unittest.TestCase):
    """Happy-path coverage for all Sync Server HTTP routes (excluding destructive /shutdown)."""

    sandbox: BackendSandbox
    server: ServerHandle
    client: HttpClient
    _diag_folder: str | None = None

    @classmethod
    def setUpClass(cls) -> None:
        cls.sandbox = BackendSandbox(prefix="gh_t1_sync_")
        cls.server = cls.sandbox.start_server(standby=True, lan_enabled=False, language="en")
        cls.client = HttpClient("127.0.0.1", cls.server.ws_port, timeout=get_config().http_timeout_s)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.sandbox.cleanup()

    def test_01_status_schema_and_cors(self) -> None:
        resp = self.client.get("/status")
        self.assertEqual(resp.status, 200)
        assert_cors_pna_headers(self, resp)
        data = resp.json()
        self.assertEqual(data["status"], "connected")
        self.assertEqual(data["version"], "5.8.0")
        self.assertEqual(data["script_version"], "v5.8.0")
        for key in SYNC_STATUS_CORE_KEYS:
            self.assertIn(key, data, f"Missing /status key: {key}")
        for key in NAV_EXTENDED_STATUS_KEYS:
            self.assertIn(key, data, f"Missing extended /status key: {key}")
        self.assertIsInstance(data["queue_len"], int)
        self.assertIsInstance(data["latest_seq"], int)
        self.assertIsInstance(data["engine_active"], bool)
        self.assertIsInstance(data["audio_volume"], int)
        self.assertFalse(data["engine_active"])  # started with --standby

    def test_02_poll_since_negative_one_immediate_heartbeat(self) -> None:
        resp = self.client.get("/poll?since=-1", timeout=2.0)
        self.assertEqual(resp.status, 200)
        assert_cors_pna_headers(self, resp)
        data = resp.json()
        self.assertEqual(data["type"], "HEARTBEAT")
        self.assertEqual(data["script_version"], "v5.8.0")
        self.assertIsInstance(data["_seq"], int)
        self.assertGreaterEqual(data["_seq"], 0)

    def test_03_set_engine_state_roundtrip(self) -> None:
        r1 = self.client.post_json("/set_engine_state", {"active": True})
        self.assertEqual(r1.status, 200)
        assert_cors_pna_headers(self, r1)
        self.assertEqual(r1.json(), {"ok": True, "engine_active": True})

        r2 = self.client.post_json("/set_engine_state", {"active": False})
        self.assertEqual(r2.status, 200)
        self.assertEqual(r2.json(), {"ok": True, "engine_active": False})

    def test_04_set_auto_sync_toggle(self) -> None:
        r1 = self.client.post_json("/set_auto_sync", {"auto_sync_m": False})
        self.assertEqual(r1.status, 200)
        assert_cors_pna_headers(self, r1)
        self.assertEqual(r1.json(), {"ok": True, "auto_sync_m": False})

        r2 = self.client.post_json("/set_auto_sync", {"auto_sync_m": True})
        self.assertEqual(r2.status, 200)
        self.assertEqual(r2.json(), {"ok": True, "auto_sync_m": True})

    def test_05_set_single_monitor_toggle(self) -> None:
        r1 = self.client.post_json("/set_single_monitor", {"single_monitor_mode": True})
        self.assertEqual(r1.status, 200)
        assert_cors_pna_headers(self, r1)
        self.assertEqual(r1.json(), {"ok": True, "single_monitor_mode": True})
        # Deprecated since v5.5.0: the flag is only persisted for config/HTTP compatibility.
        # It no longer pops a HUD toast (feedback owner is auto-detected via /tab_visibility),
        # so nothing may become visible on the desktop; any visible window must stay offscreen.
        self.assertTrue(self.sandbox.read_config().get("single_monitor_mode"))
        _assert_pid_visible_windows_offscreen(
            self,
            self.server,
            expect_at_least_one=False,
        )

        r2 = self.client.post_json("/set_single_monitor", {"single_monitor_mode": False})
        self.assertEqual(r2.status, 200)
        self.assertEqual(r2.json(), {"ok": True, "single_monitor_mode": False})
        self.assertFalse(self.sandbox.read_config().get("single_monitor_mode"))

    def test_06_action_feedback(self) -> None:
        resp = self.client.post_json(
            "/action_feedback",
            {"event_type": "SYNC_AND_MARK", "status": "marked", "seq": 1001, "title": "Marked"},
        )
        self.assertEqual(resp.status, 200)
        assert_cors_pna_headers(self, resp)
        data = resp.json()
        self.assertTrue(data["ok"])
        self.assertIn("shown", data)
        self.assertFalse(data["deduped"])

    def test_07_quick_switch(self) -> None:
        # The sandbox must never move focus on the user's real desktop (GH_DESKTOP_SANDBOX=1):
        # the switch decision still runs against real windows, but no window is focused.
        self.assertIn(
            "[Startup] Desktop sandbox mode active",
            self.server.read_log_tail(max_chars=200_000),
        )
        resp = self.client.post_json("/quick_switch", {})
        self.assertEqual(resp.status, 200)
        assert_cors_pna_headers(self, resp)
        data = resp.json()
        self.assertTrue(data["ok"])
        self.assertIn(data["target"], ("game", "browser", "no_browser", "none"))

    def test_08_set_hotkey_single_and_action(self) -> None:
        r_single = self.client.post_json("/set_hotkey", {"action": "debug", "hotkey": "alt+f10"})
        self.assertEqual(r_single.status, 200)
        assert_cors_pna_headers(self, r_single)
        d1 = r_single.json()
        self.assertTrue(d1["ok"])
        self.assertTrue(d1["results"]["debug"]["ok"])
        self.assertEqual(d1["results"]["debug"]["hotkey"], "alt+f10")

        r_legacy = self.client.post_json("/set_hotkey", {"hotkey": "alt+f9"})
        self.assertEqual(r_legacy.status, 200)
        self.assertTrue(r_legacy.json()["results"]["sync"]["ok"])

    def test_09_set_hotkeys_batch_alias(self) -> None:
        r_batch = self.client.post_json(
            "/set_hotkeys",
            {"hotkey_sync": "alt+f9", "hotkey_nav_toggle": "alt+p"},
        )
        self.assertEqual(r_batch.status, 200)
        assert_cors_pna_headers(self, r_batch)
        d2 = r_batch.json()
        self.assertTrue(d2["ok"])
        self.assertTrue(d2["results"]["sync"]["ok"])
        self.assertTrue(d2["results"]["nav_toggle"]["ok"])

    def test_10_check_hotkey_conflicts_readonly(self) -> None:
        r_all = self.client.post_json(
            "/check_hotkey_conflicts",
            {"bindings": {"sync": "alt+f11", "debug": "alt+f12"}, "lang": "en"},
        )
        self.assertEqual(r_all.status, 200)
        assert_cors_pna_headers(self, r_all)
        d_all = r_all.json()
        self.assertTrue(d_all["ok"])
        self.assertIn("actions", d_all)
        self.assertIn("sync", d_all["actions"])
        self.assertIsInstance(d_all.get("active_backups"), list)

        r_single = self.client.post_json(
            "/check_hotkey_conflicts",
            {"hotkey": "alt+f11", "action": "sync", "bindings": {"sync": "alt+f11"}, "lang": "en"},
        )
        self.assertEqual(r_single.status, 200)
        d_single = r_single.json()
        self.assertTrue(d_single["ok"])
        self.assertIn("single", d_single)
        self.assertIn("severity", d_single["single"])

    def test_11_remap_and_restore_external_hotkey_safe_validation(self) -> None:
        r_remap = self.client.post_json(
            "/remap_external_hotkey",
            {
                "app_id": "sandbox_unsupported_app",
                "shortcut_id": "toggle_overlay",
                "new_external_hotkey": "ctrl+shift+f9",
                "lang": "en",
            },
        )
        self.assertEqual(r_remap.status, 400)
        assert_cors_pna_headers(self, r_remap)
        self.assertFalse(r_remap.json()["ok"])

        r_restore = self.client.post_json(
            "/restore_external_hotkey",
            {"app_id": "sandbox_unsupported_app", "shortcut_id": "toggle_overlay"},
        )
        self.assertEqual(r_restore.status, 400)
        assert_cors_pna_headers(self, r_restore)
        self.assertFalse(r_restore.json()["ok"])

    def test_12_report_state_and_browser_state(self) -> None:
        state_in = {
            "center": [123.5, -456.25],
            "zoom": 1.25,
            "markersCount": 19,
            "activeAreaId": 2,
            "scriptVersion": "v5.8.0",
        }
        r_post = self.client.post_json("/report_state", state_in)
        self.assertEqual(r_post.status, 200)
        assert_cors_pna_headers(self, r_post)
        self.assertEqual(r_post.json(), {})

        r_get = self.client.get("/browser_state")
        self.assertEqual(r_get.status, 200)
        assert_cors_pna_headers(self, r_get)
        got = r_get.json()
        self.assertEqual(got["center"], [123.5, -456.25])
        self.assertEqual(got["markersCount"], 19)

    def test_13_trigger_diagnostic_writes_to_sandbox(self) -> None:
        r_diag = self.client.post_json("/trigger_diagnostic", {"note": "initial sandbox note"})
        self.assertEqual(r_diag.status, 200)
        assert_cors_pna_headers(self, r_diag)
        payload = r_diag.json()
        self.assertTrue(payload["ok"])
        bundle = payload["bundle"]
        self.assertTrue(bundle["success"], f"Diagnostic bundle failed: {bundle}")
        folder_name = bundle["folder_name"]
        self.assertTrue(folder_name.startswith("diag_"))
        type(self)._diag_folder = folder_name

        bundle_path = os.path.abspath(bundle["bundle_dir"])
        sandbox_diag_root = os.path.abspath(str(self.sandbox.diagnostics_dir))
        self.assertTrue(bundle_path.startswith(sandbox_diag_root))
        self.assertTrue(os.path.isdir(bundle_path))

    def test_14_save_diagnostic_note(self) -> None:
        folder_name = type(self)._diag_folder
        if not folder_name:
            diag = self.client.post_json("/trigger_diagnostic", {"note": "seed"}).json()
            folder_name = diag["bundle"]["folder_name"]
        r_note = self.client.post_json(
            "/save_diagnostic_note",
            {"folder_name": folder_name, "note": "updated sandbox note"},
        )
        self.assertEqual(r_note.status, 200)
        assert_cors_pna_headers(self, r_note)
        note_json = r_note.json()
        self.assertTrue(note_json["ok"])
        self.assertEqual(note_json["folder_name"], folder_name)
        self.assertEqual(note_json["user_note"], "updated sandbox note")

    def test_15_trigger_sync(self) -> None:
        resp = self.client.post_json(
            "/trigger_sync",
            {"lat": 42.5, "lng": -18.75, "map_id": 2, "has_world_coords": True},
        )
        self.assertEqual(resp.status, 200)
        assert_cors_pna_headers(self, resp)
        data = resp.json()
        self.assertTrue(data["ok"])
        self.assertEqual(data["broadcasted"]["type"], "SYNC_AND_MARK")
        self.assertEqual(data["broadcasted"]["lat"], 42.5)

    def test_16_set_language(self) -> None:
        r_vi = self.client.post_json("/set_language", {"language": "vi"})
        self.assertEqual(r_vi.status, 200)
        assert_cors_pna_headers(self, r_vi)
        self.assertEqual(r_vi.json(), {"ok": True, "language": "vi", "resolved": "vi"})

        r_en = self.client.post_json("/set_language", {"language": "en"})
        self.assertEqual(r_en.status, 200)
        self.assertEqual(r_en.json(), {"ok": True, "language": "en", "resolved": "en"})

    def test_17_set_minimap_tracking(self) -> None:
        r1 = self.client.post_json("/set_minimap_tracking", {"enabled": False})
        self.assertEqual(r1.status, 200)
        assert_cors_pna_headers(self, r1)
        self.assertEqual(r1.json(), {"ok": True, "enabled": False, "minimap_tracking": False})

        r2 = self.client.post_json("/set_minimap_tracking", {"enabled": True})
        self.assertEqual(r2.status, 200)
        self.assertEqual(r2.json(), {"ok": True, "enabled": True, "minimap_tracking": True})

    def test_18_set_nav_hud_and_edit_mode(self) -> None:
        resp = self.client.post_json(
            "/set_nav_hud",
            {
                "enabled": True,
                "position": "right_of_minimap",
                "capture_visible": False,
                "guide_zoom_mode": "compact",
                "custom_pos": [0.25, 0.35],
            },
        )
        self.assertEqual(resp.status, 200)
        assert_cors_pna_headers(self, resp)
        data = resp.json()
        self.assertTrue(data["ok"])
        self.assertTrue(data["enabled"])
        self.assertEqual(data["position"], "right_of_minimap")
        self.assertEqual(data["guide_zoom_mode"], "compact")

    def test_19_set_audio(self) -> None:
        resp = self.client.post_json(
            "/set_audio",
            {"audio_enabled": False, "audio_volume": 35, "test": False},
        )
        self.assertEqual(resp.status, 200)
        assert_cors_pna_headers(self, resp)
        self.assertEqual(
            resp.json(),
            {"ok": True, "tested": False, "audio_enabled": False, "audio_volume": 35},
        )

    def test_20_nearby_pins(self) -> None:
        pins = build_sample_nearby_pins(map_id=2)
        r_pins = self.client.post_json(
            "/nearby_pins",
            {"map_id": 2, "lat": 20.0, "lng": 25.0, "coverage_radius": 200.0, "pins": pins},
        )
        self.assertEqual(r_pins.status, 200)
        assert_cors_pna_headers(self, r_pins)
        self.assertEqual(r_pins.json()["ok"], True)
        self.assertEqual(r_pins.json()["count"], len(pins))

    def test_21_nav_command_canonical_actions(self) -> None:
        self.client.post_json("/nearby_pins", {"map_id": 2, "pins": build_sample_nearby_pins(map_id=2)})
        for act in ("next", "prev", "skip", "mark", "undo", "blacklist", "guide", "layout", "refresh", "clear_route"):
            r_cmd = self.client.post_json("/nav_command", {"action": act})
            self.assertEqual(r_cmd.status, 200, f"/nav_command {act} failed: {r_cmd.text()}")
            assert_cors_pna_headers(self, r_cmd)
            self.assertTrue(r_cmd.json()["ok"])
            self.assertEqual(r_cmd.json()["action"], act)
            if act == "layout":
                _assert_pid_visible_windows_offscreen(
                    self,
                    self.server,
                    expect_at_least_one=(sys.platform == "win32"),
                )

    def test_22_plan_tsp_route(self) -> None:
        pins = build_sample_nearby_pins(map_id=2)
        resp = self.client.post_json(
            "/plan_tsp_route",
            {"map_id": 2, "start_pos": [0.0, 0.0], "pins": pins, "teleport_penalty": 180.0},
        )
        self.assertEqual(resp.status, 200)
        assert_cors_pna_headers(self, resp)
        data = resp.json()
        self.assertTrue(data["ok"])
        self.assertEqual(data["map_id"], 2)
        self.assertEqual(len(data["ordered_pins"]), len(pins))
        self.assertIsInstance(data["legs"], list)
        self.assertIsInstance(data["teleport_hints"], list)

    def test_23_cross_map_export_canonical(self) -> None:
        pt_env, mk_env, _ = build_hoyolab_export_payload(12, map_id=2)
        resp = self.client.post_json(
            "/cross_map_export",
            {
                "target_format": "canonical",
                "map_id": 2,
                "point_list": pt_env,
                "marked_list": mk_env,
            },
        )
        self.assertEqual(resp.status, 200)
        assert_cors_pna_headers(self, resp)
        data = resp.json()
        self.assertTrue(data["ok"])
        self.assertEqual(data["format"], "canonical")
        self.assertIsNotNone(data["payload"])
        self.assertEqual(data["report"]["total_source_pins"], 12)

    def test_24_lan_companion_control_status_start_stop(self) -> None:
        r_status = self.client.post_json("/lan_companion", {"action": "status"})
        self.assertEqual(r_status.status, 200)
        assert_cors_pna_headers(self, r_status)
        self.assertTrue(r_status.json()["ok"])
        self.assertFalse(r_status.json()["running"])

        r_start = self.client.post_json("/lan_companion", {"action": "start"})
        self.assertEqual(r_start.status, 200)
        d_start = r_start.json()
        self.assertTrue(d_start["ok"])
        self.assertTrue(d_start["running"])
        self.assertEqual(d_start.get("bind_host"), "127.0.0.1")
        self.assertEqual(d_start.get("host"), "127.0.0.1")
        self.assertTrue(d_start["pin"])
        self.assertTrue(d_start["token"])
        self.assertIn("<svg", d_start["qr_svg"])

        r_stop = self.client.post_json("/lan_companion", {"action": "stop"})
        self.assertEqual(r_stop.status, 200)
        self.assertTrue(r_stop.json()["ok"])
        self.assertFalse(r_stop.json()["running"])

    def test_25_launch_borderless_dry_run(self) -> None:
        resp = self.client.post_json(
            "/launch_borderless",
            {"dry_run": True, "width": 1920, "height": 1080},
        )
        self.assertEqual(resp.status, 200)
        assert_cors_pna_headers(self, resp)
        data = resp.json()
        self.assertIn("ok", data)
        self.assertFalse(data["launched"])
        self.assertIsInstance(data["message"], str)

    def test_26_sandbox_isolation_ports_and_config_paths(self) -> None:
        self.assertNotIn(self.server.ws_port, FORBIDDEN_DEFAULT_PORTS)
        self.assertNotIn(self.server.lan_port, FORBIDDEN_DEFAULT_PORTS)
        self.assertTrue(self.sandbox.config_path.is_file())
        self.assertTrue((self.sandbox.data_dir / "manifest.json").is_file())
        cfg = self.sandbox.read_config()
        self.assertEqual(cfg["ws_port"], self.server.ws_port)
        self.assertEqual(cfg["lan_companion_port"], self.server.lan_port)


class TestTier1LanCompanionHappyPath(unittest.TestCase):
    """Happy-path contract tests for LAN Companion HTTP, PIN auth, QR SVG, and RFC 6455 WS (F3)."""

    sandbox: BackendSandbox
    server: ServerHandle
    lan_cli: HttpClient
    pin: str
    token: str

    @classmethod
    def setUpClass(cls) -> None:
        cls.sandbox = BackendSandbox(prefix="gh_t1_lan_")
        cls.server = cls.sandbox.start_server(standby=True, lan_enabled=False, language="en")
        sync_cli = HttpClient("127.0.0.1", cls.server.ws_port)
        sync_cli.post_json("/nearby_pins", {"map_id": 2, "pins": build_sample_nearby_pins()})
        start_resp = sync_cli.post_json("/lan_companion", {"action": "start"}).json()
        if not start_resp.get("ok"):
            raise RuntimeError(f"Failed to start LAN Companion: {start_resp}")
        cls.pin = str(start_resp["pin"])
        cls.token = str(start_resp["token"])
        cls.lan_cli = HttpClient("127.0.0.1", cls.server.lan_port, timeout=10.0)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.sandbox.cleanup()

    def test_01_lan_health(self) -> None:
        resp = self.lan_cli.get("/health")
        self.assertEqual(resp.status, 200)
        assert_lan_security_headers(self, resp)
        self.assertEqual(resp.json(), {"ok": True, "clients": 0})

    def test_02_lan_root_login_html_and_csp(self) -> None:
        resp = self.lan_cli.get("/")
        self.assertEqual(resp.status, 200)
        assert_lan_security_headers(self, resp)
        self.assertIn("text/html", resp.header("content-type", ""))
        self.assertIn("default-src 'self'", resp.header("content-security-policy", ""))
        self.assertNotIn(self.token, resp.text())

    def test_03_lan_auth_pin_success(self) -> None:
        resp = self.lan_cli.post_json("/auth/pin", {"pin": self.pin})
        self.assertEqual(resp.status, 200)
        assert_lan_security_headers(self, resp)
        data = resp.json()
        self.assertTrue(data["ok"])
        self.assertEqual(data["token"], self.token)
        self.assertEqual(data["companion_path"], f"/companion?token={self.token}")

    def test_04_lan_companion_html_via_query_and_bearer(self) -> None:
        r_q = self.lan_cli.get(f"/companion?token={self.token}")
        self.assertEqual(r_q.status, 200)
        assert_lan_security_headers(self, r_q)
        self.assertIn("ws:", r_q.header("content-security-policy", ""))

        r_b = self.lan_cli.get("/companion", headers=bearer_headers(self.token))
        self.assertEqual(r_b.status, 200)

    def test_05_lan_qr_svg_valid_xml(self) -> None:
        r_qr = self.lan_cli.get(f"/qr.svg?token={self.token}")
        self.assertEqual(r_qr.status, 200)
        assert_lan_security_headers(self, r_qr)
        self.assertIn("image/svg+xml", r_qr.header("content-type", ""))
        svg_root = ET.fromstring(r_qr.text())
        self.assertTrue(svg_root.tag.endswith("svg"))

    def test_06_lan_ws_handshake_initial_state_and_ping_pong(self) -> None:
        with WsClient("127.0.0.1", self.server.lan_port, f"/ws/{self.token}") as ws:
            init_state = ws.recv_json(timeout=4.0)
            self.assertEqual(init_state["type"], "STATE")
            self.assertIn("nearby_pins", init_state)
            self.assertIn("route_legs", init_state)

            ws.send_ping(b"hello")
            op, pong = ws.recv_frame(timeout=4.0)
            self.assertEqual(op, 0xA)
            self.assertEqual(pong, b"hello")


class TestTier1NativeHostAndShutdown(unittest.TestCase):
    """Happy-path coverage for Native Messaging Host (F4) and /shutdown (F2)."""

    def test_01_native_host_offline_ping(self) -> None:
        with BackendSandbox(prefix="gh_t1_host_off_") as sb:
            offline_port = allocate_free_port()
            with NativeHostClient(
                sb.host_command(),
                port=offline_port,
                extra_args=["chrome-extension://nfkijdoblmphmdljfogooajhbldkeojh/"],
                cwd=str(sb.run_dir),
                env=sb.build_child_env(),
            ) as host:
                host.send_json({"cmd": "ping"})
                resp = host.recv_json()
                self.assertTrue(resp["ok"])
                self.assertEqual(resp["host_version"], "5.8.0")
                self.assertFalse(resp["server_running"])
                self.assertFalse(resp["engine_active"])
                rc, rem_out, _ = host.close_and_wait()
                self.assertEqual(rc, 0)
                self.assertEqual(rem_out, b"")

    def test_02_native_host_online_ping_wake_standby_and_shutdown(self) -> None:
        with BackendSandbox(prefix="gh_t1_host_on_") as sb:
            srv = sb.start_server(standby=True, lan_enabled=False, language="en")
            with NativeHostClient(
                sb.host_command(),
                port=srv.ws_port,
                extra_args=["chrome-extension://nfkijdoblmphmdljfogooajhbldkeojh/"],
                cwd=str(sb.run_dir),
                env=sb.build_child_env(),
            ) as host:
                host.send_json({"cmd": "ping"})
                r_ping = host.recv_json()
                self.assertTrue(r_ping["ok"])
                self.assertTrue(r_ping["server_running"])
                self.assertFalse(r_ping["engine_active"])
                self.assertEqual(r_ping["version"], "5.8.0")

                host.send_json({"cmd": "start_server"})
                r_wake = host.recv_json()
                self.assertTrue(r_wake["ok"])
                self.assertEqual(r_wake["started"], "woke_standby")
                self.assertTrue(r_wake["engine_active"])

                host.send_json({"cmd": "start_server"})
                r_active = host.recv_json()
                self.assertTrue(r_active["ok"])
                self.assertEqual(r_active["started"], "already_active")

                host.send_json({"cmd": "stop_server", "mode": "standby"})
                r_standby = host.recv_json()
                self.assertTrue(r_standby["ok"])
                self.assertEqual(r_standby["mode"], "standby")
                self.assertFalse(r_standby["engine_active"])

                host.send_json({"cmd": "stop_server", "mode": "shutdown"})
                r_shut = host.recv_json()
                self.assertTrue(r_shut["ok"])
                self.assertEqual(r_shut["mode"], "shutdown")
                self.assertTrue(r_shut["acknowledged"])
                self.assertFalse(r_shut["server_running"])

                rc, rem_out, _ = host.close_and_wait()
                self.assertEqual(rc, 0)
                self.assertEqual(rem_out, b"")

            self.assertEqual(srv.process.wait(timeout=5.0), 0)

    def test_03_native_host_stop_server_when_offline(self) -> None:
        with BackendSandbox(prefix="gh_t1_host_offstop_") as sb:
            offline_port = allocate_free_port()
            with NativeHostClient(
                sb.host_command(),
                port=offline_port,
                cwd=str(sb.run_dir),
                env=sb.build_child_env(),
            ) as host:
                host.send_json({"cmd": "stop_server", "mode": "standby"})
                r1 = host.recv_json()
                self.assertTrue(r1["ok"])
                self.assertFalse(r1["server_running"])
                self.assertEqual(r1["mode"], "standby")

                host.send_json({"cmd": "stop_server", "mode": "shutdown"})
                r2 = host.recv_json()
                self.assertTrue(r2["ok"])
                self.assertFalse(r2["acknowledged"])
                self.assertEqual(r2["mode"], "shutdown")
                rc, rem_out, _ = host.close_and_wait()
                self.assertEqual(rc, 0)
                self.assertEqual(rem_out, b"")

    def test_04_sync_server_direct_shutdown_route(self) -> None:
        with BackendSandbox(prefix="gh_t1_shut_") as sb:
            srv = sb.start_server(standby=True, lan_enabled=False)
            cli = HttpClient("127.0.0.1", srv.ws_port)
            resp = cli.post_json("/shutdown", {})
            self.assertEqual(resp.status, 200)
            assert_cors_pna_headers(self, resp)
            self.assertEqual(resp.json(), {"ok": True, "shutting_down": True})
            self.assertEqual(srv.process.wait(timeout=5.0), 0)

    def test_05_sandbox_distinct_ports_across_concurrent_sandboxes(self) -> None:
        with BackendSandbox(prefix="gh_t1_iso_a_") as sb_a, BackendSandbox(prefix="gh_t1_iso_b_") as sb_b:
            self.assertNotEqual(sb_a.root, sb_b.root)
            self.assertTrue((sb_a.data_dir / "terrain_n2_v4.npz").is_file())
            self.assertTrue((sb_b.data_dir / "cave_masks_v4.npz").is_file())


if __name__ == "__main__":
    unittest.main()
