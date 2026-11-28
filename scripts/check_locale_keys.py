#!/usr/bin/env python3
"""Guard: every app locale key is used by the Rust app, or is listed as reviewed-unused.

A key nobody reads usually means a feature that was planned and never built. The Alt+F12
"diagnostic saved" toast sat as an unused `toast.diagnostic_saved` key from v5.3 until 2026-11-28
while the hotkey stayed silent in game. Rules, checked against `assets/locales/{vi,en}.json`:

1. vi and en have the same keys.
2. Each key appears as a string literal (`"key"`) somewhere under `rust/crates/**/*.rs`, or is
   listed in `scripts/locale_unused_allowlist.json` with a reason.
3. The allowlist holds no stale entries: no key that is now used and no key that no longer exists.

Usage: python scripts/check_locale_keys.py [--repo <root>]   (exit 0 pass, 1 fail)
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def load_json(path: Path) -> dict:
    with path.open(encoding="utf-8") as fh:
        return json.load(fh)


def _skip_literal(text: str, i: int) -> int:
    """Index just past the string / raw string / char literal starting at `i`, or `i` if none."""
    if text.startswith("b'", i) or (text[i] == "'" and i + 2 < len(text)):
        j = i + (2 if text[i] == "b" else 1)
        # char literal: 'x', '\n', '\u{..}', but not a lifetime ('a ...)
        if text[j] == "\\":
            end = text.find("'", j + 2)
            return end + 1 if end != -1 else i
        if j + 1 < len(text) and text[j + 1] == "'":
            return j + 2
        return i
    if text[i] == "r" and i + 1 < len(text) and text[i + 1] in '#"':
        j = i + 1
        hashes = 0
        while j < len(text) and text[j] == "#":
            hashes += 1
            j += 1
        if j < len(text) and text[j] == '"':
            end = text.find('"' + "#" * hashes, j + 1)
            return end + 1 + hashes if end != -1 else len(text)
        return i
    if text[i] == '"':
        j = i + 1
        while j < len(text):
            if text[j] == "\\":
                j += 2
                continue
            if text[j] == '"':
                return j + 1
            j += 1
        return len(text)
    return i


def strip_test_modules(text: str) -> str:
    """Drops `#[cfg(test)] mod ... { ... }` blocks so keys named only by tests do not count."""
    out = []
    pos = 0
    marker = "#[cfg(test)]"
    while True:
        start = text.find(marker, pos)
        if start == -1:
            out.append(text[pos:])
            break
        after = text[start + len(marker):].lstrip()
        if not after.startswith("mod "):
            out.append(text[pos:start + len(marker)])
            pos = start + len(marker)
            continue
        brace = text.find("{", start)
        if brace == -1:
            out.append(text[pos:])
            break
        depth = 0
        i = brace
        while i < len(text):
            nxt = _skip_literal(text, i)
            if nxt != i:
                i = nxt
                continue
            if text.startswith("//", i):
                nl = text.find("\n", i)
                i = len(text) if nl == -1 else nl
                continue
            if text[i] == "{":
                depth += 1
            elif text[i] == "}":
                depth -= 1
                if depth == 0:
                    i += 1
                    break
            i += 1
        out.append(text[pos:start])
        pos = i
    return "".join(out)


def rust_sources(root: Path) -> str:
    """App code only: crate `src/` files without their `#[cfg(test)] mod` blocks."""
    parts = []
    for path in sorted((root / "rust" / "crates").rglob("*.rs")):
        if "target" in path.parts or "src" not in path.parts:
            continue
        parts.append(strip_test_modules(path.read_text(encoding="utf-8", errors="ignore")))
    return "\n".join(parts)


def check(root: Path) -> list[str]:
    errors: list[str] = []
    vi = load_json(root / "assets" / "locales" / "vi.json")
    en = load_json(root / "assets" / "locales" / "en.json")
    allow_path = root / "scripts" / "locale_unused_allowlist.json"
    allow = load_json(allow_path).get("keys", {}) if allow_path.exists() else {}

    for key in sorted(set(vi) - set(en)):
        errors.append(f"{key}: in vi.json but missing from en.json")
    for key in sorted(set(en) - set(vi)):
        errors.append(f"{key}: in en.json but missing from vi.json")

    src = rust_sources(root)
    keys = sorted(set(vi) | set(en))
    for key in keys:
        used = f'"{key}"' in src
        if not used and key not in allow:
            errors.append(
                f"{key}: no Rust code uses it. Wire it up, delete it, or list it in "
                f"{allow_path.relative_to(root).as_posix()} with a reason"
            )
        if used and key in allow:
            errors.append(f"{key}: listed as unused but Rust code now uses it; remove it from the allowlist")
    for key in sorted(set(allow) - set(keys)):
        errors.append(f"{key}: listed as unused but no longer in the locale files; remove it from the allowlist")
    for key, why in allow.items():
        if not str(why).strip():
            errors.append(f"{key}: allowlist entry needs a reason")
    return errors


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--repo", default=str(Path(__file__).resolve().parent.parent))
    args = ap.parse_args(argv)
    errors = check(Path(args.repo))
    for err in errors:
        print(f"[locale-keys] FAIL: {err}")
    if not errors:
        print("[locale-keys] every app locale key is used or reviewed")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
