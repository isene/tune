//! tune — Spotify Connect controller TUI. Part of Fe₂O₃.
//!
//! Architecture: rspotify (sync, ureq backend) for all API calls;
//! crust for the panes. Single foreground thread — Spotify state
//! polled at `config.poll_s` cadence (default 2s) from the input
//! loop, between key events.

mod auth;
mod config;
mod player;

use crust::{Crust, Cursor, Input, Pane, style};
use glow::Display;
use std::path::PathBuf;
use rspotify::{AuthCodePkceSpotify};
use rspotify::clients::{BaseClient, OAuthClient};
use rspotify::model::{
    AdditionalType, Country, CurrentPlaybackContext, Device, FullArtist, FullTrack,
    Market, PlayableItem, PlayContextId, RepeatState, SearchResult, SearchType,
    SimplifiedPlaylist,
};

/// One row in the Search results list. Artists and tracks both show
/// up in the same scrollable list (artists first so the user can
/// drill into them); ENTER dispatches per-variant.
#[derive(Debug, Clone)]
enum SearchRow {
    Artist(FullArtist),
    Track(FullTrack),
}
use rspotify::model::idtypes::Id;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Search,
    Playlists,
    PlaylistTracks,
    Saved,
    Queue,
    Devices,
    Help,
}

struct App {
    // Spotify
    spotify: AuthCodePkceSpotify,
    user_display_name: String,

    // Live state (refreshed via poll_state)
    playback: Option<CurrentPlaybackContext>,
    last_poll: std::time::Instant,
    poll_interval: std::time::Duration,
    /// Last wall-clock instant at which we re-rendered the
    /// now-playing strip with a locally-extrapolated progress value.
    /// Lets the progress bar tick every ~1 s without hitting the
    /// Spotify Web API (which we still poll every `poll_interval`
    /// seconds for ground-truth resync). Battery cheap: no network,
    /// no syscalls, just a stdout diff write to the 4-row pane.
    last_progress_tick: std::time::Instant,

    // UI
    header: Pane,
    main_p: Pane,
    now_p:  Pane,   // 4-row now-playing strip
    footer: Pane,
    cols:   u16,
    rows:   u16,
    view:   View,
    status: Option<(String, u8)>,

    // List state per view (selected row + items)
    search_query:     String,
    search_results:   Vec<SearchRow>,
    search_idx:       usize,
    playlists:        Vec<SimplifiedPlaylist>,
    playlists_idx:    usize,
    playlist_tracks:  Vec<FullTrack>,
    playlist_tracks_idx: usize,
    open_playlist:    Option<SimplifiedPlaylist>,
    saved_tracks:     Vec<FullTrack>,
    saved_idx:        usize,
    queue:            Vec<FullTrack>,
    queue_idx:        usize,
    devices:          Vec<Device>,
    devices_idx:      usize,
    devices_loaded_at: Option<std::time::Instant>,
    /// Config-supplied preferred device — matched by name or id. Empty
    /// = "no preference, pick whatever Spotify has active".
    default_device: String,

    /// glow renderer for the album cover thumbnail rendered in the
    /// left edge of the now-playing strip. Lazy-init on first show so
    /// a Connect-only session (no playback yet) doesn't probe kitty /
    /// sixel / chafa at startup. `cover_track_id` is the currently-
    /// displayed track id; we re-download + re-place only on change.
    cover_display:  Option<Display>,
    cover_track_id: Option<String>,
}

/// Height of the now-playing strip in rows. 10 rows of cover height
/// at typical 8×16-pixel terminal cells = 160 px tall. Paired with
/// COVER_W below to keep the thumbnail roughly square.
const NOW_H: u16 = 10;
/// Cover thumbnail width in cells. 20 cells × 8 px ≈ 160 px wide
/// (roughly square at typical cell aspect ratios — terminal cells
/// are ~1:2, so 20w × 10h cells maps to a near-square pixel box).
const COVER_W: u16 = 20;
/// Left padding before the cover. Starting at col 2 leaves a one-
/// cell gutter against the terminal edge instead of butting the
/// art straight up against the frame.
const COVER_X: u16 = 2;

impl App {
    fn new(spotify: AuthCodePkceSpotify, poll_s: u64, default_device: String) -> Self {
        let (cols, rows) = Crust::terminal_size();
        let header = Pane::new(1, 1, cols, 1, t::FG_BRIGHT as u16, t::BG_BAR as u16);
        let main_p = Pane::new(1, 2, cols, rows.saturating_sub(2 + NOW_H + 1),
                               t::FG as u16, 0);
        // now-pane spans full width — glow draws the cover at z=1
        // (above text) so the leftmost cells of the pane bg show
        // through everywhere the cover doesn't cover. Text content
        // is indented past `COVER_W` cells inside render_now.
        let now_p  = Pane::new(1, rows - NOW_H, cols, NOW_H,
                               t::FG_BRIGHT as u16, t::BG_NOW as u16);
        let footer = Pane::new(1, rows, cols, 1, t::FG as u16, t::BG_BAR as u16);
        let mut header = header; header.wrap = false; header.scroll = false;
        let mut footer = footer; footer.wrap = false; footer.scroll = false;
        let mut now_p  = now_p;  now_p.wrap  = false; now_p.scroll  = false;

        let user_display_name = spotify.current_user().ok()
            .map(|u| u.display_name.unwrap_or_else(|| u.id.id().to_string()))
            .unwrap_or_default();

        Self {
            spotify, user_display_name,
            playback: None,
            last_poll: std::time::Instant::now() - std::time::Duration::from_secs(60),
            poll_interval: std::time::Duration::from_secs(poll_s.max(1)),
            last_progress_tick: std::time::Instant::now(),
            header, main_p, now_p, footer,
            cols, rows,
            view: View::Search,
            status: None,
            search_query: String::new(),
            search_results: Vec::new(), search_idx: 0,
            playlists: Vec::new(),      playlists_idx: 0,
            playlist_tracks: Vec::new(), playlist_tracks_idx: 0,
            open_playlist: None,
            saved_tracks: Vec::new(),   saved_idx: 0,
            queue: Vec::new(),          queue_idx: 0,
            devices: Vec::new(),        devices_idx: 0,
            devices_loaded_at: None,
            default_device,
            cover_display: None,
            cover_track_id: None,
        }
    }

    /// Pick a device id for transport calls. Spotify's playback
    /// endpoints return 404 when there's no active device AND no
    /// `device_id` query parameter, so every play / pause / next /
    /// seek call needs an explicit target when nothing is active.
    /// Resolution order:
    ///   1. Active device from current playback state, if any.
    ///   2. Config-supplied `default_device` (matched by name first,
    ///      then by id), if it's currently online.
    ///   3. First device on the Spotify Connect device list.
    ///   4. None — caller should surface a "no devices online" hint.
    fn resolve_device_id(&mut self) -> Option<String> {
        if let Some(pb) = &self.playback {
            if let Some(id) = pb.device.id.clone() {
                return Some(id);
            }
        }
        // Refresh the device list at most every 30 s; an inactive
        // pick made off a stale snapshot would 404 anyway.
        let stale = self.devices_loaded_at
            .map(|t| t.elapsed().as_secs() > 30)
            .unwrap_or(true);
        if stale { self.load_devices(); }
        if !self.default_device.is_empty() {
            if let Some(d) = self.devices.iter().find(|d|
                d.name == self.default_device
                || d.id.as_deref() == Some(&self.default_device))
            {
                if let Some(id) = d.id.clone() { return Some(id); }
            }
        }
        self.devices.iter().find(|d| d.is_active).and_then(|d| d.id.clone())
            .or_else(|| self.devices.first().and_then(|d| d.id.clone()))
    }

    fn render_all(&mut self) {
        self.render_header();
        self.render_main();
        self.render_now();
        self.render_footer();
        // Park the terminal cursor in an invisible corner — no field
        // needs a visible caret on this screen.
        Cursor::hide();
    }

    fn render_header(&mut self) {
        let dev = self.playback.as_ref()
            .map(|p| p.device.name.as_str()).unwrap_or("—");
        let view_lbl = match self.view {
            View::Search          => "Search",
            View::Playlists       => "Playlists",
            View::PlaylistTracks  => self.open_playlist.as_ref()
                .map(|p| p.name.as_str()).unwrap_or("Playlist"),
            View::Saved           => "Saved",
            View::Queue           => "Queue",
            View::Devices         => "Devices",
            View::Help            => "Help",
        };
        let left = format!(" tune  [{}]", style::bold(&style::fg(view_lbl, t::ACCENT)));
        let right = format!("{}  {}  v{} ",
            style::fg(&self.user_display_name, t::FG_MUTED),
            style::fg(&format!("◆ {}", dev), t::CYAN),
            VERSION);
        let pad_w = (self.cols as usize)
            .saturating_sub(crust::display_width(&left) + crust::display_width(&right));
        self.header.set_text(&format!("{}{}{}", left, " ".repeat(pad_w), right));
        self.header.refresh();
    }

    fn render_main(&mut self) {
        let mut lines: Vec<String> = Vec::new();
        match self.view {
            View::Search          => self.lines_search(&mut lines),
            View::Playlists       => self.lines_playlists(&mut lines),
            View::PlaylistTracks  => self.lines_playlist_tracks(&mut lines),
            View::Saved           => self.lines_saved(&mut lines),
            View::Queue           => self.lines_queue(&mut lines),
            View::Devices         => self.lines_devices(&mut lines),
            View::Help            => self.lines_help(&mut lines),
        }
        self.main_p.set_text(&lines.join("\n"));
        // Adjust scroll so the selected row stays in view with a
        // 3-row scrolloff (vim convention). Then diff-render via
        // refresh() — full_refresh wipes the pane and causes flicker
        // every keystroke on long lists.
        self.adjust_main_scroll();
        self.main_p.refresh();
    }

    /// Keep the selected list row within `SCROLLOFF` rows of the
    /// viewport edges. Each `lines_*` view emits three header rows
    /// (blank / title / blank) before the list rows, so the visual
    /// row index of the selected item is `LIST_HEADER_OFFSET +
    /// list_idx`.
    fn adjust_main_scroll(&mut self) {
        const SCROLLOFF: usize = 3;
        const HEADER_OFFSET: usize = 3;
        let h = self.main_p.h as usize;
        if h == 0 { return; }
        let (idx, len) = self.current_list_indices();
        if len.is_none() { return; } // help view — no list
        let target = HEADER_OFFSET + idx;
        let top = self.main_p.ix;
        let bot = top + h.saturating_sub(1);
        if target < top + SCROLLOFF {
            self.main_p.ix = target.saturating_sub(SCROLLOFF);
        } else if target + SCROLLOFF > bot {
            self.main_p.ix = (target + SCROLLOFF + 1).saturating_sub(h);
        }
    }

    fn render_now(&mut self) {
        // Indent each line past the cover thumbnail: COVER_X cells
        // of left gutter, COVER_W cells of cover, and one more cell
        // of gap before the text. The pane spans full width so its
        // bg fills edge-to-edge; the kitty cover image is placed
        // at z=1 and overlays the leftmost cells without clobbering
        // the bg color underneath.
        let pad = " ".repeat((COVER_X + COVER_W + 1) as usize);
        let mut lines: Vec<String> = Vec::new();
        lines.push(String::new());
        match &self.playback {
            None => {
                lines.push(format!("{}{}", pad,
                    style::fg("Nothing playing.", t::FG_DIM)));
                lines.push(format!("{}{}", pad,
                    style::fg("Press SPACE to resume on your last device, or `d` to pick one.",
                              t::FG_DIM)));
            }
            Some(pb) => {
                let (title, artists) = match &pb.item {
                    Some(PlayableItem::Track(t)) => (
                        t.name.clone(),
                        t.artists.iter().map(|a| a.name.as_str())
                            .collect::<Vec<_>>().join(", "),
                    ),
                    Some(PlayableItem::Episode(e)) => (
                        e.name.clone(),
                        e.show.name.clone(),
                    ),
                    Some(PlayableItem::Unknown(j)) => unknown_title_artists(j),
                    _ => ("—".to_string(), "—".to_string()),
                };
                let playing = pb.is_playing;
                let symbol = if playing { "▶" } else { "⏸" };
                let symcol = if playing { t::OK } else { t::AMBER };
                let progress_ms = pb.progress.map(|d| d.num_milliseconds() as u64).unwrap_or(0);
                let total_ms = match &pb.item {
                    Some(PlayableItem::Track(t))   => t.duration.num_milliseconds() as u64,
                    Some(PlayableItem::Episode(e)) => e.duration.num_milliseconds() as u64,
                    Some(PlayableItem::Unknown(j)) => unknown_duration_ms(j),
                    _ => 0,
                };
                // Progress bar width = (total cols) − indent − fmt_ms slots.
                let bar_w = (self.cols as usize)
                    .saturating_sub(pad.len() + 20);
                let bar = progress_bar(progress_ms, total_ms, bar_w);
                let shuffle = if pb.shuffle_state { "🔀" } else { "  " };
                let repeat = match pb.repeat_state {
                    RepeatState::Off => "  ",
                    RepeatState::Context => "🔁",
                    RepeatState::Track => "🔂",
                };
                lines.push(format!("{}{} {}  {}  {}  {}",
                    pad,
                    style::fg(symbol, symcol),
                    style::bold(&style::fg(&title, t::FG_BRIGHT)),
                    style::fg("—", t::FG_DIM),
                    style::fg(&artists, t::FG),
                    style::fg(&format!("[{} {}]", shuffle, repeat), t::FG_DIM)));
                lines.push(format!("{}{}  {}  {}",
                    pad,
                    style::fg(&fmt_ms(progress_ms), t::FG_DIM),
                    bar,
                    style::fg(&fmt_ms(total_ms), t::FG_DIM)));
            }
        }
        self.now_p.set_text(&lines.join("\n"));
        self.now_p.refresh();
    }

    fn render_footer(&mut self) {
        let (msg, col) = match &self.status {
            Some((m, c)) => (m.clone(), *c),
            None => (
                " /:search  P:playlists  L:saved  Q:queue  d:devices  ?:help  q:quit ".to_string(),
                t::FG_MUTED,
            ),
        };
        self.footer.set_text(&style::fg(&msg, col));
        self.footer.refresh();
    }

    fn set_status(&mut self, msg: &str, color: u8) {
        self.status = Some((format!(" {}", msg), color));
        self.render_footer();
    }
    fn clear_status(&mut self) {
        self.status = None;
        self.render_footer();
    }

    // ---- Polling ----------------------------------------------------

    /// Extrapolate the playback `progress` by the wall-clock time
    /// since the last extrapolation, re-render the now-playing
    /// strip, and reset the timestamp. Skipped if less than 1 s has
    /// passed, if nothing is playing, or if the track is paused.
    ///
    /// The API poll (every `poll_interval` s) is the ground truth —
    /// it overwrites `progress` with whatever Spotify reports. The
    /// local tick only fills in the gap between polls so the
    /// progress bar visibly moves.
    /// Download (cache-aware) and place the album cover for the
    /// currently playing track. Called from `poll_state` after the
    /// API gives us the item details. Skipped if the same track id
    /// is still showing.
    fn sync_cover(&mut self) {
        let (track_id, url) = match self.playback.as_ref().and_then(|pb| pb.item.as_ref()) {
            Some(item) => match cover_id_and_url(item) {
                Some(pair) => pair,
                None => { self.clear_cover(); return; }
            },
            None => { self.clear_cover(); return; }
        };
        if self.cover_track_id.as_deref() == Some(track_id.as_str()) { return; }
        let cache_path = cover_cache_path(&track_id);
        if !cache_path.exists() {
            if let Err(e) = download_cover(&url, &cache_path) {
                eprintln!("cover download failed for {}: {}", track_id, e);
                return;
            }
        }
        let (_, rows) = Crust::terminal_size();
        let display = self.cover_display.get_or_insert_with(Display::new);
        // Place at (COVER_X, top row of the now-pane). show() sizes
        // to (COVER_W × NOW_H) cells; glow scales the image to fit.
        let placed = display.show(
            &cache_path.to_string_lossy(),
            COVER_X, rows.saturating_sub(NOW_H) + 1,
            COVER_W, NOW_H);
        if placed { self.cover_track_id = Some(track_id); }
    }

    fn clear_cover(&mut self) {
        if self.cover_track_id.is_none() { return; }
        let (_, rows) = Crust::terminal_size();
        if let Some(d) = self.cover_display.as_mut() {
            let (cols, _) = Crust::terminal_size();
            d.clear(COVER_X, rows.saturating_sub(NOW_H) + 1, COVER_W, NOW_H,
                    cols, rows);
        }
        self.cover_track_id = None;
    }

    fn tick_progress(&mut self) {
        let elapsed = self.last_progress_tick.elapsed();
        if elapsed < std::time::Duration::from_secs(1) { return; }
        self.last_progress_tick = std::time::Instant::now();
        let is_playing = self.playback.as_ref().map(|p| p.is_playing).unwrap_or(false);
        if !is_playing { return; }
        if let Some(pb) = self.playback.as_mut() {
            if let Some(p) = pb.progress {
                let bump = chrono::Duration::from_std(elapsed).unwrap_or_default();
                pb.progress = Some(p + bump);
            }
        }
        self.render_now();
    }

    fn poll_state(&mut self) {
        if self.last_poll.elapsed() < self.poll_interval { return; }
        self.last_poll = std::time::Instant::now();
        let additional = &[AdditionalType::Track, AdditionalType::Episode];
        match self.spotify.current_playback(None, Some(additional)) {
            Ok(pb) => {
                self.playback = pb;
                // API replaced `progress` with ground truth — restart
                // the local tick window from now so the next
                // extrapolation doesn't double-count the time that
                // elapsed during the API call.
                self.last_progress_tick = std::time::Instant::now();
                self.render_header();
                self.render_now();
                self.sync_cover();
            }
            Err(_) => { /* transient — keep last known state */ }
        }
    }

    // ---- View loaders -----------------------------------------------

    fn ensure_playlists_loaded(&mut self) {
        if !self.playlists.is_empty() { return; }
        let mut out = Vec::new();
        let mut iter = self.spotify.current_user_playlists();
        while let Some(item) = iter.next() {
            match item {
                Ok(pl) => out.push(pl),
                Err(_) => break,
            }
            if out.len() >= 200 { break; }
        }
        self.playlists = out;
    }

    fn ensure_saved_loaded(&mut self) {
        if !self.saved_tracks.is_empty() { return; }
        let mut out = Vec::new();
        let mut iter = self.spotify.current_user_saved_tracks(None);
        while let Some(item) = iter.next() {
            match item {
                Ok(st) => out.push(st.track),
                Err(_) => break,
            }
            if out.len() >= 200 { break; }
        }
        self.saved_tracks = out;
    }

    fn ensure_queue_loaded(&mut self) {
        match self.spotify.current_user_queue() {
            Ok(q) => self.queue = q.queue.into_iter().filter_map(|pi| match pi {
                PlayableItem::Track(t) => Some(t),
                _ => None,
            }).collect(),
            Err(_) => {}
        }
    }

    fn load_devices(&mut self) {
        self.devices = self.spotify.device().unwrap_or_default();
        self.devices_loaded_at = Some(std::time::Instant::now());
    }

    fn load_playlist_tracks(&mut self, pl: SimplifiedPlaylist) {
        let mut out = Vec::new();
        let mut iter = self.spotify.playlist_items(pl.id.clone_static(), None, None);
        while let Some(item) = iter.next() {
            match item {
                Ok(pi) => {
                    if let Some(PlayableItem::Track(t)) = pi.item {
                        out.push(t);
                    }
                }
                Err(_) => break,
            }
            if out.len() >= 500 { break; }
        }
        self.playlist_tracks = out;
        self.playlist_tracks_idx = 0;
        self.open_playlist = Some(pl);
    }

    // ---- Rendering per view -----------------------------------------

    fn lines_search(&self, out: &mut Vec<String>) {
        out.push(String::new());
        out.push(format!("  {}  {}",
            style::fg("/", t::ACCENT),
            if self.search_query.is_empty() {
                style::fg("Press / to search…", t::FG_DIM).to_string()
            } else {
                format!("{}  {}",
                    style::fg("query:", t::FG_DIM),
                    style::fg(&self.search_query, t::FG))
            }));
        out.push(String::new());
        for (i, row) in self.search_results.iter().enumerate() {
            let selected = i == self.search_idx;
            out.push(match row {
                SearchRow::Artist(a) => artist_row(a, selected),
                SearchRow::Track(t)  => track_row(t, selected),
            });
        }
        if self.search_results.is_empty() && !self.search_query.is_empty() {
            out.push(format!("  {}", style::fg("(no results)", t::FG_DIM)));
        }
    }

    fn lines_playlists(&self, out: &mut Vec<String>) {
        out.push(String::new());
        out.push(format!("  {}",
            style::bold(&style::fg("Your playlists", t::ACCENT))));
        out.push(String::new());
        if self.playlists.is_empty() {
            out.push(format!("  {}", style::fg("Loading…", t::FG_DIM)));
            return;
        }
        for (i, pl) in self.playlists.iter().enumerate() {
            let cursor = if i == self.playlists_idx { "▸" } else { " " };
            let count = pl.items.total;
            let line = format!("  {}  {:<40}  {}",
                cursor,
                truncate(&pl.name, 40),
                style::fg(&format!("{} tracks", count), t::FG_DIM));
            out.push(if i == self.playlists_idx {
                style::bold(&style::fg(&line, t::FG_BRIGHT)).to_string()
            } else { line });
        }
    }

    fn lines_playlist_tracks(&self, out: &mut Vec<String>) {
        out.push(String::new());
        let title = self.open_playlist.as_ref().map(|p| p.name.as_str())
            .unwrap_or("Playlist");
        out.push(format!("  {}",
            style::bold(&style::fg(title, t::ACCENT))));
        out.push(String::new());
        if self.playlist_tracks.is_empty() {
            out.push(format!("  {}", style::fg("Loading…", t::FG_DIM)));
            return;
        }
        for (i, t) in self.playlist_tracks.iter().enumerate() {
            out.push(track_row(t, i == self.playlist_tracks_idx));
        }
    }

    fn lines_saved(&self, out: &mut Vec<String>) {
        out.push(String::new());
        out.push(format!("  {}",
            style::bold(&style::fg("Your liked songs", t::ACCENT))));
        out.push(String::new());
        if self.saved_tracks.is_empty() {
            out.push(format!("  {}", style::fg("Loading…", t::FG_DIM)));
            return;
        }
        for (i, t) in self.saved_tracks.iter().enumerate() {
            out.push(track_row(t, i == self.saved_idx));
        }
    }

    fn lines_queue(&self, out: &mut Vec<String>) {
        out.push(String::new());
        out.push(format!("  {}",
            style::bold(&style::fg("Up next", t::ACCENT))));
        out.push(String::new());
        if self.queue.is_empty() {
            out.push(format!("  {}",
                style::fg("(queue empty — Spotify shows context-driven autoplay here)",
                    t::FG_DIM)));
            return;
        }
        for (i, t) in self.queue.iter().enumerate() {
            out.push(track_row(t, i == self.queue_idx));
        }
    }

    fn lines_devices(&self, out: &mut Vec<String>) {
        out.push(String::new());
        out.push(format!("  {}",
            style::bold(&style::fg("Spotify Connect devices", t::ACCENT))));
        out.push(String::new());
        if self.devices.is_empty() {
            out.push(format!("  {}",
                style::fg("No devices available (open Spotify on your phone or desktop first).",
                    t::FG_DIM)));
            return;
        }
        for (i, dev) in self.devices.iter().enumerate() {
            let active = dev.is_active;
            let cursor = if i == self.devices_idx { "▸" } else { " " };
            let marker = if active { "●" } else { "○" };
            let kind: &'static str = (&dev._type).into();
            let vol = dev.volume_percent.map(|v| format!("{}%", v))
                .unwrap_or_else(|| "—".to_string());
            let line = format!("  {}  {} {:<24}  {:<10}  vol {}",
                cursor, marker,
                truncate(&dev.name, 24),
                kind, vol);
            out.push(if i == self.devices_idx {
                style::bold(&style::fg(&line, t::FG_BRIGHT)).to_string()
            } else if active {
                style::fg(&line, t::OK).to_string()
            } else { line });
        }
    }

    fn lines_help(&self, out: &mut Vec<String>) {
        let k = |s: &str| style::fg(s, t::ACCENT);
        let h = |s: &str| style::bold(&style::fg(s, t::CYAN));
        out.push(String::new());
        out.push(format!("  {}", style::bold(&style::fg(
            "tune — keys", t::ACCENT))));
        out.push(String::new());
        out.push(format!("  {}", h("Views")));
        for (key, desc) in [
            ("/", "Search"),
            ("P", "Playlists"),
            ("L", "Liked / saved tracks"),
            ("Q", "Up-next queue"),
            ("d", "Devices (Spotify Connect)"),
            ("?", "This help"),
        ] {
            out.push(format!("    {:<8} {}", k(key), desc));
        }
        out.push(String::new());
        out.push(format!("  {}", h("Playback")));
        for (key, desc) in [
            ("SPACE",  "Play / pause"),
            ("n",      "Next track"),
            ("b",      "Previous track"),
            ("+ / -",  "Volume up / down (5%)"),
            ("[ / ]",  "Seek -5s / +5s"),
            ("s",      "Toggle shuffle"),
            ("r",      "Cycle repeat mode (off / context / track)"),
        ] {
            out.push(format!("    {:<8} {}", k(key), desc));
        }
        out.push(String::new());
        out.push(format!("  {}", h("Lists")));
        for (key, desc) in [
            ("j / k",  "Down / up"),
            ("g / G",  "Top / bottom"),
            ("ENTER",  "Play this item / open playlist"),
            ("a",      "Add this track to the queue"),
            ("h",      "Back to playlist list"),
        ] {
            out.push(format!("    {:<8} {}", k(key), desc));
        }
        out.push(String::new());
        out.push(format!("  {}", h("Misc")));
        for (key, desc) in [
            ("R",      "Refresh now-playing"),
            ("q",      "Quit"),
        ] {
            out.push(format!("    {:<8} {}", k(key), desc));
        }
    }

    // ---- Key dispatch -----------------------------------------------

    fn handle(&mut self, key: &str) -> bool {
        self.clear_status();
        // Help view: q backs out instead of quitting (otherwise `?`
        // → `q` flow would quit the app, which surprises everyone).
        if (key == "q" || key == "Q") && self.view == View::Help {
            self.view = View::Search;
            self.render_header();
            self.render_main();
            return false;
        }
        if key == "q" { return true; }
        let was = self.view;
        match key {
            // ---- view switching ----
            "?"  => { self.view = View::Help;       self.main_p.ix = 0; }
            "/"  => { self.view = View::Search;     self.main_p.ix = 0;
                      self.search_prompt(); }
            "P"  => { self.view = View::Playlists;  self.main_p.ix = 0;
                      self.ensure_playlists_loaded(); }
            "L"  => { self.view = View::Saved;      self.main_p.ix = 0;
                      self.ensure_saved_loaded(); }
            "Q"  => { self.view = View::Queue;      self.main_p.ix = 0;
                      self.ensure_queue_loaded(); }
            "d"  => { self.view = View::Devices;    self.main_p.ix = 0;
                      self.load_devices(); }
            "h" if self.view == View::PlaylistTracks => {
                self.view = View::Playlists;        self.main_p.ix = 0;
            }
            // ---- list navigation ----
            "j" | "DOWN" => self.list_down(),
            "k" | "UP"   => self.list_up(),
            "g" | "HOME" => self.list_top(),
            "G" | "END"  => self.list_bottom(),
            "PgDOWN"     => for _ in 0..10 { self.list_down(); },
            "PgUP"       => for _ in 0..10 { self.list_up(); },
            "ENTER"      => self.list_activate(),
            "a"          => self.queue_current(),
            // ---- playback ----
            " " | "SPACE" => self.toggle_play(),
            "n"           => self.skip_next(),
            "b"           => self.skip_prev(),
            "+"           => self.bump_volume( 5),
            "-"           => self.bump_volume(-5),
            "]"           => self.seek_relative( 5_000),
            "["           => self.seek_relative(-5_000),
            "s"           => self.toggle_shuffle(),
            "r"           => self.cycle_repeat(),
            "R"           => { self.last_poll = std::time::Instant::now() - self.poll_interval * 2;
                              self.poll_state(); }
            _ => {}
        }
        if self.view != was {
            self.render_header();
        }
        self.render_main();
        false
    }

    fn search_prompt(&mut self) {
        Cursor::show();
        let q = self.footer.ask(" search: ", &self.search_query);
        Cursor::hide();
        let q = q.trim().to_string();
        if q.is_empty() { return; }
        self.search_query = q.clone();
        // Spotify capped the Search API `limit` to 10 (down from 50)
        // in Feb 2026 — passing more returns 400 Bad Request. We do
        // two narrow calls (Artist + Track) and stitch them so the
        // user sees both kinds in one list, artists first (smaller
        // count, prominent).
        let market = Some(Market::Country(Country::Norway));
        let mut rows: Vec<SearchRow> = Vec::new();
        match self.spotify.search(&q, SearchType::Artist, market.clone(),
                                  None, Some(5), None) {
            Ok(SearchResult::Artists(p)) => {
                for a in p.items { rows.push(SearchRow::Artist(a)); }
            }
            Ok(_) => {}
            Err(e) => {
                self.set_status(&format!("Search (artists) failed: {}", e), t::ERR);
            }
        }
        match self.spotify.search(&q, SearchType::Track, market,
                                  None, Some(10), None) {
            Ok(SearchResult::Tracks(p)) => {
                for t in p.items { rows.push(SearchRow::Track(t)); }
            }
            Ok(_) => self.set_status("Unexpected response shape", t::ERR),
            Err(e) => self.set_status(&format!("Search failed: {}", e), t::ERR),
        }
        self.search_results = rows;
        self.search_idx = 0;
        self.render_footer();
    }

    fn list_down(&mut self) {
        let (idx, len) = self.current_list_indices();
        if let Some(len) = len { if idx + 1 < len { self.set_list_idx(idx + 1); } }
    }
    fn list_up(&mut self) {
        let (idx, _) = self.current_list_indices();
        if idx > 0 { self.set_list_idx(idx - 1); }
    }
    fn list_top(&mut self) { self.set_list_idx(0); }
    fn list_bottom(&mut self) {
        if let Some(len) = self.current_list_indices().1 {
            if len > 0 { self.set_list_idx(len - 1); }
        }
    }
    fn current_list_indices(&self) -> (usize, Option<usize>) {
        match self.view {
            View::Search          => (self.search_idx,          Some(self.search_results.len())),
            View::Playlists       => (self.playlists_idx,       Some(self.playlists.len())),
            View::PlaylistTracks  => (self.playlist_tracks_idx, Some(self.playlist_tracks.len())),
            View::Saved           => (self.saved_idx,           Some(self.saved_tracks.len())),
            View::Queue           => (self.queue_idx,           Some(self.queue.len())),
            View::Devices         => (self.devices_idx,         Some(self.devices.len())),
            View::Help            => (0, None),
        }
    }
    fn set_list_idx(&mut self, i: usize) {
        match self.view {
            View::Search          => self.search_idx = i,
            View::Playlists       => self.playlists_idx = i,
            View::PlaylistTracks  => self.playlist_tracks_idx = i,
            View::Saved           => self.saved_idx = i,
            View::Queue           => self.queue_idx = i,
            View::Devices         => self.devices_idx = i,
            View::Help            => {}
        }
    }

    fn current_track_uri(&self) -> Option<rspotify::model::TrackId<'static>> {
        let t = match self.view {
            View::Search         => match self.search_results.get(self.search_idx)? {
                SearchRow::Track(t)  => Some(t),
                SearchRow::Artist(_) => None,
            },
            View::PlaylistTracks => self.playlist_tracks.get(self.playlist_tracks_idx),
            View::Saved          => self.saved_tracks.get(self.saved_idx),
            View::Queue          => self.queue.get(self.queue_idx),
            _ => None,
        }?;
        t.id.clone().map(|id| id.clone_static())
    }

    fn list_activate(&mut self) {
        match self.view {
            View::Playlists => {
                if let Some(pl) = self.playlists.get(self.playlists_idx).cloned() {
                    self.view = View::PlaylistTracks;
                    self.load_playlist_tracks(pl);
                }
            }
            View::PlaylistTracks => {
                // Play from this offset within the playlist context.
                // rspotify's Offset::Position is a chrono::Duration whose
                // `num_milliseconds()` is read as the integer track index
                // (a Spotify API quirk), so `Duration::milliseconds(idx)`
                // is exactly what we want.
                let pl  = self.open_playlist.clone();
                let idx = self.playlist_tracks_idx;
                if let Some(pl) = pl {
                    let Some(device_id) = self.resolve_device_id() else {
                        self.set_status(
                            "No Spotify Connect devices online — open Spotify on a phone or desktop first.",
                            t::AMBER);
                        return;
                    };
                    let ctx = PlayContextId::Playlist(pl.id.clone_static());
                    let offset = Some(rspotify::model::Offset::Position(
                        chrono::Duration::milliseconds(idx as i64)));
                    match self.spotify.start_context_playback(
                        ctx, Some(&device_id), offset, None)
                    {
                        Ok(_)  => self.set_status(
                            &format!("Playing from #{}", idx + 1), t::OK),
                        Err(e) => self.set_status(
                            &format!("Play failed: {}", e), t::ERR),
                    }
                }
            }
            View::Devices => {
                if let Some(dev) = self.devices.get(self.devices_idx).cloned() {
                    if let Some(id) = dev.id {
                        match self.spotify.transfer_playback(&id, Some(true)) {
                            Ok(_)  => self.set_status(
                                &format!("Transferred to {}", dev.name), t::OK),
                            Err(e) => self.set_status(
                                &format!("Transfer failed: {}", e), t::ERR),
                        }
                    }
                }
            }
            View::Search => {
                let Some(device_id) = self.resolve_device_id() else {
                    self.set_status(
                        "No Spotify Connect devices online — open Spotify on a phone or desktop first.",
                        t::AMBER);
                    return;
                };
                match self.search_results.get(self.search_idx).cloned() {
                    Some(SearchRow::Track(t)) => {
                        if let Some(id) = t.id.clone() {
                            let uri = rspotify::model::PlayableId::Track(
                                id.clone_static());
                            match self.spotify.start_uris_playback(
                                std::iter::once(uri), Some(&device_id), None, None) {
                                Ok(_)  => self.set_status("Playing", t::OK),
                                Err(e) => self.set_status(
                                    &format!("Play failed: {}", e), t::ERR),
                            }
                        }
                    }
                    Some(SearchRow::Artist(a)) => {
                        // Play the artist's "essentials" radio (top
                        // tracks + similar) — Spotify resolves this
                        // from the artist context_uri.
                        let ctx = PlayContextId::Artist(a.id.clone_static());
                        match self.spotify.start_context_playback(
                            ctx, Some(&device_id), None, None)
                        {
                            Ok(_)  => self.set_status(
                                &format!("Playing {}", a.name), t::OK),
                            Err(e) => self.set_status(
                                &format!("Play failed: {}", e), t::ERR),
                        }
                    }
                    None => {}
                }
            }
            View::Saved | View::Queue => {
                if let Some(id) = self.current_track_uri() {
                    let Some(device_id) = self.resolve_device_id() else {
                        self.set_status(
                            "No Spotify Connect devices online — open Spotify on a phone or desktop first.",
                            t::AMBER);
                        return;
                    };
                    let uri = rspotify::model::PlayableId::Track(id);
                    match self.spotify.start_uris_playback(
                        std::iter::once(uri), Some(&device_id), None, None) {
                        Ok(_)  => self.set_status("Playing", t::OK),
                        Err(e) => self.set_status(&format!("Play failed: {}", e), t::ERR),
                    }
                }
            }
            View::Help => {}
        }
        self.last_poll = std::time::Instant::now() - self.poll_interval * 2;
        self.poll_state();
    }

    fn queue_current(&mut self) {
        let Some(id) = self.current_track_uri() else { return };
        let device_id = self.resolve_device_id();
        let uri = rspotify::model::PlayableId::Track(id);
        match self.spotify.add_item_to_queue(uri, device_id.as_deref()) {
            Ok(_)  => self.set_status("Added to queue", t::OK),
            Err(e) => self.set_status(&format!("Queue failed: {}", e), t::ERR),
        }
    }

    fn toggle_play(&mut self) {
        let Some(device_id) = self.resolve_device_id() else {
            self.set_status(
                "No Spotify Connect devices online — open Spotify on a phone or desktop first.",
                t::AMBER);
            return;
        };
        let playing = self.playback.as_ref().map(|p| p.is_playing).unwrap_or(false);
        let active_id = self.playback.as_ref().and_then(|p| p.device.id.clone());
        let r = if playing {
            self.spotify.pause_playback(Some(&device_id))
        } else if active_id.is_some() {
            // Active device → just resume what was paused.
            self.spotify.resume_playback(Some(&device_id), None)
        } else {
            // No active device: transferring with play=true activates
            // the picked device AND starts playback in one call. This
            // is the path used after a fresh tune launch when nothing
            // is playing anywhere.
            self.spotify.transfer_playback(&device_id, Some(true))
        };
        match r {
            Ok(_)  => {}
            Err(e) => self.set_status(&format!("Toggle failed: {}", e), t::ERR),
        }
        self.last_poll = std::time::Instant::now() - self.poll_interval * 2;
        self.poll_state();
    }

    fn skip_next(&mut self) {
        let device_id = self.resolve_device_id();
        let _ = self.spotify.next_track(device_id.as_deref());
        self.last_poll = std::time::Instant::now() - self.poll_interval * 2;
        self.poll_state();
    }
    fn skip_prev(&mut self) {
        let device_id = self.resolve_device_id();
        let _ = self.spotify.previous_track(device_id.as_deref());
        self.last_poll = std::time::Instant::now() - self.poll_interval * 2;
        self.poll_state();
    }

    fn bump_volume(&mut self, delta: i32) {
        let cur = self.playback.as_ref()
            .and_then(|p| p.device.volume_percent.map(|v| v as i32))
            .unwrap_or(50);
        let new = (cur + delta).clamp(0, 100) as u8;
        let device_id = self.resolve_device_id();
        let _ = self.spotify.volume(new, device_id.as_deref());
        self.set_status(&format!("Volume: {}%", new), t::OK);
        // Reflect immediately
        if let Some(ref mut pb) = self.playback {
            pb.device.volume_percent = Some(new as u32);
        }
        self.render_now();
    }

    fn seek_relative(&mut self, delta_ms: i64) {
        let cur = self.playback.as_ref()
            .and_then(|p| p.progress.map(|d| d.num_milliseconds()))
            .unwrap_or(0);
        let total = match self.playback.as_ref().and_then(|p| p.item.as_ref()) {
            Some(PlayableItem::Track(t))   => t.duration.num_milliseconds(),
            Some(PlayableItem::Episode(e)) => e.duration.num_milliseconds(),
            Some(PlayableItem::Unknown(j)) => unknown_duration_ms(j) as i64,
            _ => 0,
        };
        let target = (cur + delta_ms).clamp(0, total.max(0));
        let device_id = self.resolve_device_id();
        let _ = self.spotify.seek_track(
            chrono::Duration::milliseconds(target), device_id.as_deref());
        self.last_poll = std::time::Instant::now() - self.poll_interval * 2;
        self.poll_state();
    }

    fn toggle_shuffle(&mut self) {
        let cur = self.playback.as_ref().map(|p| p.shuffle_state).unwrap_or(false);
        let device_id = self.resolve_device_id();
        let _ = self.spotify.shuffle(!cur, device_id.as_deref());
        self.last_poll = std::time::Instant::now() - self.poll_interval * 2;
        self.poll_state();
    }

    fn cycle_repeat(&mut self) {
        let next = match self.playback.as_ref().map(|p| p.repeat_state).unwrap_or(RepeatState::Off) {
            RepeatState::Off     => RepeatState::Context,
            RepeatState::Context => RepeatState::Track,
            RepeatState::Track   => RepeatState::Off,
        };
        let device_id = self.resolve_device_id();
        let _ = self.spotify.repeat(next, device_id.as_deref());
        self.last_poll = std::time::Instant::now() - self.poll_interval * 2;
        self.poll_state();
    }
}

// ---- Helpers --------------------------------------------------------

fn track_row(t: &FullTrack, selected: bool) -> String {
    let cursor = if selected { "▸" } else { " " };
    let artists = t.artists.iter().map(|a| a.name.as_str())
        .collect::<Vec<_>>().join(", ");
    let line = format!("  {}  {:<40}  {:<28}  {}",
        cursor,
        truncate(&t.name, 40),
        truncate(&artists, 28),
        style::fg(&fmt_ms(t.duration.num_milliseconds() as u64), t::FG_DIM));
    if selected {
        style::bold(&style::fg(&line, t::FG_BRIGHT)).to_string()
    } else { line }
}

fn truncate(s: &str, max: usize) -> String {
    if crust::display_width(s) <= max { return s.to_string(); }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = crust::display_width(&c.to_string());
        if w + cw > max.saturating_sub(1) { break; }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Pick (track_id, smallest_cover_url) out of a playback item. Returns
/// None when the item has no id or no images — both legitimate
/// (private content, ad slot, podcast trailer with no cover).
fn cover_id_and_url(item: &PlayableItem) -> Option<(String, String)> {
    match item {
        PlayableItem::Track(t) => {
            let id = t.id.as_ref()?.id().to_string();
            let url = t.album.images.iter()
                .min_by_key(|i| i.width.unwrap_or(u32::MAX))
                .map(|i| i.url.clone())?;
            Some((id, url))
        }
        PlayableItem::Episode(e) => {
            let id = e.id.id().to_string();
            let url = e.images.iter()
                .min_by_key(|i| i.width.unwrap_or(u32::MAX))
                .map(|i| i.url.clone())?;
            Some((id, url))
        }
        PlayableItem::Unknown(j) => {
            let id = j.get("id").and_then(|v| v.as_str())?.to_string();
            // Try track.album.images, then episode.images, then a
            // bare images array as a last resort.
            let images = j.get("album").and_then(|a| a.get("images"))
                .or_else(|| j.get("images"))
                .and_then(|v| v.as_array())?;
            let url = images.iter()
                .min_by_key(|i| i.get("width")
                    .and_then(|w| w.as_u64()).unwrap_or(u64::MAX))
                .and_then(|i| i.get("url").and_then(|u| u.as_str()))
                .map(String::from)?;
            Some((id, url))
        }
    }
}

fn cover_cache_path(track_id: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = PathBuf::from(home).join(".tune").join("cover_cache");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{}.jpg", track_id))
}

fn download_cover(url: &str, dest: &std::path::Path) -> Result<(), String> {
    use std::io::Read;
    let resp = ureq::get(url).call()
        .map_err(|e| format!("GET {}: {}", url, e))?;
    let mut body = Vec::new();
    resp.into_reader().read_to_end(&mut body)
        .map_err(|e| format!("read: {}", e))?;
    std::fs::write(dest, body).map_err(|e| format!("write {:?}: {}", dest, e))
}

fn artist_row(a: &FullArtist, selected: bool) -> String {
    // `followers` and `genres` are both flagged deprecated by
    // rspotify (Spotify removed/changed them — see rspotify#550),
    // so we keep the row minimal: marker + name + the literal
    // "artist" tag so the user can tell artist rows from track
    // rows at a glance.
    let cursor = if selected { "▸" } else { " " };
    let line = format!("  {}  {:<60}  {}",
        cursor,
        truncate(&format!("♪ {}", a.name), 60),
        style::fg("artist", t::FG_DIM));
    if selected {
        style::bold(&style::fg(&line, t::FG_BRIGHT)).to_string()
    } else {
        style::fg(&line, t::CYAN).to_string()
    }
}

/// Pull `name` + comma-joined `artists[].name` from a raw
/// `PlayableItem::Unknown` JSON value. Shape matches the Spotify
/// Web API track object — we only need three fields so we don't
/// repeat rspotify's full deserialization, just walk the JSON tree.
fn unknown_title_artists(j: &serde_json::Value) -> (String, String) {
    let title = j.get("name").and_then(|v| v.as_str())
        .map(|s| s.to_string()).unwrap_or_else(|| "—".into());
    let artists = j.get("artists").and_then(|v| v.as_array())
        .map(|arr| arr.iter()
            .filter_map(|a| a.get("name").and_then(|n| n.as_str()))
            .collect::<Vec<_>>()
            .join(", "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "—".into());
    (title, artists)
}

fn unknown_duration_ms(j: &serde_json::Value) -> u64 {
    j.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0)
}

fn fmt_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let m = secs / 60;
    let s = secs % 60;
    format!("{}:{:02}", m, s)
}

fn progress_bar(cur_ms: u64, total_ms: u64, width: usize) -> String {
    if total_ms == 0 || width < 4 { return "—".repeat(width); }
    let ratio = (cur_ms as f64 / total_ms as f64).clamp(0.0, 1.0);
    let filled = (ratio * width as f64).round() as usize;
    let unfilled = width.saturating_sub(filled);
    format!("{}{}",
        style::fg(&"━".repeat(filled),   t::ACCENT),
        style::fg(&"─".repeat(unfilled), t::FG_DIM))
}

// ---- Theme ---------------------------------------------------------

mod t {
    pub const FG:         u8 = 252;
    pub const FG_BRIGHT:  u8 = 255;
    pub const FG_MUTED:   u8 = 245;
    pub const FG_DIM:     u8 = 240;
    pub const BG_BAR:     u8 = 235;
    pub const BG_NOW:     u8 = 234;
    pub const ACCENT:     u8 = 28;   // Spotify-ish green
    pub const CYAN:       u8 = 51;
    pub const AMBER:      u8 = 220;
    pub const OK:         u8 = 156;
    pub const ERR:        u8 = 196;
}

// ---- Main ---------------------------------------------------------

fn main() {
    // Trivial CLI surface — kept tiny on purpose so we don't grow an
    // arg parser. Other args fall through to TUI mode (with the
    // is_tty guard below catching cases where it shouldn't).
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--version" | "-V" | "-v" => {
                println!("tune {}", VERSION);
                return;
            }
            "--help" | "-h" => {
                println!("tune {} — Spotify Connect controller TUI", VERSION);
                println!();
                println!("Usage: tune");
                println!();
                println!("First run prompts for a Spotify Developer client ID");
                println!("(see https://developer.spotify.com/dashboard), then opens");
                println!("the browser for the one-time OAuth grant. Press ? inside");
                println!("the TUI for the key reference.");
                return;
            }
            _ => {}
        }
    }
    // Refuse to launch the alt-screen TUI if stdin isn't a terminal —
    // otherwise the input loop spins on failed reads and burns
    // thousands of wakeups per second (one observed at 2255 wakes/s
    // in a stray `tune --version` that fell through to TUI mode
    // without a TTY). Battery first.
    if unsafe { libc::isatty(0) } == 0 {
        eprintln!("tune: stdin is not a terminal — refusing to start the TUI.");
        eprintln!("Run `tune` interactively, or `tune --help` for usage.");
        std::process::exit(1);
    }

    let mut cfg = config::load();
    if cfg.client_id.trim().is_empty() {
        config::print_setup_instructions();
        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            eprintln!("aborted");
            std::process::exit(1);
        }
        let id = buf.trim().to_string();
        if id.is_empty() {
            eprintln!("No client ID — aborting.");
            std::process::exit(1);
        }
        cfg.client_id = id;
        if let Err(e) = config::save(&cfg) {
            eprintln!("Could not save config: {}", e);
            std::process::exit(1);
        }
        eprintln!("Saved to ~/.tune/config.yml.\n");
    }

    let mut spotify = auth::build_client(&cfg.client_id);
    // Try cached token first. Re-authorize when the cache predates
    // a scope addition (e.g. `streaming`, added when librespot
    // landed) so the user doesn't hit a "Premium required" 403 on
    // a track stream that's really a missing-scope problem.
    let cached = spotify.read_token_cache(true).ok().flatten();
    let need_reauth = match &cached {
        Some(tok) => !auth::token_has_all_scopes(tok),
        None => true,
    };
    if need_reauth {
        if cached.is_some() {
            eprintln!("tune: cached token is missing newly-required scopes; \
                       re-authorizing once.");
        }
        if let Err(e) = auth::authorize(&mut spotify) {
            eprintln!("Authorization failed: {}", e);
            std::process::exit(1);
        }
    } else if let Some(tok) = cached {
        *spotify.get_token().lock().unwrap() = Some(tok);
    }

    // Spawn the librespot Spirc session BEFORE entering the TUI so
    // any auth/audio-backend errors print to stderr (the alt-screen
    // would otherwise swallow them). The handle is held for the
    // lifetime of main(); dropping it on quit tears down Spirc and
    // de-registers the device from Spotify Connect.
    //
    // Force a token refresh before handing the access_token to
    // librespot. Spotify's AP server is stricter about token
    // freshness than the Web API — a token that's 5+ minutes old
    // can be rejected with "Bad credentials" even when expires_at
    // is still 50 minutes in the future. Refreshing right before
    // librespot's login attempt cuts that failure mode dramatically.
    let _local_player = if cfg.local_player {
        let _ = spotify.refresh_token();
        let access_token = spotify.get_token()
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|t| t.access_token.clone()))
            .unwrap_or_default();
        if access_token.is_empty() {
            eprintln!("tune: no access token available, skipping local player.");
            None
        } else {
            Some(player::LocalPlayer::start(access_token, cfg.device_name.clone()))
        }
    } else {
        None
    };

    // Redirect stderr to ~/.tune/tune.log BEFORE entering the TUI's
    // alt-screen so librespot / rspotify error messages survive (the
    // alt-screen otherwise swallows everything written to fd 2).
    // Best-effort: failure here is non-fatal, just means we lose
    // log visibility for that session.
    redirect_stderr_to_log();
    // Install a default log filter that surfaces librespot's session
    // / playback / spirc warnings + errors. `RUST_LOG=...` overrides
    // for deep debugging.
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,librespot=info"))
        .target(env_logger::Target::Stderr)
        .init();

    Crust::init();
    Crust::set_app_identity("Tune");
    Crust::clear_screen();
    let mut app = App::new(spotify, cfg.poll_s, cfg.default_device);
    app.poll_state();
    app.render_all();

    loop {
        // 1-second input timeout. crust's `Input::getchr` parameter
        // is `Option<u64>` of SECONDS (not ms — easy mistake), so 1
        // is the minimum tick granularity without bypassing crust.
        // That's exactly what the progress bar wants: wake every
        // second, run tick_progress, re-render the now-strip.
        if let Some(key) = Input::getchr(Some(1)) {
            if app.handle(&key) { break; }
        }
        app.poll_state();      // Web API (every poll_interval s)
        app.tick_progress();   // local extrapolation (every 1 s)
        if app.handle_resize_if_needed() { app.render_all(); }
    }
    Cursor::show();
    Crust::cleanup();
}

/// Reopen fd 2 (stderr) onto `~/.tune/tune.log` (append) so anything
/// printed via `eprintln!` / librespot's `log` / rspotify error paths
/// gets captured for post-mortem instead of vanishing into the
/// alt-screen. Truncates the file on each launch to keep it small.
fn redirect_stderr_to_log() {
    use std::os::fd::AsRawFd;
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = std::path::PathBuf::from(&home).join(".tune");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("tune.log");
    let Ok(f) = std::fs::OpenOptions::new()
        .create(true).write(true).truncate(true)
        .open(&path) else { return; };
    let raw = f.as_raw_fd();
    // SAFETY: dup2 swaps fd 2 to point at the log file. The original
    // stderr fd is closed by dup2; we let the `File` go out of scope
    // after the dup so the underlying fd survives via fd 2.
    unsafe { libc::dup2(raw, 2); }
    std::mem::forget(f); // keep the fd alive; dup2 already aliased it
    eprintln!("=== tune v{} starting at {} ===",
        VERSION,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0));
}

impl App {
    fn handle_resize_if_needed(&mut self) -> bool {
        let (cols, rows) = Crust::terminal_size();
        if cols == self.cols && rows == self.rows { return false; }
        self.cols = cols; self.rows = rows;
        self.header = Pane::new(1, 1, cols, 1, t::FG_BRIGHT as u16, t::BG_BAR as u16);
        self.header.wrap = false; self.header.scroll = false;
        self.main_p = Pane::new(1, 2, cols, rows.saturating_sub(2 + NOW_H + 1),
                                t::FG as u16, 0);
        self.now_p  = Pane::new(1, rows - NOW_H, cols, NOW_H,
                                t::FG_BRIGHT as u16, t::BG_NOW as u16);
        self.now_p.wrap = false; self.now_p.scroll = false;
        self.footer = Pane::new(1, rows, cols, 1, t::FG as u16, t::BG_BAR as u16);
        // Force re-placement of the cover after resize.
        self.cover_track_id = None;
        self.footer.wrap = false; self.footer.scroll = false;
        Crust::clear_screen();
        true
    }
}
