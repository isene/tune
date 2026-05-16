//! Tiny config loader for `~/.tune/config.yml`. Single key for now —
//! `client_id` — but the file is YAML so we can add theme / default
//! device / polling cadence later without breaking existing installs.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Spotify Developer client ID (PKCE, no secret needed).
    #[serde(default)]
    pub client_id: String,

    /// Polling cadence for now-playing state, seconds. Lower = snappier
    /// progress bar but more API calls (50 ms granularity is overkill
    /// — Spotify rate limits hard around 1 req/s sustained). Default
    /// 2 s feels live enough without burning quota.
    #[serde(default = "default_poll_s")]
    pub poll_s: u64,

    /// Preferred device id to fall back to when nothing is playing.
    /// Empty = whatever Spotify Connect picks (last-used device).
    #[serde(default)]
    pub default_device: String,
}

fn default_poll_s() -> u64 { 2 }

pub fn tune_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".tune")
}

pub fn config_path() -> PathBuf { tune_dir().join("config.yml") }

pub fn load() -> Config {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_yaml::from_str(&s).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}

pub fn save(cfg: &Config) -> std::io::Result<()> {
    let dir = tune_dir();
    std::fs::create_dir_all(&dir)?;
    let s = serde_yaml::to_string(cfg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(config_path(), s)
}

/// Step-by-step instructions printed when the user runs tune for the
/// first time and no client_id is configured. Distinct from a `--help`
/// blurb — these are walkthrough instructions for the Spotify
/// developer dashboard.
pub fn print_setup_instructions() {
    eprintln!("\n┌── tune — first-time setup ───────────────────────────────────────┐");
    eprintln!("│                                                                  │");
    eprintln!("│ tune is a Spotify Connect controller. Set up takes ~2 minutes:   │");
    eprintln!("│                                                                  │");
    eprintln!("│  1. Open https://developer.spotify.com/dashboard                 │");
    eprintln!("│  2. Log in with your regular Spotify account, click 'Create app' │");
    eprintln!("│  3. Name: tune    Description: anything                          │");
    eprintln!("│  4. Add Redirect URI: http://127.0.0.1:8888/callback             │");
    eprintln!("│  5. Check 'Web API' for the API/SDKs you intend to use, save     │");
    eprintln!("│  6. Open the app's Settings, copy the Client ID                  │");
    eprintln!("│                                                                  │");
    eprintln!("│ Then paste the Client ID below. tune will write it to            │");
    eprintln!("│   ~/.tune/config.yml                                             │");
    eprintln!("│ and open your browser for the one-time authorization.            │");
    eprintln!("│                                                                  │");
    eprintln!("└──────────────────────────────────────────────────────────────────┘\n");
    eprint!("Spotify Client ID: ");
    let _ = std::io::Write::flush(&mut std::io::stderr());
}
