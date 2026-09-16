//! The files view (design doc 6): a full page in the `CentralPanel`, not a
//! `Modal`, because a modal would block the printer chips and a nested
//! `show_rows` inside the outer `ScrollArea` breaks virtualisation.
//!
//! It shows what `browser::BrowserState` holds for one printer: Timelapses,
//! Recordings and Print files, with the session state the G4 gate wants on
//! the header line. This stage has no download actions at all, so it shows
//! none: no dead buttons, no trust action, and never an endless spinner.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use chrono::NaiveDateTime;
use egui::{Color32, CornerRadius, RichText, Sense, Stroke, Ui, vec2};

use crate::browser::{BrowserState, Cmd, ConnState, DirState, FileItem,
                     FileKind, RECORDINGS_DIR, VisibleSince};
use crate::config;
use crate::ftp::{FtpError, RemoteEntry, ServerProfile};
use crate::theme;
use crate::tls;
use crate::ui::panel::card_frame;

/// Thumbnails are downscaled to this on the lane thread (design doc 12: A1
/// frames are 1536x1080, 6.6 MB as RGBA).
pub const THUMB_PX: u32 = 320;
/// Textures the view keeps; the least recently shown tiles are dropped and
/// fetched again if they come back (design doc 12).
const TEXTURE_CAP: usize = 150;
/// Directories whose used space the header line sums, in this order.
const SPACE_DIRS: [&str; 5] = ["timelapse", "ipcam", "cache", "model", "/"];
/// How deep an "Other folder" may be opened, one level at a time (5.5).
const FOLDER_DEPTH: usize = 4;

const TILE_W: f32 = 168.0;
const TILE_IMAGE_H: f32 = 94.0;
/// picture, then the date and size lines, plus the frame's margins and the
/// spacing between the three: a shorter row would clip the size line
const TILE_H: f32 = TILE_IMAGE_H + 62.0;
const ROW_H: f32 = 34.0;
const HEADING_H: f32 = 24.0;
const DETAIL_W: f32 = 268.0;

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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// leave the files view
    Back,
    Refresh,
    /// the Retry button of an error card
    Retry,
    /// the refusal card's "Edit printer"
    EditPrinter,
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
    pub now: Instant,
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
    visible: VisibleSince,
    frame: u64,
    /// the refusal card's Close button holds the focus (section 6)
    close_focused: bool,
}

impl FilesUi {
    /// Leaving the view drops its textures (design doc 12).
    pub fn close(&mut self) {
        self.textures.clear();
        self.close_focused = false;
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
    ui.add_space(6.0);

    // H2C / P2S / X2D: the answer comes from the model name, and nothing is
    // listed or connected (5.3, Models)
    if let Some(message) = config::files_refused_by_name(view.serial) {
        files.close_focused = false;
        refusal_card(ui, files, "⚠ File access disabled for this model",
                     &message, &mut out);
        return out;
    }
    // a refused certificate replaces the content for this printer (5.3)
    if state.cert_alert.is_some() {
        files.close_focused = false;
        refusal_card(ui, files, "⚠ FTP connection refused", tls::REFUSAL_TEXT,
                     &mut out);
        return out;
    }
    files.close_focused = false;

    if let Some(notice) = not_tested_notice(view.serial, view.profile) {
        banner(ui, theme::WARN, theme::WARN_BG, &notice);
    }
    status_line(ui, state, view, &mut out);
    if view.printing {
        ui.label(RichText::new(
            "printing: transfers share the printer's Wi-Fi")
            .color(theme::TEXT_DIM).size(11.0));
    }
    if let Some(error) = state.error.clone() {
        error_card(ui, &error, view.serial, &mut out);
    }
    ui.add_space(4.0);
    controls(ui, files);
    ui.add_space(6.0);

    if files.tab == Tab::Recordings {
        out.cmds.extend(state.open_recordings());
    }
    // the tabs list the unreadable entries they cannot show (5.10)
    for (dir, count) in damaged_dirs(state, files.tab) {
        banner(ui, theme::WARN, theme::WARN_BG, &format!(
            "{count} entries in {dir} can't be read; the SD card's file \
             system looks damaged"));
    }

    let total = ui.available_width();
    let list_w = (total - DETAIL_W - 10.0).max(240.0);
    // tiles per grid row: a month's tiles wrap instead of running off the
    // edge, and the rows stay short enough to virtualise
    let gap = ui.spacing().item_spacing.x;
    let columns = (((list_w + gap) / (TILE_W + gap)).floor() as usize).max(1);
    // only orphan thumbnails, or a damaged card: say why above the tiles
    // that are left (5.5)
    if files.tab == Tab::Timelapses && !state.timelapses.is_empty()
        && let Some(notice) = state.timelapse_notice()
    {
        banner(ui, theme::TEXT_DIM, theme::CARD, notice);
        ui.add_space(4.0);
    }
    let rows = build_rows(state, files, columns);
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(vec2(list_w, ui.available_height()),
            egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_width(list_w);
            // the list never eats the detail pane's width
            ui.set_max_width(list_w);
            list(ui, state, files, view, &rows, &mut out);
        });
        ui.allocate_ui_with_layout(vec2(ui.available_width(),
                                        ui.available_height()),
            egui::Layout::top_down(egui::Align::Min), |ui| {
            detail_pane(ui, state, files, view);
        });
    });
    out
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
        ui.label(RichText::new(format!("{} / FILES", view.name))
            .font(theme::bold(16.0)));
        ui.add_space(10.0);
        let tabs = [
            (Tab::Timelapses, format!("Timelapses {timelapses}")),
            (Tab::Recordings, format!("Recordings {recordings}")),
            (Tab::Files, format!("Print files {print_files}")),
        ];
        for (tab, label) in tabs {
            ui.selectable_value(&mut files.tab, tab, label);
        }
    });
}

fn status_line(ui: &mut Ui, state: &BrowserState, view: &View<'_>,
               out: &mut Outcome) {
    let mut parts: Vec<String> = Vec::new();
    for name in SPACE_DIRS {
        let dir = match name {
            "/" => "/".to_string(),
            name => format!("/{name}"),
        };
        if !matches!(state.dirs.get(&dir), Some(DirState::Ready { .. })) {
            continue;
        }
        let bytes = state.used_bytes(&dir);
        if bytes > 0 {
            let label = if name == "/" { "root" } else { name };
            parts.push(format!("{label} {}", human_bytes(bytes)));
        }
    }
    if let Some(at) = state.updated_at() {
        parts.push(format!("updated {} ago",
                           ago(view.now.saturating_duration_since(at))));
    }
    let summary = match parts.is_empty() {
        true => "no listing yet".to_string(),
        false => parts.join("  ·  "),
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(summary).color(theme::TEXT_DIM).size(11.5));
        ui.label(RichText::new("(printer clock)").color(theme::TEXT_DIM)
            .size(11.5));
        if ui.button("Refresh").clicked() {
            out.actions.push(Action::Refresh);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
            // the line gate G4 reads (section 6)
            let mut line = format!("FTP session: {}",
                                   state.conn.label(view.now));
            if view.open_sessions > 1 {
                line.push_str(&format!("  ({} open)", view.open_sessions));
            }
            let color = match state.conn {
                ConnState::Open { .. } => theme::ACCENT,
                ConnState::Stopped(_) => theme::DANGER,
                _ => theme::TEXT_DIM,
            };
            ui.label(RichText::new(line).color(color).size(11.5));
        });
    });
}

fn controls(ui: &mut Ui, files: &mut FilesUi) {
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut files.filter)
            .hint_text("filter…")
            .desired_width(180.0));
        egui::ComboBox::from_id_salt("files-sort")
            .selected_text(format!("Sort: {}", files.sort.label()))
            .show_ui(ui, |ui| {
                for sort in [Sort::Newest, Sort::Name, Sort::Size] {
                    ui.selectable_value(&mut files.sort, sort, sort.label());
                }
            });
        if files.tab == Tab::Files {
            ui.add_space(6.0);
            for shown in [Shown::All, Shown::Sent, Shown::Cache,
                          Shown::BuiltIn, Shown::Folders] {
                ui.selectable_value(&mut files.shown, shown, shown.label());
            }
        }
    });
}

fn banner(ui: &mut Ui, color: Color32, background: Color32, text: &str) {
    egui::Frame::new()
        .fill(background)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(8)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(RichText::new(text).color(color)
                .size(12.0)).wrap());
        });
}

/// The error card of 5.10: the condition's text and a Retry. Never a spinner
/// that goes on forever.
fn error_card(ui: &mut Ui, error: &FtpError, serial: &str,
              out: &mut Outcome) {
    egui::Frame::new()
        .fill(theme::DANGER_BG)
        .stroke(Stroke::new(1.0, theme::DANGER))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(12)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(RichText::new(error.text(serial))
                .color(theme::DANGER).size(12.5)).wrap());
            ui.add_space(6.0);
            if ui.button("Retry").clicked() {
                out.actions.push(Action::Retry);
            }
        });
}

/// The refusal card of section 6. There is no trust, accept or continue
/// action; Close is the default, focused and bound to Enter and Esc.
fn refusal_card(ui: &mut Ui, files: &mut FilesUi, title: &str, body: &str,
                out: &mut Outcome) {
    let mut close = ui.input(|i| i.key_pressed(egui::Key::Enter)
        || i.key_pressed(egui::Key::Escape));
    egui::Frame::new()
        .fill(theme::DANGER_BG)
        .stroke(Stroke::new(1.0, theme::DANGER))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(16)
        .show(ui, |ui| {
            ui.set_width(ui.available_width().min(640.0));
            ui.label(RichText::new(title).color(theme::DANGER)
                .font(theme::bold(15.0)));
            ui.add_space(8.0);
            ui.add(egui::Label::new(RichText::new(body).size(13.0)).wrap());
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if ui.button("Edit printer").clicked() {
                    out.actions.push(Action::EditPrinter);
                }
                // the default action, and the only other one
                let response = ui.add(egui::Button::new(
                    RichText::new("Close").font(theme::bold(13.5))));
                if !response.has_focus() {
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

fn row_height(row: &Row) -> f32 {
    match row {
        Row::Heading(_) => HEADING_H,
        Row::Tiles(_) => TILE_H,
        Row::Item(_) | Row::Folder { .. } | Row::Skeleton => ROW_H,
        Row::Companion(_) => 26.0,
        Row::Note(_) => 22.0,
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

fn build_rows(state: &BrowserState, files: &FilesUi, columns: usize)
              -> Rows {
    match files.tab {
        Tab::Timelapses => timelapse_rows(state, files, columns),
        Tab::Recordings => recording_rows(state, files),
        Tab::Files => file_rows(state, files),
    }
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

fn timelapse_rows(state: &BrowserState, files: &FilesUi, columns: usize)
                  -> Rows {
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
    let empty = (order.is_empty() && !loading)
        .then(|| state.timelapse_notice().unwrap_or(
            "No timelapses on this printer.").to_string());
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
    let empty = (order.is_empty() && !loading).then(|| {
        missing_note(state, RECORDINGS_DIR).unwrap_or_else(||
            "No recordings on this printer.".to_string())
    });
    Rows { rows, order, empty, loading }
}

fn file_rows(state: &BrowserState, files: &FilesUi) -> Rows {
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
        let folders = folder_rows(state, files);
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
    let empty = (rows.is_empty() && !loading)
        .then(|| "No print files on this printer.".to_string());
    Rows { rows, order, empty, loading }
}

/// The "Other folders" of 5.5, opened one level at a time.
fn folder_rows(state: &BrowserState, files: &FilesUi) -> Vec<Row> {
    /// Adds this folder and, when it is open, one level of its entries.
    /// Returns whether anything here survived the filter; what did not is
    /// taken back off the row list.
    fn walk(state: &BrowserState, files: &FilesUi, entry: &RemoteEntry,
            depth: usize, rows: &mut Vec<Row>) -> bool {
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
                rows.push(Row::Note(
                    format!("{}: {}", entry.path, err.text(""))));
                kept = true;
            }
            Some(DirState::Ready { entries, .. }) => {
                if entries.is_empty() {
                    rows.push(Row::Note("(empty)".to_string()));
                }
                for child in entries {
                    if child.is_dir {
                        kept |= walk(state, files, child, depth + 1, rows);
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
        walk(state, files, entry, 0, &mut rows);
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
        view: &View<'_>, rows: &Rows, out: &mut Outcome) {
    if rows.loading && rows.rows.is_empty() {
        skeletons(ui, files.tab);
        return;
    }
    if let Some(empty) = &rows.empty {
        card_frame(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(RichText::new(empty)
                .color(theme::TEXT_DIM).size(12.5)).wrap());
        });
        return;
    }
    let mut seen: HashSet<String> = HashSet::new();
    let heights: Vec<f32> = rows.rows.iter().map(row_height).collect();
    virtual_rows(ui, &heights, |ui, index| {
        match &rows.rows[index] {
            Row::Heading(text) => {
                ui.label(RichText::new(text).color(theme::TEXT_DIM)
                    .font(theme::bold(11.5)));
            }
            Row::Tiles(tiles) => {
                // top-aligned, so tiles of a row start on the same line
                ui.horizontal_top(|ui| {
                    for tile in tiles {
                        // the row's layout is horizontal and a Frame
                        // inherits it, so each tile gets a top-down Ui of
                        // its own: picture first, then its two lines
                        ui.allocate_ui_with_layout(
                            vec2(TILE_W, TILE_H),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| timelapse_tile(ui, state, files, view,
                                                *tile, &mut seen, out));
                    }
                });
            }
            Row::Item(position) => match files.tab {
                Tab::Recordings => {
                    let entry = state.recordings[rows.order[*position]]
                        .clone();
                    entry_row(ui, files, &entry, "REC", 0.0);
                }
                _ => {
                    let item = state.files[rows.order[*position]].clone();
                    file_row(ui, files, &item, false);
                }
            },
            Row::Companion(position) => {
                let item = state.files[rows.order[*position]].clone();
                file_row(ui, files, &item, true);
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
                ui.label(RichText::new(text).color(theme::TEXT_DIM)
                    .size(11.5));
            }
            Row::Skeleton => skeleton_row(ui),
        }
    });
    if files.tab == Tab::Timelapses {
        files.visible.retain(&seen);
        let stale: Vec<String> = files.textures.keys()
            .filter(|path| !seen.contains(*path))
            .cloned()
            .collect();
        drop_textures(state, files, stale);
    }
}

/// The variant of `ScrollArea::show_rows` this view needs: its rows have
/// different heights (month headings, tile rows, companions), and it must
/// not nest inside the outer `ScrollArea` (section 6), which is why the
/// files view replaces the panel instead of being drawn inside it.
fn virtual_rows(ui: &mut Ui, heights: &[f32],
                mut render: impl FnMut(&mut Ui, usize)) {
    let spacing = ui.spacing().item_spacing.y;
    let mut offsets: Vec<f32> = Vec::with_capacity(heights.len() + 1);
    let mut y = 0.0;
    for height in heights {
        offsets.push(y);
        y += height + spacing;
    }
    offsets.push(y);
    let total = (y - spacing).max(0.0);
    egui::ScrollArea::vertical()
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
                for row in first..last {
                    ui.push_id(row, |ui| render(ui, row));
                }
            });
        });
}

fn skeletons(ui: &mut Ui, tab: Tab) {
    ui.label(RichText::new(match tab {
        Tab::Timelapses => "listing /timelapse…",
        Tab::Recordings => "listing /ipcam…",
        Tab::Files => "listing /cache…",
    }).color(theme::TEXT_DIM).size(11.5));
    for _ in 0..6 {
        skeleton_row(ui);
    }
}

fn skeleton_row(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width().min(420.0), 20.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(6), theme::CARD_HOVER);
}

fn timelapse_tile(ui: &mut Ui, state: &mut BrowserState, files: &mut FilesUi,
                  view: &View<'_>, index: usize, seen: &mut HashSet<String>,
                  out: &mut Outcome) {
    let Some(item) = state.timelapses.get(index) else { return };
    let stem = item.stem().to_string();
    let key = item.video.as_ref().or(item.thumb.as_ref())
        .map(|entry| entry.path.clone())
        .unwrap_or_else(|| stem.clone());
    let thumb = item.thumb.clone();
    let orphan = item.video.is_none();
    let started = item.started;
    let size = item.video.as_ref().or(item.thumb.as_ref())
        .map_or(0, |entry| entry.size);
    let selected = files.selected.as_deref() == Some(key.as_str());

    let response = egui::Frame::new()
        .fill(theme::CARD)
        .stroke(Stroke::new(if selected { 2.0 } else { 1.0 },
                            if selected { theme::ACCENT }
                            else { theme::BORDER }))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(6)
        .show(ui, |ui| {
            ui.set_width(TILE_W - 12.0);
            let (rect, _) = ui.allocate_exact_size(
                vec2(TILE_W - 12.0, TILE_IMAGE_H), Sense::hover());
            ui.painter().rect_filled(rect, CornerRadius::same(10),
                                     Color32::BLACK);
            let texture = match &thumb {
                Some(entry) => {
                    seen.insert(entry.path.clone());
                    tile_texture(ui.ctx(), state, files, view, entry, out)
                }
                None => None,
            };
            match texture {
                Some(handle) => {
                    let size = handle.size_vec2();
                    let scale = (rect.width() / size.x)
                        .min(rect.height() / size.y);
                    egui::Image::new((handle.id(), size))
                        .corner_radius(CornerRadius::same(8))
                        .paint_at(ui, egui::Rect::from_center_size(
                            rect.center(), size * scale));
                }
                None => {
                    let caption = match (&thumb, orphan) {
                        (None, _) => "no thumbnail",
                        (Some(_), _) => "…",
                    };
                    ui.painter().text(rect.center(),
                        egui::Align2::CENTER_CENTER, caption,
                        egui::FontId::proportional(11.0), theme::TEXT_DIM);
                }
            }
            if orphan {
                ui.label(RichText::new("⚠ no video").color(theme::WARN)
                    .size(11.0));
            } else {
                ui.label(RichText::new(when_text(started))
                    .color(theme::TEXT_DIM).size(11.0));
            }
            ui.label(RichText::new(human_bytes(size)).size(11.5));
        })
        .response
        .interact(Sense::click());
    if response.hovered() {
        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
    }
    if response.clicked() {
        files.selected = Some(key);
    }
}

/// The tile's texture, built once from the decoded picture the lane sent,
/// and asked for only after the tile stayed visible (section 4).
fn tile_texture(ctx: &egui::Context, state: &mut BrowserState,
                files: &mut FilesUi, view: &View<'_>, entry: &RemoteEntry,
                out: &mut Outcome) -> Option<egui::TextureHandle> {
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
    if files.visible.ready(&entry.path, view.now)
        && let Some(cmd) = state.request_thumb(entry, THUMB_PX)
    {
        out.cmds.push(cmd);
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
             icon: &str, indent: f32) -> bool {
    let selected = files.selected.as_deref() == Some(entry.path.as_str());
    let response = egui::Frame::new()
        .fill(if selected { theme::CARD_HOVER } else { theme::CARD })
        .stroke(Stroke::new(1.0, if selected { theme::ACCENT }
                                 else { theme::BORDER }))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width() - indent);
            ui.horizontal(|ui| {
                if indent > 0.0 {
                    ui.add_space(indent);
                }
                ui.label(RichText::new(icon).color(theme::TEXT_DIM)
                    .font(theme::bold(10.5)));
                let name = match entry.unreadable {
                    true => RichText::new(&entry.name)
                        .color(theme::TEXT_DIM).italics(),
                    false => RichText::new(&entry.name).size(12.5),
                };
                ui.add(egui::Label::new(name).truncate());
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(when_text(entry.mtime))
                            .color(theme::TEXT_DIM).size(11.0));
                        if !entry.is_dir {
                            ui.label(RichText::new(human_bytes(entry.size))
                                .color(theme::TEXT_DIM).size(11.0));
                        }
                    });
            });
        })
        .response
        .interact(Sense::click());
    if response.hovered() && !entry.unreadable {
        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
    }
    let clicked = response.clicked() && !entry.unreadable;
    if clicked && !entry.is_dir {
        files.selected = Some(entry.path.clone());
    }
    clicked
}

fn file_row(ui: &mut Ui, files: &mut FilesUi, item: &FileItem,
            companion: bool) {
    if companion {
        let plate = item.plate_hint
            .map(|plate| format!(" (plate {plate})"))
            .unwrap_or_default();
        ui.horizontal(|ui| {
            ui.add_space(22.0);
            let text = format!("↳ printer's extracted copy: {}  {}{plate}",
                               item.remote.name,
                               human_bytes(item.remote.size));
            let response = ui.add(egui::Label::new(RichText::new(text)
                .color(theme::TEXT_DIM).size(11.5))
                .sense(Sense::click()));
            if response.clicked() {
                files.selected = Some(item.remote.path.clone());
            }
        });
        return;
    }
    entry_row(ui, files, &item.remote, kind_icon(item.kind), 0.0);
}

fn folder_row(ui: &mut Ui, files: &mut FilesUi, entry: &RemoteEntry,
              depth: usize, open: bool) -> bool {
    let icon = match (entry.is_dir, open) {
        (true, true) => "▾ DIR",
        (true, false) => "▸ DIR",
        (false, _) => "FILE",
    };
    entry_row(ui, files, entry, icon, 14.0 * depth as f32)
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

fn detail_pane(ui: &mut Ui, state: &BrowserState, files: &FilesUi,
               view: &View<'_>) {
    card_frame(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new("DETAILS").color(theme::TEXT_DIM)
            .font(theme::bold(11.5)));
        ui.add_space(6.0);
        let Some(selected) = files.selected.as_deref() else {
            ui.label(RichText::new("Select a file to see its details.")
                .color(theme::TEXT_DIM).size(12.0));
            return;
        };
        if let Some(item) = state.timelapses.iter()
            .find(|item| item.video.as_ref().or(item.thumb.as_ref())
                .is_some_and(|entry| entry.path == selected))
        {
            ui.add(egui::Label::new(RichText::new(item.stem())
                .font(theme::bold(13.0))).wrap());
            ui.add_space(4.0);
            fact(ui, "PRINT", &format!("{} → {}", when_text(item.started),
                                       when_text(item.ended)));
            match &item.video {
                Some(video) => fact(ui, "VIDEO",
                    &format!("{}  ·  {}", human_bytes(video.size),
                             extension(&video.name))),
                None => fact(ui, "VIDEO",
                             "deleted; only the thumbnail is left"),
            }
            if let Some(thumb) = &item.thumb {
                fact(ui, "THUMB", &human_bytes(thumb.size));
            }
            clock_note(ui, view);
            return;
        }
        if let Some(entry) = state.recordings.iter()
            .find(|entry| entry.path == selected)
        {
            ui.add(egui::Label::new(RichText::new(&entry.name)
                .font(theme::bold(13.0))).wrap());
            ui.add_space(4.0);
            fact(ui, "SIZE", &human_bytes(entry.size));
            fact(ui, "TIME", &when_text(entry.mtime));
            clock_note(ui, view);
            return;
        }
        if let Some(item) = state.files.iter()
            .find(|item| item.remote.path == selected)
        {
            ui.add(egui::Label::new(RichText::new(&item.remote.name)
                .font(theme::bold(13.0))).wrap());
            ui.add_space(4.0);
            fact(ui, "KIND", kind_label(item.kind));
            fact(ui, "SIZE", &human_bytes(item.remote.size));
            fact(ui, "TIME", &when_text(item.remote.mtime));
            if let Some(plate) = item.plate_hint {
                fact(ui, "PLATE", &plate.to_string());
            }
            if let Some(root) = &item.companion_of {
                fact(ui, "COPY OF", root);
            }
            clock_note(ui, view);
            return;
        }
        // an "Other folder" entry, or something a refresh dropped
        let entry = state.dirs.values()
            .filter_map(|dir| match dir {
                DirState::Ready { entries, .. } => Some(entries),
                _ => None,
            })
            .flatten()
            .find(|entry| entry.path == selected);
        match entry {
            Some(entry) => {
                ui.add(egui::Label::new(RichText::new(&entry.name)
                    .font(theme::bold(13.0))).wrap());
                ui.add_space(4.0);
                fact(ui, "PATH", &entry.path);
                if !entry.is_dir {
                    fact(ui, "SIZE", &human_bytes(entry.size));
                }
                fact(ui, "TIME", &when_text(entry.mtime));
                clock_note(ui, view);
            }
            None => {
                ui.label(RichText::new("Select a file to see its details.")
                    .color(theme::TEXT_DIM).size(12.0));
            }
        }
    });
}

fn fact(ui: &mut Ui, label: &str, value: &str) {
    ui.horizontal_top(|ui| {
        ui.label(RichText::new(label).color(theme::TEXT_DIM)
            .font(theme::bold(10.5)));
        ui.add(egui::Label::new(RichText::new(value).size(12.0)).wrap());
    });
}

fn clock_note(ui: &mut Ui, view: &View<'_>) {
    let mut note = "times are the printer's clock".to_string();
    if view.printing {
        note.push_str(", and it is printing");
    }
    ui.add_space(6.0);
    ui.label(RichText::new(note).color(theme::TEXT_DIM).size(10.5));
}

// --------------------------------------------------------------- helpers

fn extension(name: &str) -> String {
    name.rsplit_once('.').map(|(_, ext)| ext.to_uppercase())
        .unwrap_or_else(|| "FILE".to_string())
}

/// Printer-clock time, never converted (section 6).
fn when_text(when: Option<NaiveDateTime>) -> String {
    match when {
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
        ctx
    }

    fn raw(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                Pos2::ZERO, Vec2::new(1180.0, 820.0))),
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

    fn view(serial: &str, now: Instant) -> View<'_> {
        View { name: "P1S #1", serial, profile: Some(ServerProfile::BblP003),
               open_sessions: 0, printing: false, now }
    }

    fn thumb_image() -> egui::ColorImage {
        egui::ColorImage::from_rgba_unmultiplied([2, 2], &[200; 16])
    }

    fn files_ui(tab: Tab) -> FilesUi {
        FilesUi { tab, ..FilesUi::default() }
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
        assert!(painted.has(&format!("Recordings {recordings}")));
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

        let out = click(&ctx, &mut state, &mut files, &view(P1S, now),
                        "Recordings 0");
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
        assert!(painted.has("the SD card's file system looks damaged"));
        assert!(painted.has("2 entries in /timelapse can't be read"));

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

    /// The detail pane shows facts only: this stage has no download, so it
    /// offers no action at all (section 6, no dead buttons).
    #[test]
    fn the_detail_pane_shows_facts_and_no_actions() {
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
        for absent in ["Download", "Save to PC", "Open in player",
                       "Show in folder", "Load", "Read header",
                       "Clear cache", "Delete"] {
            assert!(!painted.has(absent), "{absent} has no lane yet");
        }
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

    #[test]
    fn the_month_heading_names_the_month() {
        assert_eq!(month_heading(Some((2026, 7))), "JULY 2026");
        assert_eq!(month_heading(Some((2026, 1))), "JANUARY 2026");
        assert_eq!(month_heading(None), "NO DATE");
    }
}
