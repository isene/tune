//! Internet radio: stations found through radio-browser.info, a free
//! directory of tens of thousands of them, and the ones you keep in
//! `~/.tune/radio.yml`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Station {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub country: String,
    #[serde(default)]
    pub codec: String,
    #[serde(default)]
    pub bitrate: u32,
    #[serde(default)]
    pub tags: String,
    #[serde(default)]
    pub favicon: String,
    /// radio-browser's id, used to count a play there.
    #[serde(default)]
    pub uuid: String,
}

/// Whether two entries are the same station. The directory gives every
/// station its own id; several CNN entries share one stream URL, so a
/// URL match would star all of them when one is kept.
pub fn same_station(a: &Station, b: &Station) -> bool {
    if !a.uuid.is_empty() && !b.uuid.is_empty() { return a.uuid == b.uuid; }
    a.url == b.url
}

/// The directory's own servers; the first answers fastest from Europe,
/// the second picks any server that is up.
const SERVERS: [&str; 2] = ["https://de1.api.radio-browser.info", "https://all.api.radio-browser.info"];

fn agent() -> String {
    format!("tune/{}", env!("CARGO_PKG_VERSION"))
}

/// Stations whose name matches `query`, most played first. When no name
/// matches, stations tagged with it (jazz, news, classical).
pub fn search(query: &str) -> Result<Vec<Station>, String> {
    let by_name = fetch(&[("name", query)], 100)?;
    if !by_name.is_empty() { return Ok(by_name); }
    fetch(&[("tag", query)], 100)
}

/// A country's stations, most played first: by two-letter code (NO) or
/// by the start of its English name (Norway).
pub fn in_country(country: &str) -> Result<Vec<Station>, String> {
    let c = country.trim();
    if c.len() == 2 && c.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return fetch(&[("countrycode", &c.to_ascii_uppercase())], 500);
    }
    fetch(&[("country", &country_name(c))], 500)
}

/// The directory matches country names letter case and all: "Norway",
/// "United Kingdom".
fn country_name(s: &str) -> String {
    s.split_whitespace().map(|w| {
        let mut chars = w.chars();
        chars.next().map(|f| f.to_uppercase().chain(chars.flat_map(|c| c.to_lowercase())).collect::<String>()).unwrap_or_default()
    }).collect::<Vec<_>>().join(" ")
}

fn fetch(filter: &[(&str, &str)], limit: usize) -> Result<Vec<Station>, String> {
    let mut last = String::new();
    for server in SERVERS {
        let mut req = ureq::get(&format!("{}/json/stations/search", server)).set("User-Agent", &agent());
        for (field, value) in filter { req = req.query(field, value); }
        let resp = req
            .query("limit", &limit.to_string())
            .query("hidebroken", "true")
            .query("order", "clickcount")
            .query("reverse", "true")
            .timeout(std::time::Duration::from_secs(8))
            .call();
        let body = match resp.map_err(|e| e.to_string()).and_then(|r| r.into_string().map_err(|e| e.to_string())) {
            Ok(b) => b,
            Err(e) => { last = e; continue; }
        };
        let list: Vec<serde_json::Value> = serde_json::from_str(&body).map_err(|e| e.to_string())?;
        return Ok(list.iter().filter_map(station).collect());
    }
    Err(last)
}

fn station(v: &serde_json::Value) -> Option<Station> {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let url = Some(s("url_resolved")).filter(|u| !u.is_empty()).unwrap_or_else(|| s("url"));
    if url.is_empty() { return None; }
    Some(Station {
        name: s("name"),
        url,
        country: s("countrycode"),
        codec: s("codec"),
        bitrate: v.get("bitrate").and_then(|b| b.as_u64()).unwrap_or(0) as u32,
        tags: s("tags"),
        favicon: s("favicon"),
        uuid: s("stationuuid"),
    })
}

/// Tell the directory a station was played, as it asks clients to. Runs
/// on its own thread so playback never waits for it.
pub fn count_play(uuid: &str) {
    if uuid.is_empty() { return; }
    let url = format!("{}/json/url/{}", SERVERS[1], uuid);
    std::thread::spawn(move || {
        let _ = ureq::get(&url).set("User-Agent", &agent()).timeout(std::time::Duration::from_secs(8)).call();
    });
}

fn saved_path() -> PathBuf {
    crate::config::tune_dir().join("radio.yml")
}

/// The stations you keep.
pub fn saved() -> Vec<Station> {
    std::fs::read_to_string(saved_path()).ok()
        .and_then(|s| serde_yaml::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(stations: &[Station]) -> Result<(), String> {
    std::fs::create_dir_all(crate::config::tune_dir()).map_err(|e| e.to_string())?;
    let text = serde_yaml::to_string(stations).map_err(|e| e.to_string())?;
    std::fs::write(saved_path(), text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_entry_becomes_a_station() {
        let v = serde_json::json!({
            "name": " NRK P1 ", "url": "http://a/p1.pls", "url_resolved": "https://b/p1_mp3_h",
            "countrycode": "NO", "codec": "MP3", "bitrate": 192, "tags": "news", "favicon": "", "stationuuid": "u1"
        });
        let s = station(&v).unwrap();
        assert_eq!((s.name.as_str(), s.url.as_str(), s.bitrate), ("NRK P1", "https://b/p1_mp3_h", 192));
        let no_resolved = serde_json::json!({"name": "X", "url": "http://a/x"});
        assert_eq!(station(&no_resolved).unwrap().url, "http://a/x");
        assert!(station(&serde_json::json!({"name": "Y"})).is_none());
    }

    #[test]
    fn stations_sharing_a_stream_are_still_different_stations() {
        let kept = Station { name: "CNN INTERNATIONAL".into(), url: "http://s/2868".into(), uuid: "u1".into(), ..Default::default() };
        let other = Station { name: "CNN".into(), url: "http://s/2868".into(), uuid: "u2".into(), ..Default::default() };
        let no_id = Station { name: "CNN".into(), url: "http://s/2868".into(), ..Default::default() };
        assert!(same_station(&kept, &kept.clone()));
        assert!(!same_station(&kept, &other));
        assert!(same_station(&kept, &no_id), "an entry without an id falls back to its URL");
    }

    #[test]
    fn country_names_are_capitalized_the_way_the_directory_has_them() {
        assert_eq!(country_name("norway"), "Norway");
        assert_eq!(country_name("united  KINGDOM"), "United Kingdom");
    }
}
