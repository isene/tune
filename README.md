# tune

<img src="img/tune.svg" align="right" width="150">

**Spotify, your music files and internet radio. Written in Rust.**

![Rust](https://img.shields.io/badge/language-Rust-f74c00) ![License](https://img.shields.io/badge/license-Unlicense-green) ![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS-blue) ![Stay Amazing](https://img.shields.io/badge/Stay-Amazing-important)

Terminal controller for Spotify, *and* a Spotify Connect device. Search, browse playlists and saved tracks, queue items, switch devices, drive playback (play / pause / next / prev / seek / volume / shuffle / repeat). tune registers itself as a Spotify Connect endpoint via [librespot](https://github.com/librespot-org/librespot) — so audio plays right out of the same binary on whichever machine you launched it on — but it can also drive any *other* Connect device on your account (phone, desktop client, web player, smart speaker). Built on [crust](https://github.com/isene/crust). Part of the [Fe₂O₃ Rust terminal suite](https://github.com/isene/fe2o3).

tune also plays the music files on your computer, and internet radio from a directory of tens of thousands of stations. Both go through [mpv](https://mpv.io).

**Requires a Spotify Premium account.** librespot's audio stream requests are gated behind Premium; controller-only mode (search, browse, transport on *other* devices) still works for free accounts.

## Setup (one time, ~2 min)

tune talks to the Spotify Web API on your behalf, so it needs its own developer-app credentials. There's no shared client_id — every user registers their own.

1. Open <https://developer.spotify.com/dashboard>, log in with your regular Spotify account.
2. **Create app**. Name: `tune` (or whatever). Description: free text.
3. **Add Redirect URI**: `http://127.0.0.1:8888/callback`
4. Tick **Web API**, save.
5. In the new app's **Settings**, copy the **Client ID**.

Playing Spotify on this computer needs one more browser sign-in, the first time. Since August 2026 Spotify allows that playback only through its own desktop app ID. tune keeps the sign-in in `~/.tune/librespot/`. Delete that folder to sign in again.

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
| `f` | Files: folders and music files on this computer |
| `t` | Radio: your stations (`t` again leaves search results) |
| `?` | Help |
| **Playback** | |
| `SPACE` | Play / pause |
| `n` / `b` | Next / previous track |
| `+` / `-` | Volume ±5% |
| `]` / `[` | Seek +5s / −5s |
| `s` | Toggle shuffle |
| `r` | Cycle repeat (off / context / track) |
| `x` | Stop local files or radio |
| **Lists** | |
| `j` / `k` | Down / up |
| `g` / `G` | Top / bottom |
| `PgDn`/`PgUp` | Page down / up |
| `ENTER` | Play this item / open playlist / switch to device |
| `a` | Add this track to the queue; in Files, add a file to what plays; in Radio, keep a station |
| `h` | Back to playlist list (from PlaylistTracks); in Files, up a folder |
| `/` | In Radio: find stations by name or tag; `jazz @NO` keeps it to one country |
| `c` | In Radio: a country's stations, by code (`NO`) or name (`Norway`) |
| `D` | In Radio: remove one of your stations |
| **Misc** | |
| `R` | Refresh now-playing |
| `q` | Quit |

## What you can do

- **Search** for tracks (`/` → type query). ENTER plays the cursor; `a` adds it to the queue.
- **An artist's releases**: ENTER on an artist in the search results lists their albums and singles on the right, newest first. ENTER plays one; the top row plays the artist's radio. `h` and `l` move between the two lists.
- **Browse** your playlists (`P`), open one with ENTER, scroll, ENTER again to play from that track within the playlist context. `h` goes back to the playlist list.
- **Switch device** (`d`) — pick any Spotify Connect device (your phone, desktop client, a speaker) and ENTER transfers playback there.
- **Liked songs** (`L`) — your saved tracks, scroll and ENTER to play.
- **Up next** (`Q`) — shows what Spotify will play after the current track. Context-driven autoplay also shows here once `current_user_queue` resolves it.
- **Local files** (`f`): browse folders, starting in `music_dir`. ENTER on a file plays it and the rest of its folder after it; `a` adds a file to what plays. A `cover.jpg`, `folder.jpg` or `front.jpg` beside the files shows as the cover.
- **Radio** (`t`): `/` searches [radio-browser.info](https://www.radio-browser.info) by name, or by a tag like jazz or news; `jazz @NO` or `news @Norway` keeps the search to one country. `c` lists a country's stations, most played first. ENTER plays; `a` keeps a station in `~/.tune/radio.yml` and `D` removes it. The now-playing strip shows the song the station sends, and its logo.
- **Switching**: local files and radio pause Spotify when it plays on tune's own device. Playing anything from Spotify stops mpv, and so does `x`. The playback keys below work on all three.
- **Transport** — SPACE pause/resume, n/b skip, +/− volume, [/] seek, s shuffle, r repeat. Status reflects current playback state on a 2s poll.

## What you can't do

- **Edit playlists.** Currently read-only; add/remove/reorder lives behind a future scope grant.
- **Free-tier accounts.** Spotify gates audio streaming AND most playback-modify endpoints behind Premium. Search and library browsing work, but transport calls return 403.

## Config

`~/.tune/config.yml`:

```yaml
client_id: "<your spotify developer client id>"
poll_s: 2                 # now-playing refresh cadence, seconds
default_device: ""        # preferred device id; empty = last-used
local_player: true        # register tune itself as a Spotify Connect device
device_name: "tune"       # name shown in Spotify Connect picker
music_dir: ""             # where `f` starts; empty = ~/Music, or your home folder
```

Your radio stations: `~/.tune/radio.yml`.

Token cache: `~/.tune/token.json` (refresh token + access token; auto-refreshed when stale). Delete the file to force re-authorization (e.g. after adding a new scope).

### Battery-drain profile

tune is built to be quiet on a laptop:

- **Idle (nothing playing):** librespot keeps a long-lived TCP keep-alive to Spotify's access-point server (one packet every ~30 s), the now-playing pane polls the Web API every `poll_s` seconds (default 2 s, one tiny request). Audio backend's sink suspends. CPU near zero, no audio device wakeups.
- **Playing:** ogg/vorbis decode + pulseaudio write. Single-digit CPU% on any modern laptop.
- **Paused:** same as idle.
- **Local files and radio:** mpv decodes and plays. While it plays, tune asks mpv where it is once a second, over a local socket. While it is paused, tune asks nothing, and the Spotify poll stops too.

mDNS-based discovery (which would broadcast periodically) is **off by default** — tune authenticates via OAuth, so it doesn't need to advertise itself on the LAN. The librespot `with-libmdns` / `with-avahi` features are disabled.

## Part of the Rust Terminal Suite (Fe₂O₃)

See the [Fe₂O₃ suite overview](https://github.com/isene/fe2o3) and the [landing page](https://isene.github.io/fe2o3/) for the full list.

## Dependencies

**Build**: Rust toolchain.

**Runtime**: [mpv](https://mpv.io) for local files and radio (Spotify does not need it). A working browser for the one-time sign-ins (`xdg-open` / `open` / equivalent). Once authorized, tune runs offline-of-the-browser — only the Spotify Web API needs to be reachable.

## License

[Unlicense](https://unlicense.org/) — public domain.

## Credits

Built on [rspotify](https://github.com/ramsayleung/rspotify) for the Web API layer. Pair-programmed with Claude Code.
