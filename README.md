# tune

<img src="img/tune.svg" align="right" width="150">

**Spotify Connect controller. Written in Rust.**

![Rust](https://img.shields.io/badge/language-Rust-f74c00) ![License](https://img.shields.io/badge/license-Unlicense-green) ![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS-blue) ![Stay Amazing](https://img.shields.io/badge/Stay-Amazing-important)

Terminal controller for Spotify. Search, browse playlists and saved tracks, queue items, switch devices, drive playback (play / pause / next / prev / seek / volume / shuffle / repeat). No audio backend — tune is a Spotify Connect *controller*, so playback happens on whatever device you've authorized (your phone, your desktop Spotify, a speaker, the web player). Built on [crust](https://github.com/isene/crust). Part of the [Fe₂O₃ Rust terminal suite](https://github.com/isene/fe2o3).

## Setup (one time, ~2 min)

tune talks to the Spotify Web API on your behalf, so it needs its own developer-app credentials. There's no shared client_id — every user registers their own.

1. Open <https://developer.spotify.com/dashboard>, log in with your regular Spotify account.
2. **Create app**. Name: `tune` (or whatever). Description: free text.
3. **Add Redirect URI**: `http://127.0.0.1:8888/callback`
4. Tick **Web API**, save.
5. In the new app's **Settings**, copy the **Client ID**.

Run `tune` for the first time — it prints these same instructions and prompts for the client ID. Once pasted, tune writes `~/.tune/config.yml`, opens your browser for the authorization grant, captures the redirect, and caches the token at `~/.tune/token.json`. Subsequent launches skip straight to the TUI.

## Install

```bash
git clone https://github.com/isene/tune
cd tune
cargo build --release
cp target/release/tune ~/.local/bin/
```

Or symlink for live rebuilds:

```bash
ln -s "$(pwd)/target/release/tune" ~/bin/tune
```

## Keybindings

| Key | Action |
|---|---|
| **Views** | |
| `/` | Search |
| `P` | Your playlists |
| `L` | Liked / saved tracks |
| `Q` | Up-next queue |
| `d` | Spotify Connect devices |
| `?` | Help |
| **Playback** | |
| `SPACE` | Play / pause |
| `n` / `b` | Next / previous track |
| `+` / `-` | Volume ±5% |
| `]` / `[` | Seek +5s / −5s |
| `s` | Toggle shuffle |
| `r` | Cycle repeat (off / context / track) |
| **Lists** | |
| `j` / `k` | Down / up |
| `g` / `G` | Top / bottom |
| `PgDn`/`PgUp` | Page down / up |
| `ENTER` | Play this item / open playlist / switch to device |
| `a` | Add this track to the queue |
| `h` | Back to playlist list (from PlaylistTracks) |
| **Misc** | |
| `R` | Refresh now-playing |
| `q` | Quit |

## What you can do

- **Search** for tracks (`/` → type query). ENTER plays the cursor; `a` adds it to the queue.
- **Browse** your playlists (`P`), open one with ENTER, scroll, ENTER again to play from that track within the playlist context. `h` goes back to the playlist list.
- **Switch device** (`d`) — pick any Spotify Connect device (your phone, desktop client, a speaker) and ENTER transfers playback there.
- **Liked songs** (`L`) — your saved tracks, scroll and ENTER to play.
- **Up next** (`Q`) — shows what Spotify will play after the current track. Context-driven autoplay also shows here once `current_user_queue` resolves it.
- **Transport** — SPACE pause/resume, n/b skip, +/− volume, [/] seek, s shuffle, r repeat. Status reflects current playback state on a 2s poll.

## What you can't do

- **Stream audio.** tune is a Connect controller — it tells Spotify what to play, on which device. Playing on the local machine itself requires the Spotify Linux desktop client (or `librespot`) to be running there.
- **Edit playlists.** Currently read-only; add/remove/reorder lives behind a future scope grant.
- **Free-tier accounts.** Spotify gates most playback-modify endpoints behind Premium. Search and library browsing work, but transport calls will return 403.

## Config

`~/.tune/config.yml`:

```yaml
client_id: "<your spotify developer client id>"
poll_s: 2                 # now-playing refresh cadence, seconds
default_device: ""        # preferred device id; empty = last-used
```

Token cache: `~/.tune/token.json` (refresh token + access token; auto-refreshed when stale). Delete the file to force re-authorization (e.g. after adding a new scope).

## Part of the Rust Terminal Suite (Fe₂O₃)

See the [Fe₂O₃ suite overview](https://github.com/isene/fe2o3) and the [landing page](https://isene.github.io/fe2o3/) for the full list.

## Dependencies

**Build**: Rust toolchain.

**Runtime**: a working browser for the one-time OAuth flow (`xdg-open` / `open` / equivalent). Once authorized, tune runs offline-of-the-browser — only the Spotify Web API needs to be reachable.

## License

[Unlicense](https://unlicense.org/) — public domain.

## Credits

Built on [rspotify](https://github.com/ramsayleung/rspotify) for the Web API layer. Pair-programmed with Claude Code.
