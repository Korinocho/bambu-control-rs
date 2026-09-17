//! The files view (design doc 6): a full page in the `CentralPanel`, not a
//! `Modal`, because a modal would block the printer chips and a nested
//! `show_rows` inside the outer `ScrollArea` breaks virtualisation.
//!
//! It shows what `browser::BrowserState` holds for one printer: Timelapses,
//! Recordings and Print files, with the session state the G4 gate wants on
//! the header line. This stage has no download actions at all, so it shows
//! none: no dead buttons, no trust action, and never an endless spinner.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{NaiveDateTime, NaiveTime};
use egui::{Align, Color32, Modifiers, RichText, Sense, Stroke, Ui, vec2};

use crate::avi::AviIndex;
use crate::browser::{BrowserState, Cmd, ConnState, Dest, DetailState,
                     DirState, FileItem, FileKind, HeaderState,
                     RECORDINGS_DIR, ThreeMf, ThumbState, TransferPhase,
                     TransferUi, VisibleSince};
use crate::config;
use crate::ftp::{FtpError, RemoteEntry, ServerProfile};
use crate::player::{self, PlayerCmd};
use crate::theme::{self, font, pad, radius, size, space, stroke};
use crate::tls;
use crate::ui::dialogs::{accent_button_reason, accent_button_response};
use crate::ui::panel::card_frame;
use crate::ui::widgets;

/// Thumbnails are downscaled to this on the lane thread (design doc 12: A1
/// frames are 1536x1080, 6.6 MB as RGBA).
pub const THUMB_PX: u32 = 320;
/// Textures the view keeps; the least recently shown tiles are dropped and
/// fetched again if they come back (design doc 12).
const TEXTURE_CAP: usize = 150;
/// Directories whose used space the header line sums, in this order. They
/// do not contain one another; the whole card follows them as "total".
const SPACE_DIRS: [&str; 4] = ["timelapse", "ipcam", "cache", "model"];
/// How deep an "Other folder" may be opened, one level at a time (5.5).
const FOLDER_DEPTH: usize = 4;
/// A 3mf preview loads on its own only up to this (design doc 4). Anything
/// larger shows "preview: 6.9 MB · ~35 s [Load]" and waits for the click,
/// because it is a download and the user should decide to spend the time.
const AUTO_PREVIEW_MAX: u64 = 1024 * 1024;
/// The same while the printer is printing (design doc 4).
const AUTO_PREVIEW_PRINTING: u64 = 256 * 1024;

/// The heights the view reserves, derived from the style every frame and
/// never measured and fed back (docs/gui-polish-guidelines.md 1.8, E15).
/// Nothing in a row wraps, so no row can paint taller than its height.
struct Heights {
    /// a list row: a control, its padding and its outline
    row: f32,
    /// a `/cache` companion line under its job
    companion: f32,
    /// a one-line caption, and a month heading
    note: f32,
    heading: f32,
    /// a row of tiles: picture, two caption lines, padding and outline
    tile: f32,
}

impl Heights {
    fn of(ui: &Ui) -> Self {
        let control = ui.spacing().interact_size.y;
        let gap = ui.spacing().item_spacing.y;
        let caption = theme::row_height(ui, &font::caption());
        Self {
            row: control + pad::ROW.sum().y + 2.0 * stroke::HAIRLINE,
            companion: control,
            note: caption,
            heading: theme::row_height(ui, &font::label()),
            tile: pad::TILE.sum().y + 2.0 * stroke::HAIRLINE
                + size::TILE_IMAGE_H + 2.0 * gap + 2.0 * caption,
        }
    }

    /// The row kinds of `row_kind`, in its order.
    fn by_kind(&self) -> [f32; ROW_KINDS] {
        [self.heading, self.tile, self.row, self.companion, self.row,
         self.note, self.row]
    }
}

/// The player's controls row under the picture, with the spacing around
/// it: everything below it — the transfer bar and the cache line — is
/// reserved separately, so the picture never pushes them off the bottom.
fn player_controls_height(ui: &Ui) -> f32 {
    size::BUTTON_H + 2.0 * ui.spacing().item_spacing.y
}

/// The three tabs of section 6.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Timelapses,
    Recordings,
    Files,
}

impl Tab {
    /// The word `BAMBU_CONTROL_OPEN_FILES` uses for this tab (debug builds).
    /// Its only caller is gated on `debug_assertions`, so a release build has
    /// none: dead there on purpose, not by accident.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    pub fn from_word(word: &str) -> Option<Self> {
        match word.trim().to_ascii_lowercase().as_str() {
            "timelapses" => Some(Self::Timelapses),
            "recordings" => Some(Self::Recordings),
            "files" => Some(Self::Files),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    Newest,
    Name,
    Size,
}

impl Sort {
    fn label(self) -> &'static str {
        match self {
            Self::Newest => "Newest",
            Self::Name => "Name",
            Self::Size => "Size",
        }
    }
}

/// The kind filters of the Print files tab (section 6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Shown {
    #[default]
    All,
    Sent,
    Cache,
    BuiltIn,
    Folders,
}

impl Shown {
    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Sent => "Sent to printer",
            Self::Cache => "Print cache",
            Self::BuiltIn => "Built-in",
            Self::Folders => "Other folders",
        }
    }

    fn accepts(self, kind: FileKind) -> bool {
        match self {
            Self::All => true,
            Self::Sent => matches!(kind, FileKind::SentJob | FileKind::PlainGcode),
            Self::Cache =>
                matches!(kind, FileKind::CacheProject | FileKind::CacheGcode),
            Self::BuiltIn => kind == FileKind::BuiltIn,
            Self::Folders => false,
        }
    }
}

/// What the view asks the app to do. Everything else it needs it asks the
/// worker for directly, as `Outcome::cmds`.
///
/// The player and the OS both live in `main.rs`: the view says what should
/// happen and never opens a file or a window itself.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// leave the files view
    Back,
    Refresh,
    /// the Retry button of an error card
    Retry,
    /// the refusal card's "Edit printer"
    EditPrinter,
    /// open this local file in the app's MJPEG player, at the speed its
    /// kind starts on: 10x for an `/ipcam` recording (section 6, section 7)
    Play { path: PathBuf, speed: f32 },
    /// leave the player, back to the grid
    ClosePlayer,
    /// a player control: play, pause, seek or speed (5.8)
    Player(PlayerCmd),
    /// "Open in player": the OS player, always offered and the fallback
    /// when the header sniff is not MJPEG (section 7, option B)
    OpenExternally(PathBuf),
    /// "Show in folder" (section 7, option B)
    Reveal(PathBuf),
    /// Clear cache; it skips files the player has open (5.6)
    ClearCache,
}

#[derive(Default)]
pub struct Outcome {
    pub actions: Vec<Action>,
    pub cmds: Vec<Cmd>,
}

/// What the view needs about the printer itself.
pub struct View<'a> {
    pub name: &'a str,
    pub serial: &'a str,
    /// from the 220 banner; anything but BBL-P003 is "not tested" (5.2)
    pub profile: Option<ServerProfile>,
    pub open_sessions: usize,
    /// MQTT gcode_state RUNNING or PAUSE
    pub printing: bool,
    /// a dialog is open over the view (the Edit printer one the refusal
    /// card opens, for example): it owns the keyboard, so the card neither
    /// takes the focus back nor answers Enter and Esc (section 6)
    pub dialog_open: bool,
    pub now: Instant,
    /// the disk cache's usage and cap, for the footer (5.6, section 6)
    pub cache_usage: u64,
    pub cache_cap: u64,
    /// Clear cache is running on its thread (E31): the button says so and
    /// takes no second click.
    pub clearing: bool,
    /// a file is being opened in the player on its thread, so the play
    /// buttons say "Opening…" until it lands (E31, D07)
    pub opening: bool,
    /// a complete, key-matching copy of a remote file already in the disk
    /// cache (5.6). Without it a file that is on disk from an earlier
    /// session is offered as "Download ~6 min"; `None` means the view has
    /// no cache to ask, which is what the tests use.
    pub cached: Option<CachedCopy<'a>>,
    /// whether a local file may be handed to the Windows shell (F2).
    /// `None` offers no "Open in player" at all, which is the closed side
    /// of the rule and what a test that does not care about it uses.
    pub shell_openable: Option<ShellOpenable<'a>>,
    /// the open player, which takes over the grid area (section 6)
    pub player: Option<PlayerView<'a>>,
    /// why the player could not open this file: "can't play this format in
    /// the app" or "empty recording", with the OS player next to it (5.10)
    pub player_error: Option<&'a str>,
    /// the local file that error is about, so the fallback can still offer
    /// "Open in player" and "Show in folder" for it (section 7)
    pub player_error_path: Option<&'a Path>,
    /// why the last "Open in player" or "Show in folder" failed; `main.rs`
    /// clears it on Back and on the next one that works (C3)
    pub open_error: Option<&'a str>,
}

/// Where a remote file's complete copy is in the disk cache, if it is there
/// at all (5.6). The view holds a lookup rather than the cache itself, so
/// it stays testable without one.
pub type CachedCopy<'a> = &'a dyn Fn(&RemoteEntry) -> Option<PathBuf>;

/// Whether a local file may be handed to the Windows shell: its header and
/// its extension must agree that it is media (`player::openable_by_shell`,
/// stage 3 security review, F2). Like [`CachedCopy`], the view asks rather
/// than looks, so it neither reads the disk while painting nor needs real
/// files to be tested.
pub type ShellOpenable<'a> = &'a dyn Fn(&Path) -> bool;

/// The open player, as the view draws it (section 6, 5.8). `main.rs` owns
/// the `MjpegPlayer` and its texture; this is the frame's worth of it.
pub struct PlayerView<'a> {
    pub title: &'a str,
    pub texture: Option<&'a egui::TextureHandle>,
    pub index: &'a AviIndex,
    /// the frame it is on
    pub pos: u32,
    pub playing: bool,
    pub speed: f32,
    pub path: &'a Path,
    /// frames that did not decode and were skipped (5.8)
    pub skipped: u32,
}

struct Texture {
    handle: egui::TextureHandle,
    /// the frame this tile was last shown in, for the LRU
    used: u64,
}

/// The view's own state for one printer: what is selected and filtered, its
/// texture LRU and how long each tile has been visible.
#[derive(Default)]
pub struct FilesUi {
    pub tab: Tab,
    pub sort: Sort,
    pub shown: Shown,
    pub filter: String,
    /// path of the selected entry (a timelapse is keyed by its video, else
    /// by its thumbnail)
    pub selected: Option<String>,
    /// "Other folders" the user opened, one level at a time (5.5)
    open_folders: HashSet<String>,
    textures: HashMap<String, Texture>,
    /// the plate picture of the 3mf in the detail pane, decoded once and
    /// kept only while that file stays selected
    preview: Option<(String, egui::TextureHandle)>,
    visible: VisibleSince,
    frame: u64,
    /// the refusal card's Close button holds the focus (section 6)
    close_focused: bool,
    /// the largest difference, in either direction, between what a row of
    /// the last frame painted and the height reserved for it. Only the
    /// tests read it (E17): a row is never resized from what it painted.
    row_mismatch: f32,
    /// The row model and the key it was built for (E16). The key carries
    /// everything `build_rows` reads: the state's revision, the tab, the
    /// filter, the sort, the kind filter, the column count, the folders
    /// the user opened and whether a damaged card is up. It is invalidated
    /// by the key alone — nothing clears it by hand — so a listing that
    /// lands, a letter typed or a window resized rebuilds it, and an
    /// unchanged frame does not (E28).
    rows: Option<(RowKey, Rows)>,
    /// How many times the model was built, for the counter test of E27.
    builds: u64,
    /// What the detail pane shows, and the (revision, selected path) it
    /// was found for. Finding it scans every listing and clones the entry,
    /// so it is found once per key, not once per frame (E27, D37).
    chosen: Option<((u64, Option<String>), Selected)>,
    /// The size parts of the status line, and the revision they were
    /// summed for: they add up every listing (E27, D37). The "updated N s
    /// ago" part is not in here, because it changes every second.
    sizes: Option<(u64, Vec<String>)>,
}

/// What the row model depends on (E16).
#[derive(PartialEq)]
struct RowKey {
    revision: u64,
    tab: Tab,
    filter: String,
    sort: Sort,
    shown: Shown,
    columns: usize,
    /// the opened folders, order-independent: a set has no order
    folders: u64,
    damaged: bool,
}

const ROW_KINDS: usize = 7;

impl FilesUi {
    /// Leaving the view drops its textures (design doc 12).
    pub fn close(&mut self) {
        self.textures.clear();
        self.preview = None;
        self.close_focused = false;
    }

    /// How many times the row model was built. Only the E27 test reads it.
    #[cfg_attr(not(test), allow(dead_code,
        reason = "read by the stage 5 frame-cost test, not by the app"))]
    pub fn row_builds(&self) -> u64 {
        self.builds
    }

    /// Whether the refusal card's default button has the focus; the tests of
    /// section 6 assert it.
    #[cfg_attr(not(test), allow(dead_code,
        reason = "read by the section 6 tests, not by the app"))]
    pub fn close_has_focus(&self) -> bool {
        self.close_focused
    }
}

/// Renders the view and returns what the user asked for.
pub fn show(ui: &mut Ui, state: &mut BrowserState, files: &mut FilesUi,
            view: &View<'_>) -> Outcome {
    let mut out = Outcome::default();
    files.frame += 1;
    header(ui, state, files, view, &mut out);
    ui.add_space(space::S);

    // H2C / P2S / X2D: the answer comes from the model name, and nothing is
    // listed or connected (5.3, Models)
    if let Some(message) = config::files_refused_by_name(view.serial) {
        files.close_focused = false;
        refusal_card(ui, files, "⚠ File access disabled for this model",
                     &message, view.dialog_open, &mut out);
        return out;
    }
    // a refused certificate replaces the content for this printer (5.3)
    if state.cert_alert.is_some() {
        files.close_focused = false;
        refusal_card(ui, files, "⚠ FTP connection refused", tls::REFUSAL_TEXT,
                     view.dialog_open, &mut out);
        return out;
    }
    files.close_focused = false;

    // Each block that comes and goes has an id scope of its own, so its
    // appearing never renumbers the widgets after it: the filter kept
    // losing its focus and cursor when one did (E3, D05).
    if let Some(notice) = not_tested_notice(view.serial, view.profile) {
        ui.push_id("not-tested", |ui| {
            widgets::banner(ui, widgets::Tone::Warn, None, &notice);
        });
    }
    status_line(ui, state, files, view, &mut out);
    // read where it is: the card only paints and reports (E26)
    if let Some(error) = &state.error {
        ui.push_id("error-card", |ui| {
            error_card(ui, error, view.serial, &mut out);
        });
    }
    // "couldn't open the file": said here, where it happened, until Back
    // or the next open that works (C3, D22)
    if let Some(error) = view.open_error {
        ui.push_id("open-error", |ui| {
            widgets::banner(ui, widgets::Tone::Danger, None, error);
        });
    }
    // the player takes over the grid area; Back returns to the grid
    // (section 6)
    if let Some(player) = &view.player {
        ui.add_space(space::XS);
        // the transfer bar and the cache line sit below the picture here
        // too, so the picture is given what is left of the height rather
        // than a fixed margin: a transfer running while a video played used
        // to push Clear cache and that transfer's ✕ off the bottom
        let reserved = footer_height(ui, state, view);
        player_pane(ui, view, player, reserved, &mut out);
        footer(ui, state, view, &mut out);
        return out;
    }
    // the file could not be played here: the OS player is the way out
    // (5.10, section 7 option B)
    if let Some(note) = view.player_error {
        ui.push_id("player-error", |ui| {
            ui.add_space(space::XS);
            player_error_card(ui, view, note, view.player_error_path,
                              &mut out);
        });
    }
    ui.add_space(space::XS);
    ui.push_id("controls", |ui| controls(ui, files, view.serial));
    ui.add_space(space::S);

    if files.tab == Tab::Recordings {
        out.cmds.extend(state.open_recordings());
    }
    // the tabs list the unreadable entries they cannot show (5.10)
    let damaged = damaged_dirs(state, files.tab);
    for (dir, count) in &damaged {
        ui.push_id(("damaged", dir), |ui| {
            widgets::banner(ui, widgets::Tone::Warn, None, &format!(
                "{count} entries in {dir} can't be read; the SD card's file \
                 system looks damaged"));
        });
    }
    // that banner already gives the reason, so nothing repeats it below
    let damaged = !damaged.is_empty();

    let total = ui.available_width();
    // tiles per grid row: a month's tiles wrap instead of running off the
    // edge, and the rows stay short enough to virtualise
    let gap = ui.spacing().item_spacing.x;
    let list_w = (total - size::DETAIL_W - gap).max(size::LIST_MIN_W);
    let columns =
        (((list_w + gap) / (size::TILE_W + gap)).floor() as usize).max(1);
    // only orphan thumbnails, or a damaged card: say why above the tiles
    // that are left (5.5)
    if files.tab == Tab::Timelapses && !state.timelapses.is_empty()
        && !damaged
        && let Some(notice) = state.timelapse_notice()
    {
        ui.push_id("orphan-notice", |ui| {
            widgets::banner(ui, widgets::Tone::Neutral, None, notice);
            ui.add_space(space::XS);
        });
    }
    // the model is rebuilt only when its key changes: filtering, sorting
    // and grouping the whole listing ran on every frame (E16, E27, D37)
    let key = RowKey {
        revision: state.revision(),
        tab: files.tab,
        filter: files.filter.clone(),
        sort: files.sort,
        shown: files.shown,
        columns,
        folders: folders_key(&files.open_folders),
        damaged,
    };
    let rows = match files.rows.take() {
        Some((built, rows)) if built == key => rows,
        _ => {
            files.builds += 1;
            build_rows(state, files, columns, view.serial, damaged)
        }
    };
    // the transfer bar and the cache line sit below the grid, so the list
    // is given what is left rather than the whole height (section 6)
    let reserved = footer_height(ui, state, view);
    let body = (ui.available_height() - reserved)
        .max(Heights::of(ui).row);
    ui.allocate_ui_with_layout(vec2(ui.available_width(), body),
        egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.set_height(body);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(vec2(list_w, body),
                egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(list_w);
                // the list never eats the detail pane's width
                ui.set_max_width(list_w);
                list(ui, state, files, view, &rows, columns, &mut out);
            });
            ui.allocate_ui_with_layout(vec2(ui.available_width(), body),
                egui::Layout::top_down(egui::Align::Min), |ui| {
                detail_pane(ui, state, files, view, body, &mut out);
            });
        });
    });
    // kept for the next frame, with the key it was built for
    files.rows = Some((key, rows));
    footer(ui, state, view, &mut out);
    out
}

// ------------------------------------------- transfers, cache and player

/// Height the footer needs, so the grid above it is given the rest (1.8):
/// up to `TRANSFER_ROWS_VISIBLE` transfer rows — more scroll — and the
/// cache line, each with the spacing after it, plus the spacing above.
fn footer_height(ui: &Ui, state: &BrowserState, view: &View<'_>) -> f32 {
    let gap = ui.spacing().item_spacing.y;
    let shown = footer_rows(state).len()
        .min(size::TRANSFER_ROWS_VISIBLE) as f32;
    let cache = match view.cache_cap > 0 {
        true => size::BUTTON_H + gap,
        false => 0.0,
    };
    shown * (transfer_row_height() + gap) + cache + gap
}

/// A transfer row: its ✕ button and the row's padding.
fn transfer_row_height() -> f32 {
    size::BUTTON_H + pad::ROW.sum().y
}

/// Transfers the bar shows: everything still running or waiting, and the
/// ones that failed, which stay until the user dismisses them (section 6).
fn footer_rows(state: &BrowserState) -> Vec<&TransferUi> {
    state.transfers.iter()
        .filter(|transfer| transfer.active() || transfer.failure().is_some())
        .collect()
}

/// The transfer bar and the cache line of section 6: real progress, the
/// measured rate, the ETA and a Cancel per transfer.
fn footer(ui: &mut Ui, state: &mut BrowserState, view: &View<'_>,
          out: &mut Outcome) {
    // the rows are read where they are: cloning every transfer once a
    // frame copied each one's whole RemoteEntry (E26, D37). What a row's
    // buttons ask for is applied once the borrow is over.
    let mut asked: Vec<RowAction> = Vec::new();
    let rows = footer_rows(state);
    if !rows.is_empty() {
        // past three rows the bar scrolls instead of pushing Clear cache
        // and the ✕ of a failed row off the window (C13, D03)
        let gap = ui.spacing().item_spacing.y;
        let visible = rows.len().min(size::TRANSFER_ROWS_VISIBLE) as f32;
        egui::ScrollArea::vertical()
            .id_salt(("transfers", view.serial))
            .max_height(visible * transfer_row_height()
                        + (visible - 1.0) * gap)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for transfer in &rows {
                    // keyed by the transfer, so its ✕ and retry stay its
                    // own when a row above it goes away
                    ui.push_id(transfer.id, |ui| {
                        asked.extend(transfer_row(ui, view, transfer));
                    });
                }
            });
    }
    for action in asked {
        match action {
            RowAction::Dismiss(id) => state.dismiss(id),
            RowAction::Cancel(id) =>
                out.cmds.push(state.cancel_transfer(id)),
            RowAction::Retry(remote, dest) => {
                if let Some(cmd) = state.download(&remote, dest) {
                    out.cmds.push(cmd);
                }
            }
        }
    }
    if view.cache_cap == 0 {
        return;
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!(
            "cache {} / {}", human_bytes(view.cache_usage),
            human_bytes(view.cache_cap)))
            .color(theme::TEXT_DIM).font(font::caption()));
        // it skips what the player has open (5.6)
        let label = match view.clearing {
            true => "Clearing…",
            false => "Clear cache",
        };
        let running = view.clearing.then_some("Already clearing the cache");
        if widgets::button(ui, egui::Button::new(label), running) {
            out.actions.push(Action::ClearCache);
        }
    });
}

/// What a transfer row's buttons asked for. The row borrows its transfer
/// out of the state, so the state is changed after the loop (E26).
enum RowAction {
    Dismiss(u64),
    Cancel(u64),
    /// a failed transfer restarts from its own entry and destination
    Retry(RemoteEntry, Dest),
}

/// One row of the transfer bar: its glyph, name and line, and on the right
/// the bar, a retry when it failed, and ✕.
fn transfer_row(ui: &mut Ui, view: &View<'_>, transfer: &TransferUi)
                -> Option<RowAction> {
    let mut asked = None;
    let failed = transfer.failure().is_some();
    // "connecting N s" and "queued" count from the row's own start (E32)
    if transfer.active() {
        tick_seconds(ui.ctx(), transfer.started, view.now);
    }
    egui::Frame::new()
        .fill(theme::CARD)
        .corner_radius(radius::CONTROL)
        .inner_margin(pad::ROW)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // the controls on the right are laid out first, so a long line
            // truncates instead of pushing ✕ out of the row (C13)
            egui::Sides::new().shrink_left().show(ui,
                |ui| {
                    let caption = font::caption();
                    ui.label(RichText::new(match failed {
                        true => "⚠",
                        false => "↓",
                    }).color(match failed {
                        true => theme::DANGER,
                        false => theme::ACCENT,
                    }).font(caption.clone()));
                    let line = transfer_line(transfer, view);
                    // the line keeps its width, up to half of what is left;
                    // the name takes the rest and truncates first
                    let room = ui.available_width();
                    let line_w = theme::text_width(ui, &line, &caption)
                        .min(room / 2.0);
                    let name_font = font::body();
                    let name_w = theme::text_width(ui, transfer.name(),
                                                   &name_font)
                        .min((room - line_w
                              - ui.spacing().item_spacing.x).max(0.0));
                    for (text, width, font, color) in [
                        (transfer.name(), name_w, name_font, theme::TEXT),
                        (line.as_str(), line_w, caption, match failed {
                            true => theme::DANGER,
                            false => theme::TEXT_DIM,
                        }),
                    ] {
                        ui.allocate_ui_with_layout(
                            vec2(width, size::CONTROL_H),
                            egui::Layout::left_to_right(Align::Center),
                            |ui| {
                                ui.set_width(width);
                                ui.add(egui::Label::new(RichText::new(text)
                                    .font(font).color(color)).truncate());
                            });
                    }
                },
                |ui| {
                    // the same button cancels a running transfer and
                    // dismisses a failed one
                    // the same glyph cancels and dismisses, so its name
                    // says which one it is here (A9, D39)
                    let close = ui.button("✕");
                    let what = match failed {
                        true => "Dismiss",
                        false => "Cancel the download of",
                    };
                    let name = format!("{what} {}", transfer.name());
                    close.widget_info(|| egui::WidgetInfo::labeled(
                        egui::WidgetType::Button, true, &name));
                    if close.clicked() {
                        asked = Some(match failed {
                            true => RowAction::Dismiss(transfer.id),
                            false => RowAction::Cancel(transfer.id),
                        });
                    }
                    // 5.10 pairs "download interrupted" with a Retry. It
                    // restarts at 0 — there is no resume (REST is 502) — and
                    // it keeps the destination the failed transfer had.
                    if failed && ui.button("⟳ retry").clicked() {
                        asked = Some(RowAction::Retry(
                            transfer.remote.clone(), transfer.dest));
                    }
                    if let Some(done) = transfer.fraction() {
                        let bar = ui.add(egui::ProgressBar::new(done)
                            .desired_width(size::TRANSFER_BAR_W)
                            .desired_height(size::PROGRESS_H)
                            .fill(theme::ACCENT));
                        // how far along, not just that it is a bar (A10)
                        crate::ui::panel::progress_value(
                            &bar, f64::from(done) * 100.0,
                            &format!("Downloading {}", transfer.name()));
                    }
                });
        });
    asked
}

/// One transfer's line: real bytes off the socket, the measured rate and
/// the ETA, or why it is waiting or failed (section 6, 5.10).
fn transfer_line(transfer: &TransferUi, view: &View<'_>) -> String {
    match &transfer.phase {
        TransferPhase::Starting => connecting_text(transfer, view.now),
        TransferPhase::Queued(reason) => reason.clone(),
        TransferPhase::Running { done, total, bytes_per_s } => {
            let mut line = format!("{} / {}", human_bytes(*done),
                                   human_bytes(*total));
            if *bytes_per_s > 1.0 {
                line.push_str(&format!("  ·  {}/s",
                                       human_bytes(*bytes_per_s as u64)));
            }
            if let Some(left) = transfer.eta_s() {
                line.push_str(&format!("  ·  {} left", duration_text(left)));
            }
            line
        }
        TransferPhase::Done(Ok(done)) => match done.from_cache {
            true => "already downloaded".to_string(),
            false => format!("{} done", human_bytes(done.bytes)),
        },
        TransferPhase::Done(Err(err)) => err.text(view.serial),
    }
}

/// "connecting 3 s": section 6 asks for the seconds elapsed, and this phase
/// is ~0.9 s normally but also covers a stalled handshake and the one retry
/// behind it, which is when the counter is the whole point.
fn connecting_text(transfer: &TransferUi, now: Instant) -> String {
    format!("connecting {} s",
            now.saturating_duration_since(transfer.started).as_secs())
}

/// What a tile says while its file is transferring (section 6): waiting,
/// queued with its reason, a percentage, done, or a short failure.
fn transfer_note(transfer: &TransferUi, now: Instant)
                 -> Option<(String, Color32)> {
    match &transfer.phase {
        TransferPhase::Starting =>
            Some((connecting_text(transfer, now), theme::TEXT_DIM)),
        TransferPhase::Queued(reason) =>
            Some((reason.clone(), theme::TEXT_DIM)),
        TransferPhase::Running { .. } => Some((
            match transfer.percent() {
                Some(pct) => format!("↓ {pct}%"),
                None => "↓ …".to_string(),
            }, theme::ACCENT)),
        TransferPhase::Done(Ok(_)) =>
            Some(("✓ ready".to_string(), theme::ACCENT)),
        TransferPhase::Done(Err(err)) =>
            Some((short_failure(err).to_string(), theme::DANGER)),
    }
}

fn note_for(state: &BrowserState, path: &str, now: Instant)
            -> Option<(String, Color32)> {
    state.transfer_of(path)
        .and_then(|transfer| transfer_note(transfer, now))
}

/// The player of section 6: the frame, the transport controls, and the OS
/// player next to them. `reserved` is the height the transfer bar and the
/// cache line need below it.
fn player_pane(ui: &mut Ui, view: &View<'_>, player: &PlayerView<'_>,
               reserved: f32, out: &mut Outcome) {
    let frames = player.index.frames.len().max(1);
    let caption = font::caption();
    let mut facts = format!("{}x{}", player.index.width, player.index.height);
    if player.index.fps() > 0.0 {
        facts.push_str(&format!("  ·  {:.0} fps", player.index.fps()));
    }
    facts.push_str(&format!("  ·  {frames} frames"));
    // the title gives way to the facts, never the other way round (C14)
    egui::Sides::new().shrink_left().truncate().show(ui,
        |ui| {
            // named apart from the view's own Back, which leaves the files
            // view altogether (section 6)
            if ui.button("‹ Back to the list").clicked() {
                out.actions.push(Action::ClosePlayer);
            }
            ui.add(egui::Label::new(RichText::new(player.title)
                .font(font::body_strong())).truncate());
        },
        |ui| {
            ui.label(RichText::new(facts).color(theme::TEXT_DIM)
                .font(caption.clone()));
        });
    // the cut last chunk of 5.8, said plainly rather than shown as a
    // half-grey frame
    if player.index.truncated {
        ui.label(RichText::new(
            "this recording was cut short; the last frame was dropped")
            .color(theme::WARN).font(font::caption()));
    }
    if player.skipped > 0 {
        ui.label(RichText::new(format!(
            "{} frame(s) could not be decoded and were skipped",
            player.skipped)).color(theme::TEXT_DIM).font(font::caption()));
    }
    // the picture's area is whatever the controls and the footer leave;
    // the well inside it is the frame's own shape, so no black slabs
    // appear beside it (C14)
    let width = ui.available_width();
    let height = (ui.available_height() - player_controls_height(ui)
                  - reserved)
        .max(Heights::of(ui).row);
    let (area, _) = ui.allocate_exact_size(vec2(width, height),
                                           Sense::hover());
    let aspect = match player.texture {
        Some(handle) => handle.size_vec2(),
        None if player.index.width > 0 && player.index.height > 0 =>
            vec2(player.index.width as f32, player.index.height as f32),
        None => size::VIDEO_ASPECT,
    };
    let rect = egui::Rect::from_center_size(
        area.center(), widgets::fit(aspect, area.size()));
    ui.painter().rect_filled(rect, radius::CARD, theme::MEDIA_WELL);
    match player.texture {
        Some(handle) => {
            egui::Image::new((handle.id(), handle.size_vec2()))
                .corner_radius(radius::CARD)
                .paint_at(ui, rect);
        }
        None => {
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER,
                              "decoding…",
                              font::caption(),
                              theme::TEXT_DIM);
        }
    }
    ui.horizontal(|ui| {
        let label = match player.playing {
            true => "⏸",
            false => "▶",
        };
        if ui.button(label).clicked() {
            out.actions.push(Action::Player(match player.playing {
                true => PlayerCmd::Pause,
                false => PlayerCmd::Play,
            }));
        }
        // the slider takes what the clock, the speed and the OS buttons
        // leave, so none of them is pushed past the edge
        let gap = ui.spacing().item_spacing.x;
        let pad = ui.spacing().button_padding.x;
        let button = |ui: &Ui, text: &str| {
            theme::text_width(ui, text, &font::button()) + 2.0 * pad
        };
        let clock_widest = "000:00 / 000:00";
        let openable = may_reach_shell(view, player.path);
        let mut os_w = button(ui, "Show in folder");
        if openable {
            os_w += gap + button(ui, "Open in player");
        }
        let taken = theme::text_width(ui, clock_widest, &caption)
            + size::SPEED_COMBO_W + os_w + 4.0 * gap;
        let last = frames.saturating_sub(1) as u32;
        let mut at = player.pos.min(last);
        let slider = ui.push_id(("player-seek", view.serial), |ui| {
            ui.spacing_mut().slider_width =
                (ui.available_width() - taken).max(0.0);
            ui.add(egui::Slider::new(&mut at, 0..=last).show_value(false))
        }).inner;
        if slider.changed() {
            out.actions.push(Action::Player(PlayerCmd::Seek(at)));
        }
        let per_frame = player.index.frame_us() as f32 / 1_000_000.0;
        widgets::slot(ui, clock_widest, RichText::new(format!(
            "{} / {}", clock_text(player.pos as f32 * per_frame),
            clock_text(player.index.duration_s())))
            .color(theme::TEXT_DIM), &caption, Align::Min);
        // Display on an f32 drops the trailing ".0", so these read 1x, 10x
        egui::ComboBox::from_id_salt(("player-speed", view.serial))
            .width(size::SPEED_COMBO_W)
            .selected_text(format!("{}x", player.speed))
            .show_ui(ui, |ui| {
                for speed in player::SPEEDS {
                    if ui.selectable_label(
                        (player.speed - speed).abs() < f32::EPSILON,
                        format!("{speed}x")).clicked()
                    {
                        out.actions.push(
                            Action::Player(PlayerCmd::Speed(speed)));
                    }
                }
            });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
            // the app is playing this file, so the OS player is section 7's
            // documented fallback for it — but the shell still picks its
            // program by extension, so the same rule decides (F2)
            let openable = may_reach_shell(view, player.path);
            os_player_buttons(ui, player.path, openable, out);
        });
    });
}

/// "Open in player" and "Show in folder" (section 7, option B). "Show in
/// folder" is offered wherever a local copy exists; "Open in player" hands
/// the file to the Windows shell, so `offer_open` is false where the file
/// is merely a saved copy rather than something the app tried to play.
///
/// Names come off an SD card this app does not control: sanitising keeps a
/// hostile name inside `Downloads\Bambu Control\<printer>`, but it keeps
/// the extension, so a card offering "invoice.exe" would otherwise be two
/// clicks from a ShellExecute under a button labelled as a player (stage 3
/// security review, F2).
fn os_player_buttons(ui: &mut Ui, path: &Path, offer_open: bool,
                     out: &mut Outcome) {
    if ui.button("Show in folder").clicked() {
        out.actions.push(Action::Reveal(path.to_path_buf()));
    }
    if offer_open && ui.button("Open in player").clicked() {
        out.actions.push(Action::OpenExternally(path.to_path_buf()));
    }
}

/// 5.10: "can't play this format in the app" / "empty recording", with the
/// OS player offered next to it rather than a dead end.
fn player_error_card(ui: &mut Ui, view: &View<'_>, note: &str,
                     path: Option<&Path>, out: &mut Outcome) {
    egui::Frame::new()
        .fill(theme::WARN_BG)
        .corner_radius(radius::CONTROL)
        .inner_margin(pad::BANNER)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(RichText::new(note)
                    .color(theme::WARN).font(font::caption())).wrap());
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        // not a dead end: the card can be put away, and the
                        // file handed to the OS player (section 7, B)
                        let close = ui.button("✕");
                        close.widget_info(|| egui::WidgetInfo::labeled(
                            egui::WidgetType::Button, true,
                            "Close this message"));
                        if close.clicked() {
                            out.actions.push(Action::ClosePlayer);
                        }
                        if let Some(path) = path {
                            // the sniff refused it, which is exactly when
                            // section 7 hands it to the OS player — as long
                            // as its header and name agree it is media (F2)
                            let openable = may_reach_shell(view, path);
                            os_player_buttons(ui, path, openable, out);
                        }
                    });
            });
        });
}

/// `00:21`, for the player's position and length.
fn clock_text(seconds: f32) -> String {
    let seconds = seconds.max(0.0) as u64;
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

/// "21 s", "3 min", "1 h 5 min": how long something still takes.
fn duration_text(seconds: f32) -> String {
    let seconds = seconds.max(0.0) as u64;
    match seconds {
        0..=89 => format!("{seconds} s"),
        90..=3599 => format!("{} min", (seconds + 30) / 60),
        _ => format!("{} h {} min", seconds / 3600, (seconds % 3600) / 60),
    }
}

/// What a download will cost, before it starts (section 6: every action
/// shows its time cost up front). The rolling rate when there is one, the
/// documented 200 kB/s until then (5.4).
fn download_eta(size: u64, rate_bps: f64) -> String {
    let rate = match rate_bps > 1.0 {
        true => rate_bps,
        false => crate::browser::DEFAULT_RATE_BPS,
    };
    format!("~{}", duration_text((size as f64 / rate) as f32))
}

/// The confirmation wording of section 6, used before closing the app,
/// removing a printer or editing its connection while transfers run.
pub fn active_transfer_note(count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some("1 download is still running and will be discarded."
            .to_string()),
        many => Some(format!(
            "{many} downloads are still running and will be discarded.")),
    }
}

/// "not tested on this model" for every model except A1 and P1S, prefixes
/// that are not in the table, and any banner other than BBL-P003 (section 6).
pub fn not_tested_notice(serial: &str, profile: Option<ServerProfile>)
                         -> Option<String> {
    let tested_model = matches!(config::model_from_serial(serial).as_str(),
                                "Bambu Lab A1" | "Bambu Lab P1S");
    let tested_banner = profile.is_none_or(ServerProfile::is_tested);
    // never "Unknown (31B)"-style text (5.3, Models)
    (!tested_model || !tested_banner)
        .then(|| "not tested on this model".to_string())
}

// ----------------------------------------------------------------- chrome

fn header(ui: &mut Ui, state: &BrowserState, files: &mut FilesUi,
          view: &View<'_>, out: &mut Outcome) {
    let (timelapses, recordings, print_files) = state.counts();
    ui.horizontal(|ui| {
        if ui.button("‹ Back").clicked() {
            out.actions.push(Action::Back);
        }
        // /ipcam is listed when its tab opens (5.5), so until then the
        // Recordings tab shows no number rather than a false zero
        let recordings = match state.recordings_listed() {
            true => format!("Recordings {recordings}"),
            false => "Recordings".to_string(),
        };
        let tabs = [
            (Tab::Timelapses, format!("Timelapses {timelapses}")),
            (Tab::Recordings, recordings),
            (Tab::Files, format!("Print files {print_files}")),
        ];
        // the tabs keep their room and the printer's name truncates first,
        // so a long name never pushes a tab off the row (C9, D15)
        let gap = ui.spacing().item_spacing.x;
        let pad = ui.spacing().button_padding.x;
        let tabs_w: f32 = tabs.iter()
            .map(|(_, label)| theme::text_width(ui, label, &font::button())
                 + 2.0 * pad + gap)
            .sum();
        let title = format!("{} / FILES", view.name);
        let title_font = font::title();
        let title_w = theme::text_width(ui, &title, &title_font)
            .min((ui.available_width() - tabs_w - space::L).max(0.0));
        ui.allocate_ui_with_layout(vec2(title_w, size::CONTROL_H),
            egui::Layout::left_to_right(Align::Center), |ui| {
            ui.set_width(title_w);
            ui.add(egui::Label::new(RichText::new(title).font(title_font))
                .truncate());
        });
        ui.add_space(space::L - gap);
        for (tab, label) in tabs {
            ui.selectable_value(&mut files.tab, tab, label);
        }
    });
}

fn status_line(ui: &mut Ui, state: &BrowserState, files: &mut FilesUi,
               view: &View<'_>, out: &mut Outcome) {
    // summed once per listing, not once per frame (E27, D37)
    let sizes = match files.sizes.take() {
        Some((revision, sizes)) if revision == state.revision() => sizes,
        _ => size_parts(state),
    };
    let mut parts = sizes.clone();
    files.sizes = Some((state.revision(), sizes));
    if let Some(at) = state.updated_at() {
        parts.push(format!("updated {} ago",
                           ago(view.now.saturating_duration_since(at))));
        tick_seconds(ui.ctx(), at, view.now);
    }
    let summary = match parts.is_empty() {
        true => "no listing yet".to_string(),
        false => parts.join("  ·  "),
    };
    // the line gate G4 reads (section 6)
    let mut session = format!("FTP session: {}", state.conn.label(view.now));
    // it counts seconds while a session connects or sits idle (E32)
    if let ConnState::Connecting { since }
        | ConnState::Open { idle_since: Some(since) } = state.conn
    {
        tick_seconds(ui.ctx(), since, view.now);
    }
    if view.open_sessions > 1 {
        session.push_str(&format!("  ({} open)", view.open_sessions));
    }
    let session_color = match state.conn {
        ConnState::Open { .. } => theme::ACCENT,
        ConnState::Stopped(_) => theme::DANGER,
        _ => theme::TEXT_DIM,
    };
    // While printing the hint sits on this row too, so the list never jumps
    // a line when a print starts or ends (C9, D13).
    let hint = view.printing
        .then_some("printing: transfers share the printer's Wi-Fi");
    let caption = font::caption();
    // the session state keeps its width on the right; on the left the hint
    // truncates first, then the summary; "(printer clock)" and Refresh stay
    egui::Sides::new().shrink_left().show(ui,
        |ui| {
            let gap = ui.spacing().item_spacing.x;
            let clock = "(printer clock)";
            let refresh_w = theme::text_width(ui, "Refresh", &font::button())
                + 2.0 * ui.spacing().button_padding.x;
            let fixed = theme::text_width(ui, clock, &caption) + refresh_w
                + 3.0 * gap;
            let room = (ui.available_width() - fixed).max(0.0);
            let summary_w = theme::text_width(ui, &summary, &caption)
                .min(room);
            let hint_w = hint.map_or(0.0, |hint|
                theme::text_width(ui, hint, &caption)
                    .min((room - summary_w - gap).max(0.0)));
            let truncated = |ui: &mut Ui, text: &str, width: f32| {
                ui.allocate_ui_with_layout(vec2(width, size::CONTROL_H),
                    egui::Layout::left_to_right(Align::Center), |ui| {
                    ui.set_width(width);
                    ui.add(egui::Label::new(RichText::new(text)
                        .color(theme::TEXT_DIM).font(caption.clone()))
                        .truncate());
                });
            };
            truncated(ui, &summary, summary_w);
            ui.label(RichText::new(clock).color(theme::TEXT_DIM)
                .font(caption.clone()));
            if ui.button("Refresh").clicked() {
                out.actions.push(Action::Refresh);
            }
            if let Some(hint) = hint {
                truncated(ui, hint, hint_w);
            }
        },
        |ui| {
            ui.label(RichText::new(session).color(session_color)
                .font(caption.clone()));
        });
}

/// The filter, the sort and the kind filters. Their state is salted with
/// the printer, so a popup or a cursor never follows the view to another
/// printer (E4).
fn controls(ui: &mut Ui, files: &mut FilesUi, serial: &str) {
    // the kind filters move to a second line when the window is too narrow
    // for them: a row wider than the page widens everything below it, and
    // the transfer bar's ✕ went past the right edge (A14)
    ui.horizontal_wrapped(|ui| {
        let field = ui.add(egui::TextEdit::singleline(&mut files.filter)
            .id_salt(("files-filter", serial))
            .hint_text("filter…")
            .desired_width(size::FILTER_W));
        // there is no label beside it to point at, so the field carries
        // its own name for a screen reader; the hint stays the placeholder
        // and nothing on screen changes (A15)
        let typed = files.filter.clone();
        field.widget_info(|| egui::WidgetInfo {
            label: Some("Filter by name".to_owned()),
            current_text_value: Some(typed.clone()),
            hint_text: Some("filter…".to_owned()),
            ..egui::WidgetInfo::new(egui::WidgetType::TextEdit)
        });
        egui::ComboBox::from_id_salt(("files-sort", serial))
            .selected_text(format!("Sort: {}", files.sort.label()))
            .show_ui(ui, |ui| {
                for sort in [Sort::Newest, Sort::Name, Sort::Size] {
                    ui.selectable_value(&mut files.sort, sort, sort.label());
                }
            });
        if files.tab == Tab::Files {
            ui.add_space(space::S);
            for shown in [Shown::All, Shown::Sent, Shown::Cache,
                          Shown::BuiltIn, Shown::Folders] {
                ui.selectable_value(&mut files.shown, shown, shown.label());
            }
        }
    });
}

/// The error card of 5.10: the condition's text and a Retry. Never a spinner
/// that goes on forever.
fn error_card(ui: &mut Ui, error: &FtpError, serial: &str,
              out: &mut Outcome) {
    egui::Frame::new()
        .fill(theme::DANGER_BG)
        .stroke(Stroke::new(stroke::HAIRLINE, theme::DANGER))
        .corner_radius(radius::CARD)
        .inner_margin(pad::CARD)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(RichText::new(error.text(serial))
                .color(theme::DANGER).font(font::body())).wrap());
            ui.add_space(space::S);
            if accent_button_response(ui, "Retry",
                                      vec2(0.0, size::BUTTON_H))
                .clicked()
            {
                out.actions.push(Action::Retry);
            }
        });
}

/// The refusal card of section 6. There is no trust, accept or continue
/// action; Close is the default, focused and bound to Enter and Esc.
fn refusal_card(ui: &mut Ui, files: &mut FilesUi, title: &str, body: &str,
                blocked: bool, out: &mut Outcome) {
    // the keys are consumed here, so the Edit printer dialog this card
    // opens keeps its own Enter and Esc, and the card does not answer them
    // from underneath it (section 6)
    let mut close = !blocked && ui.input_mut(|i| {
        i.consume_key(Modifiers::NONE, egui::Key::Enter)
            | i.consume_key(Modifiers::NONE, egui::Key::Escape)
    });
    egui::Frame::new()
        .fill(theme::DANGER_BG)
        .stroke(Stroke::new(stroke::HAIRLINE, theme::DANGER))
        .corner_radius(radius::CARD)
        .inner_margin(pad::CARD)
        .show(ui, |ui| {
            ui.set_width(ui.available_width().min(size::REFUSAL_MAX_W));
            ui.label(RichText::new(title).color(theme::DANGER)
                .font(font::title()));
            ui.add_space(space::M);
            ui.add(egui::Label::new(RichText::new(body).font(font::body()))
                .wrap());
            ui.add_space(space::XL);
            ui.horizontal(|ui| {
                if ui.button("Edit printer").clicked() {
                    out.actions.push(Action::EditPrinter);
                }
                // the default action, and the only other one
                let response =
                    accent_button_response(ui, "Close",
                                           vec2(0.0, size::BUTTON_H));
                // a dialog over the view owns the keyboard: the card never
                // takes the focus back from it
                if !blocked && !response.has_focus() {
                    response.request_focus();
                }
                files.close_focused = response.has_focus();
                if response.clicked() {
                    close = true;
                }
            });
        });
    if close {
        out.actions.push(Action::Back);
    }
}

// ------------------------------------------------------------------ rows

/// One line of the virtualised list. Every variant owns what it draws, so
/// the row list never borrows the browser state.
enum Row {
    Heading(String),
    /// indices into the tab's ordered list
    Tiles(Vec<usize>),
    Item(usize),
    /// a `/cache` companion under its root job (5.4)
    Companion(usize),
    Folder { entry: RemoteEntry, depth: usize, open: bool },
    Note(String),
    Skeleton,
}

/// Which reserved height a row uses; rows of one kind paint alike.
fn row_kind(row: &Row) -> usize {
    match row {
        Row::Heading(_) => 0,
        Row::Tiles(_) => 1,
        Row::Item(_) => 2,
        Row::Companion(_) => 3,
        Row::Folder { .. } => 4,
        Row::Note(_) => 5,
        Row::Skeleton => 6,
    }
}

/// What the current tab shows, in view order.
struct Rows {
    rows: Vec<Row>,
    /// indices into `state.timelapses`, `state.recordings` or `state.files`
    order: Vec<usize>,
    empty: Option<String>,
    loading: bool,
}

/// A hash of the opened folders that does not depend on their order.
fn folders_key(folders: &HashSet<String>) -> u64 {
    use std::hash::{Hash, Hasher};
    folders.iter().fold(0_u64, |total, dir| {
        let mut hasher = std::hash::DefaultHasher::new();
        dir.hash(&mut hasher);
        total.wrapping_add(hasher.finish())
    })
}

fn build_rows(state: &BrowserState, files: &FilesUi, columns: usize,
              serial: &str, damaged: bool) -> Rows {
    match files.tab {
        Tab::Timelapses => timelapse_rows(state, files, columns, damaged),
        Tab::Recordings => recording_rows(state, files),
        Tab::Files => file_rows(state, files, serial),
    }
}

/// The root, or one of this tab's own directories, failed: the error card
/// above already gives the condition's text and a Retry (5.10), so the tab
/// never adds a reason of its own — "no timelapses on this printer" about a
/// printer that never answered would be untrue.
fn tab_failed(state: &BrowserState, dirs: &[&str]) -> bool {
    state.dir_failed("/")
        || dirs.iter().any(|name| state.dir_failed(&format!("/{name}")))
}

fn dir_state<'a>(state: &'a BrowserState, name: &str)
                 -> Option<&'a DirState> {
    state.dirs.get(&format!("/{name}"))
}

fn is_loading(state: &BrowserState, name: &str) -> bool {
    matches!(state.dirs.get("/"), Some(DirState::Loading))
        || matches!(dir_state(state, name), Some(DirState::Loading))
}

/// The 550 of 5.10: a directory the root listed is not there any more.
fn missing_note(state: &BrowserState, name: &str) -> Option<String> {
    matches!(dir_state(state, name), Some(DirState::Missing))
        .then(|| format!("/{name}: folder not present"))
}

fn matches_filter(name: &str, filter: &str) -> bool {
    filter.trim().is_empty()
        || name.to_lowercase().contains(&filter.trim().to_lowercase())
}

fn timelapse_rows(state: &BrowserState, files: &FilesUi, columns: usize,
                  damaged: bool) -> Rows {
    let mut order: Vec<usize> = (0..state.timelapses.len())
        .filter(|index| {
            let item = &state.timelapses[*index];
            matches_filter(item.stem(), &files.filter)
        })
        .collect();
    match files.sort {
        Sort::Newest => {}
        Sort::Name => order.sort_by(|a, b| state.timelapses[*a].stem()
            .cmp(state.timelapses[*b].stem())),
        Sort::Size => order.sort_by_key(|index| {
            std::cmp::Reverse(video_size(state, *index))
        }),
    }
    let mut rows = Vec::new();
    let mut month: Option<(i32, u32)> = None;
    let mut tiles: Vec<usize> = Vec::new();
    for index in &order {
        let item_month = state.timelapses[*index].month();
        if files.sort == Sort::Newest && item_month != month {
            if !tiles.is_empty() {
                rows.push(Row::Tiles(std::mem::take(&mut tiles)));
            }
            month = item_month;
            rows.push(Row::Heading(month_heading(item_month)));
        }
        tiles.push(*index);
        if tiles.len() >= columns {
            rows.push(Row::Tiles(std::mem::take(&mut tiles)));
        }
    }
    if !tiles.is_empty() {
        rows.push(Row::Tiles(tiles));
    }
    let loading = is_loading(state, "timelapse");
    let empty = match order.is_empty() && !loading
        && !tab_failed(state, &["timelapse"])
    {
        false => None,
        // the damaged-card banner already gave the reason (5.10)
        true => missing_note(state, "timelapse").or_else(|| match damaged {
            true => None,
            false => Some(state.timelapse_notice()
                .unwrap_or("No timelapses on this printer.").to_string()),
        }),
    };
    Rows { rows, order, empty, loading }
}

fn video_size(state: &BrowserState, index: usize) -> u64 {
    let item = &state.timelapses[index];
    item.video.as_ref().or(item.thumb.as_ref()).map_or(0, |entry| entry.size)
}

fn month_heading(month: Option<(i32, u32)>) -> String {
    const MONTHS: [&str; 12] = ["JANUARY", "FEBRUARY", "MARCH", "APRIL",
                                "MAY", "JUNE", "JULY", "AUGUST", "SEPTEMBER",
                                "OCTOBER", "NOVEMBER", "DECEMBER"];
    match month {
        Some((year, month)) => format!(
            "{} {year}",
            MONTHS.get((month as usize).saturating_sub(1))
                .copied().unwrap_or("UNKNOWN")),
        None => "NO DATE".to_string(),
    }
}

fn recording_rows(state: &BrowserState, files: &FilesUi) -> Rows {
    let mut order: Vec<usize> = (0..state.recordings.len())
        .filter(|index| matches_filter(&state.recordings[*index].name,
                                       &files.filter))
        .collect();
    match files.sort {
        Sort::Newest => {}
        Sort::Name => order.sort_by(|a, b| state.recordings[*a].name
            .cmp(&state.recordings[*b].name)),
        Sort::Size => order.sort_by(|a, b| state.recordings[*b].size
            .cmp(&state.recordings[*a].size)),
    }
    let rows = order.iter().enumerate()
        .map(|(position, _)| Row::Item(position))
        .collect();
    let loading = is_loading(state, RECORDINGS_DIR);
    let empty = (order.is_empty() && !loading
        && !tab_failed(state, &[RECORDINGS_DIR])).then(|| {
        missing_note(state, RECORDINGS_DIR).unwrap_or_else(||
            "No recordings on this printer.".to_string())
    });
    Rows { rows, order, empty, loading }
}

fn file_rows(state: &BrowserState, files: &FilesUi, serial: &str) -> Rows {
    let mut order: Vec<usize> = (0..state.files.len())
        .filter(|index| {
            let item = &state.files[*index];
            files.shown.accepts(item.kind)
                && matches_filter(&item.remote.name, &files.filter)
        })
        .collect();
    match files.sort {
        Sort::Newest => {}
        Sort::Name => order.sort_by(|a, b| state.files[*a].remote.name
            .cmp(&state.files[*b].remote.name)),
        Sort::Size => order.sort_by(|a, b| state.files[*b].remote.size
            .cmp(&state.files[*a].remote.size)),
    }
    // companions are shown under their root job, never as duplicates (5.4)
    let mut position: HashMap<&str, usize> = HashMap::new();
    for (at, index) in order.iter().enumerate() {
        position.insert(state.files[*index].remote.path.as_str(), at);
    }
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut rows: Vec<Row> = Vec::new();
    for (at, index) in order.iter().enumerate() {
        if let Some(root) = state.files[*index].companion_of.as_deref()
            && let Some(parent) = position.get(root)
        {
            children.entry(*parent).or_default().push(at);
        }
    }
    let grouped: HashSet<usize> =
        children.values().flatten().copied().collect();
    for (at, _) in order.iter().enumerate() {
        if grouped.contains(&at) {
            continue;
        }
        rows.push(Row::Item(at));
        for child in children.get(&at).into_iter().flatten() {
            rows.push(Row::Companion(*child));
        }
    }
    if matches!(files.shown, Shown::All | Shown::Folders) {
        let folders = folder_rows(state, files, serial);
        if !folders.is_empty() {
            rows.push(Row::Heading("OTHER FOLDERS".to_string()));
            rows.extend(folders);
        }
    }
    for name in ["cache", "model"] {
        if let Some(note) = missing_note(state, name) {
            rows.push(Row::Note(note));
        }
    }
    let loading = is_loading(state, "cache");
    let empty = (rows.is_empty() && !loading
        && !tab_failed(state, &["cache", "model"]))
        .then(|| "No print files on this printer.".to_string());
    Rows { rows, order, empty, loading }
}

/// The "Other folders" of 5.5, opened one level at a time.
fn folder_rows(state: &BrowserState, files: &FilesUi, serial: &str)
               -> Vec<Row> {
    /// Adds this folder and, when it is open, one level of its entries.
    /// Returns whether anything here survived the filter; what did not is
    /// taken back off the row list.
    fn walk(state: &BrowserState, files: &FilesUi, serial: &str,
            entry: &RemoteEntry, depth: usize, rows: &mut Vec<Row>) -> bool {
        let start = rows.len();
        let open = files.open_folders.contains(&entry.path);
        rows.push(Row::Folder { entry: entry.clone(), depth, open });
        let mut kept = matches_filter(&entry.name, &files.filter);
        if !open || depth >= FOLDER_DEPTH {
            if !kept {
                rows.truncate(start);
            }
            return kept;
        }
        match state.dirs.get(&entry.path) {
            // an open folder stays visible while it answers, whatever the
            // filter says, so the user can see what it is doing
            Some(DirState::Loading) => {
                rows.push(Row::Skeleton);
                kept = true;
            }
            Some(DirState::Missing) => {
                rows.push(Row::Note(
                    format!("{}: folder not present", entry.path)));
                kept = true;
            }
            Some(DirState::Failed(err)) => {
                // the model picks the wording, and it is known here (5.10)
                rows.push(Row::Note(
                    format!("{}: {}", entry.path, err.text(serial))));
                kept = true;
            }
            Some(DirState::Ready { entries, .. }) => {
                if entries.is_empty() {
                    rows.push(Row::Note("(empty)".to_string()));
                }
                for child in entries {
                    if child.is_dir {
                        kept |= walk(state, files, serial, child, depth + 1,
                                     rows);
                    } else if matches_filter(&child.name, &files.filter) {
                        rows.push(Row::Folder { entry: child.clone(),
                                                depth: depth + 1,
                                                open: false });
                        kept = true;
                    }
                }
            }
            None => {}
        }
        if !kept {
            rows.truncate(start);
        }
        kept
    }
    let mut rows = Vec::new();
    for entry in &state.other_dirs {
        walk(state, files, serial, entry, 0, &mut rows);
    }
    rows
}

/// Directories whose unreadable entries the current tab would show (5.10).
fn damaged_dirs(state: &BrowserState, tab: Tab) -> Vec<(String, usize)> {
    let wanted = |dir: &str| match tab {
        Tab::Timelapses => dir.contains("timelapse"),
        Tab::Recordings => dir.contains(RECORDINGS_DIR),
        Tab::Files => !dir.contains("timelapse")
            && !dir.contains(RECORDINGS_DIR),
    };
    let mut dirs: Vec<(String, usize)> = state.unreadable.iter()
        .filter(|(dir, _)| wanted(dir))
        .map(|(dir, count)| (dir.clone(), *count))
        .collect();
    dirs.sort();
    dirs
}

// -------------------------------------------------------------- the list

fn list(ui: &mut Ui, state: &mut BrowserState, files: &mut FilesUi,
        view: &View<'_>, rows: &Rows, columns: usize, out: &mut Outcome) {
    if rows.loading && rows.rows.is_empty() {
        skeletons(ui, files.tab, columns);
        return;
    }
    if let Some(empty) = &rows.empty {
        card_frame(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(RichText::new(empty)
                .color(theme::TEXT_DIM).font(font::body())).wrap());
        });
        return;
    }
    let mut seen: HashSet<String> = HashSet::new();
    // the worker keeps one prefetch (section 4), so the view asks for one
    // tile at a time: every further request comes back Cancelled and is
    // asked for again on the next frame, which is a stream of commands for
    // no picture
    let mut may_request = !state.thumb_in_flight();
    let reserved = Heights::of(ui);
    let by_kind = reserved.by_kind();
    let heights: Vec<f32> = rows.rows.iter()
        .map(|row| by_kind[row_kind(row)])
        .collect();
    let ids = row_ids(state, files.tab, rows);
    // one scroll offset per printer and per tab (E1, D06)
    let list_salt = ("files-list", view.serial, files.tab as u8);
    let measured = virtual_rows(ui, list_salt, &heights, &ids, |ui, index| {
        match &rows.rows[index] {
            Row::Heading(text) => {
                ui.label(RichText::new(text).color(theme::TEXT_DIM)
                    .font(font::label()));
            }
            Row::Tiles(tiles) => {
                // top-aligned, so tiles of a row start on the same line
                ui.horizontal_top(|ui| {
                    for tile in tiles {
                        // the row's layout is horizontal and a Frame
                        // inherits it, so each tile gets a top-down Ui of
                        // its own: picture first, then its two lines
                        ui.allocate_ui_with_layout(
                            vec2(size::TILE_W, reserved.tile),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| timelapse_tile(ui, state, files, view,
                                *tile,
                                &mut TileCtx { seen: &mut seen,
                                               may_request: &mut may_request,
                                               out: &mut *out }));
                    }
                });
            }
            Row::Item(position) => match files.tab {
                Tab::Recordings => {
                    let entry = state.recordings[rows.order[*position]]
                        .clone();
                    let note = note_for(state, &entry.path, view.now);
                    entry_row(ui, files, &entry, "REC", 0.0, note);
                }
                _ => {
                    let item = state.files[rows.order[*position]].clone();
                    let note = note_for(state, &item.remote.path, view.now);
                    file_row(ui, files, &item, false, note);
                }
            },
            Row::Companion(position) => {
                let item = state.files[rows.order[*position]].clone();
                let note = note_for(state, &item.remote.path, view.now);
                file_row(ui, files, &item, true, note);
            }
            Row::Folder { entry, depth, open } => {
                let entry = entry.clone();
                let (depth, open) = (*depth, *open);
                if folder_row(ui, files, &entry, depth, open)
                    && entry.is_dir && !entry.unreadable
                {
                    if open {
                        files.open_folders.remove(&entry.path);
                    } else {
                        files.open_folders.insert(entry.path.clone());
                        out.cmds.extend(state.open_folder(&entry.path));
                    }
                }
            }
            Row::Note(text) => {
                // one line: a long failure is truncated, and its whole
                // text is on hover (C11)
                ui.add(egui::Label::new(RichText::new(text)
                    .color(theme::TEXT_DIM).font(font::caption()))
                    .truncate());
            }
            Row::Skeleton => skeleton(
                ui, vec2(ui.available_width()
                             .min(size::SKELETON_MAX_W),
                         reserved.row)),
        }
    });
    // the heights come from the style, never from what was painted: this
    // only records how far they disagree, for the tests (E15, E17)
    files.row_mismatch = measured.iter()
        .map(|(_, error)| *error)
        .fold(0.0, f32::max);
    if files.tab == Tab::Timelapses {
        // the tiles on screen, for the 500 ms prefetch gate. The texture of
        // a tile that scrolled away is kept until the cap evicts it (design
        // doc 12), so scrolling back paints it again instead of fetching it
        // from the printer once more.
        files.visible.retain(&seen);
    }
}

/// A stable identity for each row (E2): what it shows, never its index,
/// counted when two rows say the same thing ("(empty)" in two folders).
fn row_ids(state: &BrowserState, tab: Tab, rows: &Rows) -> Vec<egui::Id> {
    let mut seen: HashMap<egui::Id, u32> = HashMap::new();
    rows.rows.iter()
        .map(|row| {
            let base = match row {
                Row::Heading(text) => egui::Id::new(("heading", text)),
                Row::Tiles(tiles) => egui::Id::new(("tiles",
                    tiles.first().and_then(|at| state.timelapses.get(*at))
                        .map(|item| item.stem()))),
                Row::Item(at) | Row::Companion(at) => {
                    let index = rows.order.get(*at).copied();
                    let path = match tab {
                        Tab::Recordings => index
                            .and_then(|i| state.recordings.get(i))
                            .map(|entry| entry.path.as_str()),
                        _ => index.and_then(|i| state.files.get(i))
                            .map(|item| item.remote.path.as_str()),
                    };
                    egui::Id::new(("entry", path))
                }
                Row::Folder { entry, .. } =>
                    egui::Id::new(("folder", &entry.path)),
                Row::Note(text) => egui::Id::new(("note", text)),
                Row::Skeleton => egui::Id::new("skeleton"),
            };
            let count = seen.entry(base).or_default();
            *count += 1;
            base.with(*count)
        })
        .collect()
}

/// The variant of `ScrollArea::show_rows` this view needs: its rows have
/// different heights (month headings, tile rows, companions), and it must
/// not nest inside the outer `ScrollArea` (section 6), which is why the
/// files view replaces the panel instead of being drawn inside it.
///
/// Each row is given the height the table reserved for it, and reports how
/// far its content, its allocation and its top are from that promise. The
/// caller never feeds it back: a row that
/// painted taller than declared would shorten the scroll range, so the
/// declared heights are built to be exact instead (1.8).
fn virtual_rows(ui: &mut Ui, salt: impl std::hash::Hash + std::fmt::Debug,
                heights: &[f32],
                ids: &[egui::Id], mut render: impl FnMut(&mut Ui, usize))
                -> Vec<(usize, f32)> {
    let spacing = ui.spacing().item_spacing.y;
    let mut offsets: Vec<f32> = Vec::with_capacity(heights.len() + 1);
    let mut y = 0.0;
    for height in heights {
        offsets.push(y);
        y += height + spacing;
    }
    offsets.push(y);
    let total = (y - spacing).max(0.0);
    let mut measured: Vec<(usize, f32)> = Vec::new();
    egui::ScrollArea::vertical()
        .id_salt(salt)
        .auto_shrink(false)
        .show_viewport(ui, |ui, viewport| {
            ui.set_height(total);
            if heights.is_empty() {
                return;
            }
            let first = offsets.partition_point(|offset| *offset
                    <= viewport.min.y)
                .saturating_sub(1)
                .min(heights.len() - 1);
            let last = offsets.partition_point(|offset| *offset
                    < viewport.max.y)
                .clamp(first + 1, heights.len());
            let top = ui.max_rect().top();
            let rect = egui::Rect::from_x_y_ranges(
                ui.max_rect().x_range(),
                (top + offsets[first])..=(top + offsets[last]));
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                for (row, &height) in heights.iter().enumerate()
                    .take(last).skip(first)
                {
                    // keyed by what the row shows, not where it is, so its
                    // widgets keep their state when rows above it change
                    let placed = ui.push_id(ids[row], |ui| {
                        ui.allocate_ui_with_layout(
                            vec2(ui.available_width(), height),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                // before the content: egui adds a minimum
                                // height below the cursor, so set after it
                                // would stack a second row's worth of space
                                ui.set_min_height(height);
                                ui.scope(|ui| render(ui, row)).response.rect
                                    .height()
                            })
                    }).inner;
                    // how far this row is from what the offsets promised:
                    // its content, its allocation, and where it starts
                    let error = [
                        placed.inner - height,
                        placed.response.rect.height() - height,
                        placed.response.rect.top() - (top + offsets[row]),
                    ].into_iter().map(f32::abs).fold(0.0, f32::max);
                    measured.push((row, error));
                }
            });
        });
    measured
}

fn skeletons(ui: &mut Ui, tab: Tab, columns: usize) {
    ui.label(RichText::new(match tab {
        Tab::Timelapses => "listing /timelapse…",
        Tab::Recordings => "listing /ipcam…",
        Tab::Files => "listing /cache…",
    }).color(theme::TEXT_DIM).font(font::caption()));
    let reserved = Heights::of(ui);
    // the placeholders have the shape of what replaces them, so the list
    // does not relayout when the listing lands (C17, D30)
    if tab == Tab::Timelapses {
        for _ in 0..2 {
            ui.horizontal_top(|ui| {
                for _ in 0..columns {
                    skeleton(ui, vec2(size::TILE_W, reserved.tile));
                }
            });
        }
        return;
    }
    for _ in 0..6 {
        skeleton(ui, vec2(ui.available_width().min(size::SKELETON_MAX_W),
                          reserved.row));
    }
}

/// A placeholder shaped like what will replace it (C17).
fn skeleton(ui: &mut Ui, size: egui::Vec2) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, radius::CONTROL, theme::CARD_HOVER);
}

/// What a tile needs from the frame around it: which tiles were painted
/// (the 500 ms prefetch gate), whether a thumbnail may still be asked for
/// (one in flight, section 4), and where its commands go.
struct TileCtx<'a> {
    seen: &'a mut HashSet<String>,
    may_request: &'a mut bool,
    out: &'a mut Outcome,
}

fn timelapse_tile(ui: &mut Ui, state: &mut BrowserState, files: &mut FilesUi,
                  view: &View<'_>, index: usize, tile: &mut TileCtx<'_>) {
    let Some(item) = state.timelapses.get(index) else { return };
    let stem = item.stem().to_string();
    let key = item.video.as_ref().or(item.thumb.as_ref())
        .map(|entry| entry.path.clone())
        .unwrap_or_else(|| stem.clone());
    let thumb = item.thumb.clone();
    let orphan = item.video.is_none();
    let started = item.started;
    // the per-tile transfer state of section 6, taken before the state is
    // borrowed mutably below
    let note = item.video.as_ref()
        .map(|video| video.path.clone())
        .and_then(|path| note_for(state, &path, view.now));
    let size = item.video.as_ref().or(item.thumb.as_ref())
        .map_or(0, |entry| entry.size);
    let selected = files.selected.as_deref() == Some(key.as_str());

    // the outline is a hairline in every state, so a tile is exactly
    // TILE_W wide whether or not it is selected (C10, E10). The tile is
    // itself the widget, so hover, pressed and focus land on it (C2, C10)
    let shown = widgets::clickable(
        ui, ("tile", key.as_str()), &stem,
        widgets::Surface::card()
            .radius(radius::CARD)
            .padding(pad::TILE)
            .selected(selected, false),
        |ui| {
            let inner = size::TILE_W - pad::TILE.sum().x
                - 2.0 * stroke::HAIRLINE;
            ui.set_width(inner);
            let (rect, _) = ui.allocate_exact_size(
                vec2(inner, size::TILE_IMAGE_H), Sense::hover());
            ui.painter().rect_filled(rect, radius::MEDIA, theme::MEDIA_WELL);
            let texture = match &thumb {
                Some(entry) => {
                    tile.seen.insert(entry.path.clone());
                    tile_texture(ui.ctx(), state, files, view, entry, tile)
                }
                None => None,
            };
            let failed = thumb.as_ref().is_some_and(|entry|
                matches!(state.thumbs.get(&entry.path),
                         Some(ThumbState::Failed(_))));
            match texture {
                Some(handle) => {
                    let size = handle.size_vec2();
                    let scale = (rect.width() / size.x)
                        .min(rect.height() / size.y);
                    egui::Image::new((handle.id(), size))
                        .corner_radius(radius::MEDIA)
                        .paint_at(ui, egui::Rect::from_center_size(
                            rect.center(), size * scale));
                }
                None => {
                    ui.place(rect, egui::Label::new(
                        RichText::new(tile_caption(state, thumb.as_ref()))
                            .font(font::caption()).color(theme::TEXT_DIM))
                        .truncate());
                }
            }
            // where the retry sits, for the click below: the tile's own
            // click is registered over its contents, so a button inside it
            // would never see the pointer
            let mut retry_at = None;
            if let Some((text, color)) = &note {
                // A tile that is transferring says so instead of its date.
                // The 5.10 queued wording is 50 characters and a tile is
                // 154 px wide, so it is truncated like every tile line: a
                // wrapped line would paint the row taller than the height
                // reserved for it. egui shows the whole text on hover, and
                // the transfer bar below carries the reason in full.
                ui.add(egui::Label::new(RichText::new(text).color(*color)
                    .font(font::caption())).truncate());
            } else if orphan {
                ui.add(egui::Label::new(RichText::new("⚠ no video")
                    .color(theme::WARN).font(font::caption())).truncate());
            } else if failed {
                // a real widget, so it hovers and takes the keyboard; its
                // interact rect is grown to INLINE_TARGET_H without moving
                // the line (A5, C10)
                ui.spacing_mut().button_padding.x = 0.0;
                let retry = ui.add(egui::Button::new(
                    RichText::new("⟳ retry").color(theme::ACCENT)
                        .font(font::caption())).small().frame(false));
                let grow = ((size::INLINE_TARGET_H - retry.rect.height())
                    / 2.0).max(0.0);
                let target = retry.rect.expand2(vec2(0.0, grow));
                let hit = ui.interact(target, retry.id.with("target"),
                                      Sense::click());
                retry_at = Some((target, retry.clicked() || hit.clicked()));
            } else {
                ui.add(egui::Label::new(RichText::new(when_text(started))
                    .color(theme::TEXT_DIM).font(font::caption()))
                    .truncate());
            }
            ui.add(egui::Label::new(RichText::new(human_bytes(size))
                .font(font::caption())).truncate());
            retry_at
        });
    let retry_at = shown.inner;
    let response = shown.response
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    // the retry takes its own click; the tile's click over the same spot
    // is the same answer, so a pointer the scope kept still retries
    let retried = retry_at.is_some_and(|(_, clicked)| clicked);
    if retried || response.clicked() {
        let on_retry = retried || retry_at
            .zip(response.interact_pointer_pos())
            .is_some_and(|((rect, _), pos)| rect.contains(pos));
        match (on_retry, &thumb) {
            // a failed tile is asked for again here and nowhere else:
            // never once per frame for as long as it is on screen
            (true, Some(entry)) => state.forget_thumb(&entry.path),
            _ => files.selected = Some(key),
        }
    }
}

/// The tile's texture, built once from the decoded picture the lane sent,
/// and asked for only after the tile stayed visible (section 4).
/// What a tile without a picture says: waiting, queued and failed are told
/// apart (section 6), never one ellipsis for all of them.
fn tile_caption(state: &BrowserState, thumb: Option<&RemoteEntry>) -> String {
    let Some(entry) = thumb else {
        return "no thumbnail".to_string();
    };
    if entry.unreadable {
        return "name can't be read".to_string();
    }
    match state.thumbs.get(&entry.path) {
        Some(ThumbState::Loading) => "loading…".to_string(),
        Some(ThumbState::Failed(err)) => short_failure(err).to_string(),
        _ => "queued".to_string(),
    }
}

/// A tile is 156 px wide, so a failed one is named in two or three words;
/// the condition's own 5.10 text is on the error card above.
fn short_failure(err: &FtpError) -> &'static str {
    match err {
        FtpError::NotFound => "thumbnail gone",
        FtpError::TooLarge { .. } => "too big to show",
        FtpError::Truncated { .. } | FtpError::SessionLost(_) =>
            "transfer cut",
        FtpError::Local(_) => "not a picture",
        FtpError::UnreadableName => "name can't be read",
        _ => "couldn't load",
    }
}

fn tile_texture(ctx: &egui::Context, state: &mut BrowserState,
                files: &mut FilesUi, view: &View<'_>, entry: &RemoteEntry,
                tile: &mut TileCtx<'_>) -> Option<egui::TextureHandle> {
    let frame = files.frame;
    if let Some(texture) = files.textures.get_mut(&entry.path) {
        texture.used = frame;
        return Some(texture.handle.clone());
    }
    if let Some(image) = state.take_ready_thumb(&entry.path) {
        let handle = ctx.load_texture(format!("tl-{}", entry.path), image,
                                      Default::default());
        evict_textures(state, files);
        files.textures.insert(entry.path.clone(),
                              Texture { handle: handle.clone(), used: frame });
        return Some(handle);
    }
    // every visible tile keeps its 500 ms timer running, but only one
    // request is in flight (section 4)
    let ready = files.visible.ready(&entry.path, view.now);
    // the gate is a deadline, not a poll: the thumbnail of a tile that is
    // still waiting depends on this repaint (E32)
    if let Some(left) = files.visible.wait(&entry.path, view.now) {
        ctx.request_repaint_after(left);
    }
    if ready && *tile.may_request
        && let Some(cmd) = state.request_thumb(entry, THUMB_PX)
    {
        *tile.may_request = false;
        tile.out.cmds.push(cmd);
    }
    None
}

fn evict_textures(state: &mut BrowserState, files: &mut FilesUi) {
    if files.textures.len() < TEXTURE_CAP {
        return;
    }
    let mut by_age: Vec<(u64, String)> = files.textures.iter()
        .map(|(path, texture)| (texture.used, path.clone()))
        .collect();
    by_age.sort();
    let drop: Vec<String> = by_age.into_iter()
        .take(files.textures.len() + 1 - TEXTURE_CAP)
        .map(|(_, path)| path)
        .collect();
    drop_textures(state, files, drop);
}

/// A dropped texture also forgets the picture, so a tile that comes back is
/// fetched once more instead of staying blank.
fn drop_textures(state: &mut BrowserState, files: &mut FilesUi,
                 paths: Vec<String>) {
    for path in paths {
        files.textures.remove(&path);
        state.forget_thumb(&path);
    }
}

fn entry_row(ui: &mut Ui, files: &mut FilesUi, entry: &RemoteEntry,
             icon: &str, indent: f32,
             note: Option<(String, Color32)>) -> bool {
    let selected = files.selected.as_deref() == Some(entry.path.as_str());
    let response = ui.horizontal(|ui| {
        // a nested level steps in from the left, outside the frame: every
        // row's right edge, and with it the size and date columns, stays
        // where the other rows have it (C11, D25)
        if indent > 0.0 {
            ui.add_space(indent);
        }
        // the row is the widget: hover, pressed and focus land on it and
        // its selection is a ring over the same hairline (C2, C11, E24)
        widgets::clickable(
            ui, ("row", entry.path.as_str()), &entry.name,
            widgets::Surface::card()
                .radius(radius::CONTROL)
                .padding(pad::ROW)
                .selected(selected, true),
            |ui| {
                ui.set_width(ui.available_width());
                let caption = font::caption();
                let dim = theme::TEXT_DIM;
                // the columns on the right are laid out first, in slots, so
                // they line up from row to row and the name gives way
                egui::Sides::new().shrink_left().show(ui,
                    |ui| {
                        widgets::slot(ui, "▾ DIR",
                                      RichText::new(icon).color(dim),
                                      &font::label(), Align::Min);
                        let name = match entry.unreadable {
                            true => RichText::new(&entry.name).color(dim)
                                .italics(),
                            false => RichText::new(&entry.name)
                                .font(font::body()),
                        };
                        ui.add(egui::Label::new(name).truncate());
                    },
                    |ui| {
                        // the widest date and size the helpers write: a
                        // time of day is wider than a year
                        widgets::slot(ui, "May 30 23:59",
                                      RichText::new(when_text(entry.mtime))
                                          .color(dim),
                                      &caption, Align::Max);
                        let size = match entry.is_dir {
                            true => String::new(),
                            false => human_bytes(entry.size),
                        };
                        widgets::slot(ui, "1023 MB",
                                      RichText::new(size).color(dim),
                                      &caption, Align::Max);
                        // the per-row transfer state of section 6, which
                        // leaves the name at least half of what is left
                        if let Some((text, color)) = &note {
                            let width = theme::text_width(ui, text, &caption)
                                .min(ui.available_width() / 2.0);
                            ui.allocate_ui_with_layout(
                                vec2(width, size::CONTROL_H),
                                egui::Layout::right_to_left(Align::Center),
                                |ui| {
                                    ui.set_width(width);
                                    ui.add(egui::Label::new(
                                        RichText::new(text).color(*color)
                                            .font(caption.clone()))
                                        .truncate());
                                });
                        }
                    });
            })
            .response
    }).inner;
    // a name the listing could not decode is not a target, so the cursor
    // stays as it is (A11, E23)
    let response = match entry.unreadable {
        true => response,
        false => response.on_hover_cursor(egui::CursorIcon::PointingHand),
    };
    let clicked = response.clicked() && !entry.unreadable;
    if clicked && !entry.is_dir {
        files.selected = Some(entry.path.clone());
    }
    clicked
}

fn file_row(ui: &mut Ui, files: &mut FilesUi, item: &FileItem,
            companion: bool, note: Option<(String, Color32)>) {
    if companion {
        let plate = item.plate_hint
            .map(|plate| format!(" (plate {plate})"))
            .unwrap_or_default();
        let selected = files.selected.as_deref()
            == Some(item.remote.path.as_str());
        ui.horizontal(|ui| {
            ui.add_space(space::XL);
            // the selection is painted behind the line, in the row's own
            // colours, so selecting it shows and moves nothing (C11, D21)
            let backdrop = ui.painter().add(egui::Shape::Noop);
            let text = format!("↳ printer's extracted copy: {}  {}{plate}",
                               item.remote.name,
                               human_bytes(item.remote.size));
            // one line, so it never runs under the detail pane (D15)
            let response = ui.add(egui::Label::new(RichText::new(text)
                .color(theme::TEXT_DIM).font(font::caption()))
                .sense(Sense::click())
                .truncate())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if selected {
                let rect = egui::Rect::from_min_max(
                    egui::pos2(response.rect.min.x - pad::ROW.leftf(),
                               ui.min_rect().min.y),
                    egui::pos2(ui.max_rect().max.x, ui.min_rect().max.y));
                ui.painter().set(backdrop, egui::Shape::Rect(
                    egui::epaint::RectShape::new(
                        rect, radius::CONTROL, theme::CARD_HOVER,
                        Stroke::new(stroke::SELECTED, theme::ACCENT),
                        egui::StrokeKind::Inside)));
            }
            if response.clicked() {
                files.selected = Some(item.remote.path.clone());
            }
        });
        return;
    }
    entry_row(ui, files, &item.remote, kind_icon(item.kind), 0.0, note);
}

fn folder_row(ui: &mut Ui, files: &mut FilesUi, entry: &RemoteEntry,
              depth: usize, open: bool) -> bool {
    let icon = match (entry.is_dir, open) {
        (true, true) => "▾ DIR",
        (true, false) => "▸ DIR",
        (false, _) => "FILE",
    };
    entry_row(ui, files, entry, icon, space::XL * depth as f32, None)
}

fn kind_icon(kind: FileKind) -> &'static str {
    match kind {
        FileKind::SentJob => "3MF",
        FileKind::CacheProject => "3MF",
        FileKind::CacheGcode | FileKind::PlainGcode => "G",
        FileKind::BuiltIn => "STD",
        FileKind::Other => "···",
    }
}

fn kind_label(kind: FileKind) -> &'static str {
    match kind {
        FileKind::SentJob => "sent from Studio",
        FileKind::CacheProject => "printer's print cache (project)",
        FileKind::CacheGcode => "printer's print cache (G-code)",
        FileKind::BuiltIn => "built-in sample",
        FileKind::PlainGcode => "plain G-code",
        FileKind::Other => "file",
    }
}

// ------------------------------------------------------------ detail pane

/// What the detail pane is showing, cloned out of the browser state so the
/// pane can then borrow that state mutably to start a download or a
/// preview.
enum Selected {
    Timelapse {
        stem: String,
        video: Option<RemoteEntry>,
        thumb: Option<RemoteEntry>,
        started: Option<NaiveDateTime>,
        ended: Option<NaiveDateTime>,
    },
    Recording(RemoteEntry),
    File(Box<FileItem>),
    /// an "Other folder" entry
    Entry(RemoteEntry),
    /// selected, but not in the latest listing: a refresh dropped it
    Gone(String),
    None,
}

fn selection(state: &BrowserState, files: &FilesUi) -> Selected {
    let Some(selected) = files.selected.as_deref() else {
        return Selected::None;
    };
    if let Some(item) = state.timelapses.iter()
        .find(|item| item.video.as_ref().or(item.thumb.as_ref())
            .is_some_and(|entry| entry.path == selected))
    {
        return Selected::Timelapse {
            stem: item.stem().to_string(),
            video: item.video.clone(),
            thumb: item.thumb.clone(),
            started: item.started,
            ended: item.ended,
        };
    }
    if let Some(entry) = state.recordings.iter()
        .find(|entry| entry.path == selected)
    {
        return Selected::Recording(entry.clone());
    }
    if let Some(item) = state.files.iter()
        .find(|item| item.remote.path == selected)
    {
        return Selected::File(Box::new(item.clone()));
    }
    // something a refresh dropped, or a file inside an opened folder
    state.dirs.values()
        .filter_map(|dir| match dir {
            DirState::Ready { entries, .. } => Some(entries),
            _ => None,
        })
        .flatten()
        .find(|entry| entry.path == selected)
        .map_or_else(|| Selected::Gone(selected.to_string()),
                     |entry| Selected::Entry(entry.clone()))
}

/// Timelapses and `/ipcam` recordings are AVI. The player sniffs the real
/// format when it opens the file (5.8), so this only decides whether the
/// app's own player is offered, never what the player believes: a `.avi`
/// that is not one ends on the error card, which is the documented path.
fn is_playable(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".avi")
}

/// Whether this local file may be handed to the Windows shell (stage 3
/// security review, F2).
///
/// A different question from [`is_playable`], which reads a remote name to
/// decide whether to offer the app's own player. The shell picks its
/// program by extension and runs whatever it finds, so this answer comes
/// from the file on disk — header and extension both — and never from a
/// name the printer chose.
///
/// The view does not read the file itself: `main.rs` answers, and memoises,
/// so nothing touches the disk on the paint path (5.1, rule 5). With no
/// answer available the button is not offered, which is the closed side.
fn may_reach_shell(view: &View<'_>, path: &Path) -> bool {
    view.shell_openable.is_some_and(|openable| openable(path))
}

fn detail_pane(ui: &mut Ui, state: &mut BrowserState, files: &mut FilesUi,
               view: &View<'_>, height: f32, out: &mut Outcome) {
    // found once per (revision, selection), not once per frame (E27)
    let key = (state.revision(), files.selected.clone());
    let picked = match files.chosen.take() {
        Some((found, picked)) if found == key => picked,
        _ => selection(state, files),
    };
    // what does not fit the body scrolls inside the pane, actions included,
    // instead of pushing them and the footer off the window (C12, D01)
    let inner = (height - pad::CARD.sum().y - 2.0 * stroke::HAIRLINE)
        .max(0.0);
    card_frame(ui, |ui| {
        ui.set_width(ui.available_width());
        // salted: the list and this pane share one stable id (D33)
        egui::ScrollArea::vertical()
            .id_salt(("files-detail", view.serial))
            .max_height(inner)
            .auto_shrink([false, true])
            .show(ui, |ui| {
        ui.label(RichText::new("DETAILS").color(theme::TEXT_DIM)
            .font(font::label()));
        ui.add_space(space::S);
        // by reference: the pane keeps the found selection for the next
        // frame instead of taking it apart (E27)
        match &picked {
            Selected::None => {
                ui.label(RichText::new("Select a file to see its details.")
                    .color(theme::TEXT_DIM).font(font::caption()));
            }
            // the pane says the file went, instead of falling back to
            // "Select a file" while it stays selected (C12, D21)
            Selected::Gone(path) => {
                let name = path.rsplit('/').next().unwrap_or(path);
                ui.add(egui::Label::new(RichText::new(name)
                    .font(font::body_strong())).wrap());
                ui.add_space(space::XS);
                ui.label(RichText::new("Not in the latest listing.")
                    .color(theme::TEXT_DIM).font(font::caption()));
            }
            Selected::Timelapse { stem, video, thumb, started, ended } => {
                ui.add(egui::Label::new(RichText::new(stem.as_str())
                    .font(font::body_strong())).wrap());
                ui.add_space(space::XS);
                facts(ui, |ui| {
                    fact(ui, "PRINT", &format!("{} → {}",
                                               when_text(*started),
                                               when_text(*ended)));
                    match video {
                        Some(video) => fact(ui, "VIDEO",
                            &format!("{}  ·  {}", human_bytes(video.size),
                                     extension(&video.name))),
                        None => fact(ui, "VIDEO",
                                     "deleted; only the thumbnail is left"),
                    }
                    if let Some(thumb) = thumb {
                        fact(ui, "THUMB", &human_bytes(thumb.size));
                    }
                });
                clock_note(ui, view);
                // an orphan thumbnail has nothing to download (5.4)
                if let Some(video) = video {
                    let playable = is_playable(&video.name);
                    file_actions(ui, state, view, video, playable, out);
                }
            }
            Selected::Recording(entry) => {
                ui.add(egui::Label::new(RichText::new(&entry.name)
                    .font(font::body_strong())).wrap());
                ui.add_space(space::XS);
                facts(ui, |ui| {
                    fact(ui, "SIZE", &human_bytes(entry.size));
                    fact(ui, "TIME", &when_text(entry.mtime));
                });
                clock_note(ui, view);
                let playable = is_playable(&entry.name);
                file_actions(ui, state, view, entry, playable, out);
            }
            Selected::File(item) => {
                ui.add(egui::Label::new(RichText::new(&item.remote.name)
                    .font(font::body_strong())).wrap());
                ui.add_space(space::XS);
                facts(ui, |ui| {
                    fact(ui, "KIND", kind_label(item.kind));
                    fact(ui, "SIZE", &human_bytes(item.remote.size));
                    fact(ui, "TIME", &when_text(item.remote.mtime));
                    if let Some(plate) = item.plate_hint {
                        fact(ui, "PLATE", &plate.to_string());
                    }
                    if let Some(root) = &item.companion_of {
                        fact(ui, "COPY OF", root);
                    }
                });
                clock_note(ui, view);
                if item.remote.name.to_ascii_lowercase().ends_with(".3mf") {
                    threemf_pane(ui, state, files, view, item, out);
                } else if matches!(item.kind, FileKind::PlainGcode
                                   | FileKind::CacheGcode)
                {
                    ui.add_space(space::S);
                    gcode_pane(ui, state, view, &item.remote, out);
                }
                file_actions(ui, state, view, &item.remote, false, out);
            }
            Selected::Entry(entry) => {
                ui.add(egui::Label::new(RichText::new(&entry.name)
                    .font(font::body_strong())).wrap());
                ui.add_space(space::XS);
                facts(ui, |ui| {
                    fact(ui, "PATH", &entry.path);
                    if !entry.is_dir {
                        fact(ui, "SIZE", &human_bytes(entry.size));
                    }
                    fact(ui, "TIME", &when_text(entry.mtime));
                });
                clock_note(ui, view);
                if !entry.is_dir && !entry.unreadable {
                    let playable = is_playable(&entry.name);
                    file_actions(ui, state, view, entry, playable, out);
                }
            }
        }
            });
    });
    // kept for the next frame, with the key it was found for
    files.chosen = Some((key, picked));
}

/// The actions of section 6 for one remote file. A file already on disk
/// offers the players and the folder; one that is not offers the downloads,
/// each with the time it will cost.
fn file_actions(ui: &mut Ui, state: &mut BrowserState, view: &View<'_>,
                remote: &RemoteEntry, playable: bool, out: &mut Outcome) {
    ui.add_space(space::M);
    // The borrow ends here, so the buttons below can start a download. A
    // copy this session downloaded is the first answer; after it, a
    // complete key-matching copy already in the disk cache, so a file that
    // is on disk from an earlier session is not advertised as a download
    // with six minutes on it (5.6).
    let local = state.local_copy(&remote.path).map(Path::to_path_buf)
        .or_else(|| view.cached.and_then(|cached| cached(remote)));
    let width = ui.available_width();
    // a file already on its way is not downloaded twice: the buttons say
    // so instead of dropping the click (D11, browser.rs `download`)
    let busy = state.transfer_of(&remote.path)
        .is_some_and(TransferUi::active)
        .then_some("Already downloading");
    // and a player still opening takes no second file (E31, D07)
    let opening = view.opening.then_some("Opening the last file…");
    if view.opening {
        ui.label(RichText::new("Opening…").color(theme::TEXT_DIM)
            .font(font::caption()));
    }
    match &local {
        Some(path) => {
            if playable
                && accent_button_reason(ui, "Play",
                                        vec2(width, size::BUTTON_H), opening)
            {
                // the remote path decides the speed, not the cached name
                out.actions.push(Action::Play {
                    path: path.clone(),
                    speed: player::default_speed(&remote.path) });
            }
            // a saved copy of anything: the shell only gets a file whose
            // header and extension agree that it is media (F2)
            let openable = may_reach_shell(view, path);
            ui.horizontal(|ui| os_player_buttons(ui, path, openable, out));
            // a key-matching copy is copied, never downloaded again (5.6)
            if widgets::button(ui, egui::Button::new("Save to PC"), busy)
                && let Some(cmd) = state.download(remote, Dest::SaveToPc)
            {
                out.cmds.push(cmd);
            }
        }
        None => {
            ui.label(RichText::new(format!(
                "Download {}", download_eta(remote.size, state.rate_bps)))
                .color(theme::TEXT_DIM).font(font::caption()));
            if playable
                && accent_button_reason(ui, "Download & play",
                                        vec2(width, size::BUTTON_H),
                                        busy.or(opening))
                && let Some(cmd) = state.download(
                    remote, Dest::Cache { open_after: true })
            {
                out.cmds.push(cmd);
            }
            if widgets::button(ui, egui::Button::new("Save to PC"), busy)
                && let Some(cmd) = state.download(remote, Dest::SaveToPc)
            {
                out.cmds.push(cmd);
            }
        }
    }
    if let Some((text, color)) = note_for(state, &remote.path, view.now) {
        ui.label(RichText::new(text).color(color).font(font::caption()));
    }
}

/// The 3mf preview: automatic for small files, "[Load]" for the rest
/// (design doc 4), then what the file says about itself (5.7).
fn threemf_pane(ui: &mut Ui, state: &mut BrowserState, files: &mut FilesUi,
                view: &View<'_>, item: &FileItem, out: &mut Outcome) {
    // 1 MB, or 256 KB while the printer prints (design doc 4)
    let cap = match view.printing {
        true => AUTO_PREVIEW_PRINTING,
        false => AUTO_PREVIEW_MAX,
    };
    ui.add_space(space::S);
    let known = state.details.contains_key(&item.remote.path);
    if !known && item.remote.size <= cap {
        if let Some(cmd) = state.request_details(&item.remote,
                                                 item.plate_hint)
        {
            out.cmds.push(cmd);
        }
    } else if !known {
        // too big to load on its own: it is a download, so the user says
        // when, and the cost is on the button (section 6)
        ui.label(RichText::new(format!(
            "preview: {}  ·  {}", human_bytes(item.remote.size),
            download_eta(item.remote.size, state.rate_bps)))
            .color(theme::TEXT_DIM).font(font::caption()));
        if ui.button("Load preview").clicked()
            && let Some(cmd) = state.request_details(&item.remote,
                                                     item.plate_hint)
        {
            out.cmds.push(cmd);
        }
    }
    // the detail is read where it is: cloning it copied the decoded
    // plate picture on every frame (E26, D36). The one change it can ask
    // for waits until the borrow is over.
    let mut forget = false;
    match state.details.get(&item.remote.path) {
        Some(DetailState::Loading) => {
            ui.label(RichText::new("reading the 3mf…")
                .color(theme::TEXT_DIM).font(font::caption()));
        }
        Some(DetailState::Failed(err)) => {
            ui.add(egui::Label::new(RichText::new(err.text(view.serial))
                .color(theme::DANGER).font(font::caption())).wrap());
            forget = ui.button("⟳ retry").clicked();
        }
        Some(DetailState::Ready(three)) => {
            plate_picture(ui, files, &item.remote.path, three);
            threemf_facts(ui, three);
        }
        None => {}
    }
    if forget {
        state.forget_details(&item.remote.path);
    }
}

/// The sliced plate's picture, decoded once and kept while it is shown.
fn plate_picture(ui: &mut Ui, files: &mut FilesUi, path: &str,
                 three: &ThreeMf) {
    // decoded and downscaled on the lane thread; this only uploads it
    // (5.1, rule 5), like `Event::Thumb` does for a tile
    let Some(picture) = &three.plate else { return };
    let stale = files.preview.as_ref()
        .is_none_or(|(shown, _)| shown != path);
    if stale {
        let handle = ui.ctx().load_texture(format!("plate-{path}"),
                                           picture.0.clone(),
                                           Default::default());
        files.preview = Some((path.to_string(), handle));
    }
    if let Some((shown, handle)) = &files.preview
        && shown == path
    {
        let size = handle.size_vec2();
        let scale = (ui.available_width() / size.x).min(1.0);
        ui.add(egui::Image::new((handle.id(), size))
            .fit_to_exact_size(size * scale)
            .corner_radius(radius::MEDIA));
    }
}

/// What a 3mf says about itself (5.7).
fn threemf_facts(ui: &mut Ui, three: &ThreeMf) {
    facts(ui, |ui| threemf_fact_rows(ui, three));
}

fn threemf_fact_rows(ui: &mut Ui, three: &ThreeMf) {
    let info = &three.info;
    if let Some(plate) = info.plate {
        fact(ui, "PLATE", &plate.to_string());
    }
    if !info.printer_model.is_empty() {
        fact(ui, "SLICED FOR", &info.printer_model);
    }
    if let Some(seconds) = info.prediction_s {
        fact(ui, "TIME", &duration_text(seconds as f32));
    }
    if let Some(grams) = info.weight_g {
        fact(ui, "WEIGHT", &format!("{grams:.2} g"));
    }
    match (info.layers, info.max_z_mm) {
        (Some(layers), Some(z)) =>
            fact(ui, "LAYERS", &format!("{layers}  ·  {z:.1} mm")),
        (Some(layers), None) => fact(ui, "LAYERS", &layers.to_string()),
        _ => {}
    }
    if !info.bed_type.is_empty() {
        fact(ui, "BED", &info.bed_type);
    }
    for filament in &info.filaments {
        let mut line = filament.kind.clone();
        if !filament.color.is_empty() {
            line.push_str(&format!("  {}", filament.color));
        }
        if let Some(grams) = filament.used_g {
            line.push_str(&format!("  ·  {grams:.2} g"));
        }
        fact(ui, "FILAMENT", &line);
    }
    if !info.objects.is_empty() {
        fact(ui, "OBJECTS", &info.objects.len().to_string());
    }
    for warning in &info.warnings {
        ui.add(egui::Label::new(RichText::new(warning)
            .color(theme::WARN).font(font::caption())).wrap());
    }
}

/// "Read header (~2 s)" (5.7): never automatic, and it costs a session of
/// its own, so only the button starts it.
fn gcode_pane(ui: &mut Ui, state: &mut BrowserState, view: &View<'_>,
              remote: &RemoteEntry, out: &mut Outcome) {
    // read where it is, and the two changes it can ask for wait until
    // the borrow is over (E26)
    let (mut read, mut forget) = (false, false);
    match state.headers.get(&remote.path) {
        None => {
            read = ui.button("Read header (~2 s)").clicked();
        }
        Some(HeaderState::Loading) => {
            ui.label(RichText::new("reading the header…")
                .color(theme::TEXT_DIM).font(font::caption()));
        }
        Some(HeaderState::Failed(err)) => {
            ui.add(egui::Label::new(RichText::new(err.text(view.serial))
                .color(theme::DANGER).font(font::caption())).wrap());
            forget = ui.button("⟳ retry").clicked();
        }
        Some(HeaderState::Ready(header)) => {
            facts(ui, |ui| {
                if let Some(seconds) = header.prediction_s {
                    fact(ui, "TIME", &duration_text(seconds as f32));
                }
                if let Some(layers) = header.layers {
                    fact(ui, "LAYERS", &layers.to_string());
                }
                if let Some(grams) = header.weight_g {
                    fact(ui, "WEIGHT", &format!("{grams:.2} g"));
                }
                if let Some(z) = header.max_z_mm {
                    fact(ui, "HEIGHT", &format!("{z:.1} mm"));
                }
            });
            // a head read that stopped early is never shown as the whole
            // truth (5.7)
            if !header.complete {
                ui.label(RichText::new(
                    "the header was cut short; this is what it carried")
                    .color(theme::TEXT_DIM).font(font::caption()));
            }
        }
    }
    if read && let Some(cmd) = state.request_header(remote) {
        out.cmds.push(cmd);
    }
    if forget {
        state.forget_header(&remote.path);
    }
}

/// A run of facts, `space::XS` apart (C12).
fn facts(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.scope(|ui| {
        theme::tight_stack(ui);
        add(ui);
    });
}

/// One fact: its label in a fixed column, so the values line up, and the
/// value wrapping inside the column beside it (C12).
fn fact(ui: &mut Ui, label: &str, value: &str) {
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(vec2(size::FACT_LABEL_W, 0.0),
            egui::Layout::left_to_right(Align::Min), |ui| {
            ui.set_width(size::FACT_LABEL_W);
            ui.add(egui::Label::new(RichText::new(label)
                .color(theme::TEXT_DIM).font(font::label())).truncate());
        });
        ui.add(egui::Label::new(RichText::new(value).font(font::caption())
            .color(theme::TEXT)).wrap());
    });
}

fn clock_note(ui: &mut Ui, view: &View<'_>) {
    let mut note = "times are the printer's clock".to_string();
    if view.printing {
        note.push_str(", and it is printing");
    }
    ui.add_space(space::S);
    ui.label(RichText::new(note).color(theme::TEXT_DIM).font(font::caption()));
}

// --------------------------------------------------------------- helpers

fn extension(name: &str) -> String {
    name.rsplit_once('.').map(|(_, ext)| ext.to_uppercase())
        .unwrap_or_else(|| "FILE".to_string())
}

/// Printer-clock time, never converted (section 6). A LIST line that
/// carried a year instead of a clock has no time of day, so it shows the
/// year rather than a midnight the printer never reported.
fn when_text(when: Option<NaiveDateTime>) -> String {
    match when {
        Some(when) if when.time() == NaiveTime::MIN =>
            when.format("%b %d %Y").to_string(),
        Some(when) => when.format("%b %d %H:%M").to_string(),
        None => "—".to_string(),
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    match value < 10.0 {
        true => format!("{value:.1} {}", UNITS[unit]),
        false => format!("{value:.0} {}", UNITS[unit]),
    }
}

/// Asks for a repaint when a counter that reads whole seconds changes,
/// instead of ten times a second (E32, D34). `since` is what the counter
/// counts from, so the repaint lands on its own boundary.
fn tick_seconds(ctx: &egui::Context, since: Instant, now: Instant) {
    let elapsed = now.saturating_duration_since(since);
    let into_second = Duration::from_nanos(u64::from(elapsed.subsec_nanos()));
    ctx.request_repaint_after(Duration::from_secs(1) - into_second);
}

/// What the card and its directories hold, for the status line. The parts
/// never count the same file twice.
fn size_parts(state: &BrowserState) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    for name in SPACE_DIRS {
        let dir = format!("/{name}");
        if !matches!(state.dirs.get(&dir), Some(DirState::Ready { .. })) {
            continue;
        }
        let bytes = state.used_bytes(&dir);
        if bytes > 0 {
            parts.push(format!("{name} {}", human_bytes(bytes)));
        }
    }
    let total = state.total_bytes();
    if total > 0 {
        parts.push(format!("total {}", human_bytes(total)));
    }
    parts
}

fn ago(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        0..=59 => format!("{secs} s"),
        60..=3599 => format!("{} min", secs / 60),
        3600..=86399 => format!("{} h", secs / 3600),
        _ => format!("{} d", secs / 86400),
    }
}

/// The view of section 6 against the states `BrowserState` can be in: the
/// refusal card and its lack of any trust action, the model gating, the
/// session line gate G4 reads, the listing plan's empty, missing and error
/// cards, companion grouping, "Other folders" one level at a time, the
/// prefetch rule and the virtualised grid.
///
/// Every test renders the real view through `Context::run_ui` and reads the
/// text it painted, so what is asserted is what the user sees.
#[cfg(test)]
mod tests {
    use egui::{Modifiers, Pos2, Rect, Vec2};

    use super::*;
    use crate::browser::{Event, PREFETCH_VISIBLE, ThumbState};
    use crate::ftp::{DateRule, parse_list_line};
    use crate::tls::testkit::has_serial_run;
    use crate::tls::{PrinterCertError, Refusal};

    const A1: &str = "03900Z9X8W7V6U5";
    const P1S: &str = "01P00Z9X8W7V6U5";
    const H2C: &str = "31B00Z9X8W7V6U5";

    const ROOT: [&str; 7] = [
        "drw-rw-rw-   1 root  root         0 Oct 08 2025 cache",
        "drw-rw-rw-   1 root  root         0 Jan 01 1980 timelapse",
        "drw-rw-rw-   1 root  root         0 Jan 01 1980 ipcam",
        "drw-rw-rw-   1 root  root         0 Oct 27 2025 image",
        "drw-rw-rw-   1 root  root         0 Oct 27 2025 logger",
        "drw-rw-rw-   1 root  root         0 Oct 27 2025 spool",
        "-rw-rw-rw-   1 root  root     52341 Sep 07 19:38 job.gcode.3mf",
    ];
    const CACHE: [&str; 3] = [
        "-rw-rw-rw-   1 root  root  41123456 Sep 07 19:39 job_plate_1.gcode",
        "-rw-rw-rw-   1 root  root     11234 Sep 07 19:39 1_job.gcode.bbl",
        "-rw-rw-rw-   1 root  root     52341 Sep 07 19:39 job.3mf",
    ];
    const TIMELAPSE: [&str; 2] = [
        "drw-rw-rw-   1 root  root         0 May 30 05:16 thumbnail",
        "-rw-rw-rw-   1 root  root   4411548 Jun 01 06:17 \
         video_2026-06-01_06-11-57.avi",
    ];
    const THUMBS: [&str; 1] = [
        "-rw-rw-rw-   1 root  root     19830 Jun 01 06:17 \
         video_2026-06-01_06-11-57.jpg",
    ];
    const IPCAM: [&str; 2] = [
        "-rw-rw-rw-   1 root  root  13421772 Jun 01 06:17 \
         ipcam-record.2026-06-01.1.avi",
        "-rw-rw-rw-   1 root  root  13421772 Jun 02 06:17 \
         ipcam-record.2026-06-02.1.avi",
    ];
    const THUMB_PATH: &str =
        "/timelapse/thumbnail/video_2026-06-01_06-11-57.jpg";

    /// A context with the app's fonts, so `theme::bold` resolves.
    fn ctx() -> egui::Context {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx);
        // a tooltip waits for a real pointer to rest; there is none here,
        // so the wait goes and what it would say is painted
        ctx.all_styles_mut(|style| {
            style.interaction.tooltip_delay = 0.0;
            style.interaction.show_tooltips_only_when_still = false;
        });
        ctx
    }

    fn raw(events: Vec<egui::Event>) -> egui::RawInput {
        raw_at(Vec2::new(1180.0, 820.0), events)
    }

    /// The same, on a window of a given size: a short one paints fewer rows.
    fn raw_at(size: Vec2, events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
            events,
            ..Default::default()
        }
    }

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true,
                           repeat: false, modifiers: Modifiers::default() }
    }

    /// What one rendered frame painted, and what the view asked for.
    struct Painted {
        spots: Vec<(String, Pos2)>,
        out: Outcome,
    }

    impl Painted {
        fn has(&self, needle: &str) -> bool {
            self.spots.iter().any(|(text, _)| text.contains(needle))
        }

        fn exact(&self, text: &str) -> bool {
            self.spots.iter().any(|(painted, _)| painted == text)
        }

        fn spot(&self, label: &str) -> Option<Pos2> {
            self.spots.iter().find(|(text, _)| text == label)
                .or_else(|| self.spots.iter()
                    .find(|(text, _)| text.contains(label)))
                .map(|(_, pos)| *pos)
        }

        fn lists(&self) -> Vec<String> {
            self.out.cmds.iter()
                .filter_map(|cmd| match cmd {
                    Cmd::List { dir, .. } => Some(dir.clone()),
                    _ => None,
                })
                .collect()
        }

        fn thumbs(&self) -> Vec<String> {
            self.out.cmds.iter()
                .filter_map(|cmd| match cmd {
                    Cmd::Thumb { remote, .. } => Some(remote.path.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    fn collect(shape: &egui::Shape, spots: &mut Vec<(String, Pos2)>) {
        match shape {
            egui::Shape::Text(text) => spots.push((
                text.galley.text().to_string(),
                text.pos + text.galley.rect.size() * 0.5)),
            egui::Shape::Vec(shapes) =>
                shapes.iter().for_each(|shape| collect(shape, spots)),
            _ => {}
        }
    }

    fn frame(ctx: &egui::Context, input: egui::RawInput,
             state: &mut BrowserState, files: &mut FilesUi,
             view: &View<'_>) -> Painted {
        let mut out = Outcome::default();
        let full = ctx.run_ui(input, |ui| {
            out = show(ui, state, files, view);
        });
        let mut spots = Vec::new();
        for clipped in &full.shapes {
            collect(&clipped.shape, &mut spots);
        }
        Painted { spots, out }
    }

    /// Hover, press and release over whatever painted `label`.
    fn click(ctx: &egui::Context, state: &mut BrowserState,
             files: &mut FilesUi, view: &View<'_>, label: &str) -> Outcome {
        let spot = frame(ctx, raw(Vec::new()), state, files, view)
            .spot(label)
            .unwrap_or_else(|| panic!("nothing painted for {label:?}"));
        let button = |pressed| egui::Event::PointerButton {
            pos: spot, button: egui::PointerButton::Primary, pressed,
            modifiers: Modifiers::default(),
        };
        let moved = egui::Event::PointerMoved(spot);
        frame(ctx, raw(vec![moved.clone()]), state, files, view);
        frame(ctx, raw(vec![moved.clone(), button(true)]), state, files, view);
        frame(ctx, raw(vec![moved, button(false)]), state, files, view).out
    }

    /// The pointer resting over whatever painted `label`, and what that
    /// frame painted — a disabled control's reason among it.
    ///
    /// It builds a context of its own: egui keeps one tooltip open per
    /// context, and a second hover in the same one never opens a tooltip
    /// for the widget it moved to.
    fn hover(state: &mut BrowserState, files: &mut FilesUi,
             view: &View<'_>, label: &str) -> Painted {
        let ctx = ctx();
        // one frame to size everything, so the label is where it will stay
        frame(&ctx, raw(Vec::new()), state, files, view);
        let spot = frame(&ctx, raw(Vec::new()), state, files, view)
            .spot(label)
            .unwrap_or_else(|| panic!("nothing painted for {label:?}"));
        let moved = egui::Event::PointerMoved(spot);
        // a tooltip opens once the pointer has rested on the widget past
        // the delay, so the clock moves with the frames, forwards only
        let start = ctx.input(|input| input.time);
        let timed = |time: f64, events: Vec<egui::Event>| egui::RawInput {
            time: Some(start + time),
            ..raw(events)
        };
        frame(&ctx, timed(0.1, vec![moved.clone()]), state, files, view);
        frame(&ctx, timed(0.8, vec![moved]), state, files, view)
    }

    fn entries(dir: &str, lines: &[&str]) -> Vec<RemoteEntry> {
        lines.iter()
            .filter_map(|line| parse_list_line(dir, line,
                                               DateRule::CalendarYear, 2026))
            .collect()
    }

    fn listed(state: &mut BrowserState, dir: &str, lines: &[&str]) {
        let generation = state.generation();
        let result = Ok(entries(dir, lines));
        state.apply(Event::Listed { dir: dir.to_string(), generation,
                                    result });
    }

    /// A printer whose root, cache, timelapses and thumbnails are listed.
    fn browsed() -> BrowserState {
        let mut state = BrowserState::default();
        let _ = state.refresh();
        listed(&mut state, "/", &ROOT);
        listed(&mut state, "/cache", &CACHE);
        listed(&mut state, "/timelapse", &TIMELAPSE);
        listed(&mut state, "/timelapse/thumbnail", &THUMBS);
        state
    }

    /// A printer with `count` timelapses, each with its thumbnail: more
    /// tiles than one window shows.
    fn browsed_tiles(count: usize) -> BrowserState {
        let mut state = BrowserState::default();
        let _ = state.refresh();
        listed(&mut state, "/", &ROOT);
        let videos: Vec<String> = (0..count).map(|index| format!(
            "-rw-rw-rw-   1 root  root   4411548 Jun 01 06:17 \
             video_2026-06-01_06-{index:02}-00.avi")).collect();
        let thumbs: Vec<String> = (0..count).map(|index| format!(
            "-rw-rw-rw-   1 root  root     19830 Jun 01 06:17 \
             video_2026-06-01_06-{index:02}-00.jpg")).collect();
        let lines: Vec<&str> = videos.iter().map(String::as_str).collect();
        listed(&mut state, "/timelapse", &lines);
        let lines: Vec<&str> = thumbs.iter().map(String::as_str).collect();
        listed(&mut state, "/timelapse/thumbnail", &lines);
        state
    }

    fn view(serial: &str, now: Instant) -> View<'_> {
        View { name: "P1S #1", serial, profile: Some(ServerProfile::BblP003),
               open_sessions: 0, printing: false, dialog_open: false, now,
               cache_usage: 0, cache_cap: 5 * 1024 * 1024 * 1024,
               clearing: false, opening: false,
               cached: None, shell_openable: None,
               player: None, player_error: None, player_error_path: None,
               open_error: None }
    }

    fn thumb_image() -> egui::ColorImage {
        egui::ColorImage::from_rgba_unmultiplied([2, 2], &[200; 16])
    }

    fn files_ui(tab: Tab) -> FilesUi {
        FilesUi { tab, ..FilesUi::default() }
    }

    /// The shell verdict a test wants, without a file on disk: `main.rs`
    /// answers this in the app, from the file's header and extension
    /// (`player::openable_by_shell`, stage 3 security review, F2), and the
    /// rule itself is tested there against real files.
    fn shell_says(answer: bool) -> impl Fn(&Path) -> bool {
        move |_: &Path| answer
    }

    // --------------------------------------------------- refusal and models

    /// Section 6 and T25: no trust, accept or continue action exists.
    #[test]
    fn the_refusal_card_offers_no_trust_action() {
        let ctx = ctx();
        let mut state = browsed();
        state.cert_alert =
            Some(Refusal::Cert(PrinterCertError::SerialMismatch));
        let mut files = FilesUi::default();
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, Instant::now()));

        assert!(painted.has("This is not a certificate Bambu Lab issued"));
        assert!(painted.has("the access code was not sent over this \
                             connection"));
        assert!(painted.exact("Edit printer"));
        assert!(painted.exact("Close"));
        for (text, _) in &painted.spots {
            let lower = text.to_lowercase();
            for forbidden in ["trust", "accept", "continue", "anyway",
                              "proceed", "ignore"] {
                assert!(!lower.contains(forbidden),
                        "refusal card offers {forbidden:?}: {text:?}");
            }
        }
        // and it replaces the content: no tab list is shown below it
        assert!(!painted.has("listing /"));
        assert!(painted.out.cmds.is_empty(), "{:?}", painted.out.cmds);
    }

    /// Close is the default: focused, and bound to Enter and Esc.
    #[test]
    fn close_is_the_default_and_answers_enter_and_escape() {
        for pressed in [egui::Key::Enter, egui::Key::Escape] {
            let ctx = ctx();
            let mut state = browsed();
            state.cert_alert = Some(Refusal::HandshakeSignature);
            let mut files = FilesUi::default();
            let now = Instant::now();
            // the first frame asks for the focus, the second holds it
            frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                  &view(P1S, now));
            let second = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                               &view(P1S, now));
            assert!(files.close_has_focus(), "Close is not focused");
            assert!(second.out.actions.is_empty());

            let answer = frame(&ctx, raw(vec![key(pressed)]), &mut state,
                               &mut files, &view(P1S, now));
            assert!(answer.out.actions.contains(&Action::Back),
                    "{pressed:?} did not close the card: {:?}",
                    answer.out.actions);
        }
    }

    /// Section 6: while a dialog this card opened is on screen, the card
    /// takes no focus back and answers no key, so the dialog keeps its own
    /// keyboard.
    #[test]
    fn a_dialog_over_the_refusal_card_keeps_the_keyboard() {
        let ctx = ctx();
        let mut state = browsed();
        state.cert_alert = Some(Refusal::HandshakeSignature);
        let mut files = FilesUi::default();
        let now = Instant::now();
        let blocked = View { dialog_open: true, ..view(P1S, now) };
        frame(&ctx, raw(Vec::new()), &mut state, &mut files, &blocked);
        let second = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                           &blocked);
        assert!(!files.close_has_focus(),
                "the card took the focus from the dialog");
        assert!(second.out.actions.is_empty());
        for pressed in [egui::Key::Enter, egui::Key::Escape] {
            let answer = frame(&ctx, raw(vec![key(pressed)]), &mut state,
                               &mut files, &blocked);
            assert!(answer.out.actions.is_empty(),
                    "{pressed:?} closed the view under the dialog");
        }
    }

    #[test]
    fn edit_printer_is_the_other_action_of_the_refusal_card() {
        let ctx = ctx();
        let mut state = browsed();
        state.cert_alert = Some(Refusal::HandshakeSignature);
        let mut files = FilesUi::default();
        let out = click(&ctx, &mut state, &mut files,
                        &view(P1S, Instant::now()), "Edit printer");
        assert!(out.actions.contains(&Action::EditPrinter),
                "{:?}", out.actions);
    }

    /// T25: H2C, P2S and X2D are refused by name, and the view asks for no
    /// listing at all, so nothing connects.
    #[test]
    fn a_model_refused_by_name_never_asks_for_a_listing() {
        let ctx = ctx();
        let mut state = BrowserState::default();
        let mut files = FilesUi::default();
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(H2C, Instant::now()));

        assert!(painted.has("Bambu Lab H2C uses Bambu's newer certificate \
                             authority (BBL CA2)"));
        assert!(painted.has("file access is disabled for it"));
        assert!(painted.out.cmds.is_empty(), "{:?}", painted.out.cmds);
        assert!(state.dirs.is_empty(), "a refused model listed something");
        // and the card still offers only the two actions
        assert!(painted.exact("Close"));
        assert!(painted.exact("Edit printer"));
    }

    /// T25: every model except A1 and P1S, and every banner but BBL-P003.
    #[test]
    fn unobserved_models_say_not_tested_on_this_model() {
        let tested = "not tested on this model";
        for serial in [A1, P1S] {
            assert_eq!(not_tested_notice(serial,
                                         Some(ServerProfile::BblP003)), None,
                       "{serial}");
        }
        for prefix in ["030", "01S", "00M", "00W", "03W", "094", "093",
                       "239", "26A", "ZZZ"] {
            let serial = format!("{prefix}00Z9X8W7V6U5");
            assert_eq!(not_tested_notice(&serial,
                                         Some(ServerProfile::BblP003)),
                       Some(tested.to_string()), "{prefix}");
        }
        // a tested model on a banner that is not BBL-P003
        for profile in [ServerProfile::Vsftpd, ServerProfile::Unknown] {
            assert_eq!(not_tested_notice(P1S, Some(profile)),
                       Some(tested.to_string()), "{profile:?}");
        }
        // before the banner is known, a tested model says nothing
        assert_eq!(not_tested_notice(A1, None), None);
    }

    /// T25: no refusal or gating text names an unrecognised model by prefix.
    #[test]
    fn no_refusal_text_shows_unknown_with_a_prefix() {
        let ctx = ctx();
        let unknown = "ZZZ00Z9X8W7V6U5";
        let mut state = browsed();
        state.error = Some(FtpError::TlsRejected);
        let mut files = FilesUi::default();
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(unknown, Instant::now()));
        assert!(painted.has("not tested on this model"));
        for (text, _) in &painted.spots {
            assert!(!text.contains("Unknown ("), "{text:?}");
        }
    }

    // --------------------------------------------------------- the header

    /// The line gate G4 reads (section 6).
    #[test]
    fn the_header_shows_the_browse_session_state() {
        let ctx = ctx();
        let start = Instant::now();
        let now = start + Duration::from_secs(10);
        for (conn, expected) in [
            (ConnState::Closed, "FTP session: closed"),
            (ConnState::Connecting { since: start + Duration::from_secs(8) },
             "FTP session: connecting 2 s"),
            (ConnState::Open { idle_since: Some(start
                + Duration::from_secs(5)) },
             "FTP session: open, idle 5 s"),
        ] {
            let mut state = browsed();
            state.conn = conn;
            let mut files = FilesUi::default();
            let painted = frame(&ctx, raw(Vec::new()), &mut state,
                                &mut files, &view(P1S, now));
            assert!(painted.has(expected), "missing {expected:?}");
        }
    }

    #[test]
    fn the_header_sums_used_space_and_dates_the_listing() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = FilesUi::default();
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, Instant::now()));
        // the 4,411,548 B video and its 19,830 B thumbnail
        assert!(painted.has("timelapse 4.2 MB"), "{:?}", painted.spots);
        assert!(painted.has("cache 39 MB"));
        // the whole card follows the directories it contains, and the
        // parts of the line never count the same file twice
        assert!(painted.has(&format!("total {}",
                                     human_bytes(state.total_bytes()))));
        assert!(!painted.has("root "), "the root figure contained the rest");
        assert!(painted.has("updated 0 s ago"));
        assert!(painted.has("(printer clock)"));
        assert!(painted.exact("Refresh"));
    }

    #[test]
    fn the_tabs_count_what_the_listings_hold() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = FilesUi::default();
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, Instant::now()));
        let (timelapses, recordings, print_files) = state.counts();
        assert_eq!((timelapses, recordings), (1, 0));
        assert!(painted.has(&format!("Timelapses {timelapses}")));
        // /ipcam is listed when its tab opens (5.5), so the count waits
        assert!(painted.exact("Recordings"));
        assert!(painted.has(&format!("Print files {print_files}")));
    }

    #[test]
    fn back_leaves_the_view() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = FilesUi::default();
        let out = click(&ctx, &mut state, &mut files,
                        &view(P1S, Instant::now()), "‹ Back");
        assert!(out.actions.contains(&Action::Back), "{:?}", out.actions);
    }

    /// `/ipcam` is listed only when the Recordings tab opens (5.5).
    #[test]
    fn the_recordings_tab_lists_ipcam_when_it_opens() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = FilesUi::default();
        let now = Instant::now();
        let first = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                          &view(P1S, now));
        assert!(first.lists().is_empty(), "{:?}", first.lists());
        assert!(first.exact("Recordings"),
                "a count before /ipcam was listed would be a false zero");

        let out = click(&ctx, &mut state, &mut files, &view(P1S, now),
                        "Recordings");
        assert_eq!(files.tab, Tab::Recordings);
        assert_eq!(out.cmds.iter().filter_map(|cmd| match cmd {
            Cmd::List { dir, .. } => Some(dir.as_str()),
            _ => None,
        }).collect::<Vec<_>>(), ["/ipcam"]);

        // and once it is listed, the recordings show newest first
        listed(&mut state, "/ipcam", &IPCAM);
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, now));
        let rows: Vec<&String> = painted.spots.iter()
            .map(|(text, _)| text)
            .filter(|text| text.starts_with("ipcam-record"))
            .collect();
        assert_eq!(rows, ["ipcam-record.2026-06-02.1.avi",
                          "ipcam-record.2026-06-01.1.avi"]);
        assert!(painted.has("Recordings 2"), "the count arrives with them");
    }

    // ------------------------------------------- listings, empty and error

    #[test]
    fn a_loading_listing_shows_skeletons_and_never_an_empty_reason() {
        let ctx = ctx();
        let mut state = BrowserState::default();
        let _ = state.refresh();
        let mut files = FilesUi::default();
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, Instant::now()));
        assert!(painted.has("listing /timelapse…"));
        assert!(!painted.has("No timelapses on this printer"));
    }

    /// A8: Tab walks the header in the order the eye reads it.
    #[test]
    fn tab_walks_the_header_in_its_visual_order() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = files_ui(Tab::Timelapses);
        let now = Instant::now();
        let view = view(P1S, now);
        // the first frame lays it out; then Tab from nothing focused
        frame(&ctx, raw(Vec::new()), &mut state, &mut files, &view);
        let mut walked: Vec<(String, egui::Pos2)> = Vec::new();
        for _ in 0..5 {
            let painted = frame(&ctx, raw(vec![key(egui::Key::Tab)]),
                                &mut state, &mut files, &view);
            let Some(focused) = ctx.memory(|m| m.focused()) else { continue };
            let Some(response) = ctx.read_response(focused) else { continue };
            let name = painted.spots.iter()
                .filter(|(_, at)| response.rect.contains(*at))
                .map(|(text, _)| text.clone())
                .next()
                .unwrap_or_default();
            walked.push((name, response.rect.min));
        }
        assert!(walked.len() >= 4, "Tab focused {} widgets", walked.len());
        // the first four are the Back button and the three tabs, in the
        // order they are painted, left to right on one line
        // the counts come with the tab labels, so the names are matched
        // by what they start with
        let wanted = ["‹ Back", "Timelapses", "Recordings", "Print files"];
        for (at, (name, _)) in walked.iter().take(4).enumerate() {
            assert!(name.starts_with(wanted[at]),
                    "{at}: {name:?} is not {:?} — {walked:?}", wanted[at]);
        }
        let tops: Vec<f32> = walked.iter().take(4)
            .map(|(_, at)| at.y).collect();
        // a selectable tab is one pixel taller than the Back button, so
        // "one line" is a row height, not the same pixel
        let row = Heights::of(&egui::Ui::new(
            ctx.clone(), egui::Id::new("row-height"),
            egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(
                egui::Pos2::ZERO, vec2(100.0, 100.0))))).row;
        assert!(tops.windows(2).all(|pair| (pair[0] - pair[1]).abs() < row),
                "the header is not one line: {walked:?}");
        let lefts: Vec<f32> = walked.iter().take(4)
            .map(|(_, at)| at.x).collect();
        assert!(lefts.windows(2).all(|pair| pair[0] < pair[1]),
                "Tab does not walk left to right: {walked:?}");
    }

    /// A9, A10, D39: every node a screen reader can click has a name and a
    /// role, and a progress bar says how far along it is.
    #[test]
    fn every_clickable_node_has_a_name_and_a_role() {
        use egui::accesskit::{Action, Role};

        let ctx = ctx();
        ctx.enable_accesskit();
        let mut state = browsed();
        let mut files = files_ui(Tab::Timelapses);
        let video = state.timelapses[0].video.clone()
            .expect("a timelapse with a video");
        files.selected = Some(video.path.clone());
        // a transfer running, so its bar and its row are in the tree
        let id = match state.download(&video, Dest::SaveToPc) {
            Some(Cmd::Download { id, .. }) => id,
            other => panic!("no download: {other:?}"),
        };
        state.apply(Event::Progress { id, done: 1_000,
                                      total: 4_000, bytes_per_s: 500.0 });
        let now = Instant::now();
        let view = view(P1S, now);
        frame(&ctx, raw(Vec::new()), &mut state, &mut files, &view);
        let full = ctx.run_ui(raw(Vec::new()), |ui| {
            show(ui, &mut state, &mut files, &view);
        });
        let update = full.platform_output.accesskit_update
            .expect("an AccessKit tree");
        let mut clickable = 0;
        let mut bars = 0;
        for (id, node) in &update.nodes {
            if node.supports_action(Action::Click) {
                clickable += 1;
                // a Label carries its text in `value`, everything else in
                // `label` (egui `fill_accesskit_node_from_widget_info`)
                let name = node.label().or_else(|| node.value());
                assert!(name.is_some_and(|name| !name.is_empty()),
                        "a clickable node has no name: {id:?} {:?}",
                        node.role());
                assert_ne!(node.role(), Role::Unknown,
                           "a clickable node has no role: {:?}",
                           node.label());
            }
            if node.role() == Role::ProgressIndicator {
                bars += 1;
                assert!(node.numeric_value().is_some(),
                        "a progress bar with no value: {:?}", node.label());
            }
        }
        assert!(clickable >= 10, "only {clickable} clickable nodes");
        assert!(bars >= 1, "no progress bar in the tree");
        // the glyph buttons say what they do, not what they look like
        let named: Vec<String> = update.nodes.iter()
            .filter_map(|(_, node)| node.label().map(str::to_owned))
            .collect();
        assert!(named.iter().any(|name|
                    name.starts_with("Cancel the download of")),
                "the ✕ of a running transfer is not named: {named:?}");
    }

    /// E31, D07, D08: work that runs on a thread says so on the control
    /// that started it, and takes no second click.
    #[test]
    fn work_on_a_thread_says_so_where_it_started() {
        let mut state = browsed();
        let mut files = files_ui(Tab::Timelapses);
        files.selected = Some(
            "/timelapse/video_2026-06-01_06-11-57.avi".to_string());
        let now = Instant::now();
        let with = |view: View<'_>, state: &mut BrowserState,
                    files: &mut FilesUi| {
            let ctx = ctx();
            frame(&ctx, raw(Vec::new()), state, files, &view);
            frame(&ctx, raw(Vec::new()), state, files, &view)
        };
        // the positive control: nothing is running, so nothing says it
        let quiet = with(view(P1S, now), &mut state, &mut files);
        assert!(quiet.exact("Clear cache"), "{:?}", quiet.spots);
        assert!(!quiet.has("Clearing…"));
        assert!(!quiet.has("Opening…"));
        // Clear cache while its thread runs
        let mut clearing = view(P1S, now);
        clearing.clearing = true;
        let painted = with(clearing, &mut state, &mut files);
        assert!(painted.exact("Clearing…"), "{:?}", painted.spots);
        assert!(!painted.has("Clear cache"));
        // a player still opening
        let mut opening = view(P1S, now);
        opening.opening = true;
        let painted = with(opening, &mut state, &mut files);
        assert!(painted.exact("Opening…"), "{:?}", painted.spots);
        // and the reason is on the button it would have started
        let painted = {
            let mut view = view(P1S, now);
            view.opening = true;
            hover(&mut state, &mut files, &view, "Download & play")
        };
        assert!(painted.has("Opening the last file…"),
                "{:?}", painted.spots);
    }

    /// E32, D34: the view has no repaint rate of its own. What it asks for
    /// is the counter's next second, or the gate a tile is waiting on.
    #[test]
    fn the_view_repaints_on_deadlines_not_on_a_timer() {
        fn delay(ctx: &egui::Context, state: &mut BrowserState,
                 files: &mut FilesUi, view: &View<'_>) -> Duration {
            let full = ctx.run_ui(raw(Vec::new()), |ui| {
                show(ui, state, files, view);
            });
            full.viewport_output[&egui::ViewportId::ROOT].repaint_delay
        }
        // Print files: a listing is on screen, no transfer, no tile gate.
        // The only counter is "updated N s ago", and the listing landed
        // now, so the next second is a second away.
        let ctx = ctx();
        let mut state = browsed();
        let mut files = files_ui(Tab::Files);
        let now = Instant::now();
        delay(&ctx, &mut state, &mut files, &view(P1S, now));
        let idle = delay(&ctx, &mut state, &mut files, &view(P1S, now));
        assert!(idle >= Duration::from_millis(900) && idle.as_secs() <= 1,
                "an idle list asked for {idle:?}");
        // Timelapses: the tiles are waiting on their 500 ms gate, so the
        // view asks for that, and no later — the thumbnails depend on it
        let mut files = files_ui(Tab::Timelapses);
        delay(&ctx, &mut state, &mut files, &view(P1S, now));
        let waiting = delay(&ctx, &mut state, &mut files, &view(P1S, now));
        assert!(waiting <= crate::browser::PREFETCH_VISIBLE,
                "a waiting tile asked for {waiting:?}");
        assert!(waiting > Duration::ZERO, "a busy loop");
    }

    /// E16, E27, D37: the row model is built once for a key and not again
    /// on a frame that changed nothing.
    #[test]
    fn the_row_model_is_built_once_per_key() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = files_ui(Tab::Timelapses);
        let now = Instant::now();
        for _ in 0..4 {
            frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                  &view(P1S, now));
        }
        assert_eq!(files.row_builds(), 1, "four frames, one key");
        // a letter typed into the filter is a new key
        files.filter = "video".to_string();
        frame(&ctx, raw(Vec::new()), &mut state, &mut files, &view(P1S, now));
        assert_eq!(files.row_builds(), 2);
        frame(&ctx, raw(Vec::new()), &mut state, &mut files, &view(P1S, now));
        assert_eq!(files.row_builds(), 2, "the same filter twice");
        // and so is anything the worker says
        listed(&mut state, "/timelapse", &TIMELAPSE);
        frame(&ctx, raw(Vec::new()), &mut state, &mut files, &view(P1S, now));
        assert_eq!(files.row_builds(), 3, "a listing that landed");
        frame(&ctx, raw(Vec::new()), &mut state, &mut files, &view(P1S, now));
        assert_eq!(files.row_builds(), 3);
    }

    /// C17, D30: what stands in for a listing has the shape of what
    /// replaces it — tiles on Timelapses, rows on Print files — so the
    /// list does not relayout when the listing lands.
    #[test]
    fn the_placeholders_have_the_shape_of_what_replaces_them() {
        /// Every rect painted in `fill`, on a tab given as it is.
        fn rects(state: &mut BrowserState, files: &mut FilesUi,
                 fill: egui::Color32) -> Vec<Rect> {
            let ctx = ctx();
            let view = view(P1S, Instant::now());
            let mut rects = Vec::new();
            fn walk(shape: &egui::Shape, fill: egui::Color32,
                    rects: &mut Vec<Rect>) {
                match shape {
                    egui::Shape::Rect(rect) if rect.fill == fill =>
                        rects.push(rect.rect),
                    egui::Shape::Vec(shapes) => shapes.iter()
                        .for_each(|shape| walk(shape, fill, rects)),
                    _ => {}
                }
            }
            // one frame to size everything, then the one that is read
            frame(&ctx, raw(Vec::new()), state, files, &view);
            let full = ctx.run_ui(raw(Vec::new()), |ui| {
                show(ui, state, files, &view);
            });
            for clipped in &full.shapes {
                walk(&clipped.shape, fill, &mut rects);
            }
            rects
        }
        // buttons and the selected tab are painted in the placeholder's
        // own fill, so a placeholder is what the list body holds: below
        // the filter row and above the cache line
        let placeholders = |tab: Tab| {
            let mut files = files_ui(tab);
            let mut loading = BrowserState::default();
            let _ = loading.refresh();
            let chrome = frame(&ctx(), raw(Vec::new()), &mut loading,
                               &mut files, &view(P1S, Instant::now()));
            let filter = chrome.spot("filter…").expect("no filter row").y;
            let cache = chrome.spot("cache ").expect("no cache line").y;
            let placed: Vec<Rect> =
                rects(&mut loading, &mut files, theme::CARD_HOVER)
                    .into_iter()
                    .filter(|rect| rect.min.y > filter && rect.max.y < cache)
                    .collect();
            let shapes = rects(&mut browsed(), &mut files, theme::CARD);
            (placed, shapes)
        };
        // the grid: a placeholder is a tile's box, and they stand in rows
        // of their own, not one bar per line
        let (placed, shapes) = placeholders(Tab::Timelapses);
        assert!(!placed.is_empty(), "no placeholder on a loading grid");
        let tile = shapes.into_iter()
            .find(|rect| rect.width() == size::TILE_W)
            .expect("no tile once the listing landed");
        for rect in &placed {
            assert_eq!(rect.width(), tile.width(),
                       "placeholder {rect:?} against tile {tile:?}");
            // to the pixel: a placeholder stands in for the tile
            assert!((rect.height() - tile.height()).abs() < 1.0,
                    "placeholder {rect:?} against tile {tile:?}");
        }
        let first = placed[0].min.y;
        assert!(placed.iter().filter(|rect| rect.min.y == first).count() >= 2,
                "the placeholders are stacked, not a grid: {placed:?}");
        // the list: a placeholder is as tall as the row that replaces it
        let (placed, shapes) = placeholders(Tab::Files);
        assert!(!placed.is_empty(), "no placeholder on a loading list");
        let row = shapes.into_iter()
            .filter(|rect| rect.width() > size::TILE_W)
            .min_by(|a, b| a.height().total_cmp(&b.height()))
            .expect("no row once the listing landed");
        for rect in &placed {
            assert_eq!(rect.height(), row.height(),
                       "placeholder {rect:?} against row {row:?}");
        }
    }

    /// 5.5: the reasons an empty Timelapses tab gives.
    #[test]
    fn the_empty_timelapse_tab_gives_the_reason() {
        let ctx = ctx();
        let mut files = FilesUi::default();
        let now = Instant::now();

        // only orphan thumbnails
        let mut state = BrowserState::default();
        let _ = state.refresh();
        listed(&mut state, "/", &ROOT);
        listed(&mut state, "/timelapse", &TIMELAPSE[..1]);
        listed(&mut state, "/timelapse/thumbnail", &THUMBS);
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, now));
        assert!(painted.has("the videos for these thumbnails were deleted"));

        // a damaged card: unreadable entries are counted and never addressed
        let mut state = BrowserState::default();
        let _ = state.refresh();
        listed(&mut state, "/", &ROOT);
        listed(&mut state, "/timelapse", &[
            "-rw-rw-rw-   1 root  root         0 Jan 01 1980 ?????",
            "-rw-rw-rw-   1 root  root         0 Jan 01 1980 ????? ??",
        ]);
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, now));
        assert!(painted.has("2 entries in /timelapse can't be read"));
        // the 5.10 banner says it, and nothing repeats it below
        let said = painted.spots.iter()
            .filter(|(text, _)| text.contains(
                "the SD card's file system looks damaged"))
            .count();
        assert_eq!(said, 1, "{:?}", painted.spots);

        // nothing at all
        let mut state = BrowserState::default();
        let _ = state.refresh();
        listed(&mut state, "/", &ROOT);
        listed(&mut state, "/timelapse", &[]);
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, now));
        assert!(painted.has("No timelapses on this printer."));
    }

    /// 5.10: a 550 on a listed directory reads as "folder not present".
    #[test]
    fn a_missing_directory_reads_as_folder_not_present() {
        let ctx = ctx();
        let mut state = browsed();
        let generation = state.generation();
        state.apply(Event::Listed { dir: "/model".to_string(), generation,
                                    result: Err(FtpError::NotFound) });
        let mut files = files_ui(Tab::Files);
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, Instant::now()));
        assert!(painted.has("/model: folder not present"), "{:?}",
                painted.spots);
    }

    /// 5.10 and section 6: the error text plus a Retry, never a spinner.
    #[test]
    fn an_error_card_shows_the_5_10_text_and_retries() {
        let ctx = ctx();
        let mut state = browsed();
        state.error = Some(FtpError::PortClosed);
        state.conn = ConnState::Stopped(FtpError::PortClosed);
        let mut files = FilesUi::default();
        let now = Instant::now();
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, now));
        assert!(painted.has(
            "FTP port closed (LAN mode / Developer Mode off?)"));
        assert!(painted.exact("Retry"));

        let out = click(&ctx, &mut state, &mut files, &view(P1S, now),
                        "Retry");
        assert!(out.actions.contains(&Action::Retry), "{:?}", out.actions);
    }

    // ------------------------------------------------ print files and tiles

    /// 5.4: `/cache` companions are grouped under the root job they were
    /// extracted from, never shown as unrelated duplicates.
    #[test]
    fn cache_companions_are_grouped_under_their_root_job() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = files_ui(Tab::Files);
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, Instant::now()));

        assert!(painted.exact("job.gcode.3mf"), "{:?}", painted.spots);
        assert!(painted.has(
            "printer's extracted copy: job_plate_1.gcode"));
        assert!(painted.has("printer's extracted copy: job.3mf"));
        // the companions are not rows of their own
        assert!(!painted.exact("job_plate_1.gcode"));
        assert!(!painted.exact("job.3mf"));
        // and the hidden .bbl never shows
        assert!(!painted.has(".bbl"));
    }

    /// 5.5: root directories outside the known set, one level at a time,
    /// with logger, recorder, image and System Volume Information excluded.
    #[test]
    fn other_folders_exclude_the_hidden_ones_and_open_one_level() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = files_ui(Tab::Files);
        let now = Instant::now();
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, now));
        assert!(painted.has("OTHER FOLDERS"));
        assert!(painted.exact("spool"));
        for hidden in ["logger", "image", "recorder",
                       "System Volume Information"] {
            assert!(!painted.exact(hidden), "{hidden} is offered");
        }

        let out = click(&ctx, &mut state, &mut files, &view(P1S, now),
                        "spool");
        assert_eq!(out.cmds.iter().filter_map(|cmd| match cmd {
            Cmd::List { dir, .. } => Some(dir.as_str()),
            _ => None,
        }).collect::<Vec<_>>(), ["/spool"], "one level, on demand");

        // its entries appear only once it answers, and a subdirectory is
        // shown as an entry, not walked
        listed(&mut state, "/spool", &[
            "drw-rw-rw-   1 root  root         0 Oct 27 2025 inner",
            "-rw-rw-rw-   1 root  root      1024 Oct 27 2025 note.txt",
        ]);
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, now));
        assert!(painted.exact("inner"));
        assert!(painted.exact("note.txt"));
        assert!(painted.lists().is_empty(), "a subdirectory was walked");
        // the folder the user opened is open; the one inside it is not
        assert!(painted.exact("▾ DIR"), "the opened folder reads as closed");
        assert!(painted.exact("▸ DIR"),
                "a folder nobody opened reads as open");
    }

    /// Section 4: a tile is prefetched only after it stayed visible, its
    /// picture becomes a texture once, and it is never fetched twice.
    #[test]
    fn a_tile_is_prefetched_only_after_it_stayed_visible() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = FilesUi::default();
        let start = Instant::now();

        let first = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                          &view(P1S, start));
        assert!(first.thumbs().is_empty(), "prefetched too early");

        let later = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                          &view(P1S, start + PREFETCH_VISIBLE));
        assert_eq!(later.thumbs(), [THUMB_PATH]);

        let generation = state.generation();
        state.apply(Event::Thumb { path: THUMB_PATH.to_string(), generation,
                                   result: Ok(thumb_image()) });
        let shown = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                          &view(P1S, start + PREFETCH_VISIBLE));
        assert!(shown.thumbs().is_empty(), "asked for a loaded tile again");
        assert!(matches!(state.thumbs.get(THUMB_PATH),
                         Some(ThumbState::Shown)),
                "the view did not take the picture into a texture");

        let thumb = entries("/timelapse/thumbnail", &THUMBS)
            .into_iter().next().expect("a thumbnail entry");
        assert!(state.request_thumb(&thumb, THUMB_PX).is_none());
    }

    /// Section 6: the grid is virtualised, so a card with hundreds of
    /// timelapses paints only what fits on screen.
    #[test]
    fn the_timelapse_grid_is_virtualised() {
        let ctx = ctx();
        let mut state = BrowserState::default();
        let _ = state.refresh();
        listed(&mut state, "/", &ROOT);
        let generation = state.generation();
        let videos: Vec<RemoteEntry> = (0..400)
            .map(|index: usize| {
                let line = format!(
                    "-rw-rw-rw-   1 root  root   4411548 Jun 01 06:17 \
                     video_2026-06-{:02}_{:02}-{:02}-00.avi",
                    (index % 28) + 1, (index / 60) % 24, index % 60);
                parse_list_line("/timelapse", &line, DateRule::CalendarYear,
                                2026).expect("a video line")
            })
            .collect();
        assert_eq!(videos.len(), 400);
        state.apply(Event::Listed { dir: "/timelapse".to_string(), generation,
                                    result: Ok(videos) });
        assert_eq!(state.counts().0, 400);

        let mut files = FilesUi::default();
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, Instant::now()));
        assert!(painted.spots.len() < 200,
                "painted {} pieces of text for 400 tiles",
                painted.spots.len());
        // and the month headings group them
        assert!(painted.has("JUNE 2026"));
    }

    #[test]
    fn the_filter_and_the_sort_pick_what_is_shown() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = files_ui(Tab::Files);
        files.filter = "job.gcode".to_string();
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, Instant::now()));
        assert!(painted.exact("job.gcode.3mf"));

        files.filter = "nothing-like-this".to_string();
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, Instant::now()));
        assert!(!painted.exact("job.gcode.3mf"));
        assert!(painted.has("No print files on this printer."));
    }

    /// Section 6: the detail pane keeps its facts and now performs the
    /// actions this stage added. This replaces the stage 2 test that
    /// asserted their absence, which the transfer lane supersedes.
    #[test]
    fn the_detail_pane_shows_facts_and_this_stages_actions() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = files_ui(Tab::Files);
        files.selected = Some("/job.gcode.3mf".to_string());
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, Instant::now()));
        assert!(painted.has("DETAILS"));
        assert!(painted.has("51 KB"));
        assert!(painted.has("Sep 07 19:38"));
        assert!(painted.has("sent from Studio"));
        assert!(painted.has("times are the printer's clock"));
        // the actions the transfer lane can finally perform
        assert!(painted.exact("Save to PC"), "{:?}", painted.spots);
        // and the cost of it, before the click (section 6)
        assert!(painted.has("Download ~"));
        // a 3mf is not played in the app
        assert!(!painted.has("Download & play"));
        // still not built: deletion is v2 (section 9)
        assert!(!painted.has("Delete"));
    }

    /// Section 6: Download & play starts one transfer for that file, with
    /// the destination that opens the player when it lands.
    #[test]
    fn download_and_play_starts_one_transfer_for_the_file() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = files_ui(Tab::Timelapses);
        let video = "/timelapse/video_2026-06-01_06-11-57.avi";
        files.selected = Some(video.to_string());
        let now = Instant::now();

        let out = click(&ctx, &mut state, &mut files, &view(P1S, now),
                        "Download & play");
        let started = out.cmds.iter().find_map(|cmd| match cmd {
            Cmd::Download { id, remote, dest } =>
                Some((*id, remote.path.clone(), *dest)),
            _ => None,
        });
        let (id, path, dest) =
            started.unwrap_or_else(|| panic!("no download: {:?}", out.cmds));
        assert_eq!(path, video);
        assert_eq!(dest, Dest::Cache { open_after: true });
        assert_eq!(state.active_transfers(), 1);

        // a second click while it runs does not start it twice
        let again = click(&ctx, &mut state, &mut files, &view(P1S, now),
                          "Download & play");
        assert!(!again.cmds.iter().any(|cmd|
            matches!(cmd, Cmd::Download { .. })), "{:?}", again.cmds);

        // real bytes off the socket drive the bar, the rate and the ETA
        state.apply(Event::Progress { id, done: 1_000_000,
                                      total: 4_411_548,
                                      bytes_per_s: 205_000.0 });
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, now));
        assert!(painted.has("/ 4.2 MB"), "{:?}", painted.spots);
        assert!(painted.has("KB/s"), "no rate on the transfer bar");
        assert!(painted.has("left"), "no ETA on the transfer bar");
        assert_eq!(state.running_percent(), Some(23));
        // the tile says so too (the per-tile state of section 6)
        assert!(painted.has("↓ 23%"));

        // and the bar's ✕ asks the worker to cancel that transfer
        let out = click(&ctx, &mut state, &mut files, &view(P1S, now), "✕");
        assert!(out.cmds.iter().any(|cmd|
            matches!(cmd, Cmd::Cancel(cancelled) if *cancelled == id)),
            "{:?}", out.cmds);
    }

    /// D11, D23: a file already on its way says so on both buttons instead
    /// of taking a click that starts nothing (`BrowserState::download`).
    #[test]
    fn a_transferring_file_says_why_its_buttons_are_off() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = files_ui(Tab::Timelapses);
        let video = "/timelapse/video_2026-06-01_06-11-57.avi";
        files.selected = Some(video.to_string());
        let now = Instant::now();
        let labels = ["Download & play", "Save to PC"];
        // the positive control: nothing is running, so nothing says this
        for label in labels {
            let painted = hover(&mut state, &mut files,
                                &view(P1S, now), label);
            assert!(!painted.has("Already downloading"),
                    "{label} before any transfer");
        }
        click(&ctx, &mut state, &mut files, &view(P1S, now),
              "Download & play");
        assert_eq!(state.active_transfers(), 1);
        for label in labels {
            let painted = hover(&mut state, &mut files,
                                &view(P1S, now), label);
            assert!(painted.has("Already downloading"),
                    "{label} says nothing: {:?}", painted.spots);
        }
    }

    /// 5.10: a queued transfer says why it is waiting, on its tile.
    #[test]
    fn a_queued_transfer_says_why_it_waits() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = files_ui(Tab::Timelapses);
        let video = "/timelapse/video_2026-06-01_06-11-57.avi";
        files.selected = Some(video.to_string());
        let now = Instant::now();
        let out = click(&ctx, &mut state, &mut files, &view(P1S, now),
                        "Download & play");
        let id = out.cmds.iter().find_map(|cmd| match cmd {
            Cmd::Download { id, .. } => Some(*id),
            _ => None,
        }).expect("a download");

        let reason = "waiting: printer is printing, one download at a time";
        state.apply(Event::Queued { id, reason: reason.to_string() });
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, now));
        assert!(painted.has(reason), "{:?}", painted.spots);
    }

    /// Design doc 4: a small 3mf previews on its own; a big one is a
    /// download, so it waits for the click and says what it will cost.
    #[test]
    fn a_small_3mf_previews_itself_and_a_big_one_waits() {
        let ctx = ctx();
        let now = Instant::now();
        let mut state = browsed();
        let mut files = files_ui(Tab::Files);
        files.selected = Some("/job.gcode.3mf".to_string());
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, now));
        assert!(painted.out.cmds.iter().any(|cmd| matches!(cmd,
            Cmd::Details { remote, .. }
                if remote.path == "/job.gcode.3mf")),
            "a 51 KB 3mf did not preview itself: {:?}", painted.out.cmds);

        let mut state = BrowserState::default();
        let _ = state.refresh();
        listed(&mut state, "/", &[
            "-rw-rw-rw-   1 root  root   7234567 Sep 07 19:38 \
             big.gcode.3mf"]);
        let mut files = files_ui(Tab::Files);
        files.selected = Some("/big.gcode.3mf".to_string());
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, now));
        assert!(!painted.out.cmds.iter()
                    .any(|cmd| matches!(cmd, Cmd::Details { .. })),
                "a 6.9 MB 3mf previewed itself");
        assert!(painted.has("preview: 6.9 MB"), "{:?}", painted.spots);
        let out = click(&ctx, &mut state, &mut files, &view(P1S, now),
                        "Load preview");
        assert!(out.cmds.iter().any(|cmd|
            matches!(cmd, Cmd::Details { .. })), "{:?}", out.cmds);
    }

    /// Section 6: the player takes over the grid, shows what it is playing
    /// and offers the OS player next to its own controls.
    #[test]
    fn the_player_takes_over_the_grid_and_comes_back() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = FilesUi::default();
        let index = AviIndex { width: 1280, height: 720,
                               us_per_frame: 41667,
                               frames: vec![(0, 10); 48],
                               truncated: false };
        let path = Path::new("cache/file/0123456789abcdef.avi");
        let now = Instant::now();
        let openable = shell_says(true);
        let shown = || View {
            shell_openable: Some(&openable),
            player: Some(PlayerView {
                title: "video_2026-06-01_06-11-57.avi",
                texture: None,
                index: &index,
                pos: 24,
                playing: true,
                speed: 1.0,
                path,
                skipped: 0,
            }),
            ..view(P1S, now)
        };
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &shown());
        assert!(painted.has("1280x720"), "{:?}", painted.spots);
        assert!(painted.has("24 fps"));
        assert!(painted.has("00:01 / 00:02"));
        assert!(painted.exact("Open in player"));
        assert!(painted.exact("Show in folder"));
        // the grid is not drawn underneath it
        assert!(!painted.has("queued"), "the grid is still there");

        let out = click(&ctx, &mut state, &mut files, &shown(),
                        "‹ Back to the list");
        assert!(out.actions.contains(&Action::ClosePlayer), "{:?}",
                out.actions);
        let out = click(&ctx, &mut state, &mut files, &shown(),
                        "Show in folder");
        assert!(out.actions.contains(&Action::Reveal(path.to_path_buf())),
                "{:?}", out.actions);
    }

    /// 5.10 and section 7: a format the app cannot decode says so and hands
    /// the file to the OS player instead of failing.
    #[test]
    fn an_unplayable_format_offers_the_os_player() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = FilesUi::default();
        let path = Path::new("cache/file/0123456789abcdef.mp4");
        let now = Instant::now();
        // an MP4 the app cannot decode, but whose header and name agree it
        // is media: section 7's fallback applies to it (F2)
        let openable = shell_says(true);
        let shown = || View {
            player_error: Some("can't play this format in the app (MP4)"),
            player_error_path: Some(path),
            shell_openable: Some(&openable),
            ..view(P1S, now)
        };
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &shown());
        assert!(painted.has("can't play this format in the app"),
                "{:?}", painted.spots);
        assert!(painted.exact("Open in player"));

        let out = click(&ctx, &mut state, &mut files, &shown(),
                        "Open in player");
        assert!(out.actions
                    .contains(&Action::OpenExternally(path.to_path_buf())),
                "{:?}", out.actions);
    }

    /// Section 6: the cache line shows usage against the cap, and Clear
    /// cache asks for it.
    #[test]
    fn the_cache_line_shows_usage_and_clears_it() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = FilesUi::default();
        let now = Instant::now();
        let shown = || View { cache_usage: 1_288_490_188,
                              ..view(P1S, now) };
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &shown());
        assert!(painted.has("cache 1.2 GB / 5.0 GB"), "{:?}", painted.spots);
        let out = click(&ctx, &mut state, &mut files, &shown(),
                        "Clear cache");
        assert!(out.actions.contains(&Action::ClearCache), "{:?}",
                out.actions);
    }

    /// Section 6: closing, removing or editing with transfers running says
    /// how many would be discarded.
    #[test]
    fn the_confirmations_name_the_running_transfers() {
        assert_eq!(active_transfer_note(0), None);
        let one = active_transfer_note(1).expect("a note");
        assert!(one.starts_with("1 download is"), "{one}");
        assert!(one.contains("discarded"), "{one}");
        let many = active_transfer_note(3).expect("a note");
        assert!(many.starts_with("3 downloads are"), "{many}");
    }

    /// T17: no message the view paints carries any run of the serial.
    #[test]
    fn no_view_text_carries_serial_characters() {
        let ctx = ctx();
        let now = Instant::now();
        let mut painted: Vec<String> = Vec::new();
        for serial in [P1S, H2C] {
            for tab in [Tab::Timelapses, Tab::Recordings, Tab::Files] {
                for error in [None, Some(FtpError::TlsRejected),
                              Some(FtpError::AuthRejected)] {
                    let mut state = browsed();
                    listed(&mut state, "/ipcam", &IPCAM);
                    state.error = error;
                    let mut files = files_ui(tab);
                    let frame = frame(&ctx, raw(Vec::new()), &mut state,
                                      &mut files, &view(serial, now));
                    painted.extend(frame.spots.into_iter()
                        .map(|(text, _)| text));
                }
            }
            // the refusal card too
            let mut state = browsed();
            state.cert_alert =
                Some(Refusal::Cert(PrinterCertError::SerialMismatch));
            let mut files = FilesUi::default();
            let frame = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                              &view(serial, now));
            painted.extend(frame.spots.into_iter().map(|(text, _)| text));
        }
        assert!(!painted.is_empty());
        for text in &painted {
            for serial in [P1S, H2C] {
                assert!(!has_serial_run(text, serial),
                        "serial characters in {text:?}");
            }
        }
    }

    /// 5.10 and section 6: a listing that failed shows the condition's text
    /// and a Retry on every tab, never "no timelapses on this printer".
    #[test]
    fn a_failed_listing_shows_the_error_card_not_an_empty_tab() {
        let ctx = ctx();
        for tab in [Tab::Timelapses, Tab::Recordings, Tab::Files] {
            let mut state = BrowserState::default();
            let _ = state.refresh();
            let generation = state.generation();
            state.apply(Event::Listed { dir: "/".to_string(), generation,
                                        result: Err(FtpError::Offline) });
            let mut files = files_ui(tab);
            let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                                &view(P1S, Instant::now()));
            assert!(painted.has("printer offline"), "{tab:?}");
            assert!(painted.exact("Retry"), "{tab:?}");
            for wrong in ["No timelapses on this printer",
                          "No recordings on this printer",
                          "No print files on this printer"] {
                assert!(!painted.has(wrong), "{tab:?} said {wrong:?}");
            }
        }
    }

    /// Section 4: the worker keeps one prefetch, so the view asks for one
    /// tile at a time instead of having every other request cancelled and
    /// issued again on the next frame.
    #[test]
    fn only_one_thumbnail_is_asked_for_at_a_time() {
        let ctx = ctx();
        let mut state = browsed_tiles(6);
        let mut files = FilesUi::default();
        let start = Instant::now();
        let ready = start + PREFETCH_VISIBLE;
        frame(&ctx, raw(Vec::new()), &mut state, &mut files,
              &view(P1S, start));
        let asked = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                          &view(P1S, ready));
        assert_eq!(asked.thumbs().len(), 1, "{:?}", asked.thumbs());
        let again = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                          &view(P1S, ready));
        assert!(again.thumbs().is_empty(),
                "asked again while one was in flight: {:?}", again.thumbs());

        // when that one lands, the next tile is asked for
        let path = asked.thumbs()[0].clone();
        let generation = state.generation();
        state.apply(Event::Thumb { path: path.clone(), generation,
                                   result: Ok(thumb_image()) });
        let next = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                         &view(P1S, ready));
        assert_eq!(next.thumbs().len(), 1);
        assert_ne!(next.thumbs()[0], path);
    }

    /// Design doc 12: the view's texture LRU keeps tiles that left the
    /// window, so coming back paints them again instead of fetching them
    /// from the printer a second time.
    #[test]
    fn a_tile_that_leaves_the_window_keeps_its_texture() {
        let ctx = ctx();
        let mut state = browsed_tiles(30);
        let mut files = FilesUi::default();
        let now = Instant::now();
        let generation = state.generation();
        let shown: Vec<String> = state.timelapses.iter().take(6)
            .filter_map(|item| item.thumb.as_ref())
            .map(|thumb| thumb.path.clone())
            .collect();
        assert_eq!(shown.len(), 6);
        for path in &shown {
            state.apply(Event::Thumb { path: path.clone(), generation,
                                       result: Ok(thumb_image()) });
        }
        frame(&ctx, raw(Vec::new()), &mut state, &mut files, &view(P1S, now));
        assert_eq!(files.textures.len(), 6, "the pictures became textures");

        // a short window paints fewer rows: the last tiles are off screen
        frame(&ctx, raw_at(Vec2::new(1180.0, 360.0), Vec::new()), &mut state,
              &mut files, &view(P1S, now));
        assert_eq!(files.textures.len(), 6, "a texture was thrown away");
        let last = shown.last().expect("a tile");
        assert!(matches!(state.thumbs.get(last), Some(ThumbState::Shown)),
                "the tile was forgotten, so it would be fetched again");

        // and back, with nothing asked for a second time
        let back = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                         &view(P1S, now + PREFETCH_VISIBLE));
        assert!(!back.thumbs().contains(last),
                "the tile was fetched from the printer again");
        assert_eq!(files.textures.len(), 6);
    }

    /// Section 6: a tile without its picture says which state it is in, and
    /// a failed one is asked for again only when the user says so.
    #[test]
    fn a_tile_says_whether_it_is_queued_or_failed_and_retries_on_demand() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = FilesUi::default();
        let now = Instant::now();
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, now));
        assert!(painted.has("queued"), "{:?}", painted.spots);

        let generation = state.generation();
        state.apply(Event::Thumb {
            path: THUMB_PATH.to_string(), generation,
            result: Err(FtpError::Local(
                "thumbnail could not be decoded".into())) });
        let later = now + PREFETCH_VISIBLE * 4;
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, later));
        assert!(painted.has("not a picture"), "{:?}", painted.spots);
        assert!(painted.exact("⟳ retry"));
        let again = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                          &view(P1S, later));
        assert!(again.thumbs().is_empty(),
                "a failed tile was asked for again on its own");

        click(&ctx, &mut state, &mut files, &view(P1S, later), "⟳ retry");
        let after = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                          &view(P1S, later));
        assert_eq!(after.thumbs(), [THUMB_PATH], "the retry asks once more");
    }

    /// Section 6: the virtualised list reserves what its rows really paint,
    /// so the scroll range matches the content and the tail of a long list
    /// can be reached.
    #[test]
    fn the_reserved_row_heights_match_what_the_rows_paint() {
        let ctx = ctx();
        let now = Instant::now();
        // the heights come from the style, so they are right on the very
        // first frame: nothing is measured and reserved on the next (E15)
        for (tab, mut state) in [(Tab::Files, browsed()),
                                 (Tab::Timelapses, browsed_tiles(40))] {
            let mut files = files_ui(tab);
            frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                  &view(P1S, now));
            assert!(files.row_mismatch < 0.5,
                    "{tab:?}: a row paints {} px off its reserved height",
                    files.row_mismatch);
        }
    }

    /// 1.8 and D04: a long failed-folder note paints one line, so it never
    /// inflates the note rows after it, and the heights stay the formulas'.
    #[test]
    fn a_long_folder_failure_keeps_every_row_at_its_formula_height() {
        let ctx = ctx();
        let now = Instant::now();
        let mut state = browsed();
        let mut files = files_ui(Tab::Files);
        let _ = state.open_folder("/spool");
        files.open_folders.insert("/spool".to_string());
        let generation = state.generation();
        state.apply(Event::Listed { dir: "/spool".to_string(), generation,
                                    result: Err(FtpError::HandshakeStall) });
        for pass in 0..2 {
            // narrow, so the note is longer than the list is wide, and
            // tall enough for the folder rows
            let painted = frame(&ctx, raw_at(Vec2::new(700.0, 900.0),
                                              Vec::new()), &mut state,
                                &mut files, &view(P1S, now));
            assert!(painted.has("/spool: printer's FTP didn't answer"),
                    "the note row is not on screen: {:?}", painted.spots);
            assert!(files.row_mismatch < 0.5,
                    "frame {pass}: a row paints {} px off its height",
                    files.row_mismatch);
        }
        // and what is reserved is the formula, at zoom 1: a control, 2 x 4
        // of padding and 2 x 1 of outline for a row
        let _ = ctx.run_ui(raw(Vec::new()), |ui| {
            let heights = Heights::of(ui);
            assert_eq!(heights.row, 42.0);
            assert_eq!(heights.companion, 32.0);
            assert_eq!(heights.tile, 14.0 + 94.0 + 16.0 + 2.0 * heights.note);
        });
    }

    // --------------------------------------------------------- small parts

    #[test]
    fn the_env_var_names_the_tab() {
        assert_eq!(Tab::from_word("timelapses"), Some(Tab::Timelapses));
        assert_eq!(Tab::from_word("Recordings"), Some(Tab::Recordings));
        assert_eq!(Tab::from_word(" files "), Some(Tab::Files));
        assert_eq!(Tab::from_word("nope"), None);
    }

    #[test]
    fn sizes_and_ages_read_like_the_mock() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(4411548), "4.2 MB");
        assert_eq!(human_bytes(86527810), "83 MB");
        assert_eq!(human_bytes(8_700_000_000), "8.1 GB");
        assert_eq!(ago(Duration::from_secs(4)), "4 s");
        assert_eq!(ago(Duration::from_secs(125)), "2 min");
        assert_eq!(ago(Duration::from_secs(7300)), "2 h");
        assert_eq!(ago(Duration::from_secs(200_000)), "2 d");
    }

    /// Section 6: printer-clock times, and a row that carried a year keeps
    /// it instead of showing a midnight the printer never reported.
    #[test]
    fn a_row_with_a_year_shows_the_year_not_a_midnight() {
        let dated = parse_list_line(
            "/", "drw-rw-rw-   1 root  root  0 Oct 08 2025 cache",
            DateRule::CalendarYear, 2026).expect("a dated row");
        assert_eq!(when_text(dated.mtime), "Oct 08 2025");
        let clocked = parse_list_line(
            "/", "-rw-rw-rw-   1 root  root  9 Sep 07 19:38 job.gcode.3mf",
            DateRule::CalendarYear, 2026).expect("a row with a clock");
        assert_eq!(when_text(clocked.mtime), "Sep 07 19:38");
        assert_eq!(when_text(None), "—");
    }

    #[test]
    fn the_month_heading_names_the_month() {
        assert_eq!(month_heading(Some((2026, 7))), "JULY 2026");
        assert_eq!(month_heading(Some((2026, 1))), "JANUARY 2026");
        assert_eq!(month_heading(None), "NO DATE");
    }

    // ------------------------------------------- the disk cache and time

    /// 5.6 and section 6: a complete, key-matching copy already in the disk
    /// cache is not a download. The view only knows the transfers it
    /// started this session, so it asks the cache as well — otherwise a
    /// file that is already on disk was offered as "Download ~6 min", which
    /// is the one direction "every action shows its time cost up front"
    /// must not get wrong.
    #[test]
    fn a_file_already_in_the_cache_is_played_not_downloaded() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = files_ui(Tab::Timelapses);
        let video = "/timelapse/video_2026-06-01_06-11-57.avi";
        files.selected = Some(video.to_string());
        let now = Instant::now();
        let copy = PathBuf::from("cache/file/0123456789abcdef.avi");
        let on_disk = copy.clone();
        let cached: &dyn Fn(&RemoteEntry) -> Option<PathBuf> =
            &move |_: &RemoteEntry| Some(on_disk.clone());
        let openable = shell_says(true);
        let shown = || View { cached: Some(cached),
                              shell_openable: Some(&openable),
                              ..view(P1S, now) };

        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &shown());
        assert!(painted.exact("Play"), "{:?}", painted.spots);
        assert!(painted.exact("Open in player"));
        assert!(painted.exact("Show in folder"));
        assert!(!painted.has("Download &"),
                "a file on disk was offered as a download");
        assert!(!painted.has("Download ~"),
                "a file on disk was given a download's ETA");

        // Play opens the copy on disk, at the speed the remote path asks
        // for — the cached name is a hash, so it cannot say (section 7)
        let out = click(&ctx, &mut state, &mut files, &shown(), "Play");
        assert!(out.actions.contains(&Action::Play { path: copy,
                                                     speed: 1.0 }),
                "{:?}", out.actions);

        // and with no cache to ask, the same file is a download again
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, now));
        assert!(painted.has("Download ~"), "{:?}", painted.spots);
    }

    /// The other half of the rule above: a saved copy the app would never
    /// play is not handed to the Windows shell. Names come off an SD card
    /// this app does not control, and sanitising keeps the extension, so a
    /// card offering "invoice.exe" would otherwise sit two clicks from a
    /// ShellExecute under a button labelled as a player (stage 3 security
    /// review, F2). "Show in folder" stays, because it opens the folder.
    #[test]
    fn a_saved_copy_the_app_cannot_play_is_not_offered_to_the_shell() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = files_ui(Tab::Files);
        let hostile = "/cache/invoice.exe";
        state.files = vec![FileItem {
            remote: RemoteEntry {
                path: hostile.to_string(),
                name: "invoice.exe".to_string(),
                size: 4096,
                is_dir: false,
                mtime: None,
                unreadable: false,
            },
            kind: FileKind::Other,
            plate_hint: None,
            companion_of: None,
        }];
        files.selected = Some(hostile.to_string());
        let now = Instant::now();
        let copy = PathBuf::from("cache/file/0123456789abcdef.exe");
        let on_disk = copy.clone();
        let cached: &dyn Fn(&RemoteEntry) -> Option<PathBuf> =
            &move |_: &RemoteEntry| Some(on_disk.clone());
        // the rule refused this file: its header and name do not agree that
        // it is media (player::openable_by_shell, tested there)
        let openable = shell_says(false);
        let shown = || View { cached: Some(cached),
                              shell_openable: Some(&openable),
                              ..view(P1S, now) };

        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &shown());
        assert!(!painted.has("Open in player"),
                "a file the app cannot play was offered to the shell: {:?}",
                painted.spots);
        assert!(painted.exact("Show in folder"), "{:?}", painted.spots);
        assert!(!painted.has("Play"), "{:?}", painted.spots);
    }

    /// 5.10: a failed transfer offers Retry, which restarts at 0 — there is
    /// no resume (REST is 502). The only control on a failed row used to be
    /// ✕, which dismisses it.
    #[test]
    fn a_failed_transfer_can_be_retried_from_the_bar() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = files_ui(Tab::Timelapses);
        let video = "/timelapse/video_2026-06-01_06-11-57.avi";
        files.selected = Some(video.to_string());
        let now = Instant::now();
        let out = click(&ctx, &mut state, &mut files, &view(P1S, now),
                        "Download & play");
        let id = out.cmds.iter().find_map(|cmd| match cmd {
            Cmd::Download { id, .. } => Some(*id),
            _ => None,
        }).expect("a download");
        state.apply(Event::Done {
            id, result: Err(FtpError::Truncated { got: 1, want: 2 }) });

        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, now));
        assert!(painted.has("download interrupted"), "{:?}", painted.spots);
        assert!(painted.exact("⟳ retry"));

        let out = click(&ctx, &mut state, &mut files, &view(P1S, now),
                        "⟳ retry");
        let again = out.cmds.iter().find_map(|cmd| match cmd {
            Cmd::Download { id, remote, dest } =>
                Some((*id, remote.path.clone(), *dest)),
            _ => None,
        }).unwrap_or_else(|| panic!("no retry: {:?}", out.cmds));
        assert_eq!(again.1, video);
        assert_eq!(again.2, Dest::Cache { open_after: true },
                   "the retry did not keep the destination");
        assert_ne!(again.0, id, "the retry is a transfer of its own");
    }

    /// Section 6: with the player open, the transfer bar and the cache line
    /// still fit on screen. The picture used to take a fixed margin that
    /// covered its own controls and the cache line only, so a transfer
    /// running while a video played pushed Clear cache off the bottom.
    #[test]
    fn the_player_leaves_room_for_the_transfer_bar() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = FilesUi::default();
        let video = "/timelapse/video_2026-06-01_06-11-57.avi";
        files.selected = Some(video.to_string());
        let now = Instant::now();
        click(&ctx, &mut state, &mut files, &view(P1S, now),
              "Download & play");
        assert_eq!(state.active_transfers(), 1);

        let index = AviIndex { width: 1280, height: 720, us_per_frame: 41667,
                               frames: vec![(0, 10); 48], truncated: false };
        let path = Path::new("cache/file/0123456789abcdef.avi");
        let height = 820.0;
        let shown = || View {
            player: Some(PlayerView { title: "video.avi", texture: None,
                                      index: &index, pos: 0, playing: true,
                                      speed: 1.0, path, skipped: 0 }),
            ..view(P1S, now)
        };
        let painted = frame(&ctx, raw_at(Vec2::new(1180.0, height),
                                         Vec::new()),
                            &mut state, &mut files, &shown());
        let cache_line = painted.spot("Clear cache")
            .expect("the cache line");
        assert!(cache_line.y < height,
                "Clear cache is off the bottom at {cache_line:?}");
        let row = painted.spot("connecting").expect("the transfer row");
        assert!(row.y < height,
                "the transfer row is off the bottom at {row:?}");
    }

    /// Section 6: a tile truncates the queued reason. The 5.10 wording is
    /// 50 characters and a tile is 156 px wide, so wrapped it made every
    /// tile row in the grid that tall — the virtualiser reserves what a row
    /// kind really paints — for as long as the transfer waited.
    #[test]
    fn a_queued_tile_keeps_the_grids_row_height() {
        let ctx = ctx();
        let now = Instant::now();
        for reason in ["waiting",
                       "waiting: printer is printing, one download at a time"]
        {
            let mut state = browsed_tiles(8);
            let mut files = FilesUi::default();
            let video = state.timelapses[0].video.clone().expect("a video");
            let cmd = state.download(&video,
                                     Dest::Cache { open_after: true })
                .expect("a download");
            let Cmd::Download { id, .. } = cmd else {
                panic!("a download");
            };
            state.apply(Event::Queued { id, reason: reason.to_string() });
            let painted = frame(&ctx, raw(Vec::new()), &mut state,
                                &mut files, &view(P1S, now));
            assert!(painted.has("waiting"), "{:?}", painted.spots);
            assert!(files.row_mismatch < 0.5,
                    "{reason:?}: a tile row paints {} px off its height",
                    files.row_mismatch);
        }
    }

    /// Section 6: "connecting…" carries the seconds elapsed. The phase is
    /// ~0.9 s normally, but it also covers a stalled handshake and the one
    /// retry behind it, which is when the counter is the point.
    #[test]
    fn a_starting_transfer_counts_the_seconds() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = files_ui(Tab::Timelapses);
        let video = "/timelapse/video_2026-06-01_06-11-57.avi";
        files.selected = Some(video.to_string());
        click(&ctx, &mut state, &mut files, &view(P1S, Instant::now()),
              "Download & play");
        let started = Instant::now();

        let later = view(P1S, started + Duration::from_secs(3));
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &later);
        let seconds = painted.spots.iter()
            .find_map(|(text, _)| text.strip_prefix("connecting ")
                .and_then(|rest| rest.trim_end_matches(" s").parse::<u64>()
                    .ok()))
            .unwrap_or_else(|| panic!("no counter: {:?}", painted.spots));
        assert!(seconds >= 3, "connecting {seconds} s after 3 s");
        assert!(!painted.exact("connecting…"),
                "the bare ellipsis is still painted");
    }

    /// 5.10: the "can't play this format in the app" card can be put away.
    /// It used to stay above the grid for the rest of the visit, on every
    /// tab, with nothing to dismiss it.
    #[test]
    fn the_player_error_card_can_be_dismissed() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = FilesUi::default();
        let path = Path::new("cache/file/0123456789abcdef.mp4");
        let now = Instant::now();
        let shown = || View {
            player_error: Some("can't play this format in the app (MP4)"),
            player_error_path: Some(path),
            ..view(P1S, now)
        };
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &shown());
        assert!(painted.exact("✕"), "the card cannot be dismissed");

        let out = click(&ctx, &mut state, &mut files, &shown(), "✕");
        assert!(out.actions.contains(&Action::ClosePlayer), "{:?}",
                out.actions);
    }

    // ------------------------------------------------- stage 2: fit (1.8)

    /// A piece of text as painted: what it says, its rect, and the clip
    /// rect it was painted under.
    struct Text {
        text: String,
        rect: Rect,
        clip: Rect,
    }

    impl Text {
        /// Painted whole inside its clip and inside a window of `size`.
        fn on_screen(&self, size: Vec2) -> bool {
            let screen = Rect::from_min_size(Pos2::ZERO, size);
            let inside = |outer: Rect| outer.min.x <= self.rect.min.x + 0.5
                && outer.min.y <= self.rect.min.y + 0.5
                && self.rect.max.x <= outer.max.x + 0.5
                && self.rect.max.y <= outer.max.y + 0.5;
            inside(self.clip) && inside(screen)
        }
    }

    fn texts(ctx: &egui::Context, input: egui::RawInput,
             state: &mut BrowserState, files: &mut FilesUi,
             view: &View<'_>) -> Vec<Text> {
        fn walk(shape: &egui::Shape, clip: Rect, found: &mut Vec<Text>) {
            match shape {
                egui::Shape::Text(text) => found.push(Text {
                    text: text.galley.text().to_string(),
                    // a right-aligned galley starts left of its pos
                    rect: text.galley.rect.translate(text.pos.to_vec2()),
                    clip,
                }),
                egui::Shape::Vec(shapes) => shapes.iter()
                    .for_each(|shape| walk(shape, clip, found)),
                _ => {}
            }
        }
        let full = ctx.run_ui(input, |ui| {
            show(ui, state, files, view);
        });
        let mut found = Vec::new();
        for clipped in &full.shapes {
            walk(&clipped.shape, clipped.clip_rect, &mut found);
        }
        found
    }

    fn find<'a>(texts: &'a [Text], label: &str) -> Option<&'a Text> {
        texts.iter().find(|text| text.text == label)
    }

    /// C12, D01: a 3mf with its plate picture and eight facts is taller than
    /// the pane at 1080x780. The pane scrolls, so the wheel over it brings
    /// Save to PC into the window. It takes three notches, not the one the
    /// guidelines hoped for: egui scrolls 40 points a notch on Windows, and
    /// the button starts below the pane's edge by more than two of them.
    /// Decision O3 pins the actions, which removes the scroll altogether.
    #[test]
    fn a_tall_detail_pane_scrolls_its_actions_into_view() {
        use crate::threemf::{Filament, ThreeMfInfo};

        let ctx = ctx();
        let size = Vec2::new(1080.0, 780.0);
        let mut state = browsed();
        let path = "/job.gcode.3mf";
        let filament = |kind: &str, color: &str| Filament {
            kind: kind.to_string(), color: color.to_string(),
            used_g: Some(12.5), used_m: Some(4.1) };
        let info = ThreeMfInfo {
            plate: Some(1),
            printer_model: "Bambu Lab A1".to_string(),
            prediction_s: Some(5_978),
            weight_g: Some(38.42),
            layers: Some(212),
            max_z_mm: Some(42.4),
            bed_type: "Textured PEI Plate".to_string(),
            filaments: vec![filament("PLA", "#FF6A13"),
                            filament("PETG", "#161616")],
            ..Default::default()
        };
        let picture = egui::ColorImage::from_rgba_unmultiplied(
            [256, 256], &[90; 256 * 256 * 4]);
        state.details.insert(path.to_string(), DetailState::Ready(Box::new(
            ThreeMf { info, plate: Some(crate::browser::Picture(picture)) })));
        let mut files = files_ui(Tab::Files);
        files.selected = Some(path.to_string());
        let now = Instant::now();

        let before = texts(&ctx, raw_at(size, Vec::new()), &mut state,
                           &mut files, &view(P1S, now));
        let details = find(&before, "DETAILS").expect("the pane");
        // the premise: the pane really is taller than the window allows
        let save = find(&before, "Save to PC");
        assert!(save.is_none_or(|save| !save.on_screen(size)),
                "Save to PC is on screen before any scroll");

        let over = details.rect.center() + Vec2::new(0.0, 200.0);
        let notch = || egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: Vec2::new(0.0, -1.0),
            phase: egui::TouchPhase::Move,
            modifiers: Modifiers::default(),
        };
        let mut notches = 0;
        let visible = loop {
            texts(&ctx, raw_at(size, vec![egui::Event::PointerMoved(over),
                                          notch()]),
                  &mut state, &mut files, &view(P1S, now));
            notches += 1;
            // the scroll animates over a few frames
            let mut after = Vec::new();
            for _ in 0..30 {
                after = texts(&ctx, raw_at(size, vec![
                    egui::Event::PointerMoved(over)]),
                    &mut state, &mut files, &view(P1S, now));
            }
            let save = find(&after, "Save to PC")
                .is_some_and(|save| save.on_screen(size));
            if save || notches == 3 {
                break save;
            }
        };
        assert!(visible, "Save to PC is off screen after {notches} notches");
    }

    /// C13, D03: four failed transfers at 700x480 scroll inside the bar, so
    /// Clear cache and the ✕ of every row on screen stay in the window.
    #[test]
    fn four_failed_transfers_keep_clear_cache_on_screen() {
        let ctx = ctx();
        let size = Vec2::new(700.0, 480.0);
        let mut state = browsed();
        let mut remotes: Vec<RemoteEntry> = state.files.iter()
            .map(|item| item.remote.clone()).collect();
        remotes.extend(state.timelapses.iter()
            .filter_map(|item| item.video.clone()));
        assert!(remotes.len() >= 4, "{} files", remotes.len());
        for remote in remotes.iter().take(4) {
            let Some(Cmd::Download { id, .. }) =
                state.download(remote, Dest::SaveToPc)
            else {
                panic!("a download");
            };
            state.apply(Event::Done { id, result: Err(
                FtpError::SessionLost("connection reset".to_string())) });
        }
        let mut files = files_ui(Tab::Files);
        let painted = texts(&ctx, raw_at(size, Vec::new()), &mut state,
                            &mut files, &view(P1S, Instant::now()));
        let clear = find(&painted, "Clear cache").expect("the cache line");
        assert!(clear.on_screen(size), "Clear cache at {:?}", clear.rect);
        let dismiss: Vec<&Text> = painted.iter()
            .filter(|text| text.text == "✕").collect();
        // the control: the rows are really there, three of them showing
        let shown: Vec<&&Text> = dismiss.iter()
            .filter(|text| text.on_screen(size)).collect();
        assert!(shown.len() >= 3, "{} ✕ on screen", shown.len());
        for text in &dismiss {
            let visible = text.rect.intersects(text.clip);
            assert!(!visible || text.rect.max.y <= size.y,
                    "a ✕ is painted past the window at {:?}", text.rect);
        }
    }

    /// E10, D13: selecting a tile paints a ring over it and changes no
    /// size, so no text of the grid moves.
    #[test]
    fn selecting_a_tile_moves_no_other_text() {
        let ctx = ctx();
        let mut state = browsed_tiles(8);
        let mut files = FilesUi::default();
        let now = Instant::now();
        let positions = |texts: &[Text]| {
            let mut by_text: HashMap<String, Vec<(i32, i32)>> =
                HashMap::new();
            for text in texts {
                by_text.entry(text.text.clone()).or_default().push((
                    (text.rect.min.x * 10.0) as i32,
                    (text.rect.min.y * 10.0) as i32));
            }
            by_text
        };
        let before = positions(&texts(&ctx, raw(Vec::new()), &mut state,
                                      &mut files, &view(P1S, now)));
        let video = state.timelapses[3].video.as_ref().expect("a video")
            .path.clone();
        files.selected = Some(video);
        let after = positions(&texts(&ctx, raw(Vec::new()), &mut state,
                                     &mut files, &view(P1S, now)));
        let mut compared = 0;
        for (text, spots) in &before {
            let Some(now_at) = after.get(text) else { continue };
            for spot in spots {
                assert!(now_at.contains(spot),
                        "{text:?} moved from {spot:?} to {now_at:?}");
                compared += 1;
            }
        }
        assert!(compared > 20, "only {compared} texts compared");
    }

    /// C9, D15: a 60-character printer name at 700x480 truncates, and the
    /// three tabs stay whole in the window.
    #[test]
    fn a_long_printer_name_leaves_the_tabs_on_screen() {
        let ctx = ctx();
        let size = Vec2::new(700.0, 480.0);
        let name =
            "Taller del fondo, impresora grande junto a la ventana del 60";
        assert_eq!(name.chars().count(), 61 - 1);
        let mut state = browsed();
        let mut files = FilesUi::default();
        let long = View { name, ..view(P1S, Instant::now()) };
        let painted = texts(&ctx, raw_at(size, Vec::new()), &mut state,
                            &mut files, &long);
        for tab in ["Timelapses", "Recordings", "Print files"] {
            let text = painted.iter()
                .find(|text| text.text.starts_with(tab))
                .unwrap_or_else(|| panic!("no {tab} tab painted"));
            assert!(text.on_screen(size), "{tab} at {:?}", text.rect);
        }
        // an elided galley keeps the whole text, so the truncation is read
        // from the geometry: the title ends before the first tab begins
        let title = painted.iter()
            .find(|text| text.text.starts_with("Taller del fondo"))
            .expect("the title");
        let first_tab = painted.iter()
            .find(|text| text.text.starts_with("Timelapses"))
            .expect("the first tab");
        assert!(title.rect.max.x <= first_tab.rect.min.x,
                "the title {:?} runs into the tabs {:?}", title.rect,
                first_tab.rect);
    }

    // ------------------------------------------- stage 3: identity and state

    /// E3, E4, D05: typing in the filter keeps its focus while blocks above
    /// it come and go — an error card, the open-error banner, a print.
    #[test]
    fn the_filter_keeps_its_focus_while_blocks_above_it_change() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = files_ui(Tab::Timelapses);
        let now = Instant::now();
        click(&ctx, &mut state, &mut files, &view(P1S, now), "filter…");
        let typed = |text: &str| egui::Event::Text(text.to_string());
        frame(&ctx, raw(vec![typed("a")]), &mut state, &mut files,
              &view(P1S, now));
        // the control: the click really focused the filter
        assert_eq!(files.filter, "a");

        state.error = Some(FtpError::PortClosed);
        let changed = View { printing: true,
                             open_error: Some("couldn't open the file (x)"),
                             ..view(P1S, now) };
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &changed);
        assert!(painted.exact("Retry"), "the error card is not there");
        frame(&ctx, raw(vec![typed("b")]), &mut state, &mut files, &changed);
        assert_eq!(files.filter, "ab", "the filter lost its focus");
    }

    /// E1, D06: a tab and a printer each keep their own scroll offset.
    #[test]
    fn each_tab_and_printer_keeps_its_own_scroll_offset() {
        let ctx = ctx();
        let mut state = browsed_tiles(40);
        let mut files = FilesUi::default();
        let now = Instant::now();
        // a tile further down: its start time says where the grid is
        let date = "Jun 01 06:20";
        let tile_y = |texts: &[Text]| texts.iter()
            .filter(|text| text.text == date
                    && text.rect.intersects(text.clip))
            .map(|text| text.rect.min.y)
            .fold(f32::INFINITY, f32::min);
        let render = |state: &mut BrowserState, files: &mut FilesUi,
                      serial: &str, events: Vec<egui::Event>| {
            texts(&ctx, raw(events), state, files, &view(serial, now))
        };
        let top = tile_y(&render(&mut state, &mut files, P1S, Vec::new()));
        let wheel = egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: Vec2::new(0.0, -4.0),
            phase: egui::TouchPhase::Move,
            modifiers: Modifiers::default(),
        };
        let over = egui::Event::PointerMoved(Pos2::new(200.0, 500.0));
        render(&mut state, &mut files, P1S, vec![over.clone(), wheel]);
        let mut scrolled = top;
        for _ in 0..30 {
            scrolled = tile_y(&render(&mut state, &mut files, P1S,
                                      vec![over.clone()]));
        }
        assert!(scrolled < top - 100.0, "no scroll: {top} -> {scrolled}");

        // another tab, then back: the grid is where it was left
        files.tab = Tab::Recordings;
        render(&mut state, &mut files, P1S, Vec::new());
        files.tab = Tab::Timelapses;
        let back = tile_y(&render(&mut state, &mut files, P1S, Vec::new()));
        assert_eq!(back, scrolled, "the tab switch lost the offset");

        // another printer's grid starts at the top
        let other = tile_y(&render(&mut state, &mut files, A1, Vec::new()));
        assert_eq!(other, top, "the offset followed the view to A1");
    }

    /// C12, D21: a selection a refresh dropped says so, rather than the pane
    /// falling back to "Select a file" while it stays selected.
    #[test]
    fn a_selection_the_listing_dropped_says_so() {
        let ctx = ctx();
        let mut state = browsed();
        let mut files = files_ui(Tab::Files);
        files.selected = Some("/cache/gone_plate_2.gcode".to_string());
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, Instant::now()));
        assert!(painted.exact("Not in the latest listing."),
                "{:?}", painted.spots);
        assert!(painted.exact("gone_plate_2.gcode"));
        assert!(!painted.has("Select a file"));
        // and with nothing selected, the pane still asks for a selection
        files.selected = None;
        let painted = frame(&ctx, raw(Vec::new()), &mut state, &mut files,
                            &view(P1S, Instant::now()));
        assert!(painted.has("Select a file"));
        assert!(!painted.has("Not in the latest listing."));
    }
}
