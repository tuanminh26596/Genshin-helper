## Summary

Briefly describe what this pull request changes and why.

## Related Issue

Closes #(issue)

## Scope & AI Assistance

- [ ] **Strictly Scoped:** Every changed line relates directly to this PR (no unrelated edits; large features / new platforms were discussed in an Issue first and include CI coverage for any new platform).
- [ ] **AI Disclosure & Ownership:** _State "None" or name the AI tool/model used:_ `None` — I have personally read, understood, and verified every change in this PR and take full responsibility for it.

## Safety, Privacy & License Checklist

- [ ] **Passive CV / Anti-Cheat Invariant:** No game memory reads/writes, no DLL injection, and no `SendInput` against the game window.
- [ ] **Protected Invariants Preserved:** Kept `DECISION:` blocks, typed `gh-win32` Win32 bindings (zero `SendInput`, GDI `SRCCOPY` only), lossless `/poll?since=<seq>` ring buffer, `TM_CCORR_NORMED` + `TM_CCOEFF_NORMED >= 0.65` zoom gate, and pinned Extension ID (`nfkijdoblmphmdljfogooajhbldkeojh`).
- [ ] **License & IP Hygiene:** Only original MIT code or properly attributed MIT/permissive ports (with top-of-file attribution + `THIRD_PARTY_NOTICES.md` update); zero copyrighted HoYoverse artwork, `*.npz` datasets, or `rust/target/` artifacts committed; zero AGPL/GPL/unlicensed code copied.
- [ ] **UTF-8 & Build Hygiene:** UTF-8 preserved in Rust/JS/Python tooling and `0` bytes in `rust/target/`.

## Verification

Check all verification commands run locally:

- [ ] `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings`
- [ ] `cargo nextest run --manifest-path rust/Cargo.toml --workspace`
- [ ] `.\.venv\Scripts\python.exe tests/verify_i18n_and_ext_v5.py`
- [ ] `.\.venv\Scripts\python.exe tests/test_blackbox_contracts.py --backend rust -v`
- [ ] `.\.venv\Scripts\python.exe scripts/audit_release.py --strict-ip`
- [ ] `.\.venv\Scripts\python.exe scripts/audit_release.py --dir rust --strict-ip`
- [ ] Updated `README.md` / `README.vi.md` if user-facing behavior changed
