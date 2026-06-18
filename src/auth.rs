//! Spotify OAuth — Authorization Code with PKCE.
//!
//! Flow on first run:
//!   1. Print step-by-step instructions if no `client_id` is configured.
//!   2. Build the authorize URL with PKCE challenge.
//!   3. Open the URL in the user's browser via `webbrowser`.
//!   4. Spawn a one-shot `TcpListener` on 127.0.0.1:`AUTH_PORT`, wait for
//!      the redirect with `?code=…`.
//!   5. Exchange the code for an access + refresh token; cache it on
//!      disk at `~/.tune/token.json`.
//!
//! Subsequent runs just load the cached token. Refresh happens
//! transparently on the next API call when the access token is near
//! expiry — rspotify's `AuthCodePkceSpotify` handles that internally.

use rspotify::{AuthCodePkceSpotify, Config, Credentials, OAuth};
use rspotify::scopes;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;

pub const AUTH_PORT: u16 = 8888;
pub const REDIRECT_URI: &str = "http://127.0.0.1:8888/callback";

/// Scopes we need: playback read + modify, currently-playing, library
/// read, playlist read. Add new scopes carefully — Spotify silently
/// rejects requests for scopes the user didn't grant on the original
/// authorize flow, so any extension here requires re-running the
/// authorize URL (delete `~/.tune/token.json` to force).
pub fn required_scopes() -> std::collections::HashSet<String> {
    scopes!(
        // Controller (Web API).
        "user-read-playback-state",
        "user-modify-playback-state",
        "user-read-currently-playing",
        "user-read-private",
        "user-library-read",
        "playlist-read-private",
        "playlist-read-collaborative",
        // Local audio device (librespot Spirc session).
        // `streaming` is the gate on actually playing audio — Premium
        // only, and without it librespot's connection to the AP server
        // succeeds but every track stream request returns 403.
        "streaming"
    )
}

/// Whether the cached token covers every scope in `required_scopes`.
/// Used by main() to detect a stale cache after we extend the scope
/// list (e.g. when adding the librespot `streaming` scope on v0.2)
/// and force a one-time re-authorization instead of letting playback
/// silently fail with "Premium required" errors that are actually
/// "wrong scope" errors.
pub fn token_has_all_scopes(tok: &rspotify::Token) -> bool {
    let need = required_scopes();
    need.iter().all(|s| tok.scopes.contains(s))
}

fn token_cache_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = PathBuf::from(home).join(".tune");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("token.json")
}

/// Build the configured Spotify client. Caller must run [`authorize`]
/// before any API call if no cached token is found on disk.
pub fn build_client(client_id: &str) -> AuthCodePkceSpotify {
    let creds  = Credentials::new_pkce(client_id);
    let oauth  = OAuth {
        redirect_uri: REDIRECT_URI.to_string(),
        scopes: required_scopes(),
        ..Default::default()
    };
    let config = Config {
        token_cached: true,
        cache_path:   token_cache_path(),
        token_refreshing: true,
        ..Default::default()
    };
    AuthCodePkceSpotify::with_config(creds, oauth, config)
}

/// First-time auth: open browser, catch the redirect on
/// 127.0.0.1:AUTH_PORT, exchange code → token, cache to disk.
pub fn authorize(spotify: &mut AuthCodePkceSpotify) -> Result<(), String> {
    use rspotify::clients::OAuthClient;
    let url = spotify.get_authorize_url(None)
        .map_err(|e| format!("get_authorize_url: {}", e))?;
    let listener = TcpListener::bind(("127.0.0.1", AUTH_PORT))
        .map_err(|e| format!("bind 127.0.0.1:{}: {}", AUTH_PORT, e))?;
    if let Err(e) = webbrowser::open(&url) {
        eprintln!("Could not open browser ({}). Paste this URL manually:\n  {}", e, url);
    } else {
        eprintln!("Opened browser for Spotify authorization. Waiting for callback…");
    }
    let code = catch_callback(listener)?;
    spotify.request_token(&code)
        .map_err(|e| format!("request_token: {}", e))?;
    Ok(())
}

/// Remove the on-disk token cache so the next authorize starts clean.
/// Called when Spotify rejects the refresh token (`invalid_grant`).
pub fn discard_token_cache() {
    let _ = std::fs::remove_file(token_cache_path());
}

/// Refresh the access token, recovering from an expired or revoked refresh
/// token. From 2026-07-20 Spotify expires refresh tokens six months after
/// issuance; the refresh then fails with `invalid_grant`. Per Spotify's
/// guidance we must NOT retry the dead token — discard it and send the user
/// back through sign-in. Returns Ok(true) when a re-authorization ran,
/// Ok(false) on a normal refresh, and Err on a transient (e.g. network)
/// failure where the cached token is still worth keeping.
pub fn refresh_or_reauthorize(spotify: &mut AuthCodePkceSpotify) -> Result<bool, String> {
    use rspotify::clients::BaseClient;
    match spotify.refresh_token() {
        Ok(()) => Ok(false),
        Err(e) => {
            // `invalid_grant` (or a bare 400/401 from the token endpoint)
            // means the refresh token is dead, not a transient blip.
            let m = e.to_string().to_lowercase();
            let token_dead = m.contains("invalid_grant")
                || m.contains("invalid grant")
                || m.contains("400")
                || m.contains("401")
                || m.contains("unauthorized");
            if token_dead {
                eprintln!("tune: Spotify refresh token expired/revoked — re-authorizing.");
                discard_token_cache();
                if let Ok(mut g) = spotify.get_token().lock() { *g = None; }
                authorize(spotify).map(|_| true)
            } else {
                Err(format!("refresh_token: {}", e))
            }
        }
    }
}

/// Read one HTTP request off the listener, pull `code=…` out of the
/// request line, write a brief success page back, and return the
/// code. We don't bother with HTTP parsing — Spotify's redirect is a
/// plain `GET /callback?code=…&state=… HTTP/1.1` line and we just
/// need the query string.
fn catch_callback(listener: TcpListener) -> Result<String, String> {
    let (mut stream, _addr) = listener.accept()
        .map_err(|e| format!("accept: {}", e))?;
    let line = {
        let mut reader = BufReader::new(&mut stream);
        let mut first = String::new();
        reader.read_line(&mut first).map_err(|e| format!("read: {}", e))?;
        // Drain remaining headers so the browser sees a complete response.
        let mut throwaway = String::new();
        while reader.read_line(&mut throwaway).map(|n| n > 2).unwrap_or(false) {
            throwaway.clear();
        }
        first
    };
    // Line looks like: "GET /callback?code=...&state=... HTTP/1.1"
    let q = line.split_whitespace().nth(1)
        .ok_or_else(|| format!("malformed request line: {:?}", line))?;
    let query = q.splitn(2, '?').nth(1).unwrap_or("");
    let mut code: Option<String> = None;
    let mut err:  Option<String> = None;
    for kv in query.split('&') {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        match k {
            "code"  => code = Some(urldecode(v)),
            "error" => err  = Some(urldecode(v)),
            _ => {}
        }
    }
    let body = if let Some(ref e) = err {
        format!("<html><body style='font-family:sans-serif;max-width:32em;margin:4em auto;color:#c33'>\
                 <h2>Authorization failed</h2>\
                 <p>Spotify returned: <code>{}</code></p>\
                 <p>You can close this tab and re-run <code>tune</code>.</p>\
                 </body></html>", html_escape(e))
    } else {
        "<html><body style='font-family:sans-serif;max-width:32em;margin:4em auto;color:#080'>\
         <h2>tune is authorized</h2>\
         <p>You can close this tab and return to the terminal.</p>\
         </body></html>".to_string()
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body);
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    if let Some(e) = err { return Err(format!("spotify: {}", e)); }
    code.ok_or_else(|| "no `code` in callback query".to_string())
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => { out.push(b' '); i += 1; }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i+1] as char).to_digit(16);
                let lo = (bytes[i+2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push(((h << 4) | l) as u8);
                    i += 3;
                } else { out.push(bytes[i]); i += 1; }
            }
            b => { out.push(b); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
