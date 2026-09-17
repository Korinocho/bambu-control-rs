# Bambu Control: GUI polish guidelines

Baseline: branch `printer-files`, commit `d9a75ff`, eframe/egui 0.35.0. Every `file:line` below points at that commit and was checked against the source, not against a running build. When a number comes from egui's layout rules and was not measured, it is marked "≈" and names the stage test that must measure it. If HEAD has moved, re-anchor the line numbers before acting on them.

---

## 0. Scope, ground rules and the rebuild

### 0.1 The instruction

The owner, verbatim: **"la idea es que el diseño general se conserve, pero que lo pula"**.

This is a polish pass, not a redesign. The research inputs were gathered as if the app were being rebuilt, and this document overrides that. Some inputs recommended redesign moves: Windows 11 4/8 px radii, sentence case, an icon font, Semibold, a resizable side panel, `egui_extras` tables, stacked breakpoints, undo in place of confirmation. Each of those is either rejected or listed in section 7 for the owner to decide.

### 0.2 Must be preserved

- **The screens.** The printer panel. The files view, where the player takes over the grid area. The 11 `Dialog` variants (`dialogs.rs:16-29`). The close and connection-edit confirmations (`main.rs:1114-1150`).
- **What lives where, and how each feature is reached:**
  - Top bar: printer chips plus Edit and Add (Remove moved into the Edit dialog with decision O13).
  - Panel: camera, job card, control cards, AMS.
  - The FILES card opens the files view, which has the Timelapses, Recordings and Print files tabs.
  - The detail pane holds Play, Download & play, Save to PC, Show in folder, Open in player, Load preview and Read header.
  - The transfer bar and the cache line sit at the bottom of the files view. The refusal card stays as it is.
- **Navigation.** A chip selects a printer. The FILES card opens the view, "‹ Back" leaves it, and "‹ Back to the list" leaves the player. The view follows the selected chip.
- **The dark look.** The `src/theme.rs` palette (except the values marked "changed" in 1.2), Segoe UI with Bold, radius-14 cards, ALL-CAPS section labels, the green accent and the glyph icons.
- **Behaviour.** What every control does. The wording of design doc 5.10. The refusal card with no trust action. The confirmations. The prefetch and texture rules.

### 0.3 What is being fixed

- Spacing, alignment and type that don't agree with each other.
- Colours used ad hoc instead of by role.
- Missing loading, empty, error, disabled and offline states.
- Contrast failures.
- Text that overflows or truncates badly.
- Layout that breaks on resize or at the minimum window size.
- Per-frame work and UI-thread I/O.
- Id collisions and state bleed.
- Anything that jumps, flickers, or loses focus, scroll or selection.

### 0.4 Forbidden without the owner's yes (put it in section 7)

- Moving a feature to another screen or region.
- Changing navigation.
- Adding or removing a screen, dialog or control.
- Changing what a control does.
- New interaction paradigms: shortcuts, resizable panes, menus, undo, new drag interactions.
- Changing wording that tests assert or that design doc 5.10 specifies, except wording listed as a defect in section 5.
- New crates.

Test for "structural": if a user who knows today's app would have to hunt for something, or would be surprised by what a click does, it is structural.

### 0.5 Code that is out of bounds

| Area | Rule |
|---|---|
| `src/tls.rs`, `src/tls/**`, `src/ftp.rs`, `vendor/suppaftp/**` | FTPS/TLS layer. No changes. |
| `src/browser.rs` worker side (`FtpWorker`, lanes, `Cmd`, `Event`, what `apply` does) and `src/cache.rs` | Transfer lane and cache. No changes. One exception: a read-only `BrowserState::revision()` counter, bumped where `apply` already changes a listing. |
| `src/mqtt.rs`, `src/camera.rs`, `src/player.rs`, `src/avi.rs`, `src/instance.rs`, `src/config.rs` | No changes to control flow, timing or security. One exception: a pure function `ProbeOutcome::text()` whose result replaces `{why:?}` at `mqtt.rs:517-518`. |
| Off-thread work this pass adds (player open, cache walk, Clear cache) | Spawned from `src/main.rs`, calling the existing public functions unchanged. |
| `tests/source_rules.rs` | Existing tests stay. New scans are added. |

### 0.6 Rebuild from HEAD before any visual review

Part of the "buggy" impression came from a release binary built two days before `3b3922b` (stage 3: transfer lane, disk cache, player). That binary had no "Save to PC". A visual finding only counts if it was seen on a binary built from the commit under review.

1. `git status --short` prints nothing. Record `git log -1 --oneline` in the review.
2. Close every running Bambu Control. The app holds a single-instance mutex (`main.rs:1407-1414`), so a second launch shows "already running" and exits. That can look like the new build started when it didn't.
3. Run `cargo build --release`, then `target\release\bambu-control.exe`. The exe's modified time must be later than `git log -1 --format=%ci`.
4. Smoke check: open Files and select a timelapse that has a video. The detail pane must show "Save to PC" (`files_view.rs:1986`, `:2004`).
5. Screenshot matrix, before and after every stage. Each item is shot at 1080×780 (default, `main.rs:1417`), 700×480 (minimum, `main.rs:1418`) and 1920×1080, at zoom 1.0 and 1.5 (Ctrl +):
   - panel idle, printing, and with one printer offline;
   - each files tab;
   - a 3mf selected;
   - the player;
   - two transfers running and one failed;
   - the Skip objects, Movement, Device info and Maintenance dialogs.

   Debug builds can jump straight to these screens: `BAMBU_CONTROL_OPEN_FILES="0:files"` and `BAMBU_CONTROL_PLAY` (`main.rs:352-358`, `:460-482`).
6. `cargo test` and `cargo clippy --all-targets` pass before and after.

### 0.7 Keep the design doc true

Some changes alter a behaviour that `docs/printer-files-design.md` §6 records as built. Examples: "The virtualised list reserves what its rows really paint" (line 1242) and "`TEXT_DIM` 11-12 captions" (line 1147). Update that note in the same commit.

---

## 1. Design tokens

### 1.1 How tokens live

- All tokens are declared in `src/theme.rs`: `pub const` for colours, sizes, radii and margins, and `pub fn` for fonts. Fonts are functions because the bold family holds an `Arc<str>`.
- Existing colour names (`BG`, `CARD`, …) keep their names so call sites don't churn. New groups go in modules: `theme::font`, `theme::space`, `theme::pad`, `theme::radius`, `theme::stroke`, `theme::size`.
- **T1.** No literal colour, font size, radius, margin, spacing, stroke width or widget size appears outside `theme.rs`. Exceptions:
  - geometry private to one custom-painted widget, declared as a named module-level `const` in that file (jog label offsets, tool-icon strokes, drag-marker geometry);
  - multipliers computed in code (opacity 0.35, `gamma_multiply`);
  - tests.
- **T2.** T1 is enforced by the scans in 1.11.

### 1.2 Colour roles

Every ratio is the WCAG contrast ratio, computed from the hex values.

| Token | Value | Status | Role | Contrast |
|---|---|---|---|---|
| `BG` | `#0B0D0C` | kept | Canvas: top bar, CentralPanel, popup fill (a modal is `CARD` since decision O11) | TEXT 16.73, TEXT_DIM 7.28 |
| `CARD` | `#181C1A` | kept | Cards, tiles, list rows, chips, transfer rows | TEXT 14.77, TEXT_DIM 6.42 |
| `CARD_HOVER` | `#232826` | kept | Rest fill of buttons and inputs (`extreme_bg_color`, progress track); hover fill of clickable surfaces; job-thumbnail well; skeletons; jog inner ring | TEXT 12.84, TEXT_DIM 5.59 |
| `HOVER_FILL` | `#2B312D` | named (literal at `theme.rs:61-62`) | Hover fill of egui widgets; jog home disc (was `#2C322F`, `widgets.rs:121`) | TEXT 11.41, TEXT_DIM 4.96, DANGER 4.59 |
| `PRESSED_FILL` | `#1D2220` | **changed** (was `#14171A`, `theme.rs:64-65`: blue-tinted and darker than CARD) | Pressed and keyboard-focused fill | TEXT 13.84, TEXT_DIM 6.02, DANGER 5.57 |
| `BORDER` | `#454C48` | **lighter** (decision O11; was `#2B302D`) | 1 px rest outline of cards, tiles, rows, chips, modal, inputs | decorative (A3); 1.95 on CARD, 2.21 on BG |
| `TEXT` | `#ECEEED` | kept | Primary text; selected-tab text; focus ring; plate-map numbers | — |
| `TEXT_DIM` | `#98A09B` | kept | Captions, units, section labels, placeholders | ≥ 4.96 on every surface above. **Illegal on ACCENT_DARK (2.30).** |
| `ACCENT` | `#22B14C` | kept | Primary button fill, toggle on, progress fill, selection ring, Running state | CARD 6.12, HOVER_FILL 4.72. **Illegal on ACCENT_DARK (2.19).** |
| `ACCENT_DARK` | `#177033` | kept | Selection fill (tabs, filters, text selection); jog hover wedge at 45 % | TEXT on it 5.30; against BG 3.16 |
| `ACCENT_BRIGHT` | `#2CC95A` | named (`widgets.rs:224`) | Plate-map selected-object stroke | 8.18 against PLATE_BG |
| `ON_ACCENT` | `#06130A` | named (`dialogs.rs:71`, `widgets.rs:236`) | Text on an ACCENT fill | 6.75 |
| `WARN` | `#D98A00` | kept | Warning text, glyphs and strokes; Paused and Preparing states | WARN_BG 5.33, CARD 6.22, HOVER_FILL 4.80 |
| `WARN_BG` | `#33260B` | kept | Warning banner fill | — |
| `DANGER` | `#EF716C` | **changed** (was `#D64541`: 4.44 on BG, 3.92 on CARD, 3.67 on DANGER_BG) | Error text, glyphs, strokes; Failed state; destructive buttons | BG 6.73, CARD 5.94, CARD_HOVER 5.17, HOVER_FILL 4.59, PRESSED 5.57, DANGER_BG 5.56, WARN_BG 5.09 |
| `DANGER_BG` | `#3A1614` | kept | Error banner and card fill | — |
| `BLUE` | `#3F8CFF` | kept | Finished state | BG 5.95, CARD 5.25, CARD_HOVER 4.57. **Illegal on HOVER_FILL (4.05).** |
| `TRACK_OFF` | `#6B736E` | **changed** (was `#3A403D` at `widgets.rs:25`, 1.62:1) | Toggle track when off | CARD 3.53, CARD_HOVER 3.07; white knob on it 4.88 |
| `KNOB` | `#FFFFFF` | named (`widgets.rs:33`) | Toggle knob | — |
| `MEDIA_WELL` | `#000000` | named (`panel.rs:289`, `files_view.rs:628`, `:1522`) | Camera, player and tile picture wells | TEXT_DIM 7.84 |
| `PLATE_BG` | `#151816` | named (`widgets.rs:182`) | Plate-map background | — |
| `PLATE_GRID` | `rgba(0x20,0x24,0x22,0x60)` | named (`widgets.rs:198`) | Plate-map grid lines | decorative |
| `PLATE_OBJECT` | `#3A403D` | named (`widgets.rs:226`, `:229`) | Object fill at rest and on hover | TEXT on it 9.10 (numbers move from TEXT_DIM, which was 3.96) |
| `SWATCH_UNKNOWN` | `#444444` | named (`panel.rs:199`, `:202`) | AMS swatch with a missing or unparseable colour | — |

Other literals map onto existing tokens:

| Literal | Location | Becomes |
|---|---|---|
| Jog outer ring `#1B1F1D` | `widgets.rs:79` | `CARD` |
| Jog inner ring `#232826` | `widgets.rs:81` | `CARD_HOVER` |
| Plate hover stroke `#E8EBE9` | `widgets.rs:227` | `TEXT` |
| Chip ring `Color32::WHITE` | `main.rs:743`, `:849` | `KNOB` value, named `CHIP_SELECTED` |

**Legal-pairs rule.** A text colour may only be painted on a fill if the pair is listed above at ≥ 4.5 (≥ 3.0 for `font::metric`). Status colours never appear on disabled controls (A3).

### 1.3 Type

| Token | Size | Family | Use | Replaces |
|---|---|---|---|---|
| `font::caption()` | 12 | Segoe UI | Captions, metadata, units, notes, status line, tile lines, fact values, transfer and cache lines, row date and size, hints | `.size(10.0)`, `(10.5)`, `(11.0)` ×27, `(11.5)` ×7, `(12.0)`; `FontId::proportional(11.0\|12.0)` |
| `font::label()` | 12 | Segoe UI Bold | ALL-CAPS section and card titles, fact labels, month headings, row kind badges, chip state word and percentages | `bold(10.5)`, `bold(11.0)`, `bold(11.5)`, `bold(12.0)` |
| `font::body()` | 14 | Segoe UI | `TextStyle::Body` (kept); list-row names; error and refusal body; empty-state text; AMS rows; HMS descriptions in the dialog; camera placeholder; jog axis letters | `.size(12.5)`, `.size(13.0)`, `FontId::proportional(13.0\|14.0)` |
| `font::body_strong()` | 14 | Bold | Chip name, job name, job state, detail and player titles, banner titles, confirm question, emphasised dialog lines | `bold(12.5)`, `bold(13.0)`, `bold(13.5)` (non-button), `bold(14.0)` |
| `font::button()` | 13.5 | Segoe UI | `TextStyle::Button` (kept) | — |
| `font::button_strong()` | 13.5 | Bold | Accent button labels; the current option in the Speed dialog | `bold(13.5)` at `dialogs.rs:72`, `:218` |
| `font::title()` | 16 | Bold | Dialog headers, files page title, card values, refusal title, "Add a printer to get started", card chevron | `bold(15.0)`, `bold(16.0)`, `bold(16.5)`, `.size(15.0)`, `.size(16.0)` |
| `font::metric()` | 24 | Bold | The one primary number on a card: temperature, job %, fan %, the temperature input | `bold(22.0)`, `bold(24.0)`, `bold(26.0)`, `FontId::proportional(22.0)` |
| `font::icon_large()` | 20 | Segoe UI | Jog-wheel home glyph | `FontId::proportional(20.0)` (`widgets.rs:143`) |

- `TextStyle` registration in `apply()`: Body 14 (kept), Button 13.5 (kept), **Small 11 → 12**, **Heading 17 → 16 bold** (unused today; aligned with `title`). Monospace is unchanged.
- That leaves six sizes: 12, 13.5, 14, 16, 20 and 24. At HEAD there are 23.
- The bold face stays Segoe UI Bold (`theme.rs:98`). Semibold is decision O10.
- Font helpers must not allocate. Hold the bold family in a `static BOLD: LazyLock<egui::FontFamily>` and clone it (an `Arc` clone). Today `theme.rs:81-83` allocates an `Arc<str>` on every call.
- Line heights come from the font (`ui.fonts_mut(|f| f.row_height(&font))`), never from a literal.

### 1.4 Spacing

| Token | px | Use | Replaces |
|---|---|---|---|
| `space::XS` | 4 | Label to value inside one control; tight stacks such as AMS slots and HMS lines (as a scoped `item_spacing.y`); fact row gap | `add_space(4.0)`; `item_spacing.y = 3.0` (`panel.rs:159`, `:438`) |
| `space::S` | 6 | Between parts of one block inside a card (title to facts, bar to buttons) | `add_space(6.0)` |
| `space::M` | 8 | Global `item_spacing` x and y (kept, `theme.rs:72`); gap between cards on both axes | `add_space(8.0)`. Delete `item_spacing.y = 6.0` (`panel.rs:419`) and `= 10.0` (`dialogs.rs:42`). |
| `space::L` | 12 | After a header; before a dialog action row | `add_space(10.0)` (except `panel.rs:304`, which becomes `space::M`), `add_space(12.0)` |
| `space::XL` | 16 | Indent step for folders and companions; gap before a card's buttons after prose | `add_space(14.0)`; `14.0 * depth` (`files_view.rs:1762`); `add_space(22.0)` (`:1739`) |
| `space::XXL` | 20 | Modal padding (kept) | — |

### 1.5 Paddings (`egui::Margin`)

| Token | Value | Used by | Replaces |
|---|---|---|---|
| `pad::PAGE` | 12 | CentralPanel frame | `main.rs:1302` (10) |
| `pad::BAR` | 12 × 8 | Top bar | `main.rs:1262` (10 × 8) |
| `pad::CARD` | 12 | `card_frame`; error and refusal cards; inset frames in the Fans and HMS dialogs | `panel.rs:96` (14 × 12); `files_view.rs:921` (12), `:950` (16); `dialogs.rs:257`, `:665` (10) |
| `pad::BANNER` | 8 | All banners; player-error card | `files_view.rs:905`; `panel.rs:435`, `:472`; `files_view.rs:721` (10) |
| `pad::ROW` | 12 × 4 | List rows, transfer rows | `files_view.rs:1689`, `:455` (10 × 4) |
| `pad::CHIP` | 12 × 6 | Chips, chip drag ghost, firmware toast | `main.rs:748`, `:851`; `dialogs.rs:541` (14 × 8) |
| `pad::TILE` | 6 | Tiles | kept (`files_view.rs:1516`) |
| `pad::MODAL` | 20 | Modal frame | kept (`dialogs.rs:39`) |
| `button_padding` | 16 × 8 | egui buttons | kept (`theme.rs:73`) |

### 1.6 Radii and strokes

| Token | Value | Applies to | Replaces |
|---|---|---|---|
| `radius::CARD` | 14 | Cards, tiles, modals (`window_corner_radius`, kept); error and refusal cards; camera well and image; player well and image | `panel.rs:297` (10); `files_view.rs:628` (12), `:634` (8) |
| `radius::CONTROL` | 10 | egui widgets (kept at `theme.rs:66-68`, now also set for `noninteractive` and `open`); chips; list and transfer rows; banners; player-error card; toast; dialog inset frames; plate-map background; row skeletons; tool buttons | `files_view.rs:720` (12), `dialogs.rs:539` (12), `files_view.rs:1479` (6) |
| `radius::MEDIA` | 8 | Picture wells and pictures inside a card or tile: tile well and image, job thumbnail well and image, detail-pane plate picture | `files_view.rs:1521` (10), `panel.rs:313` (10) |
| `radius::MARK` | 3 | Plate-map object boxes | `widgets.rs:232` |
| pill | height ÷ 2 | Toggle track, progress bars (egui default), status dot | `widgets.rs:27` (13) |
| `stroke::HAIRLINE` | 1.0 | Every rest outline and widget stroke | — |
| `stroke::SELECTED` | 2.0 | Selection ring on tiles, rows and chips. **Painted with the painter, never as a `Frame` stroke** (E10). | `files_view.rs:1512`, `main.rs:743` |
| `stroke::FOCUS` | 2.0 | Focus ring; `widgets.active.bg_stroke` | new role |
| `stroke::MEDIUM` | 1.5 | AMS swatch ring; chip drag ghost | `panel.rs:208`, `main.rs:849` |
| `stroke::HEAVY` | 3.0 | Drag insertion marker; jog dividers | `main.rs:838`, `widgets.rs:117` |

- The tool-icon strokes (1.2, 1.7, 2.4 at `main.rs:667-703`) stay as named consts inside the icon painter.
- A picture painted over a well uses the well's radius.

### 1.7 Control and layout sizes

| Token | Value | Notes / replaces |
|---|---|---|
| `size::CONTROL_H` | 32 | `interact_size.y`, kept (`theme.rs:74`) |
| `size::BUTTON_H` | 34 | What a default button paints (≈ 18 px text row + 2 × 8 padding). Replaces the min heights 28 (`files_view.rs:927`), 30 (`:964`, `:1973`, `:1998`) and 34 (`dialogs.rs:57`, `:63`). |
| `size::BUTTON_H_LARGE` | 40 | Speed options (`dialogs.rs:223`); Z jog buttons (`:327`) |
| `size::STEP_BUTTON_W` | 48 | Fan + and − (`dialogs.rs:278`, `:285`) |
| `size::Z_BUTTON_W` | 92 | `dialogs.rs:327` |
| `size::ICON_BUTTON` | 36 × 34 | Tool buttons (`main.rs:661`, was 36 × 30) |
| `size::TOGGLE` | 46 × 26, knob radius 11 | kept (`widgets.rs:16`, `:33`) |
| `size::PROGRESS_H` | 8 | kept (`panel.rs:376`, `files_view.rs:501`) |
| `size::TRANSFER_BAR_W` | 160 | `files_view.rs:500` |
| `size::FILTER_W` | 180 | `files_view.rs:883` |
| `size::SPEED_COMBO_W` | 72 | `files_view.rs:670` |
| `size::JOB_THUMB` | 88 | `panel.rs:311` |
| `size::SWATCH` | 20 (ring radius 9, hole radius 3) | `panel.rs:205-209` |
| `size::PLATE_MAP_MAX` | 360 | Painted at `min(360, available width)` (`widgets.rs:177`) |
| `size::JOG` | 240 | `R_OUTER`, `R_INNER` and `R_HOME` stay in `widgets.rs:43-45` |
| `size::TILE_W` | 168 | Outer width, **stroke included** (`files_view.rs:49`) |
| `size::TILE_IMAGE_H` | 94 | `files_view.rs:50` |
| `size::DETAIL_W` | 268 | `files_view.rs:58` |
| `size::LIST_MIN_W` | 240 | `files_view.rs:383` |
| `size::FACT_LABEL_W` | 76 | Fixed label column so fact values line up (`files_view.rs:2182-2188`) |
| `size::LEFT_COLUMN` | 0.58 of width | Clamped so the right column keeps `RIGHT_COLUMN_MIN` (`panel.rs:277`) |
| `size::RIGHT_COLUMN_MIN` | 280 | New clamp |
| `size::CAMERA_MAX_H` | 420 | `panel.rs:285` |
| `size::CHIP_NAME_MAX_W` | 160 | Truncation width for chip names |
| `size::REFUSAL_MAX_W` | 640 | `files_view.rs:952` |
| `size::SKELETON_MAX_W` | 420 | `files_view.rs:1478` |
| `size::INLINE_TARGET_H` | 24 | Minimum interact height of inline text buttons (A5) |
| `size::MODAL_S` | 320 | Confirm (`dialogs.rs:843`), Speed (`:209`) |
| `size::MODAL_M` | 380 | Add/Edit printer (`:93`), Temperature (`:168`), Fans (`:243`), Info (`:420`) |
| `size::MODAL_L` | 440 | Movement (`:305`), Skip (`:569`), HMS (`:652`), Maintenance (`:731`) |
| `size::SKIP_LIST_MAX_H` | 220 | `dialogs.rs:587` |
| `size::MODULES_MAX_H` | 150 | `dialogs.rs:492` |
| `size::HMS_LIST_MAX_H` | 320 | `dialogs.rs:660` |
| `size::TRANSFER_ROWS_VISIBLE` | 3 | Rows beyond three scroll (C13) |
| `size::WINDOW` | 1080 × 780 | kept (`main.rs:1417`) |
| `size::WINDOW_MIN` | 700 × 480 | kept (`main.rs:1418`); see O2 |

### 1.8 Derived heights

These are computed from the style every frame. They are never literals and never measured.

| Height | Formula | At zoom 1.0 | HEAD value |
|---|---|---|---|
| `row_h` | `CONTROL_H + 2·pad::ROW.y + 2·HAIRLINE` | 42 | reserves 34 on the first frame (`files_view.rs:54`) |
| `companion_h` | `CONTROL_H` | 32 | 26 |
| `note_h` | `row_height(caption)` | ≈ 16 | 22 |
| `heading_h` | `row_height(label)` | ≈ 16 | 24 |
| `tile_h` | `2·(pad::TILE + HAIRLINE) + TILE_IMAGE_H + 2·item_spacing.y + 2·row_height(caption)` | 156 | 156 (`TILE_H`) |
| `chip_h` | `CONTROL_H + 2·pad::CHIP.y + 2·HAIRLINE` | 46 | — |
| `footer_h` | `min(rows, TRANSFER_ROWS_VISIBLE)·(BUTTON_H + 2·pad::ROW.y + item_spacing.y)` + cache line `(BUTTON_H + item_spacing.y)` when the cache cap > 0, + `item_spacing.y` | — | literals 26, 34, 6 |
| `player_controls_h` | `BUTTON_H + 2·item_spacing.y` | 50 | `PLAYER_CONTROLS_H` 68 |

### 1.9 Timing

These are already named where they live. Leave them there.

| Constant | Value | Location |
|---|---|---|
| Cache usage refresh | 2 s | `main.rs:496` |
| Listing stale | 60 s | `main.rs:438` |
| Toast | 3.5 s | `dialogs.rs:372`, `:411`; name it `TOAST` |
| Light pending | 6 s | `main.rs:1334`; name it `LIGHT_PENDING` |
| Prefetch gate | 500 ms | `browser.rs:44` |
| egui `animation_time` | 0.2 s | egui default; kept |

### 1.10 `theme::apply()` wiring

| Field | Value |
|---|---|
| `panel_fill`, `window_fill` | `BG` (kept) |
| `extreme_bg_color` | `CARD_HOVER` (kept) |
| `faint_bg_color` | `CARD` (kept) |
| `widgets.noninteractive` | bg `CARD`, stroke 1 `BORDER`, fg `TEXT` (kept); **radius `radius::CONTROL`** (egui default 2) |
| `widgets.inactive` | bg and weak bg `CARD_HOVER`, stroke 1 `BORDER`, fg `TEXT`, radius `CONTROL` (kept) |
| `widgets.hovered` | bg and weak bg `HOVER_FILL`, stroke 1 `BORDER`, fg `TEXT`, radius `CONTROL` |
| `widgets.active` (pressed and keyboard focus) | bg and weak bg `PRESSED_FILL`; **bg_stroke 2 `TEXT`** (egui default is 1 px white); fg `TEXT`; radius `CONTROL` |
| `widgets.open` | **Same as `inactive`.** Never set today, so an open ComboBox uses egui's grey palette with radius 2 (egui `style.rs:1707-1713`). |
| `selection.bg_fill` | `ACCENT_DARK` (kept) |
| `selection.stroke` | **1 `TEXT`** (egui default `#C0DEFF`, 4.45:1 on ACCENT_DARK) |
| `text_cursor.stroke` | **2 `ACCENT`** (egui default light blue) |
| `warn_fg_color`, `error_fg_color`, `hyperlink_color` | `WARN`, `DANGER`, `BLUE` |
| `override_text_color` | `None` (kept; the comment at `theme.rs:49` stays) |

### 1.11 Enforcement scans

Add these to `tests/source_rules.rs`. They scan the production part of `src/ui/*.rs` and `src/main.rs` (text before the first `#[cfg(test)]`), skipping lines that start with `const`, `pub const` or `//`. Use `regex-lite`, which is already a dependency.

- **Colours.** `Color32::from_(rgb|rgba_unmultiplied|rgba_premultiplied|gray|black_alpha|white_alpha)\(` and `Color32::(BLACK|WHITE)\b`.
- **Fonts.** `\.size\(\s*[0-9]`, `FontId::(new|proportional|monospace)\(`, `bold\(\s*[0-9]`.
- **Radii and margins.** `CornerRadius::same\(\s*[0-9]`, `corner_radius\(\s*[0-9]`, `Margin::(same|symmetric)\(\s*[0-9]`, `inner_margin\(\s*[0-9]`.
- **Spacing and strokes.** `add_space\(\s*[0-9]`, `item_spacing(\.[xy])?\s*=`, `Stroke::new\(\s*[0-9]`.
- **Sizes.** `vec2\(\s*[0-9.]+\s*,\s*[0-9]`, `desired_(width|height)\(\s*[0-9]`, `\.width\(\s*[0-9]`, `max_height\(\s*[0-9]`.
- **Anti-patterns.** `\.interact\(\s*(egui::)?Sense::click` (E24); `cursor_icon\s*=` (E23); `push_id\(\s*(row|index|i)\s*,` (E2); `\{[a-z_.]*:\?\}` in production strings (E35).
- **Scroll areas.** Every `ScrollArea::(vertical|horizontal|both)\(\)` statement contains `.id_salt(` before its `.show` (E1).

---

## 2. Component rules

### C1. Cards

- One constructor, `theme::card_frame()`: fill `CARD`, 1 px `BORDER`, `radius::CARD`, `pad::CARD`. It is used by the job card, control cards, AMS card, detail pane, empty-state card, error card and refusal card. Tiles use it with `pad::TILE`.
- The gap between cards is `space::M` on both axes. `ui.columns` already uses `item_spacing.x = 8`. Delete the 6 px override at `panel.rs:419`.
- Never nest a filled card inside a card. Separate parts inside a card with `space::S` or `ui.separator()`.
- Never right-align by calling `ui.set_width(ui.available_width())` inside a card. Use `egui::Sides` (E9).

### C2. Clickable surfaces (cards, banners, chips, tiles, rows, companion lines)

- One helper, for example `widgets::clickable(ui, id_salt, frame, add) -> Response`. It is built on `ui.scope_builder(UiBuilder::new().id_salt(..).sense(Sense::click()))` plus `ui.response()` (egui `ui_builder.rs:168`, `ui.rs:943`). It replaces the eight `.response.interact(Sense::click())` sites.
- Fill follows `ui.style().interact(&response)`:
  - rest: `CARD` (banners keep their tone fill);
  - hover: `CARD_HOVER`;
  - pressed: `PRESSED_FILL`.

  The stroke width stays `HAIRLINE` in every state.
- **Selected.** A `stroke::SELECTED` ring, painted inside the rect. Colour `ACCENT` for tiles and rows, `CHIP_SELECTED` (white) for chips.
- **Focus.** A `stroke::FOCUS` ring in `TEXT`, painted outside the rect with a 1 px gap. Neither ring affects layout.
- Cursor via `response.on_hover_cursor(CursorIcon::PointingHand)`, and only when the click will act.
- **Disabled.** Wrap in `ui.add_enabled_ui(false, …)` and give the reason on hover (A11).
- Every clickable control card shows the "›" chevron. The NOZZLE and BED cards lack it today, which is the inconsistency in D24.

### C3. Banners and notices

- One helper, `widgets::banner(ui, tone, title, text, clickable)`. It covers:
  - the HMS and firmware banners;
  - "not tested on this model";
  - the damaged-card banners;
  - the orphan-timelapse notice;
  - the player-error card;
  - the settings error line;
  - open and reveal errors.
- Tones: Warn (`WARN` on `WARN_BG`), Danger (`DANGER` on `DANGER_BG`), Neutral (`TEXT_DIM` on `CARD`).
- Layout: `pad::BANNER`, `radius::CONTROL`, full available width, text wraps top-down.
  - With a title: `body_strong` title, `caption` lines.
  - Without a title: `caption`.
  - Existing glyphs are kept (⚠, ⬆).
- Every banner is created inside `ui.push_id("<banner name>", …)` (E3).
- Never show an error as a bare coloured `ui.label`, as `main.rs:1305` does.

### C4. Printer chip strip and tool buttons

- Lay out the tool buttons first, right-aligned. The chips then fill the remaining width, so the tool buttons are never clipped. O1 covers what happens when the chips themselves don't fit.
- Chip content order is unchanged: dot, name, state word, percentage, ↓ badge.
  - The name uses `body_strong`, truncated at `CHIP_NAME_MAX_W`. egui's elided tooltip shows the full name.
  - The state word and percentages use `label`.
  - The percentage sits in a slot sized for "100%", and the badge in a slot sized for "↓ 100%" while it is present.
- The state word and its colour come from the single vocabulary in C6. When MQTT is disconnected the chip shows **"Offline"**, the dot and word are `TEXT_DIM`, and there is no percentage.
- The selected chip gets its white ring from the painter, so its size doesn't change. Hover fill is `CARD_HOVER`. The drag fade, insertion marker and ghost are unchanged.
- Tool buttons are `size::ICON_BUTTON`. Edit is disabled with a reason when there is no printer. Tab order matches the visual order: Edit, Add. Removing a printer lives in the Edit dialog (decision O13), at the other end of its action row from Save, and still goes through the confirmation of 5.4.
- Never reorder, hide or merge chips. Never move the tool buttons (O1, O13).

### C5. Printer panel card grid

- The order stays as it is: HMS banner, firmware banner, DEVICE CONTROL, the NOZZLE/BED, SPEED/LIGHT and FANS/MOVEMENT rows, DEVICE INFO, MAINTENANCE, FILES, AMS.
- Every control card has the same two rows:
  - **Title row:** a `Sides` with the `label`-font title on the left and the chevron (or nothing) on the right.
  - **Value row:** height `CONTROL_H` for SPEED, LIGHT, FANS, MOVEMENT, DEVICE INFO, MAINTENANCE and FILES; a `metric` row for NOZZLE and BED.

  Because both cards in a `ui.columns(2)` row share the same row types, their heights match by construction.
- Values use `title` and stay on one line, truncated.
- Column split: `left = clamp(0.58 × width, LIST_MIN_W, width − RIGHT_COLUMN_MIN − space::M)`. Read `available_width()` once per decision.
- AMS card:
  - slot rows use `space::XS` spacing inside a `ui.scope`;
  - swatch is `size::SWATCH`;
  - slot text is `body` (the active slot is `body_strong` in `ACCENT`);
  - the remaining percentage sits in a right-aligned slot sized "100%";
  - humidity stays on the right of the title row.

### C6. Readouts (temperatures, percentages, times, counts)

- **Temperature.** The current value in `metric`, in a slot sized "888". Then "/ 220°C" in `caption` `TEXT_DIM`. A missing value is "—" everywhere (the temperature card uses "--" today).
- **Job progress.** Each piece has its own fixed slot, so digit changes never reflow the row:
  - "{pct}%" in `metric`, slot "100%";
  - "~Xh YYm left" in `caption`, slot "~99h 59m left";
  - "layer n/N" in `caption`, right-aligned, slot "layer 9999/9999".
- **State vocabulary.** One function, used by both the chip and the job card:

| `gcode_state` | Word | Colour |
|---|---|---|
| `RUNNING` | Running | `ACCENT` |
| `PAUSE` | Paused | `WARN` |
| `PREPARE` | Preparing | `WARN` |
| `SLICING` | Slicing | `WARN` |
| `FINISH` | Finished | `BLUE` |
| `FAILED` | Failed | `DANGER` |
| `IDLE` | Idle | `TEXT_DIM` |
| `""` | — | `TEXT_DIM` |
| anything else | Title case of the raw value | `TEXT_DIM` |
| MQTT disconnected (overrides all of the above) | Offline | `TEXT_DIM` |

- Offline: every readout on the panel is `TEXT_DIM`. Last-known values stay visible, but never in a state colour.
- Measured values snap to their new value. They never animate.

### C7. Progress bars

- `egui::ProgressBar` with height `PROGRESS_H`, fill `ACCENT` and the style's track. Each bar carries `.text()`, or its value painted beside it (A10).
- The job bar spans the card's width. The transfer bar is `TRANSFER_BAR_W` wide.
- A bar is never drawn for a job that doesn't exist (C8 idle state).

### C8. Camera well and job card

- **Camera well.** Fit the well to the image. Its size is the stream's aspect ratio (16:9 before the first frame) fitted inside column width × `CAMERA_MAX_H`, centred in the column. Well and image both use `radius::CARD`, so no black slabs appear at the sides.
- **Camera placeholder.** The status text goes into the well as a truncated `Label` via `ui.put`, in `body` `TEXT_DIM`. Painter text is not used.
- **Job card layout** stays as it is:
  - thumbnail (`JOB_THUMB`, `radius::MEDIA`);
  - name in `body_strong`, truncated;
  - state word, with the connection label on its right;
  - progress row, bar, then the button row (Skip objects, Pause/Resume, Stop).
- **State and connection row.** Use `Sides` so the two never overlap:
  - online: "online" in `ACCENT`;
  - offline: "offline: <`ProbeOutcome::text()`>" in `DANGER`, truncated.
- **Idle state.** Applies when there is no job name and the state is in `IDLE_STATES`:
  - the name reads "No print running" in `TEXT_DIM`;
  - the percentage is "—" in `TEXT_DIM`;
  - there is no ETA or layer count;
  - the three buttons are disabled, with the reason "No print running".

  The card keeps the same height as when a job is running.
- **Skip objects** is disabled with a reason:
  - "Loading the job's objects…" while the bundle is being fetched (bundle progress in `caption`, slot "100%", `panel.rs:392-397`);
  - "This job lists no objects" when the bundle has none.
- **Stop** has `DANGER` text and outline while enabled. When disabled it looks like any other disabled button.

### C9. Files view: header, tabs, status line, controls

- **Header row.**
  - Order: "‹ Back", then "{name} / FILES" in `title`, then the three `selectable_value` tabs.
  - The name part truncates first. The tabs are never pushed off the row.
  - Selected tabs use the `ACCENT_DARK` fill with a `TEXT` label (1.10).
- **Status line.** A `Sides` row:
  - left, shrinking and truncating first: the summary in `caption` `TEXT_DIM`, "(printer clock)" and Refresh;
  - right: "FTP session: …" in `caption`, coloured by state.

  While printing, the hint "printing: transfers share the printer's Wi-Fi" sits in `caption` on this same row, between Refresh and the session text, and truncates first. Today it is its own line, and the list jumps when it appears (D13).
- **Controls.**
  - Filter: `TextEdit` with `.id_salt(("files-filter", printer_key))`, width `FILTER_W`.
  - Sort: `ComboBox::from_id_salt(("files-sort", printer_key))`.
  - The kind filters are unchanged.
- Each conditional block between the header and the list gets its own `push_id` (E3): the not-tested banner, error card, player-error card, damaged-card banners and orphan notice.

### C10. Thumbnail grid (Timelapses)

- **Tile geometry.**
  - A tile is a card with `pad::TILE`. Its outer width is `TILE_W` (168), stroke included.
  - Content width is `TILE_W − 2·(pad::TILE + HAIRLINE)`.
  - Columns = `max(1, floor((list_w + gap) / (TILE_W + gap)))`.
- **Picture well.** Content width × `TILE_IMAGE_H`, fill `MEDIA_WELL`, `radius::MEDIA`.
  - The image goes through the one shared letterbox helper, at `radius::MEDIA`.
  - The placeholder ("queued", "loading…" or a short failure) is a truncated `Label` inside the well.
- **Text lines.** Both are single lines in `caption`, truncated; nothing in a tile wraps.
  - Line 1: the transfer note, "⚠ no video", "⟳ retry" or the date.
  - Line 2: the size.
- **Retry.** "⟳ retry" is a real widget: `Button::new(..).small().frame(false)` with `ACCENT` text.
  - Its interact rect is expanded to `INLINE_TARGET_H` (A5).
  - It lives inside the tile's clickable scope, which lets it take its own click.
- **States.** Selected is an `ACCENT` ring painted over the tile, so the size never changes. Hover is `CARD_HOVER`.
- **Month heading.** `label` in `TEXT_DIM`, height `heading_h`.
- **Row height.** A tile row is exactly `tile_h` (1.8).

### C11. Lists (Recordings, Print files, Other folders)

- **Row.** A clickable surface (C2): `CARD`, 1 px `BORDER`, `radius::CONTROL`, `pad::ROW`, height `row_h`.
- **Row content, left to right:**
  1. Kind badge in `label` `TEXT_DIM`, in a slot sized for the widest badge ("▾ DIR").
  2. Name in `body`, truncated.
  3. Right-aligned fixed slots (the transfer note, then size and date), so the columns line up across rows:
     - transfer note, truncated;
     - size, slot "999 MB";
     - date, slot "Sep 07 2026".
- **Indent.** Folder depth and companion lines add `space::XL` per level to the left of the frame. The frame's right edge stays aligned with every other row.
- **Companion line.** Built with the same clickable machinery at `companion_h`. "↳ printer's extracted copy: …" in `caption`, truncated, with the same selected and hover states as a row.
- **Note row.** One line of `caption` `TEXT_DIM`, truncated, height `note_h`. The full text is in the elided tooltip.
- **States.**
  - Selected: `ACCENT` ring plus `CARD_HOVER` fill.
  - Unreadable entries stay italic `TEXT_DIM`: not clickable, and no hover cursor.

### C12. Detail pane

- **Layout.** It stays `DETAIL_W` wide beside the list. The card's content goes inside `ScrollArea::vertical().id_salt(("files-detail", printer_key))`, so anything taller than the body scrolls within the pane. The salt is mandatory: the list and the detail pane share one stable id (D33). Pinning the actions instead is decision O3.
- **Content order** is unchanged: DETAILS, title (`body_strong`, may wrap), facts, clock note, preview or header pane, actions.
- **Facts.** Two columns per row:
  - label in `label` `TEXT_DIM`, `FACT_LABEL_W` wide;
  - value in `caption` `TEXT`, wrapping inside the value column;
  - rows `space::XS` apart.
- **Plate picture.** At most the pane's content width, `radius::MEDIA`.
- **Loading panes.** The 3mf details and the G-code header share one helper:
  - Loading: `caption` `TEXT_DIM` ("reading the 3mf…", "reading the header…");
  - Failed: `caption` `DANGER`, wrapping, followed by a "⟳ retry" button;
  - Ready: the content.
- **Actions.**
  - The primary accent button (Play, or Download & play) is full width at `BUTTON_H`.
  - "Save to PC" has one call site, placed after the local-copy branch.
  - The download-cost line stays above the buttons in `caption`.
- **During a transfer.** While the selected file is transferring, "Save to PC" and "Download & play" are disabled with the reason "Already downloading". The transfer note stays below them (`files_view.rs:2011-2013`).
- **Selection gone.** If the selected path is not in the listing, the title shows the name taken from the path, and the pane shows "Not in the latest listing." in `caption` `TEXT_DIM` in place of the facts.

### C13. Transfer bar and cache line

- **Each transfer row** is wrapped in `ui.push_id(transfer.id, …)`.
  - Glyph: ↓ in `ACCENT`, or ⚠ in `DANGER`.
  - Name in `body`, truncated.
  - Status line in `caption` (`TEXT_DIM`, or `DANGER` for failures).
  - Right side: the bar (`TRANSFER_BAR_W`), "⟳ retry" when failed, then ✕.
- **More than `TRANSFER_ROWS_VISIBLE` rows** go inside `ScrollArea::vertical().id_salt(("transfers", printer_key))`, capped at three rows' height.
- **Heights** come from `footer_h` (1.8). The list body gets `available − footer_h`.
- **Cache line.** `caption` "cache X / Y", then Clear cache. While clearing runs (off-thread, E30), the button is disabled and reads "Clearing…".

### C14. Video player

- **Title row** (`Sides`):
  - left: "‹ Back to the list", then the title in `body_strong`, truncated;
  - right: the facts in `caption`, in a slot.
- **Picture.** The well is fitted to the frame's aspect inside width × (available height − `player_controls_h` − `footer_h`). Fill `MEDIA_WELL`; `radius::CARD` for both well and image. "decoding…" and "Opening…" appear as a `Label` inside the well.
- **Controls row,** `player_controls_h` tall:
  - play/pause button, named by state (A9);
  - slider taking the remaining width (`slider_width` set inside a scope);
  - clock in `caption`, slot "000:00 / 000:00";
  - speed `ComboBox::from_id_salt(("player-speed", printer_key))`, `SPEED_COMBO_W`;
  - OS buttons on the right.
- **Opening runs off the UI thread** (E30). Until the player lands, the well reads "Opening…" and the controls are disabled.
- The cut-short and skipped-frame notices stay as `caption` lines above the picture.

### C15. Dialogs

- **One shell:** `modal(ctx, id, width, header, body, footer)`.
  - Frame: fill `CARD` (decision O11; it was `BG`, which matched the canvas), 1 px `BORDER`, `radius::CARD`, `pad::MODAL`.
  - Header: `title`, centred.
  - Body: inside `ScrollArea::vertical().id_salt(id)`, max height = screen height − 2·`pad::MODAL` − header − footer − 2·`space::L`.
  - Footer: the action row or Close, outside the scroll and always visible.
  - No `item_spacing` override.
- **Widths:** `MODAL_S`, `MODAL_M` or `MODAL_L` (1.7).
- **Initial focus,** set once when the dialog opens:
  - Add/Edit printer: the first empty field;
  - Temperature: the value field;
  - confirmations: "No";
  - refusal card: Close (kept).
- **Confirmation dialogs.**
  - The question in `body_strong`.
  - "No" first, holding focus.
  - Then `space::XL`, then the destructive button (`DANGER` text, 1 px `DANGER` outline).
  - Enter activates the focused button; Escape closes (egui `Modal`, kept).
- **Add/Edit printer.** The error line stays under the grid, in `caption` `DANGER`. Empty required fields get a 1 px `DANGER` outline.
- **Temperature.** "Set temperature" stays disabled, with the reason "Enter 0–{max} °C", while the value doesn't parse or is out of range.
- **Skip objects.**
  - The plate map is drawn at `min(PLATE_MAP_MAX, body width)`.
  - If the job bundle disappears while the dialog is open, the dialog stays and says "The print changed; there are no objects to skip." with Close. It never vanishes.
- **Device info toast.**
  - Its `Area` uses `Order::Tooltip` so it paints above the modal. Today it paints under it.
  - It is anchored below the header, not over it.
  - It is informational only (A12).
- **Speed.** The current level also uses `Button::selected(true)`, so it doesn't rely on colour alone.
- **Never:**
  - a dialog body that can't scroll;
  - a dialog that disappears without a message.

### C16. Buttons

- **Default.** `egui::Button` at the style's `BUTTON_H`. Width literals only come from `size` tokens.
- **Primary.** The accent button: `ACCENT` fill, `ON_ACCENT` text in `button_strong`. At most one per surface.
- **Destructive.** `DANGER` text, 1 px `DANGER` stroke, default fill. When disabled it renders as a plain disabled button, with no red.
- **Icon-only.** `ICON_BUTTON` size, glyph in `TEXT`, a tooltip and an accessible name (A9). Names follow state: "Pause"/"Play", "Cancel download"/"Dismiss failed download".
- **Disabled.** `add_enabled(false, …)` always paired with `.on_disabled_hover_text(reason)`.
- **Pending.** The label names the operation ("Checking…", "Clearing…", "Opening…") and the button is disabled. `dialogs.rs:455-461` is the reference pattern.
- **Never** an enabled-looking control whose click is ignored.

### C17. Empty, loading, error and offline states

- **Empty.** The tab's card with the 5.5 or 5.10 reason, in `body` `TEXT_DIM`. The wording does not change.
- **Loading.**
  - A `caption` line "listing /…".
  - Static skeletons in `CARD_HOVER`, drawn at the geometry of what will replace them: tile-sized skeletons at `tile_h` on Timelapses, `row_h` rows with `radius::CONTROL` elsewhere.
  - No spinner, no shimmer.
- **Error card.**
  - Card with `DANGER_BG` fill, 1 px `DANGER` stroke, `radius::CARD`, `pad::CARD`.
  - Body in `body` `DANGER`, which passes contrast with the new `DANGER`.
  - The actions of decision O14, in one row, the first as an accent button at `BUTTON_H`: Retry for a connection that dropped; "Edit printer" then Retry for a rejected access code; "Edit printer" alone for a bad address, a missing serial or a model refused by name, where no retry can work; Retry then "Clear cache" when the volume filled up.
  - `FtpError::text` wording unchanged.
- **Refusal card.** Behaviour and wording unchanged: no trust action, Close focused, Enter and Escape. Tokens only.
- **Offline printer.**
  - Chip shows "Offline" (C4).
  - Panel readouts turn `TEXT_DIM` (C6).
  - Job card connection label gives the reason (C8).
  - Camera well shows the camera status (C8).
- **No mixed states.** A region never shows an empty state and an error at the same time (kept; test `files_view.rs:3259`).

### C18. Custom-painted widgets

- **Toggle.**
  - `size::TOGGLE`; track `ACCENT` when on, `TRACK_OFF` when off; `KNOB`.
  - Focus ring; `widget_info` reports it as toggled.
  - "On"/"Off" text beside it (kept).
  - **Pending** (the light command was sent, telemetry doesn't agree yet): the knob sits at the requested side and the track is drawn at 50 % opacity.
  - When the 6 s timeout reverts it, the card shows the `caption` "Printer didn't confirm" until the next change.
- **Jog wheel.**
  - Colours: outer ring `CARD`, inner ring `CARD_HOVER`, home disc `HOVER_FILL`.
  - Dividers `BG` at `stroke::HEAVY`; hover wedge `ACCENT_DARK` at 45 %.
  - Axis letters in `body`, step labels in `caption`, home glyph in `icon_large` `ACCENT`.
  - Label offsets are named consts in `widgets.rs`. The cursor is set with `on_hover_cursor`.
- **Plate map.**
  - Background `PLATE_BG`, grid `PLATE_GRID`, objects `PLATE_OBJECT`.
  - Locked objects: `DANGER` at 35 % with a 1 px `DANGER` stroke.
  - Selected objects: `ACCENT` fill with a 2 px `ACCENT_BRIGHT` stroke.
  - Hovered objects: 2 px `TEXT` stroke.
  - Numbers in `caption` `TEXT`, or `ON_ACCENT` on selected objects.
  - The bed scale is computed once per bundle.

---

## 3. egui engineering rules

### Ids

- **E1.** Every `ScrollArea` gets an `id_salt` naming its content, plus the printer key and tab when the view is per printer. Without a salt the id is `ui.id.with("scroll_area")` (egui `scroll_area.rs:733-734`), and `ui.id` is built only from "child" salts (egui `ui.rs:250-254`). Same-shaped Uis therefore share scroll state.
- **E2.** Inside a loop, `push_id` uses a stable identity (remote path, transfer id, printer key, object id), never a row index.
- **E3.** A conditional block placed before stateful widgets goes inside `ui.push_id("<name>", …)`. A child Ui's auto ids include its parent's counter (egui `ui.rs:255-261`), so a block that appears or disappears renumbers every widget after it.
- **E4.** Stateful widgets get an explicit salt: `TextEdit`, `ComboBox`, `Slider`, collapsing headers. In per-printer views the salt includes the printer key.
- **E5.** Sibling child Uis that hold stateful containers get distinct salts. The list and detail Uis share one stable id today (`files_view.rs:406`, `:413`).
- **E6.** Leave egui's debug checks on. `warn_on_id_clash` and `warn_if_rect_changes_id` are already on in debug builds (egui `memory/mod.rs:333`, `style.rs:1398`). The debug screenshot matrix must show no id-clash warnings.

### Layout

- **E7.** Data text is never painted with `ui.painter().text`. Use `ui.put(rect, Label::new(..).truncate())`. The only exception is the static, short axis labels inside the jog wheel and the plate map.
- **E8.** Data labels in horizontal layouts use `.truncate()`, because egui extends text there rather than wrapping it (egui `ui.rs:588-600`). egui already shows the full text on hover for elided labels (`label.rs:42`, `:285`), so don't add a second `on_hover_text`. Wrapping is allowed only in top-down blocks with a bounded width.
- **E9.** For a row with content on both sides, use `egui::Sides` with `shrink_left`/`shrink_right` so the data side gives way. A `right_to_left` sub-layout can collide with the left side.
- **E10.** A state change never changes an element's size:
  - `Frame` stroke width is the same in every state, because stroke adds to a frame's size (egui `frame.rs:327-331`);
  - selection and focus rings are painted;
  - numbers sit in slots sized for their widest string.
- **E11.** Reserved heights (list body, footer, player controls, dialog body) come from the 1.8 formulas, not literals.
- **E12.** Width splits clamp both sides. `available_width()` is read once per layout decision.
- **E13.** Any region whose content can be taller than the window scrolls: detail pane, dialog bodies, transfer rows.
- **E14.** A virtualised view is a page and never sits inside another `ScrollArea` (kept, `main.rs:1307-1313`).

### Virtualised lists

- **E15.** Row heights come from the style (1.8). They are never measured and fed back. A row can't paint taller than its declared height because nothing in it wraps.
- **E16.** The row model (filtered, sorted, grouped rows and their order) is rebuilt only when its key changes: `(revision, tab, filter, sort, shown, columns, open_folders, damaged)`.
- **E17.** Rendering still reports each painted height, but only tests read it. They assert painted == declared on the first frame.

### Styling

- **E18.** Tokens only (T1, T2).
- **E19.** One helper each for cards (C1), clickable surfaces (C2), banners (C3), letterboxed images and loading panes (C12).
- **E20.** Interaction visuals come from `Style::interact` and `interact_selectable`, not from `if selected { A } else { B }`.
- **E21.** Local spacing changes happen only inside `ui.scope`, and only with tokens.
- **E22.** `override_text_color` stays `None`.
- **E23.** Cursors are set with `Response::on_hover_cursor` or `Context::set_cursor_icon`, not `output_mut(|o| o.cursor_icon = …)`.
- **E24.** A container is made clickable with `UiBuilder::sense`, not `response.interact(Sense::click())`.

### Frame cost

- **E25.** No deep clone of worker-shared state per frame. The one exception is a single clone of the *selected* printer's MQTT map for the panel. `PrinterUi::sync` reads the four fields it needs under the lock. Clone subtrees (AMS, trays, HMS) are never taken.
- **E26.** Nothing is `.cloned()` out of `BrowserState` maps in paint code when a borrow works. A `ColorImage` is never cloned per frame.
- **E27.** O(listing) work (rows, used-space sums, selection lookup) is keyed on `BrowserState::revision()`.
- **E28.** Every memo states its invalidation next to its field:
  - the cached-copy lookup and `shell_openable` are cleared for a path when a transfer of it finishes, and entirely on Clear cache and Back;
  - the cache usage memo keeps its 2 s rule.

### Background work and repaints

- **E29.** Font helpers don't allocate (1.3).
- **E30.** Nothing that can block runs inside `logic()` or `ui()`: `MjpegPlayer::open` (with `avi::index`), `Cache::usage_bytes`, `Cache::clear`, a per-frame `is_file()`. They run on a thread spawned from `main.rs`, and the result comes back through an `Arc<Mutex<Option<_>>>` slot drained in `sync`. `store.save` on a user action (one small atomic write per drop or dialog Save) is allowed.
- **E31.** A control that started background work shows its pending state until the result lands (C16).
- **E32.** `request_repaint_after` is only used for a known deadline, and the earliest one wins:
  - the next seconds boundary while a seconds counter is on screen ("connecting N s", "idle N s", "updated N s ago");
  - the earliest `VisibleSince` time + 500 ms while a tile waits for its prefetch gate. **Thumbnails depend on this repaint.**
  - toast expiry;
  - light-pending timeout.

  There is no unconditional minimum rate.
- **E33.** `request_repaint()` runs only while an interaction is in progress (kept for chip drag, `main.rs:862`). Workers wake the UI themselves (kept).

### Textures

- **E34.** Decode on workers; upload on the UI thread (kept).
- **E35.** A texture that is replaced over time uses `TextureHandle::set`, as the camera and player already do. The job plate texture (`main.rs:253-257`) must too.
- **E36.** `load_texture` in paint code only runs behind a hit check (kept: tile LRU at `files_view.rs:1631-1641`, plate preview stale check at `:2073-2080`).
- **E37.** Texture names never contain SD-card paths or serials. Hash them, as `main.rs:199-202` does.
- **E38.** `TEXTURE_CAP` and the `forget_thumb` behaviour are unchanged (design doc §6).

### Robustness, tests, dependencies

- **E39.** Paint code never panics on network data:
  - no byte-index slicing of printer strings (use `str::get` or parse per char);
  - no unchecked arithmetic on printer integers (use `u8::try_from`).

  The release profile aborts on panic (`Cargo.toml:85`).
- **E40.** Geometry assertions run in the existing headless harness (`files_view.rs:2300-2405`) at 1080×780 and 700×480. The panel gets the same kind of harness around `panel::show` with a hand-built `PanelView`.
- **E41.** No new dependencies. `egui_extras` and `egui_kittest` are decision O16.
- **E42.** Wording that tests assert, and the 5.10 wording, changes only through section 7 or a defect listed in section 5.
- **E43.** Each commit builds and passes `cargo test` and `cargo clippy --all-targets` with no new warnings.
- **E44.** No `{…:?}` Debug formatting in text a user sees.

---

## 4. Accessibility floor

- **A1. Text contrast.** At least 4.5:1 against the fill actually painted behind the text. At least 3:1 for text ≥ 24 px regular or ≥ 18.66 px bold, which only applies to `font::metric` here. Check with the legal pairs in 1.2.
- **A2. Non-text contrast.** At least 3:1 against adjacent colours for:
  - state indicators (selection ring, toggle track, checkbox mark, progress fill against its track);
  - focus rings;
  - the glyph of an icon-only button.
- **A3. Exempt from A2:**
  - the boundary of a control that has a visible text label;
  - decorative outlines (`BORDER`);
  - disabled controls. They still never keep a status colour.
- **A4. Text size.** Nothing smaller than 12 px at zoom 1.0 (`caption`, `label`).
- **A5. Targets.**
  - Default interactive height is 32 (`CONTROL_H`).
  - Icon buttons are 36 × 34.
  - Nothing has an interact rect under 24 × 24 (`INLINE_TARGET_H`; expand the interact rect without changing layout).
  - Targets are at least `space::M` apart.
- **A6. Focus visible.** Every focusable element shows a 2 px ring at ≥ 3:1 against the surface around it:
  - egui widgets through `widgets.active.bg_stroke`;
  - custom surfaces through a painted ring (C2).

  Clicking doesn't take focus in egui, so the ring appears for keyboard focus and while pressed.
- **A7. Never colour alone.** Each state colour comes with a word, glyph or shape:
  - chip dot + state word ("Offline" included);
  - job state word;
  - transfer ↓/⚠ + text;
  - selection ring (a shape);
  - toggle knob position + "On"/"Off";
  - speed option `selected`;
  - plate-map locked objects, paired with "(already skipped)" in the list.
- **A8. Keyboard.**
  - Tab, Shift+Tab, Enter, Space and Escape reach and operate every existing control (egui provides these).
  - Tab order matches visual order. For `right_to_left` blocks, add widgets in visual order or use `Sides`.
  - Dialogs set initial focus once (C15).
  - New shortcuts are decision O5.
- **A9. Names.** Every custom-painted interactive surface and every icon-only button calls `Response::widget_info` with the right `WidgetType` and a label matching the visible text or naming the action. Names follow state.
- **A10. Progress.** Every `ProgressBar` exposes its value via `.text()` or `.show_percentage()`, and the value is also visible as text.
- **A11. Disabled with a reason.** Every `add_enabled(false, …)` and `add_enabled_ui(false, …)` has `on_disabled_hover_text`. Pending controls rename themselves (C16).
- **A12. Errors.** An error says what failed and offers the next step as a control on the same surface, with no Debug text. Errors never expire; toasts are informational only.
- **A13. Motion.**
  - Nothing flashes more than 3 times a second.
  - No shimmer, and no animation on measured values.
  - `animation_time` stays at 0.2 s.
  - No spinner that can run forever (house rule, `files_view.rs:8`).
- **A14. Zoom.** At Ctrl+ zoom 1.5 on 1080×780, every action can be reached (scrolling allowed) and no text overlaps other text.
- **A15. Label association.** The add-printer fields call `.labelled_by(label.id)`.

---

## 5. Defect list

Ranked by user-visible impact. Every location is at `d9a75ff`. "≈" marks an estimate from egui layout rules that the named stage must measure.

| # | What the user sees | Where | Fixed by | Stage |
|---|---|---|---|---|
| D01 | Selecting a 3mf whose preview loads pushes its actions (Save to PC and the rest) below the window, and nothing scrolls. At 1080×780 the pane has ≈690 px of content before Save to PC, in a body of ≈530 px. The overflow also pushes the footer down. | Unbounded `allocate_ui_with_layout` (`files_view.rs:413-417`); `detail_pane` `1867-1954`; plate picture scaled to the pane width `2084-2088`; one fact per filament and one label per warning `2116-2132`; actions last `1935`, `1959-2013` | C12, E13, E1, E5 | 2 |
| D02 | Tall dialogs don't scroll and are cut off by the window. With the plate map and 6 or more objects, Skip objects is ≈788 px tall, taller than the default 780 px window. After "Skip selected", its confirm step (≈832 px) puts "Yes, skip" fully below the window edge. At 700×480, Device info (≈520 px) and Maintenance (≈530 px) are cut off as well. | `modal` has no scroll and sets spacing to 10 (`dialogs.rs:31-46`); Skip `569-645` (map 360 px `widgets.rs:177-179`, list `587`, checkboxes ≥ 32 px, egui `checkbox.rs:85`); Info `420-554`; Maintenance `731-836` | C15, E13 | 2 |
| D03 | Each running or failed transfer pushes "Clear cache" and the ✕ further below the window. Failed rows stay until dismissed, so the ✕ that would dismiss them can end up off-screen. | `footer_height` literals 26/34/6 (`files_view.rs:427-435`) against a painted ≈50 px transfer row and ≈42 px cache line; body `401`; failed rows kept `439-443`. The existing test only checks the cache line's centre (`3594-3625`). | C13, E11, E13 | 2 |
| D04 | List rows grow and never shrink. After one wrapped folder-failure note is shown, every note row (including "(empty)") keeps that height. Tall Recordings rows inflate Print files rows. The scroll range grows and content shifts a frame later. Reopening the view keeps the inflated heights. | `set_min_height(reserved)` means measured ≥ reserved (`files_view.rs:1454`), and the maximum is written back (`1383-1394`); notes wrap (`1373-1376`, text `1242-1243`); `Row::Item` is shared by two tabs (`1341-1353`); `close` keeps `row_h` (`293-297`); `ROW_H 34` against a 42 px row (`54`); `overflow` is read only by tests | C11, E15, E17 | 2 |
| D05 | Typing in the filter box loses focus and the cursor when anything conditional above it appears or disappears: the printing hint, error card, player-error card, not-tested banner or the app error line. | `TextEdit` without a salt (`files_view.rs:881-883`, egui `text_edit/builder.rs:503-504`); conditional blocks `333-344`, `360-364`; `main.rs:1304-1306` | E3, E4 | 3 |
| D06 | The three tabs, and every printer, share one scroll position. Scroll deep into Timelapses, open Recordings (the offset clamps), come back: you are near the top. The panel's scroll offset is also shared across printers. | `files_view.rs:1427`; `main.rs:1314` | E1 | 3 |
| D07 | Opening a timelapse or recording in the player freezes the window until the whole AVI has been walked. Nothing is shown in the meantime. | `player.rs:191` into `avi.rs:293`/`:368` (one seek per chunk), from `main.rs:167-185`, called inside `logic()` (`263-266`) and for Play (`585-586`) | E30, C14 | 5 |
| D08 | Clear cache freezes the window while it deletes files. The files view also walks the whole cache directory on the UI thread every 2 s and stats the selected file every frame, despite the comment saying the paint path never touches the disk. | `main.rs:607-610` into `cache.rs:374-381`; `main.rs:496-499` into `cache.rs:315-335`; `main.rs:515-519` into `cache.rs:268-272`; comment `files_view.rs:1860-1862` | E30, E28, C13 | 5 |
| D09 | With more printers than fit, the chips run under Edit, Add and Remove, which are clipped off the right edge: ≈2–3 printers at 700 px, ≈4–6 at 1080 px, depending on names and states. | Non-wrapping chip row (`main.rs:723`) laid out before the right-to-left buttons in the same row (`1264-1297`) | C4 (partial), O1 | 2 |
| D10 | An offline printer keeps showing its last telemetry as live: the chip still reads "Running 42%" in green, and temperatures stay. Only the job card's small label changes, to something like `offline (Tls("…"))`, which is a Rust Debug value. | State is never cleared on disconnect (`mqtt.rs:517-519`); chip reads only state (`main.rs:724-775`); `panel.rs:340-347`; `mqtt.rs:518` | C4, C6, C8, C17, E44 | 4 |
| D11 | Clicks that silently do nothing:<br>• MOVEMENT while printing (it still shows the pointing-hand cursor);<br>• Skip objects before the job's objects arrive, or when the job has none;<br>• Edit and Remove with no printers;<br>• Save to PC and Download & play while that file is already transferring;<br>• Set temperature with an invalid value. | MOVEMENT: `panel.rs:544-546`, `main.rs:1176-1181`<br>Skip objects: `panel.rs:382-391`, `main.rs:1189-1198`<br>Edit/Remove: `main.rs:1268-1273`, `1284-1295`<br>Save/Download: `files_view.rs:1986-1990`, `1996-2008`, `browser.rs:2738-2744`<br>Temperature: `dialogs.rs:189-201` | C8, C12, C15, C16, A11 | 4 |
| D12 | The Skip objects dialog vanishes without a word when a new job starts while it is open. | `mem::replace` (`main.rs:876`) followed by a bare `return` (`968-972`); a new job clears the bundle (`297-302`) | C15 | 3 |
| D13 | Things move without being touched: chips to the right shift when a percentage goes 9→10→100 or the ↓ badge appears; selecting a chip or tile shifts its neighbours 2 px; the list jumps a line when a print starts or ends. | `main.rs:764-786`; selected stroke 2 vs 1 (`742-746`, egui `frame.rs:327-331`); `files_view.rs:1512-1514`; hint `337-341` | E10, C4, C6, C9, C10 | 2 |
| D14 | Red text fails 4.5:1 everywhere it's used: 4.44 on BG, 3.92 on CARD, 3.41 on the Stop button, 3.03 hovered, 3.67 on DANGER_BG (HMS lines, error card, refusal title). | `theme.rs:15`; uses at `panel.rs:346`, `405`, `441`, `451`; `files_view.rs:466`, `474`, `925`, `953`, `2054`, `2153`; `dialogs.rs:117`, `405`, `669`, `852`; `main.rs:1305` | 1.2, A1 | 1 |
| D15 | Long data pushes the layout: a long printer name pushes the tabs off-screen; the status summary pushes "(printer clock)" and Refresh away and overlaps "FTP session"; a companion line runs under the detail pane; a long job name moves the left column when a print starts; the connection detail grows over the job state. | `files_view.rs:807-808`; `854-875`; `1738-1749`; `panel.rs:329-331`; `340-347` | E8, E9, C8, C9, C11 | 2 |
| D16 | The app exits if a printer reports a filament colour that isn't ASCII hex (a byte-index slice panics, and release aborts). An AMS unit id ≥ 190 overflows the unit-letter computation: it panics in debug and silently wraps in release. Rare trigger, total impact. | `panel.rs:196-200`; `240`; `Cargo.toml:85` | E39 | 3 |
| D17 | Cards, banners, chips, tiles and rows have no hover, pressed or keyboard-focus state; only the cursor changes. | `panel.rs:101-143`, `432-490`; `main.rs:740-802`; `files_view.rs:1510-1592`, `1684-1729` | C2, A6, E24 | 4 |
| D18 | With nothing printing, the job card shows "—", "0%" and an empty bar, which reads like a print stuck at zero. | `panel.rs:323-378` | C8 | 4 |
| D19 | Selected tabs and filters show egui's light-blue label (#C0DEFF, 4.45:1 on the green fill). An open combo box (Sort, speed, nozzle) switches to egui's grey palette with 2 px corners. The text cursor is light blue. | `theme.rs:71` sets only `bg_fill`; `open` and `text_cursor` are never set (`theme.rs:43-74`) | 1.10 | 1 |
| D20 | The "New firmware … available" toast in Device information is never visible. | `Area` at the default `Order::Middle` (`dialogs.rs:532`, egui `area.rs:143`) paints under the modal's `Order::Foreground` (egui `modal.rs:45`); it is also anchored over the title (`530-534`) | C15 | 4 |
| D21 | Clicking a `/cache` companion line selects it but nothing is highlighted, and it has no hover or cursor. When a refresh drops the selected file, the pane silently reverts to "Select a file…" while the dead path stays selected. | `files_view.rs:1734-1750`; `1806-1841`, `1876-1879` | C11, C12 | 3 |
| D22 | "couldn't open the file" and "couldn't show the file" appear as a bare red line (4.44:1) in the settings-error slot and stay until a later settings save succeeds. When the line appears it also shifts the view's ids (D05). | `main.rs:595-606`, `1304-1306`, `406-415` | C3, E3 | 3 |
| D23 | Disabled controls never say why (Skip, Pause, Stop, Skip selected, Start calibration). Confirmation dialogs focus nothing, so Enter does nothing, and "Stop print" and "Remove" sit right next to "No". | `panel.rs:388-409`; `dialogs.rs:635-640`, `760-765`, `840-862` | A11, A8, C15, C16 | 4 |
| D24 | The control-card grid is ragged:<br>• the six clickable-card titles (SPEED, FANS, MOVEMENT, DEVICE INFO, MAINTENANCE, FILES) sit in a 32 px row, while NOZZLE, BED and LIGHT titles are bare labels, so titles in one row start at different heights;<br>• SPEED and LIGHT end at different heights;<br>• vertical gaps are 6 px against an 8 px gutter. | `panel.rs:104-112` (`ui.horizontal` fills 32 px, egui `layout.rs:604-614`) vs `126-127`, `514-530`; `419` | C1, C5 | 2 |
| D25 | Nested folder rows don't indent: the left edge stays put, the right edge moves in 14 px per level, and the size and date columns stop lining up between levels. | `files_view.rs:1691-1695`, indent `1762` | C11 | 2 |
| D26 | Tiles are wider than their slot: 170 px (172 when selected) painted in a 168 px slot. Columns are counted from 168, so a full row overruns the list width by 2 px per tile. | `files_view.rs:1510-1520`, `1330-1332`, `387` | C10, E10 | 2 |
| D27 | On wide windows the camera sits in a full-column black bar: its height is capped at 420 px but the well takes the whole column width. | `panel.rs:285-298` | C8 | 2 |
| D28 | The same state reads "Running" on the chip and "RUNNING" on the job card (likewise Pause/PAUSE and Finish/FINISH). | `main.rs:757-763`; `panel.rs:333-339` | C6 | 4 |
| D29 | A failed tile's "⟳ retry" is a label hit-tested by rectangle: no hover, no distinct cursor, and no keyboard access. | `files_view.rs:1553-1591` | C10, A8 | 4 |
| D30 | While Timelapses loads, six list-shaped bars are shown, then replaced by a tile grid: a full relayout. | `files_view.rs:1296-1299`, `1465-1480` | C17 | 4 |
| D31 | The toggle's off track is 1.62:1 on its card. Plate-map numbers are 3.96:1 at 11 px. Pressed and focused buttons turn a blue-tinted fill darker than the card behind them. | `widgets.rs:25`; `235-242`; `theme.rs:64-65` | 1.2 | 1 |
| D32 | No visual system:<br>• 23 type sizes, text as small as 10 px (`panel.rs:396`), `TextStyle::Heading` unused;<br>• 8 corner radii and 10 inner-margin values;<br>• 7 `add_space` values plus local overrides of 3, 6 and 10;<br>• 7 modal widths;<br>• 14 colour literals outside `theme.rs`, including the accent-text colour written twice. | See the "Replaces" columns in 1.3–1.7 | T1, T2 | 1 |
| D33 | Latent id and state bleed:<br>• global ComboBox salts, so an open popup survives a printer switch;<br>• row ids keyed by row index, so they re-bind when rows reorder;<br>• transfer ✕ and ⟳ retry are positional;<br>• the list and detail Uis share one stable id, so an unsalted detail `ScrollArea` would share the list's scroll state. | `files_view.rs:884`, `669`; `dialogs.rs:800`, `809`; `files_view.rs:1449`; `481`, `492`; `406`, `413` | E1, E2, E4, E5 | 3 (the detail salt lands in stage 2) |
| D34 | The files view repaints at least 10×/s for as long as it is open. Device info repaints 5×/s while its invisible toast is up. | `main.rs:488`; `dialogs.rs:547` | E32 | 5 |
| D35 | Every frame deep-clones every printer's whole MQTT map (not just the selected one). The panel then clones it again, plus `device_info`, the AMS subtree, the tray lists and the HMS list. | `main.rs:269` in `sync` for all printers (`1235-1237`); `1325`, `1327`; `panel.rs:160-162`, `241-243`, `423-424` | E25 | 5 |
| D36 | While a 3mf with a plate picture is selected, a ≈1 MB decoded image is cloned every frame. | `files_view.rs:2047` (`browser.rs:86-96`, `2584-2588`); same shape at `2139` | E26 | 5 |
| D37 | Per-frame O(listing) work: rows rebuilt, filtered (two lowercase Strings per item), sorted and grouped; folder entries cloned; damaged directories sorted; every transfer cloned; used space summed; selection found by linear scan and cloned. | `files_view.rs:397`, `1053-1056`, `1066-1073`, `1132-1138`, `1159-1165`, `1167-1181`, `1220`, `1255`, `1284-1288`, `449-450`, `830-845`, `1806-1841` | E16, E27 | 5 |
| D38 | Smaller costs:<br>• `theme::bold` allocates on every call (≈50 sites, some per row);<br>• HMS lookups take a global lock per code per frame;<br>• textures are named by SD-card path;<br>• the job plate texture is re-created instead of `set`;<br>• the `shell_openable` memo is never invalidated. | `theme.rs:81-83`; `panel.rs:426`, `446`; `files_view.rs:1636`, `2076`; `main.rs:253-257`; `74-78`, `527-535` | E29, E37, E35, E28 | 5 |
| D39 | Accessibility semantics:<br>• icon buttons are announced by their glyph, and ✕ means cancel or dismiss depending on state;<br>• the tool buttons and custom clickables have no role or name;<br>• progress bars have no value;<br>• form fields aren't linked to their labels;<br>• Tab order runs backwards in right-to-left rows (Remove → Add → Edit). | `files_view.rs:481`, `646-650`, `732`; `main.rs:658-714`, `1266-1295`; `panel.rs:375-378`; `files_view.rs:499-503`; `dialogs.rs:97-111` | A8, A9, A10, A15 | 6 |
| D40 | The chamber-light switch flips at once and looks confirmed. If the printer doesn't follow within 6 s, it flips back silently. | `main.rs:1329-1340`; `panel.rs:519-528` | C18 | 4 |
| D41 | The camera well's status text ("camera retry: …", "camera paused") is painter text: not truncated, spills past the well on narrow windows, and isn't a widget. | `panel.rs:299-303`; `camera.rs:82`, `94-95`; `main.rs:1356-1358` | E7, C8 | 4 |

### 5.1 Audit claims that don't hold at HEAD (don't chase these)

- **"MAINTENANCE wraps in a half-width card."** The card is full width (`panel.rs:566`).
- **"The firmware toast jumps."** It is never visible at all (D20).
- **"The plate map registers a tooltip for every object."** It registers one, for the hovered object only (`widgets.rs:244-246`).
- **"`panel.rs:419` and `dialogs.rs:42` leak spacing to siblings."** Both overrides are scoped to their own column or modal. They are still inconsistent (D32).
- **"Turn on `warn_on_id_clash`."** It is already on in debug builds (egui `memory/mod.rs:333`).
- **"Truncated labels lack tooltips."** egui shows the full text for elided labels by default (`label.rs:42`, `285`).
- **"Every drag frame writes config.toml."** It writes once per drop (`main.rs:856-870`, `650`).
- **"Fact rows are 32 px tall."** `horizontal_top` doesn't fill the row height; only `ui.horizontal` does (egui `layout.rs:604-614`).

---

## 6. Polish plan

### 6.0 What must not change in any stage

- **Structure and behaviour:**
  - information architecture, navigation, what every control does;
  - confirmations, the refusal card's lack of a trust action, prefetch and texture rules, the single-instance guard.
- **Protected code:**
  - the FTPS/TLS layer: `src/tls*`, `src/ftp.rs`, `vendor/suppaftp`;
  - the transfer lane and cache: `FtpWorker`, lanes, `Cmd`/`Event`, `src/cache.rs`;
  - MQTT transport control flow.

  The exceptions in 0.5 still apply.
- **Wording:**
  - strings that tests assert;
  - the design doc 5.10 wording, except D10's Debug text.

Every stage below ships on its own:
- it runs the 0.6 rebuild and screenshot matrix;
- it lists its visible changes in the PR;
- it updates the design doc (0.7).

Effort figures are estimates.

### Stage 1: Tokens (≈1 day)

**Changes**
- Rewrite the token set in `theme.rs` (section 1) and the `apply()` wiring (1.10), with non-allocating font helpers.
- Replace every literal in `src/ui/*.rs` and `src/main.rs`, following the "Replaces" columns.
- Delete the spacing overrides at `panel.rs:419` and `dialogs.rs:42`. Move the ones at `panel.rs:159` and `:438` to `space::XS` inside their scopes.
- Add the 1.11 scans.
- Add a unit test in `theme.rs` that computes the WCAG ratio for each legal pair in 1.2.

**Defects:** D14, D19, D31, D32.

**Visible changes allowed** (list them in the PR):
- lighter red;
- captions at 12 px and one metric size, 24;
- card padding 12, card gaps 8 in both directions;
- modal item gap 8;
- unified radii;
- lighter toggle off track;
- white tab labels;
- in-palette open combo boxes;
- green-tinted pressed state with a 2 px focus outline;
- 34 px tool buttons;
- white plate-map numbers;
- page gutter 12.

No text changes, no layout-structure changes.

**Done when:**
- `cargo build --release`, `cargo test` and `cargo clippy --all-targets` pass.
- The new scans pass with no exceptions beyond named consts.
- The contrast unit test passes.
- Before/after screenshots differ only by the listed changes.
- Design doc §6 caption note is updated.

### Stage 2: Fit (≈2 days)

**Changes**
- Detail pane `ScrollArea` with its salt (C12); compact facts.
- Dialog shell with a scrolling body (C15).
- `footer_h` formula and the scrolling transfer rows (C13).
- Style-derived row heights with no write-back (E15, E17); truncated notes and companions.
- Tile geometry (C10); painted selection rings for tiles and chips (E10).
- Number slots (C4, C6, C14).
- Truncation and `Sides` in the header, status line, companion line, job name and connection label.
- Tool buttons laid out first; chip name width capped.
- Card title rows and value-row heights (C5).
- Folder indent outside the frame (C11).
- Fitted camera well (C8).
- Printing hint moved onto the status row (C9).

**Defects:** D01, D02, D03, D04, D09 (partial), D13, D15, D24, D25, D26, D27.

**Done when** the following pass in the headless harness:
- **Detail pane.** With a 3mf fixture that has a plate picture and 8 facts, at 1080×780, one mouse-wheel event over the pane brings "Save to PC" inside the window.
- **Footer.** With 4 failed transfers at 700×480, "Clear cache" and every visible "✕" are painted inside the window.
- **Row heights.** `overflow == 0` on the first frame, and row heights match the 1.8 formulas after a frame that rendered a long failed-folder note.
- **Stability.** Selecting a tile moves no other painted text.
- **Long names.** With a 60-character printer name at 700×480, all three tab labels are painted inside the window.
- **Panel.** A new panel harness at 700 px shows SPEED and LIGHT with equal rects, and card titles in a row at the same y.

And manually:
- 6 configured printers at 700×780 (TEST-NET addresses such as 192.0.2.x are fine) keep Edit and Add visible.
- Skip objects at 1080×780 with a job of 12 or more objects shows its buttons in both steps.
- Measure the "≈" heights in D01–D03 and record them in the PR.
- Design doc §6 "reserves what its rows really paint" note is updated.

### Stage 3: Identity and state (≈1 day)

**Changes**
- Salts, identity `push_id`s and conditional id scopes (E1–E5).
- Per-printer ComboBox salts.
- Skip dialog message when the bundle disappears (C15).
- Safe AMS parsing (E39).
- Companion-line selection and the "Not in the latest listing." state (C11, C12).
- Open and reveal errors moved to a files-view banner that clears on Back or on the next successful open; the settings error drawn as a banner (C3).

**Defects:** D05, D06, D12, D16, D21, D22, D33.

**Done when:**
- **Filter focus.** In the harness, type into the filter, then toggle `view.printing` and add an error card between frames: the `TextEdit` still has focus and the same cursor (`TextEdit::load_state`).
- **Scroll offsets.** Scroll Timelapses, switch tab and back: the offset is restored (`scroll_area::State::load`). Two serials keep independent offsets.
- **AMS parsing.** A panel harness with `tray_color` set to a non-ASCII string and unit id 255 renders without panicking.
- **Debug build.** The screenshot matrix shows no id-clash warnings.

### Stage 4: States (≈2 days)

**Changes**
- Clickable helper with hover, pressed, selected and focus rings (C2).
- Disabled-with-reason everywhere D11 and D23 list.
- Offline chip, dimmed readouts and `ProbeOutcome::text()` (C4, C6, C8).
- Job-card idle state (C8).
- State vocabulary (C6).
- Toast layer and anchor (C15).
- Tile retry as a widget; tile skeletons (C10, C17).
- Light pending state (C18).
- Camera placeholder as a `Label` (C8).
- Confirmation focus and spacing; temperature validation; add-printer field outlines (C15).

**Defects:** D10, D11, D17, D18, D20, D23, D28, D29, D30, D40, D41.

**Done when:**
- **MOVEMENT disabled.** In the panel harness with state RUNNING, clicking MOVEMENT returns no `OpenMove`, and hovering paints the reason.
- **Idle job card.** With no job, "No print running" is painted and "0%" is not.
- **State vocabulary.** A unit test covers every `gcode_state`, and `ProbeOutcome::text()` contains no Debug artefacts for any variant.
- **Tile retry.** On a failed tile, Tab reaches "⟳ retry" and Enter asks for the thumbnail again.
- **Screenshots** show:
  - hover and keyboard focus on a card, a chip, a tile and a row;
  - an offline printer;
  - the firmware toast below the Device info title.

### Stage 5: Frame cost (≈1.5 days)

**Changes**
- Deadline-based repaints, including the prefetch gate (E32).
- Field extraction in `sync`, a single panel clone, no subtree clones (E25).
- Borrowed detail and header states (E26).
- Revision-keyed rows, used-space sums and selection (E16, E27); memo invalidation (E28).
- Player open, cache walk and Clear cache run on threads spawned from `main.rs`, with "Opening…" and "Clearing…" pending states (E30, E31).
- Textures: `set` for the plate, hashed names (E35, E37); HMS lookup memo.

**Defects:** D07, D08, D34, D35, D36, D37, D38.

**Done when:**
- **Repaint delay.** An idle files view (listing present, no transfers) reports `FullOutput.viewport_output[ROOT].repaint_delay` of at least the time to the next seconds boundary.
- **Prefetch gate.** With a tile waiting on its gate, the delay is at most the time left on the gate, and the existing prefetch tests still pass.
- **Row memo.** A counter test shows `build_rows` doesn't run again on a frame with an unchanged key.
- **No blocking calls on the UI thread.** A source scan finds no `MjpegPlayer::open`, `Cache::clear`, `usage_bytes` or `std::fs::` in `main.rs` outside `std::thread::spawn` closures.
- **Manual checks:**
  - a long timelapse opens with the window responsive, showing "Opening…";
  - Clear cache with 1 GB or more keeps the window responsive;
  - an idle files view uses ≈0 % CPU.

### Stage 6: Accessibility semantics (≈1 day)

**Changes**
- `widget_info` on custom widgets; names on icon-only buttons (A9).
- Progress bar values (A10).
- `labelled_by` (A15).
- Tab order matching the visual order (A8).

**Defects:** D39.

**Done when:**
- **AccessKit.** With `ctx.enable_accesskit()` in the files-view and panel harnesses, no node with a Click action has an empty name or `Role::Unknown`, the toggle exposes `toggled`, and both progress bars expose a value.
- **Tab order.** Tab through the files header in the harness and the order matches the visual order.

---

## 7. Needs the owner's decision

These are not made during the polish pass. Each needs a yes or no from the owner.

| # | Defect | Why polish can't fix it | Structural change | Cost | What it buys |
|---|---|---|---|---|---|
| O1 | D09: more printers than fit in the top bar | Stage 2 keeps the tool buttons visible and truncates names, but chips past the width are still clipped, which leaves those printers unreachable. | (a) Wrap chips onto a second row; the drag-reorder slot logic (`main.rs:816-839`) assumes one row and must be rewritten. (b) A horizontal scroll strip for chips, a new interaction that competes with drag. (c) An overflow menu for hidden printers, which is a new control. | (a) ≈0.5 day, (b) ≈1 day, (c) ≈0.5 day | Every printer and tool button reachable at any count |
| O2 | 700×480 minimum against the content (D02; right column ≈280 px, list ≈400 px) | Stages 2 and 5 make everything scroll and truncate, but the layout stays cramped at the minimum. | Raise the minimum to about 960×640 (one line, but small windows are no longer possible), or add a stacked layout below a width breakpoint (a layout redesign). | 5 min / ≈1.5 days | Readable layout at every allowed window size |
| O3 | D01: Save to PC needs scrolling for a large 3mf | With C12, the actions scroll with the content, which is their current position. | Pin the action block to the bottom of the detail pane and let the facts scroll above it. | ≈3 h | Primary actions always visible |
| | **Landed, with the owner's correction:** the block sits **right under the facts** and is pinned to the foot only once they stop fitting. The first shape — pinned always — left the buttons floating ~220 px below the facts of a short selection, which the owner refused. The block's height is added up from the lines it declares (a caption line, and a button's line plus its padding and its own outline), so the body's scroll cap is known before anything is painted; no measurement is fed back. | | | | |
| O4 | Keyboard use of the files list and tile grid | egui drops focus when a virtualised row scrolls out (egui `memory/mod.rs:614-621`), so Tab alone can't walk a long list. | Arrow keys, Home, End and PageUp/PageDown move the selection, and the list follows it. | ≈1 day | Files view usable without a mouse |
| O5 | Pointer-only surfaces: chip reorder, jog wheel, player | Needs new shortcuts. | Ctrl+Shift+←/→ on a focused chip; arrow keys on the jog wheel; Space and ←/→ in the player. | ≈0.5 day | Keyboard parity (WCAG 2.1.1) |
| O6 | The live camera can't be paused (WCAG 2.2.2) | Needs a new control. | A Pause/Resume button on the camera well, plumbed through `PrinterUi`, that stops its repaints. | ≈0.5 day | Motion control; less CPU |
| O7 | Zoom resets on every launch | Needs a new setting. | Save the zoom level (and optionally a UI-scale control) in `config.toml`. | ≈3 h | A dashboard readable across a room |
| | **Landed:** the `[ui] zoom` table. The start applies it, a Ctrl+scroll change is picked up and written once, and a value outside 0.5–3.0 opens at 1.0. No UI-scale control was added: egui's own Ctrl+scroll and Ctrl+plus are the control. | | | | |
| O8 | Emoji and symbol glyphs as icons, plus the `FontTweak` baseline hack (`theme.rs:93-108`) | Changes the look of every icon. Segoe Fluent Icons ships only with Windows 11 (Windows 10 has Segoe MDL2 Assets with different coverage). Tests assert "⟳ retry", "✕" and "▾ DIR". | Register an icon font with a Windows 10 fallback; update the tests. | ≈1 day | Consistent, baseline-aligned icons |
| O9 | ALL-CAPS labels (DEVICE CONTROL, DETAILS, month headings, OTHER FOLDERS) | A look change. Tests assert "DETAILS", "OTHER FOLDERS" and "JUNE 2026", and the design doc mocks use caps. | Sentence case everywhere. | ≈2 h | Windows-native typography |
| O10 | Bold as the emphasis weight | A look change. | Use Segoe UI Semibold (`seguisb.ttf`) for the bold family. | ≈1 h | Lighter emphasis |
| O11 | A flat look: `BORDER` at 1.28:1 on CARD, and a modal fill that matches the canvas | Decorative, so no contrast rule requires it, and changing it changes every card. | Lighten `BORDER` (e.g. `#454C48`, 1.95:1) and give modals a `CARD` fill. | ≈1 h plus review | Cards and dialogs with defined edges |
| O12 | Radius 14/10 against Windows 11's 4/8 | Rejected by default: it changes the look. Listed for completeness. | Use 4 px in-page and 8 px for overlays. | ≈2 h | A native Windows 11 look |
| O13 | Remove printer sits next to Add; removal needs a confirmation | Moving it is moving a feature, and undo is a behaviour change. | Move Remove into the Edit printer dialog, or separate it from Add, and/or replace the confirmation with a 6 s undo. | ≈3 h / ≈0.5 day | Fewer accidental removals |
| O14 | The error card only offers Retry, which doesn't help for "access code rejected" | Adds a button. Design doc 5.10 already says "links to Edit printer". | An action list per error kind on `error_card`. | ≈2 h | An error you can act on |
| O15 | Stale telemetry while still connected (no update for 15 s or more) | Needs a last-update timestamp from `mqtt.rs`, which is transport code. | Timestamp each state update; dim readouts past 15 s and show "Last update Ns ago". | ≈0.5 day | Old numbers never look live |
| O16 | Hand-written virtualiser and no snapshot tests | Needs new crates and a supply-chain review. | Adopt `egui_extras::TableBuilder` (heterogeneous rows), and/or `egui_kittest` snapshot tests. | 1–2 days | Less custom code; visual regression tests |