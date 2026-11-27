---
name: Genshin Helper
description: Teyvat Craft dark obsidian & antique-gold 520x580 Control Center, Liyue sound cues, Nebula backdrop, and unified Tinh Tú HUD/WebMap navigation.
colors:
  obs-ground: "#1A1E28"
  obs-surface: "rgba(34, 40, 54, 0.89)"
  obs-surface-solid: "#222836"
  obs-surface-head: "rgba(29, 34, 46, 0.92)"
  obs-surface-inset: "#151923"
  obs-surface-raised: "#2B3244"
  obs-surface-hover: "#333B50"
  horizon-hairline: "rgba(211, 188, 142, 0.22)"
  horizon-border-strong: "rgba(211, 188, 142, 0.44)"
  parchment-ink: "#F2ECE0"
  parchment-secondary: "#C2BAAB"
  muted-ink: "#B0A89A"
  antique-gold: "#D3BC8E"
  bright-gold: "#E4CF9F"
  gold-deep: "#B59963"
  telemetry-cyan: "#7EE8FF"
  emerald-active: "#52C4A0"
  amber-standby: "#E0AB58"
  crimson-offline: "#EE827D"
typography:
  display:
    fontFamily: "'Be Vietnam Pro', 'Segoe UI', -apple-system, BlinkMacSystemFont, Roboto, Arial, sans-serif"
    fontSize: "15px"
    fontWeight: 600
    lineHeight: 1.25
    letterSpacing: "-0.015em"
  title:
    fontFamily: "'Be Vietnam Pro', 'Segoe UI', -apple-system, BlinkMacSystemFont, Roboto, Arial, sans-serif"
    fontSize: "12px"
    fontWeight: 600
    lineHeight: 1.25
    letterSpacing: "0em"
  body:
    fontFamily: "'Be Vietnam Pro', 'Segoe UI', -apple-system, BlinkMacSystemFont, Roboto, Arial, sans-serif"
    fontSize: "12px"
    fontWeight: 400
    lineHeight: 1.35
    letterSpacing: "0em"
    fontFeature: "lnum, tnum"
  label:
    fontFamily: "'Be Vietnam Pro', 'Segoe UI', -apple-system, BlinkMacSystemFont, Roboto, Arial, sans-serif"
    fontSize: "11px"
    fontWeight: 600
    lineHeight: 1.25
    letterSpacing: "0em"
  mono:
    fontFamily: "'JetBrains Mono', Consolas, monospace"
    fontSize: "11px"
    fontWeight: 600
    lineHeight: 1.25
    letterSpacing: "0em"
    fontFeature: "lnum, tnum"
rounded:
  xs: "4px"
  chip: "6px"
  sm: "8px"
  md: "10px"
  shell: "12px"
  pill: "999px"
spacing:
  xs: "4px"
  sm: "6px"
  md: "8px"
  lg: "11px"
  xl: "14px"
components:
  pill-btn-primary:
    backgroundColor: "{colors.antique-gold}"
    textColor: "#1A1E28"
    typography: "{typography.title}"
    rounded: "{rounded.pill}"
    height: "28px"
    padding: "0 11px 0 4px"
  pill-btn-secondary:
    backgroundColor: "{colors.obs-surface-raised}"
    textColor: "{colors.parchment-ink}"
    typography: "{typography.title}"
    rounded: "{rounded.pill}"
    height: "28px"
    padding: "0 11px 0 4px"
  pill-btn-sm:
    backgroundColor: "{colors.obs-surface-raised}"
    textColor: "{colors.parchment-ink}"
    typography: "{typography.label}"
    rounded: "{rounded.pill}"
    height: "24px"
    padding: "0 10px 0 3px"
  t3-sheet:
    backgroundColor: "rgba(34, 40, 54, 0.89)"
    textColor: "{colors.parchment-ink}"
    typography: "{typography.body}"
    rounded: "{rounded.md}"
    padding: "8px 11px"
  t3-hud-tray:
    backgroundColor: "{colors.obs-surface-inset}"
    textColor: "{colors.parchment-secondary}"
    typography: "{typography.label}"
    padding: "7px 11px"
  tab-active:
    backgroundColor: "rgba(211, 188, 142, 0.16)"
    textColor: "{colors.parchment-ink}"
    typography: "{typography.title}"
    height: "32px"
    underline: "2px solid {colors.bright-gold}"
  sticky-footer:
    backgroundColor: "rgba(29, 34, 46, 0.92)"
    textColor: "{colors.parchment-secondary}"
    typography: "{typography.label}"
    padding: "0 14px"
    height: "34px"
---

# Design System: Genshin Helper

## Overview

**Creative North Star: "Teyvat Craft v4 — Dark Nebula & Antique Gold"**

Genshin Helper's Chrome Extension Control Center (`extension/popup.html`, `extension/popup.css`, `extension/popup.js`) and harmonized desktop/WebMap/mobile overlays share a unified obsidian-and-gold Teyvat Craft aesthetic anchored by the Tinh Vân (`backdrop_nebula_dark.webp`) backdrop + geometric Constellation Rosette watermark, a frameless `30×30px` brand emblem, bilaterally centered custom solid-glyph SVG iconography (`26×26px` `.t3-cell-ico` badges and `18px`/`20px` `.pill-disc` capsules), **Tinh Tú** (`Star Dart`) navigation arrows, and **Liyue** silk-guzheng audio cues. Every surface earns its pixel budget at fixed `520×580px` zero-scroll dimensions on Tab 1 and Tab 2.

Every surface earns its pixel budget through immediate scannability and single-click directness. Live engine state, Teyvat telemetry, the 3-state power tray (`Bật` / `Nghỉ 0%` / `Tắt hẳn`), and capsule primary CTAs (`Mở Map` + `Chuyển về Game`) sit together inside a dedicated **Session Band above the `32px` flush tab bar**, leaving two stacked architectural Control Deck sheets above breathing nebula space and a `34px` sticky bottom bar.

**Key Characteristics:**
- Fixed `520×580px` zero-scroll Control and Tools viewports layered over `#1A1E28` obsidian ground, `backdrop_nebula_dark.webp` (`opacity: 0.92`), a centered `0.06` Rosette SVG watermark, double gold inner filigree perimeter (`inset: 3px`), and four symmetrical L-shaped corner brackets.
- **Teyvat Obsidian & Antique Gold Palette:** Translucent slate sheets (`rgba(34, 40, 54, 0.89)` / `#222836`) with `3px` + `4px` double-bevel inset frames, recessed `#151923` wells, antique gold (`#D3BC8E` / `#E4CF9F`), and warm parchment ink (`#F2ECE0` / `#C2BAAB` / `#B0A89A`).
- Bilaterally symmetrical solid-glyph vector iconography (`viewBox="0 0 24 24"`, centered at `(12, 12)`) inside `26×26px` `.t3-cell-ico` containers and `18px`/`20px` circular `.pill-disc` badges across header updates (`done` emerald octagon, `avail`, `updating`), tabs (`Control` cog, `Tools` crossed hammer & wrench, `Hotkeys` rune tablet with active 4-point star), Control Deck rows, centered `Kéo` crosshair disc, LAN ON/OFF (`Beacon` emitter / `Power Off`), Quest-banner Bug Report (`#btn-diag-toggle`), and Mora-pouch Support (`#btn-donate-toggle`).
- Concentric medallion switches built with inline integer-centered SVGs inside `.switch-knob` (`42×22px` primary with `16×16` knob + `20px` travel, `34×18px` child with `12×12` knob + `16px` travel) paired with a custom `.t3-slider-wrap` concentric SVG volume thumb (`16×16px`), eliminating browser DPI drift.
- Harmonized **Tinh Tú (`Star Dart`)** directional arrow (`M16 3 L25.5 25 L16 20 L6.5 25 Z` with 4-point star center and `#141822` silhouette halo) shared across `gh-hud` (`nav_hud`, `radar_map`, `mmap_overlay`), WebMap (`page_main.js`, `provider_runtime.js`), and `lan_companion_ui.html`, paired with Liyue silk-guzheng PCM SFX (`gh-win32/src/audio.rs`).
- Strict bilingual (`VI | EN`) parity with `font-variant-numeric: lining-nums tabular-nums`, anchored drawers above the `34px` sticky footer with `.t3-ornament-rule` gold dividers, zero emojis in extension chrome.

## Colors

The palette balances warm dark obsidian surfaces (`#1A1E28` / `#222836`) with antique gold (`#D3BC8E` / `#E4CF9F`) instrumentation accents and strictly calibrated semantic & telemetry tones.

### Primary
- **Antique Gold & Bright Gold** (`#D3BC8E` / `#E4CF9F`): Primary capsule CTA fill (`#D3BC8E` with `#1A1E28` ink and subtle inner rim), `2px` active tab underline and tint wash, checked switch tracks, custom volume slider fill & concentric thumb ring, active `VI | EN` pill, double hairline frames, and corner filigree brackets.
- **Warm Parchment Ink Scale** (`#F2ECE0` / `#C2BAAB` / `#B0A89A`): Primary titles, secondary descriptions, and subtle captions maintaining `>= 6.5:1` AA+ readability over dark obsidian sheets.

### Secondary & Telemetry
- **Player Star Cyan** (`#7EE8FF`): Live telemetry coordinates, surface floor badge (`G`), LAN URL readouts, and the active **Tinh Tú** (`Star Dart`) player position/heading marker across HUD, Live Radar, WebMap, and LAN mobile views.

### Semantic
- **Jade Emerald** (`#52C4A0`): Header `done` update octagon, live `Active` status star/gem, `LAN Running` badge, `sync` primary hotkey dashed accent, and clean save/privacy states.
- **Amber Standby** (`#E0AB58`): `Standby (0% CPU)` warning state and caution states (with `[232, 168, 56]` in HUD rings).
- **Crimson Offline** (`#EE827D`): `Offline` disconnect star/gems, active hotkey capture rings, UIPI admin elevation warnings, and `Quit` accents.

### Neutral
- **Obsidian Ground** (`#1A1E28`): Base shell canvas behind `icons/backdrop_nebula_dark.webp` (`opacity: 0.92`) and Rosette watermark (`opacity: 0.06`).
- **Translucent Slate Surface** (`rgba(34, 40, 54, 0.89)` / `#222836`): `.session-band`, `.t3-sheet` unified sheets, Tool/Hotkey `.card` containers, and slide-up drawers with `3px` + `4px` double hairline bevels.
- **Recessed Charcoal Well** (`#151923`): Inset `#hud-sub-group.t3-hud-tray`, `.t3-cell-ico` `26×26px` icon wells, segmented Engine Power track, unchecked switch tracks, and hotkey recorder slots.
- **Raised Slate & Hairline Borders** (`#2B3244` / `#333B50` / `rgba(211, 188, 142, 0.22)`–`0.44`): Secondary capsule pills, active power segments, interactive hover states, and crisp structural separators.

### Named Rules
**The Obsidian & Gold Rule.** Antique gold (`#D3BC8E` / `#E4CF9F`) unifies primary capsule CTAs, active `2px` tab underlines, checked switches, filigree corners, and drawer borders; reserve Player Cyan (`#7EE8FF`) for map telemetry and directional player markers.
**The Honest Semantic Rule.** Never paint decorative chrome emerald, amber, or crimson; those three hues strictly report version up-to-date verification, live Engine power, UIPI risk, recording state, or validation outcome.

## Typography

**Display & UI Font:** System-safe Clean Sans (`'Be Vietnam Pro', 'Segoe UI', -apple-system, BlinkMacSystemFont, Roboto, Arial, sans-serif`) for instant zero-network MV3 startup and sharp Vietnamese diacritics.
**Telemetry & Keycap Font:** Tabular Monospace (`'JetBrains Mono', Consolas, monospace`) for Teyvat coordinates, version badges, 6-digit LAN PINs, URLs, and keycap bindings.

**Character:** Compact utility typography locked to lining tabular numerals (`font-variant-numeric: lining-nums tabular-nums`) so numeric coordinates, volume readouts, and client counters never jitter horizontally during live polling.

### Hierarchy
- **Display** (`600`, `15px`, `1.25`, `-0.015em`): Extension brand wordmark (`Genshin Helper`) paired with frameless `30×30px` brand emblem (`icons/icon128.png`).
- **Title** (`500`–`600`, `12px`, `1.25`): Navigation tabs, Control Sheet row titles, capsule action buttons, and borderless presence readouts.
- **Body** (`400`–`500`, `11px`–`12px`, `1.35`): Tool card descriptions, support copy, and toast feedback messages.
- **Label** (`500`–`600`, `11px`, `1.25`): Concise feature subtitles (`.t3-cell-desc`), HUD tray selects/toggles, switches, chips, compact `3×2` hotkey titles, and footer triggers.
- **Mono** (`500`–`700`, `11px`–`13px`, `1.25`): Header `#version-badge` (`22px` height), coordinates (`sync-coords`), hotkey keycaps (`recorder-value`), LAN PIN (`13px`, `+0.12em`), and LAN URLs.

### Named Rules
**The Compact Floor Rule.** Never drop any interactive label, caption, badge, or select option below `11px` anywhere in the popup; reserve `12px`+ for primary interactive row titles and buttons.
**The Tabular Stability Rule.** Keep `lining-nums tabular-nums` enabled globally so 2-second status polling updates never shift timestamps, coordinates, or volume readouts sideways.

## Layout

The extension popup fixes its root canvas at `520×580px` (`overflow: hidden`) over `icons/backdrop_nebula_dark.webp`, centered Rosette watermark, double inner gold border, and corner filigree brackets:
1. **Sticky Header (`44px`):** Frameless `30×30px` brand emblem (`icons/icon128.png`), `15px` title (no subtitle) + 3-state `22px` `#version-badge` pill (`vico-idle` emerald octagon, `vico-spin`, `vico-download`, `vico-ready`), and inline `VI | EN` pill toggle on the right.
2. **Session Band (above tabs):** Full-width surface with corner lattice watermark (`0.065`); left stack pairs borderless 4-point star status + live map/floor/coords telemetry with the 3-state Engine Power bar (`Bật` / `Nghỉ 0%` / `Tắt hẳn` hollow-to-filled diamond gems); right stack houses capsule CTAs (`Mở Map` + provider select pill over `Chuyển về Game` with dark circular `.pill-disc`).
3. **Flush Architectural Tab Bar (`32px`):** Equal 3-column grid (`minmax(0, 1fr)`) below the Session Band with `2px` gold bottom active underline (`::after`), hollow-diamond-to-solid-star icons, and ARIA APG keyboard arrow navigation.
4. **Flexible Viewport (`flex: 1; min-height: 0; overflow-y: auto;`):**
   - **Tab 1 (`Control`):** Two stacked full-width `.t3-sheet` double-bevel containers (`Sheet 1`: `Auto-Sync | Minimap 360°` pair row, `Mini-HUD & Thẻ Điểm` full row with compact horizontal `#hud-sub-group.t3-hud-tray` position select + `Kéo` capsule + `Thẻ ảnh` + capture-visibility toggle, and `Radar nổi Live | HoYoLAB gọn` pair row; `Sheet 2`: `.t3-audio-sheet` with `.t3-slider-wrap` concentric SVG volume thumb + `%` + `Thử` small capsule), leaving subtle Nebula & Rosette sky breathing room below.
   - **Tab 2 (`Tools`):** Balanced `2-column` double-bevel `.card` grid (`Route` | `Export/Import`) above full-width `LAN Companion QR` (`Beacon` / `Power Off` capsule glyph toggle) and `Launch Borderless`, zero scroll at rest.
   - **Tab 3 (`Hotkeys`):** Hybrid 2-sheet Teyvat Craft v4 layout (`Sheet 1`: `Sync & Mark` top full-width + `Quick Switch | Diagnostics` split row with solid keycap slots & compact mouse/key quick-pick chips; `Sheet 2`: balanced `2×4` horizontal `[Label | Keycap]` in-game navigation matrix) above the capsule `Save Hotkeys` CTA, zero scroll at rest and expanding smoothly inside `.tab-viewport` when Conflict Advisor cards appear.
5. **Sticky Footer (`34px`):** Persistent obsidian footer showing connected `Map Tabs` diamond counter on the left and capsule Quest-banner `Report Bug` + Mora-pouch `Support` triggers on the right.

### Named Rules
**The No-Scroll Deck Rule.** Tab 1, Tab 2, and Tab 3 (at rest without expanded Conflict Advisor cards) must operate comfortably within `520×580px` with zero vertical scrollbars in both Vietnamese and English locales.
**The Anchored Layer Rule.** Feedback banners and popovers render `position: absolute` above the `34px` sticky footer (`bottom: 42px`), framed with `.t3-ornament-rule` gold dividers, never pushing grid geometry or shifting underlying buttons.

## Elevation & Depth

Surface hierarchy relies primarily on **obsidian tonal steps, antique-gold perimeter borders, and `inset` double hairline bevels** (`box-shadow: inset 0 0 0 3px rgba(26, 30, 40, 0.48), inset 0 0 0 4px var(--obs-border-inner)`), reserving dark ambient drop shadows (`0 14px 32px rgba(8, 10, 16, 0.56)`) strictly for floating popovers above the footer.

## Shapes & Iconography

- **Rounded Hierarchy:** `12px` shell corners, `10px` sheets/cards/drawers, `8px` nested selects/wells, `6px` `.t3-cell-ico` icon boxes & power tray, `4px` version badge & power segments, `999px` capsule `.pill-btn` buttons, switches, language toggle, and footer triggers.
- **Centered Solid-Glyph Icon Grammar (`viewBox="0 0 24 24"`):** Every icon is bilaterally centered around `(12, 12)` (`16×16` SVG inside `26×26px` `.t3-cell-ico` = `4px` margin; `12×12` SVG inside `18px`/`20px` circular `.pill-disc`; `42×22px` & `34×18px` concentric switches around `(8, 8)`/`(6, 6)`; `16×16px` concentric SVG volume thumb), guaranteeing zero DPI off-center drift.

## Do's and Don'ts

### Do:
- **Do** preserve every DOM element ID verified by `tests/verify_i18n_and_ext_v5.py` and keep `i18n` labels wrapped inside `<span class="btn-label" data-i18n="...">` beside inline SVGs so runtime language switches never erase icons.
- **Do** validate every popup change across `VI` and `EN` strings at fixed `520×580px`, ensuring zero ellipsis clipping, zero scrollbars on Tab 1 & Tab 2, and `>= 11px` font sizing everywhere.
- **Do** keep every visual asset 100% bundled and MV3-CSP compliant (`backdrop_nebula_dark.webp`, inline SVGs, `icon128.png`, `momo_qr.jpg`, system font stack, zero `eval` or `innerHTML`).

### Don't:
- **Don't** alter HoYoLAB map tile assets, official HoYoLAB pin sprites, or CV detection ROIs.
- **Don't** use `transition: all`, uncentered `::after` switch thumbs, or decorative emojis (`🚀`, `⚡`, `💤`, `✨`, `☕`) in popup chrome or toast messages.
- **Don't** inject hard-coded local filesystem paths or unapproved MV3 host permissions anywhere inside `extension/` or design documentation.
