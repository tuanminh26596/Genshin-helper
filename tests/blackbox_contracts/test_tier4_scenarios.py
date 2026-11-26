"""Tier 4: Real-world end-to-end application scenarios (S1-S5)."""
from __future__ import annotations

import base64
from concurrent.futures import ThreadPoolExecutor
import json
from pathlib import Path
import time
import unittest
import xml.etree.ElementTree as ET

from ._helpers import (
    bearer_headers,
    build_hoyolab_export_payload,
    build_sample_nearby_pins,
)
from .clients import HttpClient, NativeHostClient, WsClient
from .config import get_config
from .sandbox import BackendSandbox, allocate_free_port

CONFIG_26_KEYS = (
    "hotkey",
    "hotkey_sync",
    "hotkey_debug",
    "hotkey_switch",
    "hotkey_nav_mark",
    "hotkey_nav_undo",
    "hotkey_nav_guide",
    "hotkey_nav_next",
    "hotkey_nav_layout",
    "hotkey_nav_toggle",
    "hotkey_suppress_conflicts",
    "audio_enabled",
    "audio_volume",
    "chest_search_radius",
    "ws_port",
    "auto_sync_m",
    "single_monitor_mode",
    "language",
    "minimap_tracking",
    "nav_hud_enabled",
    "nav_hud_capture_visible",
    "nav_hud_position",
    "nav_hud_custom_pos",
    "nav_hud_guide_zoom_mode",
    "lan_companion_enabled",
    "lan_companion_port",
)


class TestTier4Scenarios(unittest.TestCase):
    """Five end-to-end real-world scenarios covering F1-F5."""

    def test_s1_extension_power_cycle_poll_and_graceful_shutdown(self) -> None:
        """S1: Standby -> Native Host wake -> active polling -> standby -> shutdown."""
        with BackendSandbox(prefix="gh_s1_power_") as sb:
            srv = sb.start_server(standby=True, lan_enabled=False, language="en")
            cli = HttpClient("127.0.0.1", srv.ws_port, timeout=10.0)
            seq0 = int(cli.get("/poll?since=-1").json()["_seq"])

            with NativeHostClient(
                sb.host_command(),
                port=srv.ws_port,
                extra_args=["chrome-extension://nfkijdoblmphmdljfogooajhbldkeojh/"],
                cwd=str(sb.run_dir),
                env=sb.build_child_env(),
            ) as host:
                host.send_json({"cmd": "ping"})
                p1 = host.recv_json()
                self.assertTrue(p1["server_running"])
                self.assertFalse(p1["engine_active"])

                host.send_json({"cmd": "start_server"})
                woke = host.recv_json()
                self.assertEqual(woke["started"], "woke_standby")
                self.assertTrue(woke["engine_active"])

                ev1 = cli.get(f"/poll?since={seq0}", timeout=4.0).json()
                self.assertEqual(ev1["type"], "ENGINE_STATE_UPDATED")
                self.assertTrue(ev1["engine_active"])
                seq1 = int(ev1["_seq"])

                host.send_json({"cmd": "start_server"})
                already = host.recv_json()
                self.assertEqual(already["started"], "already_active")

                host.send_json({"cmd": "stop_server", "mode": "standby"})
                stby = host.recv_json()
                self.assertEqual(stby["mode"], "standby")
                self.assertFalse(stby["engine_active"])

                ev2 = cli.get(f"/poll?since={seq1}", timeout=4.0).json()
                self.assertEqual(ev2["type"], "ENGINE_STATE_UPDATED")
                self.assertFalse(ev2["engine_active"])
                seq2 = int(ev2["_seq"])

                # Start background long-poll right before shutdown so SHUTDOWN wakes it.
                with ThreadPoolExecutor(max_workers=1) as pool:
                    fut_shut = pool.submit(
                        lambda: HttpClient("127.0.0.1", srv.ws_port, timeout=6.0).get(f"/poll?since={seq2}").json()
                    )
                    time.sleep(0.15)
                    host.send_json({"cmd": "stop_server", "mode": "shutdown"})
                    shut_resp = host.recv_json()
                    self.assertTrue(shut_resp["acknowledged"])
                    self.assertFalse(shut_resp["server_running"])
                    ev_shut = fut_shut.result(timeout=5.0)
                    self.assertEqual(ev_shut["type"], "SHUTDOWN")

                rc, rem_out, _ = host.close_and_wait()
                self.assertEqual(rc, 0)
                self.assertEqual(rem_out, b"")

            self.assertEqual(srv.process.wait(timeout=5.0), 0)

    def test_s1_optional_cold_host_launch_when_explicitly_enabled(self) -> None:
        """Optional cold start_server launch test gated by GH_TEST_HOST_COLD_LAUNCH=1."""
        if not get_config().allow_cold_host_launch:
            self.skipTest("Cold host launch skipped (set GH_TEST_HOST_COLD_LAUNCH=1 to enable)")
        with BackendSandbox(prefix="gh_s1_cold_") as sb:
            port = allocate_free_port()
            sb.write_config({
                "ws_port": port,
                "lan_companion_port": allocate_free_port(),
                "hoyolab_sync": {"auto_check_on_startup": False, "auto_update_mode": "off"},
            })
            with NativeHostClient(
                sb.host_command(),
                port=port,
                cwd=str(sb.run_dir),
                env=sb.build_child_env(extra={"GH_HOST_SKIP_SCHTASKS": "1"}),
            ) as host:
                host.send_json({"cmd": "start_server"})
                res = host.recv_json(timeout=15.0)
                self.assertTrue(res["ok"])
                self.assertEqual(res["started"], "launched")
                host.send_json({"cmd": "stop_server", "mode": "shutdown"})
                host.recv_json(timeout=5.0)

    def test_s2_map_navigation_tsp_and_lan_companion_mobile_sync(self) -> None:
        """S2: /report_state -> /nearby_pins -> /plan_tsp_route -> LAN auth/QR/WS -> nav mark/undo."""
        with BackendSandbox(prefix="gh_s2_nav_") as sb:
            srv = sb.start_server(standby=False, lan_enabled=False, language="en")
            cli = HttpClient("127.0.0.1", srv.ws_port, timeout=10.0)

            self.assertEqual(
                cli.post_json("/report_state", {"center": [0.0, 0.0], "zoom": 1.0, "activeAreaId": 1}).status,
                200,
            )
            pins = build_sample_nearby_pins(map_id=2)
            self.assertEqual(
                cli.post_json("/nearby_pins", {"map_id": 2, "lat": 0.0, "lng": 0.0, "pins": pins}).status,
                200,
            )
            route = cli.post_json(
                "/plan_tsp_route",
                {"map_id": 2, "start_pos": [0.0, 0.0], "pins": pins, "teleport_penalty": 180.0},
            ).json()
            self.assertTrue(route["ok"])
            self.assertEqual(len(route["ordered_pins"]), len(pins))

            lan_start = cli.post_json("/lan_companion", {"action": "start"}).json()
            self.assertTrue(lan_start["ok"])
            pin_code = lan_start["pin"]

            lan_cli = HttpClient("127.0.0.1", srv.lan_port, timeout=10.0)
            self.assertEqual(lan_cli.get("/").status, 200)
            auth = lan_cli.post_json("/auth/pin", {"pin": pin_code}).json()
            self.assertTrue(auth["ok"])
            token = auth["token"]

            r_comp = lan_cli.get("/companion", headers=bearer_headers(token))
            self.assertEqual(r_comp.status, 200)
            r_qr = lan_cli.get("/qr.svg", headers=bearer_headers(token))
            self.assertEqual(r_qr.status, 200)
            ET.fromstring(r_qr.text())

            with WsClient("127.0.0.1", srv.lan_port, f"/ws/{token}") as ws:
                init_state = ws.recv_json(timeout=4.0)
                self.assertEqual(init_state["type"], "STATE")
                self.assertTrue(init_state["route_legs"])

                cli.post_json("/nav_command", {"action": "mark"})
                after_mark = ws.recv_json(timeout=4.0)
                self.assertEqual(after_mark["type"], "STATE")

                cli.post_json("/nav_command", {"action": "undo"})
                after_undo = ws.recv_json(timeout=4.0)
                self.assertEqual(after_undo["type"], "STATE")

            cli.post_json("/lan_companion", {"action": "stop"})

    def test_s3_large_multi_format_cross_map_export_session(self) -> None:
        """S3: >2 MiB multi-format export across canonical, kongying, appsample, genshinmap."""
        with BackendSandbox(prefix="gh_s3_export_") as sb:
            srv = sb.start_server(standby=True, lan_enabled=False)
            cli = HttpClient("127.0.0.1", srv.ws_port, timeout=30.0)

            # Build >2.2 MiB point_list payload with matching Kongying/Appsample/GenshinMap targets.
            pt_env, mk_env, ky_targets = build_hoyolab_export_payload(7500, map_id=2, extra_note_pad="z" * 160)
            identity_affine = {"ax": 1.0, "bx": 0.0, "cy": 1.0, "dy": 0.0, "map_id": 2}

            # 1. Canonical >2 MiB
            canon_req = {
                "target_format": "canonical",
                "map_id": 2,
                "point_list": pt_env,
                "marked_list": mk_env,
            }
            canon_bytes = json.dumps(canon_req, ensure_ascii=False).encode("utf-8")
            self.assertGreater(len(canon_bytes), 2 * 1024 * 1024)
            r_canon = cli.request(
                "POST",
                "/cross_map_export",
                body=canon_bytes,
                headers={"Content-Type": "application/json; charset=utf-8"},
                timeout=30.0,
            )
            self.assertEqual(r_canon.status, 200)
            d_canon = r_canon.json()
            self.assertTrue(d_canon["ok"])
            self.assertEqual(d_canon["report"]["total_source_pins"], 7500)

            # 2. Kongying (using small aligned slice + identity affine)
            pt_small, mk_small, ky_small = build_hoyolab_export_payload(12, map_id=2)
            r_ky = cli.post_json(
                "/cross_map_export",
                {
                    "target_format": "kongying",
                    "map_id": 2,
                    "point_list": pt_small,
                    "marked_list": mk_small,
                    "target_pins": ky_small,
                    "affine": identity_affine,
                },
            ).json()
            self.assertTrue(r_ky["ok"])
            self.assertEqual(len(r_ky["payload"]["Data_KYJG"]), 12)
            self.assertIn("Time_KYJG", r_ky["payload"])
            self.assertIn("updated_at", r_ky["payload"])

            # 3. Appsample (matching HoYoLAB coordinates with category_aliases appsample_code)
            app_code_map = {3: "o3", 5: "o5", 17: "o17"}
            app_targets = [
                {
                    "id": 3000 + i,
                    "x": pt["x_pos"],
                    "y": pt["y_pos"],
                    "type": app_code_map[pt["label_id"]],
                }
                for i, pt in enumerate(pt_small["data"]["point_list"])
            ]
            r_app = cli.post_json(
                "/cross_map_export",
                {
                    "target_format": "appsample",
                    "map_id": 2,
                    "point_list": pt_small,
                    "marked_list": mk_small,
                    "target_pins": app_targets,
                    "affine": identity_affine,
                },
            ).json()
            self.assertTrue(r_app["ok"])
            self.assertEqual(len(r_app["payload"]["ids"]), 12)

            # 4. GenshinMap GitHub (GM_007~ Base64 wire envelope)
            gm_targets = [
                {
                    "id": f"Anemoculus/hash_{i}",
                    "featureKey": "Anemoculus",
                    "x": pt["x_pos"],
                    "y": pt["y_pos"],
                    "category_name": pt["label_name"],
                }
                for i, pt in enumerate(pt_small["data"]["point_list"])
                if pt["label_id"] == 5
            ]
            r_gm = cli.post_json(
                "/cross_map_export",
                {
                    "target_format": "genshinmap",
                    "map_id": 2,
                    "point_list": pt_small,
                    "marked_list": mk_small,
                    "target_pins": gm_targets,
                    "affine": identity_affine,
                },
            ).json()
            self.assertTrue(r_gm["ok"])
            b64_str = r_gm["payload"]
            decoded = base64.b64decode(b64_str).decode("utf-8")
            self.assertTrue(decoded.startswith("GM_007~"))
            gm_obj = json.loads(decoded[len("GM_007~"):])
            for req_key in ("version", "options", "displayed", "completed", "editor"):
                self.assertIn(req_key, gm_obj)
            self.assertEqual(len(gm_obj["completed"]["features"]), len(gm_targets))

    def test_s4_diagnostic_capture_vietnamese_utf8_note_and_config_persistence(self) -> None:
        """S4: /report_state -> /trigger_diagnostic -> UTF-8 note -> verify bundle & 26-key config.json."""
        with BackendSandbox(prefix="gh_s4_diag_") as sb:
            srv = sb.start_server(standby=True, lan_enabled=False, language="en")
            cli = HttpClient("127.0.0.1", srv.ws_port, timeout=20.0)

            cli.post_json("/report_state", {"center": [50.0, -75.0], "zoom": 2.0, "activeAreaId": 2})
            diag = cli.post_json("/trigger_diagnostic", {"note": "initial s4"}).json()
            self.assertTrue(diag["ok"])
            bundle = diag["bundle"]
            self.assertTrue(bundle["success"])
            folder_name = bundle["folder_name"]
            bundle_dir = Path(bundle["bundle_dir"]).resolve()
            self.assertTrue(str(bundle_dir).startswith(str(sb.diagnostics_dir.resolve())))

            vi_note = "Lỗi ghim bản đồ dưới lòng đất — kiểm tra tầng B2 (tiếng Việt UTF-8)"
            saved = cli.post_json(
                "/save_diagnostic_note",
                {"folder_name": folder_name, "note": vi_note},
            ).json()
            self.assertTrue(saved["ok"])
            self.assertEqual(saved["user_note"], vi_note)

            note_file = bundle_dir / "05_user_note.txt"
            report_file = bundle_dir / "03_report.json"
            self.assertTrue(note_file.is_file())
            self.assertTrue(report_file.is_file())
            self.assertIn(vi_note, note_file.read_text(encoding="utf-8"))
            report_json = json.loads(report_file.read_text(encoding="utf-8"))
            self.assertEqual(report_json["user_note"], vi_note)

            # Mutate settings and verify all 26 config keys persist on disk in sandbox APPDATA.
            cli.post_json("/set_language", {"language": "vi"})
            cli.post_json("/set_nav_hud", {"enabled": False, "position": "right_of_minimap"})
            disk_cfg = sb.read_config()
            for key in CONFIG_26_KEYS:
                self.assertIn(key, disk_cfg, f"Missing persisted config key: {key}")
            self.assertEqual(disk_cfg["language"], "vi")
            self.assertFalse(disk_cfg["nav_hud_enabled"])
            self.assertEqual(disk_cfg["nav_hud_position"], "right_of_minimap")

    def test_s5_multitab_lossless_poll_rebind_and_port_takeover(self) -> None:
        """S5: 2 concurrent tabs receive monotonic events -> isolated second instance takes over port."""
        shared_ws_port = allocate_free_port()
        lan_a = allocate_free_port()
        lan_b = allocate_free_port()

        with BackendSandbox(prefix="gh_s5_a_") as sb_a, BackendSandbox(prefix="gh_s5_b_") as sb_b:
            srv_a = sb_a.start_server(
                ws_port=shared_ws_port,
                lan_port=lan_a,
                standby=True,
                mutex_name="Local\\GH_E2E_S5_A",
            )
            cli_a = HttpClient("127.0.0.1", shared_ws_port, timeout=10.0)
            seq0 = int(cli_a.get("/poll?since=-1").json()["_seq"])

            cli_a.post_json("/set_language", {"language": "vi"})
            cli_a.post_json("/set_hotkeys", {"hotkey_sync": "alt+f7"})

            def _drain_tab(start_seq: int) -> list[tuple[int, str]]:
                c = HttpClient("127.0.0.1", shared_ws_port, timeout=5.0)
                out: list[tuple[int, str]] = []
                cur = start_seq
                for _ in range(2):
                    ev = c.get(f"/poll?since={cur}").json()
                    cur = int(ev["_seq"])
                    out.append((cur, ev["type"]))
                return out

            with ThreadPoolExecutor(max_workers=2) as pool:
                t1 = pool.submit(_drain_tab, seq0)
                t2 = pool.submit(_drain_tab, seq0)
                evs1 = t1.result(timeout=5.0)
                evs2 = t2.result(timeout=5.0)

            self.assertEqual(evs1, evs2)
            self.assertEqual([t for _, t in evs1], ["LANGUAGE_UPDATED", "HOTKEY_UPDATED"])
            self.assertLess(evs1[0][0], evs1[1][0])

            # Second isolated instance takes over shared_ws_port via WSAEADDRINUSE -> /shutdown.
            srv_b = sb_b.start_server(
                ws_port=shared_ws_port,
                lan_port=lan_b,
                standby=True,
                mutex_name="Local\\GH_E2E_S5_B",
            )
            self.assertEqual(srv_a.process.wait(timeout=6.0), 0)
            self.assertTrue(srv_b.is_alive())
            self.assertEqual(cli_a.get("/status").json()["status"], "connected")


if __name__ == "__main__":
    unittest.main()
