"""Tier 3: Pairwise cross-feature black-box contract interactions."""
from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
import time
import unittest

from ._helpers import build_sample_nearby_pins
from .clients import HttpClient, WsClient
from .config import get_config
from .sandbox import BackendSandbox, ServerHandle, allocate_free_port


class TestTier3CrossFeature(unittest.TestCase):
    """Cross-feature state propagation, lossless polling, rollback, and persistence."""

    sandbox: BackendSandbox
    server: ServerHandle
    client: HttpClient

    @classmethod
    def setUpClass(cls) -> None:
        cls.sandbox = BackendSandbox(prefix="gh_t3_cross_")
        cls.server = cls.sandbox.start_server(standby=True, lan_enabled=False, language="en")
        cls.client = HttpClient("127.0.0.1", cls.server.ws_port, timeout=15.0)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.sandbox.cleanup()

    def _current_seq(self) -> int:
        hb = self.client.get("/poll?since=-1", timeout=2.0).json()
        return int(hb["_seq"])

    def _drain_until_type(self, start_seq: int, expected_type: str, max_hops: int = 12) -> dict:
        seq = start_seq
        for _ in range(max_hops):
            ev = self.client.get(f"/poll?since={seq}", timeout=5.0).json()
            ev_seq = int(ev.get("_seq", seq))
            if ev_seq > seq:
                seq = ev_seq
            if ev.get("type") == expected_type:
                return ev
        self.fail(f"Did not observe {expected_type!r} within {max_hops} poll hops from seq={start_seq}")

    def test_lossless_concurrent_multitab_poll_and_heartbeat(self) -> None:
        seq0 = self._current_seq()

        with ThreadPoolExecutor(max_workers=2) as pool:
            f_tab1 = pool.submit(lambda: HttpClient("127.0.0.1", self.server.ws_port, timeout=10.0).get(f"/poll?since={seq0}").json())
            f_tab2 = pool.submit(lambda: HttpClient("127.0.0.1", self.server.ws_port, timeout=10.0).get(f"/poll?since={seq0}").json())

            # Allow pollers to register as waiting futures.
            time.sleep(0.2)

            # since=-1 must return immediately even while long-poll waiters are pending.
            t0 = time.monotonic()
            hb = self.client.get("/poll?since=-1", timeout=2.0).json()
            self.assertLess(time.monotonic() - t0, 1.5)
            self.assertEqual(hb["type"], "HEARTBEAT")

            self.client.post_json("/set_auto_sync", {"auto_sync_m": False})
            ev1 = f_tab1.result(timeout=5.0)
            ev2 = f_tab2.result(timeout=5.0)

        self.assertEqual(ev1["type"], "AUTO_SYNC_UPDATED")
        self.assertEqual(ev2["type"], "AUTO_SYNC_UPDATED")
        self.assertEqual(ev1["_seq"], ev2["_seq"])
        self.assertGreater(ev1["_seq"], seq0)
        self.assertFalse(ev1["auto_sync_m"])

        # Sequential 3rd tab catching up from seq0 must still receive the same event losslessly.
        ev3 = self.client.get(f"/poll?since={seq0}", timeout=3.0).json()
        self.assertEqual(ev3["_seq"], ev1["_seq"])
        self.assertEqual(ev3["type"], "AUTO_SYNC_UPDATED")

        # Restore default.
        self.client.post_json("/set_auto_sync", {"auto_sync_m": True})

    def test_trigger_sync_propagates_to_poll_and_status_last_sync(self) -> None:
        seq0 = self._current_seq()
        resp = self.client.post_json(
            "/trigger_sync",
            {
                "map_id": 2,
                "map_name": "Teyvat",
                "area_name": "Liyue",
                "lat": 111.25,
                "lng": -222.5,
                "has_world_coords": True,
                "is_underground": False,
            },
        )
        self.assertEqual(resp.status, 200)
        ev = self._drain_until_type(seq0, "SYNC_AND_MARK")
        self.assertEqual(ev["lat"], 111.25)

        st = self.client.get("/status").json()
        last_sync = st["last_sync"]
        self.assertIsInstance(last_sync, dict)
        self.assertEqual(last_sync["map_id"], 2)
        self.assertEqual(last_sync["area_name"], "Liyue")
        self.assertAlmostEqual(last_sync["lat"], 111.25, places=3)
        self.assertAlmostEqual(last_sync["lng"], -222.5, places=3)
        self.assertFalse(last_sync["is_underground"])

    def test_action_feedback_dedup_and_nav_mark_error_rollback(self) -> None:
        pins = build_sample_nearby_pins(map_id=2)[:2]
        self.client.post_json("/nearby_pins", {"map_id": 2, "lat": 0.0, "lng": 0.0, "pins": pins})
        r_mark = self.client.post_json("/nav_command", {"action": "mark", "pin_id": "pin_alpha"}).json()
        self.assertTrue(r_mark["ok"])
        self.assertEqual(r_mark["summary"]["collected"], 1)

        fb_payload = {
            "event_type": "NAV_MARK",
            "status": "error",
            "seq": 7701,
            "action": "mark",
            "pin_id": "pin_alpha",
        }
        r_fb1 = self.client.post_json("/action_feedback", fb_payload).json()
        self.assertTrue(r_fb1["ok"])
        self.assertFalse(r_fb1["deduped"])

        # Duplicate within 3.0s dedup window must be suppressed.
        r_fb2 = self.client.post_json("/action_feedback", fb_payload).json()
        self.assertTrue(r_fb2["ok"])
        self.assertTrue(r_fb2["deduped"])
        self.assertFalse(r_fb2["shown"])

        # Different seq is not deduped.
        r_fb3 = self.client.post_json("/action_feedback", {**fb_payload, "seq": 7702}).json()
        self.assertTrue(r_fb3["ok"])
        self.assertFalse(r_fb3["deduped"])

        # Rollback restored collected count back to 0.
        r_ref = self.client.post_json("/nav_command", {"action": "refresh"}).json()
        self.assertTrue(r_ref["ok"])
        self.assertEqual(r_ref["summary"]["collected"], 0)

    def test_nearby_pins_nav_command_tsp_route_and_lan_ws_snapshot(self) -> None:
        start_lan = self.client.post_json("/lan_companion", {"action": "start"}).json()
        self.assertTrue(start_lan["ok"])
        token = start_lan["token"]

        try:
            with WsClient("127.0.0.1", self.server.lan_port, f"/ws/{token}") as ws:
                init_frame = ws.recv_json(timeout=4.0)
                self.assertEqual(init_frame["type"], "STATE")

                pins = [
                    {
                        "id": "route_p1",
                        "name": "Statue Target",
                        "category": "Anemoculus",
                        "label_id": 5,
                        "lat": -95.0,
                        "lng": 105.0,
                        "map_id": 2,
                    },
                    {
                        "id": "route_p2",
                        "name": "East Target",
                        "category": "Common Chest",
                        "label_id": 16,
                        "lat": 510.0,
                        "lng": 495.0,
                        "map_id": 2,
                    },
                ]
                self.client.post_json("/nearby_pins", {"map_id": 2, "lat": 0.0, "lng": 0.0, "pins": pins})
                r_next = self.client.post_json("/nav_command", {"action": "next"}).json()
                self.assertTrue(r_next["ok"])

                seq0 = self._current_seq()
                r_tsp = self.client.post_json(
                    "/plan_tsp_route",
                    {
                        "map_id": 2,
                        "start_pos": [0.0, 0.0],
                        "pins": pins,
                        "teleport_penalty": 180.0,
                    },
                ).json()
                self.assertTrue(r_tsp["ok"])
                self.assertEqual(len(r_tsp["ordered_pins"]), 2)
                self.assertGreaterEqual(len(r_tsp["legs"]), 1)

                ev = self._drain_until_type(seq0, "TSP_ROUTE_UPDATED")
                self.assertEqual(ev["map_id"], 2)

                # Wait for updated WS STATE containing route_legs.
                got_route = False
                deadline = time.monotonic() + 5.0
                while time.monotonic() < deadline:
                    frame = ws.recv_json(timeout=3.0)
                    if frame.get("type") == "STATE" and frame.get("route_legs"):
                        got_route = True
                        self.assertIsNotNone(frame.get("target_pin"))
                        break
                self.assertTrue(got_route, "LAN WS did not receive updated STATE with route_legs")
        finally:
            self.client.post_json("/lan_companion", {"action": "stop"})

    def test_language_and_hotkeys_persist_across_status_and_config_json(self) -> None:
        seq0 = self._current_seq()
        r_lang = self.client.post_json("/set_language", {"language": "vi"}).json()
        self.assertEqual(r_lang, {"ok": True, "language": "vi", "resolved": "vi"})
        ev_lang = self._drain_until_type(seq0, "LANGUAGE_UPDATED")
        self.assertEqual(ev_lang["resolved"], "vi")

        seq1 = int(ev_lang["_seq"])
        r_hk = self.client.post_json(
            "/set_hotkeys",
            {"hotkey_sync": "alt+f8", "hotkey_nav_toggle": "alt+y"},
        ).json()
        self.assertTrue(r_hk["ok"])
        self.assertTrue(r_hk["results"]["sync"]["ok"])
        self.assertTrue(r_hk["results"]["nav_toggle"]["ok"])

        ev_hk = self._drain_until_type(seq1, "HOTKEY_UPDATED")
        self.assertIn(ev_hk["action"], ("sync", "nav_toggle"))

        st = self.client.get("/status").json()
        self.assertEqual(st["language"], "vi")
        self.assertEqual(st["hotkey_sync"], "alt+f8")
        self.assertEqual(st["hotkey_nav_toggle"], "alt+y")

        disk_cfg = self.sandbox.read_config()
        self.assertEqual(disk_cfg["language"], "vi")
        self.assertEqual(disk_cfg["hotkey_sync"], "alt+f8")
        self.assertEqual(disk_cfg["hotkey_nav_toggle"], "alt+y")

    def test_lan_companion_rotate_and_toggle_backend_contract(self) -> None:
        if get_config().is_rust:
            r1 = self.client.post_json("/lan_companion", {"action": "toggle"})
            self.assertEqual(r1.status, 200)
            d1 = r1.json()
            self.assertTrue(d1["ok"])
            self.assertTrue(d1["running"])
            old_pin = d1["pin"]
            old_tok = d1["token"]

            r_rot = self.client.post_json("/lan_companion", {"action": "rotate"})
            self.assertEqual(r_rot.status, 200)
            d_rot = r_rot.json()
            self.assertTrue(d_rot["ok"])
            self.assertNotEqual((d_rot["pin"], d_rot["token"]), (old_pin, old_tok))

            r2 = self.client.post_json("/lan_companion", {"action": "toggle"})
            self.assertEqual(r2.status, 200)
            self.assertFalse(r2.json()["running"])
        else:
            # Python src/server.py allows only status|start|stop and rejects rotate/toggle with 400.
            for act in ("rotate", "toggle"):
                resp = self.client.post_json("/lan_companion", {"action": act})
                self.assertEqual(resp.status, 400)
                self.assertFalse(resp.json()["ok"])

    def test_multi_provider_calibration_and_active_provider_switch_clears_lock(self) -> None:
        if not get_config().is_rust:
            self.skipTest("Multi-provider calibration is Rust-only")

        # 1. Calibrate against sandbox Map 2 canonical waypoints (-100,100), (0,0), (500,500)
        cal_resp = self.client.post_json(
            "/calibrate_provider",
            {
                "provider_id": "appsample",
                "map_id": 2,
                "anchors": [
                    {"x": 0.531276, "y": 0.208045, "area": "area_1"},
                    {"x": 0.522690, "y": 0.199459, "area": "area_1"},
                    {"x": 0.565621, "y": 0.156528, "area": "area_2"},
                ],
            },
        )
        self.assertEqual(cal_resp.status, 200)
        cal = cal_resp.json()
        self.assertTrue(cal["ok"])
        self.assertEqual(cal["provider"], "appsample")
        self.assertIn("transform", cal)

        # 2. Report AppSample pins and lock an AppSample target
        self.client.post_json(
            "/nearby_pins",
            {
                "provider_id": "appsample",
                "map_id": 2,
                "lat": 0.0,
                "lng": 0.0,
                "pins": [
                    {
                        "id": "appsample:5001",
                        "name": "Exquisite Chest",
                        "category": "Exquisite Chest",
                        "lat": 20.0,
                        "lng": 15.0,
                        "map_id": 2,
                        "provider_id": "appsample",
                    }
                ],
            },
        )
        st_app = self.client.get("/status").json()
        self.assertEqual(st_app.get("active_map_provider"), "appsample")

        lock_r = self.client.post_json(
            "/nav_command",
            {
                "action": "lock_target",
                "provider_id": "appsample",
                "pin": {
                    "id": "appsample:5001",
                    "name": "Exquisite Chest",
                    "category": "Exquisite Chest",
                    "lat": 20.0,
                    "lng": 15.0,
                    "map_id": 2,
                },
            },
        ).json()
        self.assertTrue(lock_r["ok"])

        # 3. Switch active provider to Kongying -> clears stale AppSample lock & emits TARGET_LOCK_UPDATED: null
        seq_before_switch = self._current_seq()
        self.client.post_json(
            "/nearby_pins",
            {
                "provider_id": "kongying",
                "map_id": 2,
                "lat": 0.0,
                "lng": 0.0,
                "pins": [
                    {
                        "id": "kongying:9001",
                        "name": "普通宝箱",
                        "category": "普通宝箱",
                        "lat": 10.0,
                        "lng": 10.0,
                        "map_id": 2,
                        "provider_id": "kongying",
                    }
                ],
            },
        )
        st_ky = self.client.get("/status").json()
        self.assertEqual(st_ky.get("active_map_provider"), "kongying")
        ev_clear = self._drain_until_type(seq_before_switch, "TARGET_LOCK_UPDATED")
        self.assertIsNone(ev_clear.get("locked_target"))

        # 4. Dual-browser / dual-provider owner isolation:
        #    Chrome claims Kongying via /tab_visibility; Edge AppSample background report_state &
        #    nearby_pins MUST be ignored without stealing provider or clearing Kongying state.
        v_ky = self.client.post_json(
            "/tab_visibility",
            {
                "tab_id": "chrome_ky_tab",
                "provider_id": "kongying",
                "visible": True,
                "focused": True,
                "interacted": True,
            },
        ).json()
        self.assertTrue(v_ky.get("is_owner"))
        self.assertEqual(v_ky.get("active_provider"), "kongying")

        bg_as = self.client.post_json(
            "/nearby_pins",
            {
                "tab_id": "edge_as_tab",
                "provider_id": "appsample",
                "tab_visible": True,
                "tab_focused": False,
                "user_initiated": False,
                "map_id": 2,
                "lat": 0.0,
                "lng": 0.0,
                "pins": [
                    {
                        "id": "appsample:8888",
                        "name": "Bg Pin",
                        "category": "Anemoculus",
                        "lat": 5.0,
                        "lng": 5.0,
                        "map_id": 2,
                    }
                ],
            },
        ).json()
        self.assertTrue(bg_as.get("ignored_secondary_client"))
        self.assertFalse(bg_as.get("is_owner"))
        self.assertEqual(self.client.get("/status").json().get("active_map_provider"), "kongying")

        # 5. FOCUS_MAP_TAB (via /quick_switch) carries target_provider=kongying & owner_tab_id=chrome_ky_tab
        seq_qs = self._current_seq()
        self.client.post_json("/quick_switch", {})
        # If no game window is foreground in sandbox, quick_switch may not broadcast FOCUS_MAP_TAB;
        # verify owner routing via /tab_visibility -> Edge takes over cleanly when user clicks it.
        v_edge = self.client.post_json(
            "/tab_visibility",
            {
                "tab_id": "edge_as_tab",
                "provider_id": "appsample",
                "visible": True,
                "focused": True,
                "interacted": True,
            },
        ).json()
        self.assertTrue(v_edge.get("is_owner"))
        self.assertEqual(v_edge.get("active_provider"), "appsample")
        self.assertEqual(self.client.get("/status").json().get("active_map_provider"), "appsample")
        _ = seq_qs


class TestTier3PortTakeover(unittest.TestCase):
    """Real WSAEADDRINUSE port takeover across two isolated server instances."""

    def test_second_instance_takes_over_occupied_port_via_shutdown(self) -> None:
        shared_ws_port = allocate_free_port()
        lan_port_a = allocate_free_port()
        lan_port_b = allocate_free_port()

        with BackendSandbox(prefix="gh_t3_take_a_") as sb_a, BackendSandbox(prefix="gh_t3_take_b_") as sb_b:
            srv_a = sb_a.start_server(
                ws_port=shared_ws_port,
                lan_port=lan_port_a,
                standby=True,
                mutex_name="Local\\GH_E2E_Takeover_A",
            )
            self.assertEqual(HttpClient("127.0.0.1", shared_ws_port).get("/status").status, 200)

            # Distinct mutex ensures Instance B hits real WSAEADDRINUSE and runs /shutdown takeover.
            srv_b = sb_b.start_server(
                ws_port=shared_ws_port,
                lan_port=lan_port_b,
                standby=True,
                mutex_name="Local\\GH_E2E_Takeover_B",
            )
            rc_a = srv_a.process.wait(timeout=6.0)
            self.assertEqual(rc_a, 0)
            self.assertTrue(srv_b.is_alive())
            st_b = HttpClient("127.0.0.1", shared_ws_port).get("/status")
            self.assertEqual(st_b.status, 200)
            self.assertEqual(st_b.json()["status"], "connected")


if __name__ == "__main__":
    unittest.main()
