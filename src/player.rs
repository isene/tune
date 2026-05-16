//! Local audio device — registers `tune` as a Spotify Connect
//! endpoint via librespot, decodes audio from Spotify, and pipes it
//! to PulseAudio / PipeWire. Battery profile:
//!
//! - **Idle (no playback)**: librespot keeps a TCP keep-alive to
//!   Spotify's AP server (~one packet every ~30 s), audio backend
//!   suspends its sink (PipeWire auto-suspends pulse-shim nodes after
//!   ~5 s of silence). CPU near zero.
//! - **Playing**: ogg/vorbis decode + pulse write. Single-digit CPU%
//!   on any modern laptop.
//! - **Paused**: same as idle except the audio backend MAY hold the
//!   sink open for a short grace period. PipeWire's pulse compat
//!   suspends nodes on its own timer.
//!
//! No zeroconf (`with-libmdns` / `with-avahi` disabled in Cargo.toml)
//! — discovery happens via OAuth, not MDNS broadcasts.

use librespot::core::{Session, SessionConfig};
use librespot::core::authentication::Credentials;
use librespot::playback::audio_backend;
use librespot::playback::config::{AudioFormat, PlayerConfig};
use librespot::playback::mixer::{Mixer, MixerConfig, NoOpVolume};
use librespot::playback::mixer::softmixer::SoftMixer;
use librespot::playback::player::Player;
use librespot::connect::{ConnectConfig, Spirc};
use std::sync::Arc;
use std::thread;
use tokio::sync::oneshot;

/// Handle to the background librespot session. Drop it (or call
/// `shutdown`) to terminate the Spirc task and unregister the device
/// from Spotify Connect.
pub struct LocalPlayer {
    shutdown_tx: Option<oneshot::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl LocalPlayer {
    /// Start the background Spirc session. Returns immediately —
    /// the actual connection handshake happens on the background
    /// thread; if it fails, the device just never shows up in the
    /// user's Spotify Connect list and the controller-only path
    /// keeps working.
    ///
    /// `access_token` MUST carry the `streaming` scope. `device_name`
    /// is what shows up in everyone's Spotify Connect picker, so make
    /// it identifiable (default: "tune").
    pub fn start(access_token: String, device_name: String) -> Self {
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let join = thread::Builder::new()
            .name("tune-librespot".into())
            .spawn(move || run(access_token, device_name, shutdown_rx))
            .expect("spawn librespot thread");
        Self { shutdown_tx: Some(shutdown_tx), join: Some(join) }
    }

    /// Tear down the Spirc session and wait for the thread to exit.
    /// Called from main on quit. Drop-on-implicit-quit also works
    /// (Drop runs `shutdown` so the device de-registers even on a
    /// panic in tune's TUI code).
    pub fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() { let _ = tx.send(()); }
        if let Some(jh) = self.join.take() { let _ = jh.join(); }
    }
}

impl Drop for LocalPlayer {
    fn drop(&mut self) { self.shutdown(); }
}

fn run(access_token: String, device_name: String, shutdown_rx: oneshot::Receiver<()>) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    let Ok(rt) = rt else { return; };
    rt.block_on(async move {
        // Session: talks to the Spotify AP servers. Auth uses our
        // existing OAuth access token — librespot only needs it for
        // the initial handshake; the AP connection has its own
        // long-lived auth ticket after that, so OAuth expiry doesn't
        // kill the device mid-session.
        let session_config = SessionConfig {
            device_id: device_id_for_name(&device_name),
            ..SessionConfig::default()
        };
        let session = Session::new(session_config, None);
        let credentials = Credentials::with_access_token(access_token);
        if let Err(e) = session.connect(credentials.clone(), false).await {
            eprintln!("librespot: session connect failed: {}", e);
            return;
        }

        // Player: ogg decode + audio backend. PulseAudio backend
        // talks libpulse directly (PipeWire's pulse compat layer
        // routes natively; on classic PulseAudio it's the wire
        // protocol). We pass NoOpVolume because Spotify Connect
        // controllers send their own volume commands and the OS
        // mixer is the canonical control plane; layering an
        // internal mixer here would double up the curve.
        // Audio backend: Cargo.toml picks the feature per target,
        // and we pass the matching name here. The most common
        // workstation case (x86_64-linux + PipeWire-pulse) gets the
        // pulseaudio backend — sink auto-suspends, lowest idle
        // wakeups, best battery. Cross-compiled aarch64-linux + macOS
        // builds fall back to rodio, which doesn't need libpulse-dev
        // in the sysroot.
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        const BACKEND_NAME: &str = "pulseaudio";
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        const BACKEND_NAME: &str = "rodio";
        let Some(backend) = audio_backend::find(Some(BACKEND_NAME.into())) else {
            eprintln!("librespot: {} backend not found", BACKEND_NAME);
            return;
        };
        let player_config = PlayerConfig::default();
        let player = Player::new(
            player_config,
            session.clone(),
            Box::new(NoOpVolume),
            move || backend(None, AudioFormat::S16),
        );

        // Spirc: the Connect protocol speaker. Registers `tune` as a
        // remote-controllable device on the user's account and
        // dispatches play / pause / next / seek commands to `player`.
        let connect_config = ConnectConfig {
            name: device_name.clone(),
            ..ConnectConfig::default()
        };
        // SoftMixer is the Mixer impl Spirc dispatches volume
        // commands to. We keep the player itself attenuation-free
        // (NoOpVolume above) — the user's OS mixer is the canonical
        // gain stage. Spirc still receives "set volume" commands
        // from other controllers; SoftMixer just absorbs them so
        // the protocol traffic doesn't error out.
        let mixer: Arc<dyn Mixer> = match SoftMixer::open(MixerConfig::default()) {
            Ok(m) => Arc::new(m),
            Err(e) => { eprintln!("librespot: mixer open failed: {}", e); return; }
        };
        let spirc_result = Spirc::new(
            connect_config,
            session,
            credentials,
            player,
            mixer,
        ).await;
        let (spirc, spirc_task) = match spirc_result {
            Ok(p) => p,
            Err(e) => { eprintln!("librespot: spirc start failed: {}", e); return; }
        };

        tokio::select! {
            _ = spirc_task => {}
            _ = shutdown_rx => { let _ = spirc.shutdown(); }
        }
    });
}

/// Stable device id per device-name. Spotify treats this as the
/// permanent identity of the device; using a hash of the name means
/// quitting+re-launching tune doesn't litter the user's account with
/// fresh "tune (xxxx)" devices. 32 hex chars matches librespot's
/// default format.
fn device_id_for_name(name: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    "tune".hash(&mut h);
    let a = h.finish();
    let mut h2 = std::collections::hash_map::DefaultHasher::new();
    a.hash(&mut h2);
    "tune2".hash(&mut h2);
    let b = h2.finish();
    format!("{:016x}{:016x}", a, b)
}
