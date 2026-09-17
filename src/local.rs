//! Local files and internet radio, played by mpv over its socket.
//!
//! Spotify audio comes from librespot. Everything else goes through one
//! mpv process, started for a folder of files or for a station. tune
//! reads mpv's state once a second while it plays, and only checks that
//! it is still running while it is paused.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::radio::Station;

/// The file types the Files view lists.
const AUDIO: [&str; 12] = ["mp3", "flac", "ogg", "oga", "opus", "m4a", "aac", "wav", "aif", "aiff", "wma", "mka"];

pub fn is_audio(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| AUDIO.iter().any(|a| a.eq_ignore_ascii_case(e)))
}

/// What mpv was started on.
pub enum Source {
    Files,
    Radio(Station),
}

#[derive(Clone, Copy, PartialEq)]
pub enum Repeat { Off, List, Track }

/// What mpv last reported.
#[derive(Default)]
pub struct State {
    pub paused: bool,
    pub pos: f64,
    /// Zero for a station: a live stream has no end.
    pub duration: f64,
    /// A file's title tag or name; the song a station sends.
    pub title: String,
    /// A file's artist and album; the station's name.
    pub artist: String,
    /// Place in the list and how long the list is.
    pub index: usize,
    pub count: usize,
    pub path: String,
}

pub struct Mpv {
    child: Child,
    sock_path: PathBuf,
    sock: Option<BufReader<UnixStream>>,
    pub source: Source,
    pub state: State,
    pub volume: i64,
    pub repeat: Repeat,
}

impl Mpv {
    /// Play `files` from `index`, or one station.
    pub fn start(source: Source, files: &[PathBuf], index: usize) -> Result<Mpv, String> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let sock_path = dir.join(format!("tune-mpv-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);
        let mut cmd = Command::new("mpv");
        cmd.arg("--no-video").arg("--really-quiet").arg("--no-terminal")
            .arg(format!("--input-ipc-server={}", sock_path.display()))
            .arg(format!("--playlist-start={}", index));
        match &source {
            Source::Files => { cmd.args(files); }
            Source::Radio(s) => { cmd.arg(&s.url); }
        }
        let child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .spawn().map_err(|e| format!("mpv: {}", e))?;
        let state = State { index, count: files.len().max(1), ..State::default() };
        Ok(Mpv { child, sock_path, sock: None, source, state, volume: 100, repeat: Repeat::Off })
    }

    fn connect(&mut self) -> Option<&mut BufReader<UnixStream>> {
        if self.sock.is_none() {
            let s = UnixStream::connect(&self.sock_path).ok()?;
            let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(500)));
            self.sock = Some(BufReader::new(s));
        }
        self.sock.as_mut()
    }

    /// One command, one reply. mpv's event lines are skipped.
    fn cmd(&mut self, args: &[&str]) -> Option<serde_json::Value> {
        let req = serde_json::json!({"command": args}).to_string() + "\n";
        let res = self.connect().and_then(|r| exchange(r, &req));
        if res.is_none() { self.sock = None; }
        res
    }

    fn get(&mut self, prop: &str) -> Option<serde_json::Value> {
        let v = self.cmd(&["get_property", prop])?;
        (v.get("error")?.as_str()? == "success").then(|| v.get("data").cloned()).flatten()
    }

    pub fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Read mpv's state. False once mpv has exited, when the list has
    /// played out.
    pub fn refresh(&mut self) -> bool {
        if !self.running() { return false; }
        // Just started, mpv's socket may not be up yet: keep what we know.
        let Some(paused) = self.get("pause").and_then(|v| v.as_bool()) else { return true };
        self.state.paused = paused;
        let num = |m: &mut Self, prop: &str| m.get(prop).and_then(|v| v.as_f64());
        self.state.pos = num(self, "time-pos").unwrap_or(0.0);
        self.state.index = num(self, "playlist-pos").map_or(self.state.index, |i| i.max(0.0) as usize);
        self.state.count = num(self, "playlist-count").map_or(self.state.count, |c| c as usize);
        let media = self.get("media-title").and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
        let meta = self.get("metadata").unwrap_or_default();
        let tag = |key: &str| meta.as_object()
            .and_then(|o| o.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)))
            .and_then(|(_, v)| v.as_str()).unwrap_or("").trim().to_string();
        match &self.source {
            Source::Radio(station) => {
                self.state.duration = 0.0;
                let name = Some(tag("icy-name")).filter(|n| !n.is_empty()).unwrap_or_else(|| station.name.clone());
                let song = tag("icy-title");
                self.state.title = if song.is_empty() { name.clone() } else { song };
                self.state.artist = name;
            }
            Source::Files => {
                self.state.duration = num(self, "duration").unwrap_or(0.0);
                self.state.path = self.get("path").and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
                let title = tag("title");
                self.state.title = if title.is_empty() { media } else { title };
                self.state.artist = [tag("artist"), tag("album")].into_iter()
                    .filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
            }
        }
        true
    }

    pub fn toggle_pause(&mut self) {
        self.cmd(&["cycle", "pause"]);
        self.state.paused = !self.state.paused;
    }

    pub fn next(&mut self) { self.cmd(&["playlist-next"]); }
    pub fn prev(&mut self) { self.cmd(&["playlist-prev"]); }

    pub fn seek(&mut self, secs: i64) {
        self.cmd(&["seek", &secs.to_string(), "relative"]);
    }

    pub fn add_volume(&mut self, delta: i64) {
        self.volume = (self.volume + delta).clamp(0, 100);
        self.cmd(&["set", "volume", &self.volume.to_string()]);
    }

    /// Put a file at the end of the list.
    pub fn append(&mut self, file: &Path) {
        self.cmd(&["loadfile", &file.to_string_lossy(), "append-play"]);
        self.state.count += 1;
    }

    pub fn shuffle(&mut self) { self.cmd(&["playlist-shuffle"]); }

    /// Off, then the whole list over and over, then this track over and over.
    pub fn cycle_repeat(&mut self) {
        self.repeat = match self.repeat { Repeat::Off => Repeat::List, Repeat::List => Repeat::Track, Repeat::Track => Repeat::Off };
        let (list, file) = match self.repeat { Repeat::Off => ("no", "no"), Repeat::List => ("inf", "no"), Repeat::Track => ("no", "inf") };
        self.cmd(&["set", "loop-playlist", list]);
        self.cmd(&["set", "loop-file", file]);
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        self.cmd(&["quit"]);
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

fn exchange(r: &mut BufReader<UnixStream>, req: &str) -> Option<serde_json::Value> {
    r.get_mut().write_all(req.as_bytes()).ok()?;
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).ok()? == 0 { return None; }
        let v: serde_json::Value = serde_json::from_str(&line).ok()?;
        if v.get("error").is_some() { return Some(v); }
    }
}

/// A folder's sub-folders and audio files, folders first, hidden ones left out.
pub fn list_dir(dir: &Path) -> Vec<(PathBuf, bool)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<(PathBuf, bool)> = rd.flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| {
            let p = e.path();
            let is_dir = p.is_dir();
            (is_dir || is_audio(&p)).then_some((p, is_dir))
        })
        .collect();
    v.sort_by_key(|(p, is_dir)| (!is_dir, p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()));
    v
}

/// A cover image beside a file: cover, folder or front, as jpg or png.
pub fn folder_cover(file: &Path) -> Option<PathBuf> {
    let dir = file.parent()?;
    let names = ["cover", "folder", "front", "Cover", "Folder", "Front"];
    names.iter().flat_map(|n| ["jpg", "jpeg", "png"].map(|e| dir.join(format!("{}.{}", n, e)))).find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_lists_subfolders_first_then_audio_files() {
        let dir = std::env::temp_dir().join(format!("tune-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("B album")).unwrap();
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();
        for f in ["b.mp3", "A.FLAC", "notes.txt", "cover.jpg"] { std::fs::write(dir.join(f), b"").unwrap(); }
        let names: Vec<String> = list_dir(&dir).iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["B album", "A.FLAC", "b.mp3"]);
        assert_eq!(folder_cover(&dir.join("b.mp3")), Some(dir.join("cover.jpg")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two seconds of silence as a WAV file: 8 kHz, 8-bit, mono.
    fn silence(path: &Path) {
        let n: u32 = 16_000;
        let mut b = Vec::new();
        b.extend(b"RIFF"); b.extend((36 + n).to_le_bytes()); b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes()); b.extend(1u16.to_le_bytes()); b.extend(1u16.to_le_bytes());
        b.extend(8000u32.to_le_bytes()); b.extend(8000u32.to_le_bytes()); b.extend(1u16.to_le_bytes()); b.extend(8u16.to_le_bytes());
        b.extend(b"data"); b.extend(n.to_le_bytes()); b.extend(std::iter::repeat(128u8).take(n as usize));
        std::fs::write(path, b).unwrap();
    }

    #[test]
    fn mpv_plays_a_folder_pauses_skips_and_quits_with_tune() {
        if Command::new("mpv").arg("--version").stdout(Stdio::null()).status().is_err() { return; }
        let dir = std::env::temp_dir().join(format!("tune-mpv-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("mpv")).unwrap();
        // No sound out of the speakers while testing.
        std::fs::write(dir.join("mpv/mpv.conf"), "ao=null\n").unwrap();
        std::env::set_var("MPV_HOME", dir.join("mpv"));
        let files = [dir.join("a.wav"), dir.join("b.wav")];
        for f in &files { silence(f); }
        let mut m = Mpv::start(Source::Files, &files, 0).unwrap();
        let wait = |m: &mut Mpv, done: &dyn Fn(&State) -> bool| {
            for _ in 0..50 {
                assert!(m.refresh(), "mpv is running");
                if done(&m.state) { return; }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            panic!("mpv never got there: title {:?}, index {}", m.state.title, m.state.index);
        };
        wait(&mut m, &|s| s.path.ends_with("a.wav") && s.duration > 0.0);
        assert_eq!((m.state.title.as_str(), m.state.count), ("a.wav", 2));
        assert!((m.state.duration - 2.0).abs() < 0.1, "duration {}", m.state.duration);
        m.toggle_pause();
        wait(&mut m, &|s| s.paused);
        m.next();
        wait(&mut m, &|s| s.index == 1 && s.path.ends_with("b.wav"));
        let sock = m.sock_path.clone();
        drop(m);
        assert!(!sock.exists(), "quitting tune ends mpv and removes its socket");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
