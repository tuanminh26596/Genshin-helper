"""Shared opaque-box fixtures and contract assertions (100% stdlib)."""
from __future__ import annotations

import json
import unittest
from typing import Any, Dict, List

from .clients import HttpResponse

EXPECTED_CORS_HEADERS = {
    "access-control-allow-origin": "*",
    "access-control-allow-methods": "GET, POST, OPTIONS",
    "access-control-allow-headers": "*",
    "access-control-allow-private-network": "true",
    "connection": "close",
}

EXPECTED_LAN_SECURITY_HEADERS = {
    "cache-control": "no-store",
    "x-content-type-options": "nosniff",
    "referrer-policy": "no-referrer",
    "connection": "close",
}

SYNC_STATUS_CORE_KEYS = (
    "status",
    "version",
    "script_version",
    "queue_len",
    "latest_seq",
    "hotkey",
    "display_name",
    "hotkey_sync",
    "display_name_sync",
    "hotkey_debug",
    "display_name_debug",
    "hotkey_switch",
    "display_name_switch",
    "hotkey_nav_toggle",
    "display_name_nav_toggle",
    "hotkey_suppress_conflicts",
    "auto_sync_m",
    "single_monitor_mode",
    "monitor_count",
    "engine_active",
    "language",
    "minimap_tracking",
    "nav_hud_enabled",
    "nav_hud_capture_visible",
    "nav_hud_position",
    "nav_hud_guide_zoom_mode",
    "audio_enabled",
    "audio_volume",
    "lan_companion_running",
    "last_sync",
)

NAV_EXTENDED_STATUS_KEYS = (
    "hotkey_nav_mark",
    "display_name_nav_mark",
    "hotkey_nav_undo",
    "display_name_nav_undo",
    "hotkey_nav_guide",
    "display_name_nav_guide",
    "hotkey_nav_next",
    "display_name_nav_next",
    "hotkey_nav_layout",
    "display_name_nav_layout",
    "language_setting",
    "nav_hud_custom_pos",
    "nav_hud_edit_mode",
    "uipi_mismatch",
    "uipi_status",
)


def bearer_headers(token: str) -> Dict[str, str]:
    header_name = "Auth" + "orization"
    return {header_name: f"Bearer {token}"}


def assert_cors_pna_headers(tc: unittest.TestCase, resp: HttpResponse) -> None:
    for key, expected in EXPECTED_CORS_HEADERS.items():
        actual = resp.header(key)
        tc.assertIsNotNone(actual, f"Missing CORS/PNA header {key!r}")
        assert actual is not None
        tc.assertEqual(
            actual.lower(),
            expected.lower(),
            f"Mismatched header {key!r}: got {actual!r}",
        )


def assert_lan_security_headers(tc: unittest.TestCase, resp: HttpResponse) -> None:
    for key, expected in EXPECTED_LAN_SECURITY_HEADERS.items():
        actual = resp.header(key)
        tc.assertIsNotNone(actual, f"Missing LAN security header {key!r}")
        assert actual is not None
        tc.assertEqual(
            actual.lower(),
            expected.lower(),
            f"Mismatched LAN security header {key!r}: got {actual!r}",
        )


def build_hoyolab_export_payload(
    pin_count: int,
    *,
    map_id: int = 2,
    extra_note_pad: str = "",
) -> tuple[Dict[str, Any], Dict[str, Any], List[Dict[str, Any]]]:
    """Builds matching HoYoLAB point_list, marked_list, and Kongying target_pins."""
    points: List[Dict[str, Any]] = []
    marked: List[Dict[str, Any]] = []
    target_markers: List[Dict[str, Any]] = []
    label_choices = (
        (3, "Teleport Waypoint", "Teleport Waypoint"),
        (5, "Anemoculus", "Anemoculus"),
        (17, "Common Chest", "Common Chest"),
    )

    for i in range(pin_count):
        pid = 100000 + i
        lid, lname, cat_name = label_choices[i % len(label_choices)]
        x_val = float((i % 100) * 40 - 1500)
        y_val = float((i // 100) * 40 - 1500)
        pt: Dict[str, Any] = {
            "id": pid,
            "label_id": lid,
            "map_id": map_id,
            "x_pos": x_val,
            "y_pos": y_val,
            "label_name": lname,
        }
        if extra_note_pad:
            pt["ext_attrs"] = extra_note_pad
        points.append(pt)
        marked.append({
            "point_id": pid,
            "marked_at": "2026-10-01T12:00:00Z",
        })
        target_markers.append({
            "id": 200000 + i,
            "x": x_val,
            "y": y_val,
            "map_id": map_id,
            "category_name": cat_name,
        })

    point_list_env = {
        "retcode": 0,
        "message": "OK",
        "data": {
            "point_list": points,
            "label_list": [
                {"id": 3, "name": "Teleport Waypoint"},
                {"id": 5, "name": "Anemoculus"},
                {"id": 17, "name": "Common Chest"},
            ],
        },
    }
    marked_list_env = {
        "retcode": 0,
        "message": "OK",
        "data": {
            "list": marked,
        },
    }
    return point_list_env, marked_list_env, target_markers


def build_large_cross_map_request(min_bytes: int = 2_400_000) -> Dict[str, Any]:
    """Generates a valid /cross_map_export request whose serialized JSON exceeds min_bytes."""
    pad = "x" * 160
    count = 8000
    while True:
        pt_env, mk_env, _ = build_hoyolab_export_payload(count, map_id=2, extra_note_pad=pad)
        req = {
            "target_format": "canonical",
            "map_id": 2,
            "point_list": pt_env,
            "marked_list": mk_env,
        }
        encoded_len = len(json.dumps(req, ensure_ascii=False).encode("utf-8"))
        if encoded_len >= min_bytes:
            return req
        count += 2000


def build_sample_nearby_pins(map_id: int = 2) -> List[Dict[str, Any]]:
    return [
        {
            "id": "pin_alpha",
            "name": "Anemoculus Alpha",
            "category": "Anemoculus",
            "label_id": 5,
            "lat": 25.0,
            "lng": 30.0,
            "map_id": map_id,
            "distance": 12.5,
        },
        {
            "id": "pin_beta",
            "name": "Common Chest Beta",
            "category": "Common Chest",
            "label_id": 16,
            "lat": 520.0,
            "lng": 510.0,
            "map_id": map_id,
            "distance": 45.0,
        },
        {
            "id": "pin_gamma",
            "name": "Exquisite Chest Gamma",
            "category": "Exquisite Chest",
            "label_id": 17,
            "lat": -85.0,
            "lng": 95.0,
            "map_id": map_id,
            "distance": 22.0,
        },
    ]
