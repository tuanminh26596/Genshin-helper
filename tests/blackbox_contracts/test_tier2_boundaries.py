"""Tier 2: Boundary, validation, size limit, auth, and negative contract tests."""
from __future__ import annotations

import json
import struct
import unittest

from ._helpers import (
    assert_cors_pna_headers,
    assert_lan_security_headers,
    bearer_headers,
    build_large_cross_map_request,
)
from .clients import HttpClient, NativeHostClient, WsClient, WsHandshakeError
from .config import get_config
from .sandbox import BackendSandbox, ServerHandle, allocate_free_port


class TestTier2SyncBoundaries(unittest.TestCase):
    """Boundary and error-path tests for the Sync Server and sandbox config (F1, F2, F5)."""

    sandbox: BackendSandbox
    server: ServerHandle
    client: HttpClient

    @classmethod
    def setUpClass(cls) -> None:
        cls.sandbox = BackendSandbox(prefix="gh_t2_sync_")
        cls.server = cls.sandbox.start_server(standby=True, lan_enabled=False, language="en")
        cls.client = HttpClient("127.0.0.1", cls.server.ws_port, timeout=20.0)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.sandbox.cleanup()

    def test_01_options_preflight_returns_204_cors_and_pna(self) -> None:
        for route in ("/status", "/poll", "/cross_map_export", "/shutdown"):
            resp = self.client.options(route)
            self.assertEqual(resp.status, 204, f"OPTIONS {route} returned {resp.status}")
            assert_cors_pna_headers(self, resp)
            self.assertEqual(resp.header("content-length"), "0")
            self.assertEqual(resp.body, b"")

    def test_02_unknown_route_returns_404(self) -> None:
        resp = self.client.get("/does_not_exist_contract_probe")
        self.assertEqual(resp.status, 404)
        if get_config().is_rust:
            assert_cors_pna_headers(self, resp)

    def test_03_shutdown_rejects_non_post_methods_with_403(self) -> None:
        for method in ("GET", "PUT", "DELETE"):
            resp = self.client.request(method, "/shutdown")
            self.assertEqual(resp.status, 403, f"{method} /shutdown returned {resp.status}")
            assert_cors_pna_headers(self, resp)
            self.assertEqual(resp.json(), {"ok": False, "error": "Forbidden"})
        self.assertEqual(self.client.get("/status").status, 200)

    def test_04_cross_map_export_exceeding_2mib_payload_succeeds(self) -> None:
        req = build_large_cross_map_request(min_bytes=2_500_000)
        raw_bytes = json.dumps(req, ensure_ascii=False).encode("utf-8")
        self.assertGreater(len(raw_bytes), 2 * 1024 * 1024)

        resp = self.client.request(
            "POST",
            "/cross_map_export",
            body=raw_bytes,
            headers={"Content-Type": "application/json; charset=utf-8"},
            timeout=25.0,
        )
        self.assertEqual(resp.status, 200)
        assert_cors_pna_headers(self, resp)
        data = resp.json()
        self.assertTrue(data["ok"])
        self.assertEqual(data["format"], "canonical")
        expected_count = len(req["point_list"]["data"]["point_list"])
        self.assertEqual(data["report"]["total_source_pins"], expected_count)

    def test_05_cross_map_export_empty_pins_returns_zero_report(self) -> None:
        resp = self.client.post_json("/cross_map_export", {"target_format": "canonical", "pins": []})
        self.assertEqual(resp.status, 200)
        data = resp.json()
        self.assertTrue(data["ok"])
        self.assertEqual(data["report"]["total_source_pins"], 0)

    def test_06_save_diagnostic_note_traversal_returns_400(self) -> None:
        for bad in ("../bad", "diag_foo/../bar", "diag_foo/bar", "diag_foo\\bar", "foo", ""):
            resp = self.client.post_json("/save_diagnostic_note", {"folder_name": bad, "note": "x"})
            self.assertEqual(resp.status, 400, f"Expected 400 for {bad!r}, got {resp.status}")
            assert_cors_pna_headers(self, resp)
            self.assertFalse(resp.json()["ok"])

    def test_07_save_diagnostic_note_missing_bundle_returns_404(self) -> None:
        missing_resp = self.client.post_json(
            "/save_diagnostic_note",
            {"folder_name": "diag_99991231_235959", "note": "missing"},
        )
        self.assertEqual(missing_resp.status, 404)
        assert_cors_pna_headers(self, missing_resp)
        missing_json = missing_resp.json()
        self.assertFalse(missing_json["ok"])
        self.assertTrue(missing_json["not_found"])

    def test_08_set_language_invalid_returns_400(self) -> None:
        resp = self.client.post_json("/set_language", {"language": "fr"})
        self.assertEqual(resp.status, 400)
        assert_cors_pna_headers(self, resp)
        self.assertFalse(resp.json()["ok"])

    def test_09_nav_command_invalid_action_returns_400(self) -> None:
        resp = self.client.post_json("/nav_command", {"action": "invalid_nav_action"})
        self.assertEqual(resp.status, 400)
        assert_cors_pna_headers(self, resp)
        self.assertFalse(resp.json()["ok"])

    def test_10_lan_companion_invalid_action_returns_400(self) -> None:
        resp = self.client.post_json("/lan_companion", {"action": "invalid_lan_action"})
        self.assertEqual(resp.status, 400)
        assert_cors_pna_headers(self, resp)
        self.assertFalse(resp.json()["ok"])

    def test_11_cross_map_export_invalid_format_returns_400(self) -> None:
        resp = self.client.post_json("/cross_map_export", {"target_format": "unknown_format"})
        self.assertEqual(resp.status, 400)
        assert_cors_pna_headers(self, resp)
        self.assertFalse(resp.json()["ok"])

    def test_12_external_hotkey_invalid_app_id_returns_400(self) -> None:
        r1 = self.client.post_json(
            "/remap_external_hotkey",
            {"app_id": "bad_app", "shortcut_id": "x", "new_hotkey": "alt+p"},
        )
        self.assertEqual(r1.status, 400)
        self.assertFalse(r1.json()["ok"])

        r2 = self.client.post_json("/restore_external_hotkey", {"app_id": "bad_app", "shortcut_id": "x"})
        self.assertEqual(r2.status, 400)
        self.assertFalse(r2.json()["ok"])

    def test_13_audio_volume_clamped_0_to_100(self) -> None:
        r_low = self.client.post_json("/set_audio", {"audio_enabled": False, "audio_volume": -42})
        self.assertEqual(r_low.status, 200)
        self.assertEqual(r_low.json()["audio_volume"], 0)

        r_high = self.client.post_json("/set_audio", {"audio_enabled": False, "audio_volume": 999})
        self.assertEqual(r_high.status, 200)
        self.assertEqual(r_high.json()["audio_volume"], 100)

    def test_14_malformed_json_and_non_int_poll_since_handled_safely(self) -> None:
        r_bad = self.client.request(
            "POST",
            "/report_state",
            body=b"{not_valid_json",
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r_bad.status, 200)
        self.assertEqual(r_bad.json(), {})

        # Non-int ?since= falls back to legacy queue/wait path without 500; seed an event first.
        self.client.post_json("/set_auto_sync", {"auto_sync_m": True})
        r_poll = self.client.get("/poll?since=not_an_int", timeout=3.0)
        self.assertEqual(r_poll.status, 200)
        self.assertIn("type", r_poll.json())

    def test_15_nav_hud_custom_pos_clamped_and_invalid_ignored(self) -> None:
        r_clamp = self.client.post_json("/set_nav_hud", {"custom_pos": [9.5, -1.0]})
        self.assertEqual(r_clamp.status, 200)
        d_clamp = r_clamp.json()
        self.assertTrue(d_clamp["ok"])
        self.assertIsInstance(d_clamp.get("custom_pos"), list)
        self.assertAlmostEqual(d_clamp["custom_pos"][0], 0.98, places=4)
        self.assertAlmostEqual(d_clamp["custom_pos"][1], 0.0, places=4)

        r_bad = self.client.post_json("/set_nav_hud", {"custom_pos": "not_a_pair"})
        self.assertEqual(r_bad.status, 200)
        d_bad = r_bad.json()
        self.assertTrue(d_bad["ok"])
        self.assertIsNone(d_bad.get("custom_pos"))


class TestTier2LanBoundaries(unittest.TestCase):
    """Auth, rate limiting, and WebSocket upgrade boundary checks for LAN Companion (F3)."""

    sandbox: BackendSandbox
    server: ServerHandle
    lan_cli: HttpClient
    token: str
    pin: str

    @classmethod
    def setUpClass(cls) -> None:
        cls.sandbox = BackendSandbox(prefix="gh_t2_lan_")
        cls.server = cls.sandbox.start_server(standby=True, lan_enabled=False)
        sync_cli = HttpClient("127.0.0.1", cls.server.ws_port)
        start = sync_cli.post_json("/lan_companion", {"action": "start"}).json()
        cls.token = start["token"]
        cls.pin = start["pin"]
        cls.lan_cli = HttpClient("127.0.0.1", cls.server.lan_port, timeout=10.0)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.sandbox.cleanup()

    def test_01_lan_missing_token_returns_401(self) -> None:
        for path in ("/companion", "/qr.svg", "/ws"):
            r_missing = self.lan_cli.get(path)
            self.assertEqual(r_missing.status, 401, f"Missing token on {path} -> {r_missing.status}")
            assert_lan_security_headers(self, r_missing)

    def test_02_lan_wrong_token_returns_403(self) -> None:
        for path in ("/companion", "/qr.svg", "/ws"):
            r_wrong = self.lan_cli.get(f"{path}?token=wrong_tok")
            self.assertEqual(r_wrong.status, 403, f"Wrong token on {path} -> {r_wrong.status}")

    def test_03_lan_query_token_precedence_over_bearer(self) -> None:
        r_prec = self.lan_cli.get("/companion?token=wrong_tok", headers=bearer_headers(self.token))
        self.assertEqual(r_prec.status, 403)

    def test_04_ws_plain_http_returns_426_upgrade_required(self) -> None:
        r_plain_ws = self.lan_cli.get(f"/ws/{self.token}")
        self.assertEqual(r_plain_ws.status, 426)
        self.assertEqual(r_plain_ws.header("upgrade", "").lower(), "websocket")

    def test_05_ws_wrong_token_handshake_returns_403(self) -> None:
        with self.assertRaises(WsHandshakeError) as ctx:
            WsClient("127.0.0.1", self.server.lan_port, "/ws/wrong_tok")
        self.assertEqual(ctx.exception.status, 403)

    def test_06_auth_pin_malformed_json_returns_400(self) -> None:
        r_bad_json = self.lan_cli.request(
            "POST",
            "/auth/pin",
            body=b"{not_json",
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r_bad_json.status, 400)
        self.assertFalse(r_bad_json.json()["ok"])

    def test_07_lan_pin_rate_limit_429_locks_out_valid_pin_on_6th_attempt(self) -> None:
        wrong_pin = "000000" if self.pin != "000000" else "999999"
        for attempt in range(5):
            r_wrong = self.lan_cli.post_json("/auth/pin", {"pin": wrong_pin})
            self.assertEqual(r_wrong.status, 403, f"Attempt {attempt + 1} expected 403")

        r_locked = self.lan_cli.post_json("/auth/pin", {"pin": self.pin})
        self.assertEqual(r_locked.status, 429)
        self.assertFalse(r_locked.json()["ok"])


class TestTier2NativeHostBoundaries(unittest.TestCase):
    """Framing, invalid payload, unknown command, and EOF boundary tests for Native Host (F4)."""

    def test_01_invalid_json_returns_framed_error_and_stays_alive(self) -> None:
        with BackendSandbox(prefix="gh_t2_host_json_") as sb:
            with NativeHostClient(
                sb.host_command(),
                port=allocate_free_port(),
                cwd=str(sb.run_dir),
                env=sb.build_child_env(),
            ) as host:
                host.send_frame(b"{{{not_json")
                err1 = host.recv_json()
                self.assertFalse(err1["ok"])
                self.assertEqual(err1["host_version"], "5.8.0")
                self.assertTrue(err1["error"])

                host.send_json({"cmd": "ping"})
                self.assertTrue(host.recv_json()["ok"])
                rc, rem_out, _ = host.close_and_wait()
                self.assertEqual(rc, 0)
                self.assertEqual(rem_out, b"")

    def test_02_unknown_cmd_returns_framed_error(self) -> None:
        with BackendSandbox(prefix="gh_t2_host_cmd_") as sb:
            with NativeHostClient(
                sb.host_command(),
                port=allocate_free_port(),
                cwd=str(sb.run_dir),
                env=sb.build_child_env(),
            ) as host:
                host.send_json({"cmd": "unknown_contract_cmd"})
                err = host.recv_json()
                self.assertFalse(err["ok"])
                self.assertEqual(err["host_version"], "5.8.0")
                self.assertIn("unknown_contract_cmd", err["error"].lower())
                rc, rem_out, _ = host.close_and_wait()
                self.assertEqual(rc, 0)
                self.assertEqual(rem_out, b"")

    def test_03_non_dict_json_list_handled_safely(self) -> None:
        with BackendSandbox(prefix="gh_t2_host_list_") as sb:
            with NativeHostClient(
                sb.host_command(),
                port=allocate_free_port(),
                cwd=str(sb.run_dir),
                env=sb.build_child_env(),
            ) as host:
                host.send_frame(b"[]")
                err = host.recv_json()
                self.assertFalse(err["ok"])
                self.assertEqual(err["host_version"], "5.8.0")
                rc, rem_out, _ = host.close_and_wait()
                self.assertEqual(rc, 0)
                self.assertEqual(rem_out, b"")

    def _assert_clean_silent_exit(self, raw_stdin: bytes) -> None:
        with BackendSandbox(prefix="gh_t2_host_eof_") as sb:
            with NativeHostClient(
                sb.host_command(),
                port=allocate_free_port(),
                cwd=str(sb.run_dir),
                env=sb.build_child_env(),
            ) as host:
                if raw_stdin:
                    host.send_raw(raw_stdin)
                rc, rem_out, _ = host.close_and_wait(timeout=5.0)
                self.assertEqual(rc, 0)
                self.assertEqual(rem_out, b"")

    def test_04_immediate_eof_exits_0_with_zero_stdout(self) -> None:
        self._assert_clean_silent_exit(b"")

    def test_05_zero_length_frame_exits_0_with_zero_stdout(self) -> None:
        self._assert_clean_silent_exit(struct.pack("<I", 0))

    def test_06_oversized_frame_exits_0_with_zero_stdout(self) -> None:
        self._assert_clean_silent_exit(struct.pack("<I", 1_048_577) + b"{}")

    def test_07_truncated_header_exits_0_with_zero_stdout(self) -> None:
        self._assert_clean_silent_exit(b"\x05\x00")


if __name__ == "__main__":
    unittest.main()
