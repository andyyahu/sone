use std::sync::atomic::Ordering;
use tauri::{Emitter, Manager, State};

use super::playback::compute_norm_gain;
use crate::audio::AudioDevice;
use crate::audio_output::{AudioOutputConfig, AudioOutputRoute, AudioOutputState};
use crate::cache::{CacheResult, CacheTier};
use crate::AppState;
use crate::SignalPath;
use crate::SoneError;

/// Open the SONE log directory (`~/.config/sone/logs`) in the system file
/// manager, creating it if it does not exist yet.
#[tauri::command]
pub fn open_log_folder() -> Result<(), String> {
    let dir = dirs::config_dir()
        .map(|d| d.join("sone").join("logs"))
        .ok_or_else(|| "could not resolve config directory".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::process::Command::new("xdg-open")
        .arg(&dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to launch file manager: {e}"))
}

#[tauri::command]
pub fn get_signal_path(state: State<'_, AppState>) -> SignalPath {
    state.signal_path.snapshot()
}

#[tauri::command]
pub fn refresh_signal_path(state: State<'_, AppState>) -> SignalPath {
    state.pipeline_probe.refresh();
    state.signal_path.snapshot()
}

#[tauri::command]
pub async fn update_tray_tooltip(app: tauri::AppHandle, text: String) -> Result<String, SoneError> {
    #[cfg(target_os = "linux")]
    if let Some(tray_handle) = app.try_state::<crate::tray::TrayHandle>() {
        tray_handle.inner().update_tooltip(text).await;
        return Ok("updated".into());
    }
    Ok("tray not available".into())
}

#[tauri::command]
pub async fn get_image_bytes(
    state: State<'_, AppState>,
    url: String,
) -> Result<tauri::ipc::Response, SoneError> {
    cached_image_bytes(&state.disk_cache, &state.proxied_http, &url)
        .await
        .map(tauri::ipc::Response::new)
}

async fn cached_image_bytes(
    cache: &crate::cache::DiskCache,
    proxied_http: &crate::proxy_http::ProxiedHttp,
    url: &str,
) -> Result<Vec<u8>, SoneError> {
    log::debug!("[get_image_bytes]: url={}", url);

    let fetch_ticket = cache.begin_fetch().await;
    match cache.get(url, CacheTier::Image).await {
        CacheResult::Fresh(bytes) | CacheResult::Stale(bytes) => {
            log::debug!("[get_image_bytes]: cache hit ({} bytes)", bytes.len());
            Ok(bytes)
        }
        CacheResult::Miss => {
            let http_client = proxied_http
                .client()
                .map_err(|e| SoneError::ProxyBlocked { reason: e.cause })?;
            // Error pages are not cover art: caching one would make every
            // later request fail to decode until the image tier expires.
            let res = http_client.get(url).send().await?.error_for_status()?;
            let bytes = res.bytes().await?.to_vec();

            cache
                .put_if_current(&fetch_ticket, url, &bytes, CacheTier::Image, &["image"])
                .await
                .ok();
            log::debug!(
                "[get_image_bytes]: fetched and cached {} bytes",
                bytes.len()
            );

            Ok(bytes)
        }
    }
}

#[tauri::command]
pub async fn get_cache_stats(
    state: State<'_, AppState>,
) -> Result<crate::cache::CacheStats, SoneError> {
    Ok(state.disk_cache.stats().await)
}

#[tauri::command]
pub async fn clear_disk_cache(state: State<'_, AppState>) -> Result<(), SoneError> {
    log::info!("[clear_disk_cache]: user-initiated cache clear");
    state.disk_cache.clear().await;
    Ok(())
}

#[tauri::command]
pub fn get_decorations(state: State<'_, AppState>) -> bool {
    state.decorations.load(Ordering::Relaxed)
}

#[tauri::command]
pub fn set_decorations(
    window: tauri::Window,
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<(), SoneError> {
    state.settings_store.update_with_apply(
        |settings| {
            settings.decorations = enabled;
            Ok(())
        },
        |settings| {
            window
                .set_decorations(settings.decorations)
                .map_err(SoneError::from)?;
            state
                .decorations
                .store(settings.decorations, Ordering::Relaxed);
            Ok(())
        },
    )
}

#[tauri::command]
pub fn get_minimize_to_tray(state: State<'_, AppState>) -> bool {
    state.minimize_to_tray.load(Ordering::Relaxed)
}

#[tauri::command]
pub fn set_minimize_to_tray(state: State<'_, AppState>, enabled: bool) -> Result<(), SoneError> {
    state.settings_store.update_with_apply(
        |settings| {
            settings.minimize_to_tray = enabled;
            Ok(())
        },
        |settings| {
            state
                .minimize_to_tray
                .store(settings.minimize_to_tray, Ordering::Relaxed);
            Ok(())
        },
    )
}

#[tauri::command]
pub fn get_volume_normalization(state: State<'_, AppState>) -> bool {
    state.volume_normalization.load(Ordering::Relaxed)
}

#[tauri::command]
pub fn set_volume_normalization(
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<(), SoneError> {
    if state
        .audio_player
        .output_state()
        .active
        .is_some_and(|config| config.route == AudioOutputRoute::Hqplayer || config.bit_perfect)
    {
        return Err(SoneError::Audio(
            "Normalization is unavailable on the active output".into(),
        ));
    }
    state.settings_store.update_with_apply(
        |settings| {
            settings.volume_normalization = enabled;
            Ok(())
        },
        |settings| {
            let enabled = settings.volume_normalization;
            let norm_gain = if enabled {
                let rg = f64::from_bits(state.last_replay_gain.load(Ordering::Relaxed));
                let peak = f64::from_bits(state.last_peak_amplitude.load(Ordering::Relaxed));
                compute_norm_gain(
                    rg.is_finite().then_some(rg),
                    peak.is_finite().then_some(peak),
                )
            } else {
                1.0
            };
            state
                .audio_player
                .set_normalization_gain(norm_gain)
                .map_err(SoneError::Audio)?;
            state.volume_normalization.store(enabled, Ordering::Relaxed);
            state.signal_path.set_normalization_enabled(enabled);
            Ok(())
        },
    )
}

// Apply the complete output choice so a failed commit can restore it, including
// the coupling between exclusive mode, bit-perfect and the selected device.
fn apply_output_settings(state: &AppState, settings: &crate::Settings) -> Result<(), SoneError> {
    state
        .audio_player
        .configure_output(AudioOutputConfig::from_settings(settings))
        .map_err(SoneError::Audio)?;
    state
        .exclusive_mode
        .store(settings.exclusive_mode, Ordering::Relaxed);
    state
        .bit_perfect
        .store(settings.bit_perfect, Ordering::Relaxed);
    *state.exclusive_device.lock().unwrap() = settings.exclusive_device.clone();
    Ok(())
}

#[tauri::command]
pub fn get_audio_output(state: State<'_, AppState>) -> AudioOutputState {
    state.audio_player.output_state()
}

fn prepare_audio_output(mut config: AudioOutputConfig) -> Result<AudioOutputConfig, SoneError> {
    config.validate().map_err(SoneError::Audio)?;
    match config.route {
        AudioOutputRoute::Camilla => {
            #[cfg(target_os = "linux")]
            crate::camilla_fir::validate_config_file(
                config.camilla_config.as_deref().expect("validated path"),
            )
            .map_err(SoneError::Audio)?;
            #[cfg(not(target_os = "linux"))]
            return Err(SoneError::Audio("CamillaDSP requires Linux".into()));
        }
        AudioOutputRoute::Hqplayer => {
            // GetInfo never changes Desktop's transport or opens/releases a DAC.
            let mut client = crate::hqplayer::ControlSession::connect(
                &config.hqplayer_host,
                config.hqplayer_port,
            )
            .map_err(|error| SoneError::Audio(format!("HQPlayer is unavailable: {error}")))?;
            client
                .get_info()
                .map_err(|error| SoneError::Audio(format!("HQPlayer is unavailable: {error}")))?;
        }
        AudioOutputRoute::Native => {}
    }
    Ok(config)
}

#[tauri::command]
pub async fn set_audio_output(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    config: AudioOutputConfig,
) -> Result<AudioOutputState, SoneError> {
    let _output_guard = state.audio_output_settings_lock.lock().await;
    // File reads, trial DSP construction and local control probing are not
    // allowed to hold the global settings mutex or the audio actor.
    let config = tokio::task::spawn_blocking(move || prepare_audio_output(config))
        .await
        .map_err(|error| SoneError::Audio(error.to_string()))??;
    let handle = app.clone();
    let result = tokio::task::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        state.settings_store.update_with_apply_guarded(
            || state.audio_player.begin_output_update(),
            |settings| {
                config.write_settings(settings);
                Ok(())
            },
            |settings| apply_output_settings(&state, settings),
        )?;
        Ok::<_, SoneError>(state.audio_player.output_state())
    })
    .await
    .map_err(|error| SoneError::Audio(error.to_string()))??;
    let _ = app.emit("audio-output-changed", &result);
    Ok(result)
}

/// The native chooser runs on GTK's main loop; waiting for it never holds an
/// output transaction or a settings lock.
#[tauri::command]
pub async fn pick_camilla_config(app: tauri::AppHandle) -> Result<Option<String>, String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = app;
        Err("CamillaDSP requires Linux".into())
    }
    #[cfg(target_os = "linux")]
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        app.run_on_main_thread(move || {
            use gtk::prelude::*;
            let dialog = gtk::FileChooserDialog::builder()
                .title("Choose CamillaDSP configuration")
                .action(gtk::FileChooserAction::Open)
                .modal(true)
                .build();
            dialog.add_button("Cancel", gtk::ResponseType::Cancel);
            dialog.add_button("Open", gtk::ResponseType::Accept);
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("CamillaDSP YAML"));
            filter.add_pattern("*.yml");
            filter.add_pattern("*.yaml");
            dialog.add_filter(filter);
            let file = (dialog.run() == gtk::ResponseType::Accept)
                .then(|| dialog.filename())
                .flatten();
            dialog.close();
            let result = file
                .map(|file| {
                    file.into_os_string()
                        .into_string()
                        .map_err(|_| "Configuration path is not UTF-8".to_owned())
                })
                .transpose();
            let _ = tx.send(result);
        })
        .map_err(|error| error.to_string())?;
        rx.await
            .map_err(|_| "Configuration chooser closed".to_owned())?
    }
}

#[tauri::command]
pub fn get_exclusive_mode(state: State<'_, AppState>) -> bool {
    state.exclusive_mode.load(Ordering::Relaxed)
}

async fn update_native_output(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    edit: impl FnOnce(&mut crate::Settings) + Send + 'static,
) -> Result<(), SoneError> {
    let _output_guard = state.audio_output_settings_lock.lock().await;
    let handle = app.clone();
    let output = tokio::task::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        state.settings_store.update_with_apply_guarded(
            || state.audio_player.begin_output_update(),
            |settings| {
                // Resolve legacy route before changing its native preferences.
                // Disabling exclusive must not accidentally enable/disable DSP.
                settings.output_route = Some(AudioOutputConfig::from_settings(settings).route);
                edit(settings);
                Ok(())
            },
            |settings| apply_output_settings(&state, settings),
        )?;
        Ok::<_, SoneError>(state.audio_player.output_state())
    })
    .await
    .map_err(|error| SoneError::Audio(error.to_string()))??;
    let _ = app.emit("audio-output-changed", &output);
    Ok(())
}

#[tauri::command]
pub async fn set_exclusive_mode(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<(), SoneError> {
    update_native_output(app, state, move |settings| {
        settings.exclusive_mode = enabled;
        if !enabled {
            settings.bit_perfect = false;
        }
    })
    .await
}

#[tauri::command]
pub fn get_bit_perfect(state: State<'_, AppState>) -> bool {
    state.bit_perfect.load(Ordering::Relaxed)
}

#[tauri::command]
pub async fn set_bit_perfect(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<(), SoneError> {
    update_native_output(app, state, move |settings| {
        settings.bit_perfect = enabled;
        if enabled {
            settings.exclusive_mode = true;
        }
    })
    .await
}

#[tauri::command]
pub fn get_gapless(state: State<'_, AppState>) -> bool {
    state.gapless.load(Ordering::Relaxed)
}

#[tauri::command]
pub fn get_gapless_supported() -> bool {
    crate::audio::gapless_supported()
}

#[tauri::command]
pub fn set_gapless(state: State<'_, AppState>, enabled: bool) -> Result<(), SoneError> {
    state.settings_store.update_with_apply(
        |settings| {
            settings.gapless = enabled;
            Ok(())
        },
        |settings| {
            state
                .audio_player
                .set_gapless(settings.gapless)
                .map_err(SoneError::Audio)?;
            state.gapless.store(settings.gapless, Ordering::Relaxed);
            Ok(())
        },
    )
}

#[tauri::command]
pub fn get_max_quality(state: State<'_, AppState>) -> String {
    state.max_quality.lock().unwrap().clone()
}

#[tauri::command]
pub fn set_max_quality(state: State<'_, AppState>, quality: String) -> Result<(), SoneError> {
    if !matches!(quality.as_str(), "HI_RES_LOSSLESS" | "LOSSLESS" | "HIGH") {
        return Err(SoneError::Parse(format!("invalid max_quality: {quality}")));
    }
    state.settings_store.update_with_apply(
        |settings| {
            settings.max_quality = quality;
            Ok(())
        },
        |settings| {
            *state.max_quality.lock().unwrap() = settings.max_quality.clone();
            Ok(())
        },
    )
}

#[tauri::command]
pub fn get_exclusive_device(state: State<'_, AppState>) -> Option<String> {
    state.exclusive_device.lock().unwrap().clone()
}

#[tauri::command]
pub async fn set_exclusive_device(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    device: String,
) -> Result<(), SoneError> {
    update_native_output(app, state, move |settings| {
        settings.exclusive_device = Some(device);
    })
    .await
}

#[tauri::command]
pub fn list_audio_devices(
    state: State<'_, AppState>,
    force_refresh: Option<bool>,
) -> Result<Vec<AudioDevice>, SoneError> {
    state
        .cached_audio_devices
        .get(force_refresh.unwrap_or(false))
        .map_err(SoneError::Audio)
}

#[tauri::command]
pub fn get_discord_rpc(state: State<'_, AppState>) -> bool {
    state
        .load_settings()
        .map(|s| s.discord_rpc)
        .unwrap_or(false)
}

#[tauri::command]
pub fn set_discord_rpc(state: State<'_, AppState>, enabled: bool) -> Result<(), SoneError> {
    state.settings_store.update_with_apply(
        |settings| {
            settings.discord_rpc = enabled;
            Ok(())
        },
        |settings| {
            state.discord.send(if settings.discord_rpc {
                crate::discord::DiscordCommand::Connect
            } else {
                crate::discord::DiscordCommand::Disconnect
            });
            Ok(())
        },
    )
}

#[tauri::command]
pub fn get_report_plays(state: State<'_, AppState>) -> bool {
    state
        .load_settings()
        .map(|s| s.report_plays)
        .unwrap_or(true)
}

#[tauri::command]
pub async fn set_report_plays(state: State<'_, AppState>, enabled: bool) -> Result<(), SoneError> {
    // Persist first: if the write fails the caller sees an error and the
    // in-memory state still matches disk. Reversing this order can silently
    // discard a user's opt-out.
    state.update_settings(|settings| {
        settings.report_plays = enabled;
        Ok(())
    })?;

    state.tidal_reporter.set_enabled(enabled);
    if enabled {
        // Flush any offline backlog now that reporting is on.
        state.tidal_reporter.drain_queue().await;
    } else {
        // Drop the in-flight session: every lifecycle hook is gated on
        // `enabled`, so an orphaned session would keep accruing wall-clock time
        // and get reported on the next enable.
        state.tidal_reporter.clear_session().await;
    }
    Ok(())
}

#[tauri::command]
pub fn get_discord_status_text(state: State<'_, AppState>) -> String {
    state
        .load_settings()
        .map(|s| s.discord_status_text)
        .unwrap_or_default()
}

#[tauri::command]
pub fn set_discord_status_text(state: State<'_, AppState>, text: String) -> Result<(), SoneError> {
    state.settings_store.update_with_apply(
        |settings| {
            settings.discord_status_text = text;
            Ok(())
        },
        |settings| {
            state
                .discord
                .send(crate::discord::DiscordCommand::SetStatusText {
                    text: settings.discord_status_text.clone(),
                });
            Ok(())
        },
    )
}

#[tauri::command]
pub fn get_proxy_settings(state: State<'_, AppState>) -> crate::ProxySettings {
    state.load_settings().map(|s| s.proxy).unwrap_or_default()
}

/// The standing report of what the proxy is doing, for the banner that has to
/// be visible before anyone is logged in.
///
/// Reads the live cell rather than only the saved settings, because neither
/// state that strands a user is visible in the settings alone: a plan that is
/// fine and a client that could not be built (`Blocked`), and a plan and client
/// that are both fine while every request dies in transit (`Unreachable`). See
/// `ProxyStatus::observed`. `degraded` reflects the probed host capabilities.
///
/// Cheap enough to poll, which the banner does: a settings decrypt, a lock read
/// and an atomic load. It sends nothing — the reachability answer is the record
/// of requests the app already made, so asking for the status never puts a
/// packet on the wire.
#[tauri::command]
pub fn get_proxy_status(state: State<'_, AppState>) -> crate::proxy::ProxyStatus {
    let settings = state.load_settings().map(|s| s.proxy).unwrap_or_default();
    let block = state.proxied_http.client().err();
    crate::proxy::ProxyStatus::observed(
        &settings,
        &state.host_caps(),
        block.as_ref().map(|e| e.cause.as_str()),
        state.proxied_http.unreachable(),
    )
}

/// The origin both proxy probes aim at. A real API host, so a proxy that
/// allow-lists destinations is exercised the way the app will actually use it,
/// and the cheapest thing it serves.
const PROXY_PROBE_URL: &str = "https://api.tidal.com/v1/ping";

/// One request through the **live** client, so a proxy that started working
/// again can say so.
///
/// The count behind `unreachable` is cleared only by an answered request, and
/// the state it describes is precisely the one in which the app makes none —
/// the user is looking at a window whose every request is failing. Nothing else
/// clears it: `get_proxy_status` sends nothing by design, and
/// `test_proxy_connection` probes the settings being edited rather than the
/// cell. Fix the proxy externally — restart it, plug the cable back in — and
/// without this the banner stands forever.
///
/// So the banner calls this, and only while it is already reporting
/// `unreachable`. Every other state keeps the property that asking about the
/// proxy puts nothing on the wire.
///
/// The generation comes from `client_at` beside the client that sends, never a
/// separate read: an answer that lands after a settings save must be discarded
/// rather than credited to the proxy that replaced the one it used.
///
/// A blocked cell is left alone. Nothing can be sent through it, `unanswered`
/// says nothing about it, and the banner reports that state separately.
#[tauri::command]
pub async fn probe_proxy_reachability(state: State<'_, AppState>) -> Result<(), ()> {
    probe_proxy_reachability_inner(state.proxied_http.clone(), PROXY_PROBE_URL).await;
    Ok(())
}

/// The origin is a parameter only so a test can point it at a loopback
/// stand-in; the command has exactly one destination.
async fn probe_proxy_reachability_inner(cell: crate::proxy_http::ProxiedHttp, url: &str) {
    let (generation, client) = cell.client_at();
    let Ok(client) = client else { return };
    // Short on purpose: this runs on a 15s poll, and a dead proxy that holds
    // the connection open must not still be pending when the next one fires.
    let outcome = client
        .get(url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;
    // The count is the whole product; the response body is of no interest.
    let _ = cell.observe_at(generation, outcome);
}

/// Persist the proxy settings, then reconfigure the transports — in that
/// order, and the order is the whole point.
///
/// The settings file is AES-GCM encrypted and the proxy screen is its only
/// editor, so a reconfiguration failure that aborted the save would strand the
/// user: the proxy they just tried to turn *off* never reaches disk, the next
/// start reads the bad value back, and every request — including the ones the
/// UI needs — stays blocked. Saving first makes a bad proxy recoverable from
/// the same screen that set it.
///
/// Persisting first must not become swallowing, so the save succeeds *and* the
/// error is returned: a blocked plan yields `ProxyBlocked` carrying the cause,
/// alongside settings that are already on disk.
///
/// The reason now reaches the user. `NetworkTab.tsx` no longer discards the
/// rejection: it reads the cause out of `ProxyBlocked` — whose `message` is an
/// object, never a string — and prints it in the settings banner, so a refusal
/// names the field or the missing host capability instead of saying
/// "connection failed". Playback does the same, as its own toast, and that path
/// deliberately does not treat a block as an unplayable track.
///
/// `get_proxy_status` emits `ProxyStatus`, and `ProxyBlockedBanner.tsx` renders
/// a block outside the authenticated shell with a button that turns the proxy
/// off — so a block has a standing report and a way out. Its `degraded` list is
/// real: `get_proxy_status` passes the probed `HostCaps`, so a plan that serves
/// the API while refusing one audio tier is named before that tier is used —
/// `lossy` on a host below `CURL_SEEK_FIXED` with a credentialed proxy, say.
///
/// Split out from the command so the ordering can be tested without an
/// `AppState`; `persist` stands in for the encrypted read-modify-write.
async fn persist_then_reconfigure(
    settings: &crate::ProxySettings,
    persist: impl FnOnce(&crate::ProxySettings) -> Result<(), SoneError>,
    http: crate::proxy_http::ProxiedHttp,
    caps: crate::proxy::HostCaps,
) -> Result<(), SoneError> {
    // Before any transport is touched. A failure to save is the one reason to
    // leave the transports alone: nothing changed, so nothing should move.
    persist(settings)?;

    // Claimed here, adjacent to the write above, and *not* inside the closure
    // below — that placement is the entire point of this line.
    //
    // The cell is generation-ordered: the highest claim wins it, whichever
    // build finishes last. Claiming inside `spawn_blocking` made "highest"
    // mean "last to reach the blocking pool", which a scheduler decides, so
    // two overlapping saves could persist A then B while the cell took B then
    // A. Disk would say the proxy is enabled and the cell would hold the
    // earlier `Direct` client — the user believes they are proxied and every
    // request leaves from their real address. Fail-open, in the one direction
    // that matters, and previously documented here as impossible.
    //
    // Claiming next to the persist makes the cell's order the persist's order.
    // The two are adjacent synchronous statements with no `.await` between
    // them, so nothing of this task's own runs in between; making them atomic
    // against a preemption would need a lock spanning the save, and the
    // version of that which also spanned the build would queue a save behind
    // another save's `getaddrinfo` — exactly the "turn it off" path this
    // ordering exists to keep responsive.
    //
    // Not a rare race, either: `NetworkTab.tsx` debounces a save on every
    // keystroke, so typing a hostname fires several, and once one of them is a
    // SOCKS5 host that takes seconds to resolve they overlap as a matter of
    // course rather than by bad luck. Narrower than it was — the settings screen
    // now withholds an enabled proxy until both host and port are present, so
    // the half-typed prefixes no longer arrive — but every keystroke after the
    // port is set still sends one, so the overlap stands.
    let generation = http.claim();

    // Swapping the one shared cell is the whole transport update: every reqwest
    // consumer reads through it, so there is nothing left to push out to them.
    // `apply` is the single place that decides what unplannable settings mean,
    // so this cannot drift back to a direct connection independently of
    // startup. It is blocking (`build_client` may resolve the proxy host),
    // hence the detour off the runtime worker.
    let candidate = settings.clone();
    let applied = tokio::task::spawn_blocking(move || http.apply_at(generation, &candidate, &caps))
        .await
        .map_err(|e| SoneError::Io(format!("proxy update task failed: {e}")))?;

    applied.map_err(|e| {
        log::warn!("[proxy] settings saved but unusable: {}", e.cause);
        SoneError::ProxyBlocked { reason: e.cause }
    })
}

#[tauri::command]
pub async fn set_proxy_settings(
    state: State<'_, AppState>,
    settings: crate::ProxySettings,
) -> Result<(), SoneError> {
    // Re-probe first, so everything below answers from the same reading the
    // audio thread takes: it re-probes on every `SetProxySettings`, while the
    // copy stored at startup never moved. Left frozen, `explain_block` — a
    // layer that is advisory by design — is a permanent stop the containment
    // boundary would not impose.
    state.refresh_host_caps();
    let outcome = persist_then_reconfigure(
        &settings,
        |s| {
            state.update_settings(|app_settings| {
                app_settings.proxy = s.clone();
                Ok(())
            })?;

            // Mirror the two non-secret fields next to the encrypted file so
            // the next launch can decide whether to scrub the proxy
            // environment before any thread exists. Only after the save
            // succeeds: a sidecar saying "on" beside settings that never
            // reached disk would scrub for a proxy nobody configured.
            //
            // Two known holes, both rooted in the same fact: the environment
            // is immutable once GTK/glib have threads, so a mid-session change
            // cannot undo what startup did or did not do.
            //
            // 1. Enabling the proxy mid-session on a host that had an ambient
            //    `no_proxy` at launch. Startup did not scrub (the sidecar said
            //    off), and `curlhttpsrc` read that variable when its first
            //    element was constructed. reqwest and the webview reconfigure
            //    fine; the GStreamer audio path may still honour the stale
            //    `no_proxy` for matching hosts until SONE is restarted.
            //
            // 2. Disabling the proxy mid-session after startup scrubbed. The
            //    system's own configuration is what `Direct` means, and the
            //    scrub deleted part of it — `no_proxy`/`NO_PROXY` only, since
            //    that is all `SCRUBBED_PROXY_ENV_VARS` removes. reqwest is
            //    covered: `main.rs` captures the values before removing them
            //    and `proxy_http::restore_system_proxy` hands them back on the
            //    `Direct` route, so this command's own reconfiguration is
            //    correct. The GStreamer and WebKit paths are not: gio and
            //    libproxy read the process environment directly and there is
            //    nowhere to inject a captured value. Those two still find the
            //    user's `http_proxy`/`https_proxy` — those were never removed
            //    — but not their bypass list, so for the rest of the session
            //    they proxy hosts the user had excluded. That needs a restart,
            //    plainly, and is not fixable from here.
            if let Some(dir) = state.settings_path.parent() {
                crate::proxy::write_sidecar(dir, s);
            }
            Ok(())
        },
        state.proxied_http.clone(),
        state.host_caps(),
    )
    .await;

    // Applies to future GStreamer HTTP sources, and — since a live pipeline
    // carries the route it was built with and nothing re-applies one — tears the
    // playing one down and rebuilds it at its current position when the route
    // actually changes. Pushed on every outcome: the audio thread keeps its own
    // copy, and leaving it on the settings the user just replaced is wrong
    // whether or not the shared cell could be rebuilt.
    //
    // "Every outcome" is wider than it used to be. This push previously sat
    // above the save, so a `spawn_blocking` join failure `?`-returned before it
    // and the audio thread kept the old settings; now it runs on that path too.
    // Deliberate — a panicked build task says nothing about what the audio
    // thread should use — but it is a difference, not a pure reordering.
    state.audio_player.set_proxy_settings(settings);

    outcome
}

#[tauri::command]
pub async fn inhibit_idle(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<(), SoneError> {
    state.idle_inhibitor.lock().await.inhibit(&window).await;
    Ok(())
}

#[tauri::command]
pub async fn uninhibit_idle(state: State<'_, AppState>) -> Result<(), SoneError> {
    state.idle_inhibitor.lock().await.uninhibit().await;
    Ok(())
}

/// The client the "Test connection" button must use, or the reason there is
/// none. Split out from the command so the refusals can be tested without
/// touching the network.
///
/// Three things this must never do, because the banner reads any `Ok` as a
/// green success: test the *saved* proxy instead of the candidate one, build a
/// client by any path other than the one the app itself uses, or hand back a
/// direct connection. `ProxyPlan::Direct` is a refusal here — a direct
/// connection always "succeeds", and reporting that as a working proxy is the
/// exact lie this function used to tell.
fn proxy_test_client(
    settings: &crate::ProxySettings,
    caps: &crate::proxy::HostCaps,
) -> Result<reqwest::Client, String> {
    let plan = crate::proxy::plan(settings, caps).map_err(|e| e.to_string())?;
    if plan == crate::proxy::ProxyPlan::Direct {
        return Err("No proxy configured — enable one before testing".to_string());
    }
    crate::proxy_http::build_client(&plan, caps).map_err(|e| e.cause)
}

/// Probe the settings the user is editing — NOT the live cell, which still
/// holds the saved proxy.
#[tauri::command]
pub async fn test_proxy_connection(
    settings: crate::ProxySettings,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let caps = state.host_caps();
    test_proxy_connection_inner(settings, caps).await
}

async fn test_proxy_connection_inner(
    settings: crate::ProxySettings,
    caps: crate::proxy::HostCaps,
) -> Result<String, String> {
    // `build_client` resolves the proxy host for SOCKS5; keep that off the
    // runtime worker.
    let client = tokio::task::spawn_blocking(move || proxy_test_client(&settings, &caps))
        .await
        .map_err(|e| format!("proxy test task failed: {e}"))??;

    match client
        .get(PROXY_PROBE_URL)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() || status.as_u16() == 404 || status.as_u16() == 401 {
                Ok("Connection successful".to_string())
            } else {
                Ok(format!("Tidal responded with status {status}"))
            }
        }
        Err(e) => Err(format!("Connection failed: {e}")),
    }
}

fn logging_toggle_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("sone").join("logging.toggle"))
}

#[tauri::command]
pub fn get_enable_logging() -> bool {
    let Some(path) = logging_toggle_path() else {
        return true;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return true;
    };
    text.trim() != "false"
}

#[tauri::command]
pub fn set_enable_logging(enabled: bool) -> Result<(), SoneError> {
    let Some(path) = logging_toggle_path() else {
        return Err(SoneError::Io("Could not resolve config dir".into()));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SoneError::Io(format!("Failed to create config dir: {e}")))?;
    }
    let body = if enabled { "true" } else { "false" };
    std::fs::write(&path, body)
        .map_err(|e| SoneError::Io(format!("Failed to write logging toggle: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProxySettings, ProxyType};

    #[tokio::test]
    async fn image_http_errors_are_not_cached_and_a_later_success_is_reused() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/cover.jpg", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for (status, body) in [
                ("404 Not Found", "missing"),
                ("500 Internal Server Error", "unavailable"),
                ("200 OK", "cover bytes"),
            ] {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = tokio::io::BufReader::new(socket);
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).await.unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                reader
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let cache = crate::cache::DiskCache::new(
            dir.path(),
            std::sync::Arc::new(crate::crypto::Crypto::for_tests()),
        );
        let http = direct_cell();
        for _ in 0..2 {
            assert!(cached_image_bytes(&cache, &http, &url).await.is_err());
            assert_eq!(cache.stats().await.total_entries, 0);
            assert!(matches!(
                cache.get(&url, CacheTier::Image).await,
                CacheResult::Miss
            ));
        }
        assert_eq!(
            cached_image_bytes(&cache, &http, &url).await.unwrap(),
            b"cover bytes"
        );
        server.await.unwrap();
        // The server is gone: a cache hit must also work with HTTP blocked.
        let blocked = crate::proxy_http::ProxiedHttp::blocked("offline");
        assert_eq!(
            cached_image_bytes(&cache, &blocked, &url).await.unwrap(),
            b"cover bytes"
        );
        assert_eq!(cache.stats().await.total_entries, 1);
    }

    fn caps() -> crate::proxy::HostCaps {
        crate::proxy::HostCaps::assume_all_present()
    }

    fn enabled(proxy_type: ProxyType, host: &str, port: u16) -> ProxySettings {
        ProxySettings {
            enabled: true,
            proxy_type,
            host: host.to_string(),
            port,
            username: None,
            password: None,
        }
    }

    /// A stand-in for the encrypted read-modify-write, writing real bytes to a
    /// real file: "persisted" has to mean a restart would read it back, not
    /// that a closure was called.
    fn save_json(path: &std::path::Path, s: &ProxySettings) -> Result<(), SoneError> {
        std::fs::write(path, serde_json::to_string(s)?)?;
        Ok(())
    }

    /// What the next start would read.
    fn on_disk(path: &std::path::Path) -> ProxySettings {
        serde_json::from_str(&std::fs::read_to_string(path).expect("nothing was saved"))
            .expect("saved settings must parse")
    }

    fn same(a: &ProxySettings, b: &ProxySettings) -> bool {
        serde_json::to_string(a).unwrap() == serde_json::to_string(b).unwrap()
    }

    fn direct_cell() -> crate::proxy_http::ProxiedHttp {
        crate::proxy_http::ProxiedHttp::from_plan(&crate::proxy::ProxyPlan::Direct, &caps())
    }

    /// The recovery property, and the reason the order was inverted: a proxy
    /// whose client cannot be built must still reach disk. It is what the next
    /// start reads back, the file is encrypted so nothing else can edit it, and
    /// every request the app makes — including the ones behind the settings
    /// screen — is blocked until it changes.
    #[tokio::test]
    async fn a_proxy_that_cannot_be_applied_is_still_saved() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");

        // Plans fine and fails at build: reqwest resolves a SOCKS5 proxy host
        // eagerly, so this is the failure `plan()` cannot see coming. No
        // network is touched — `.invalid` can never resolve (RFC 6761).
        let bad = enabled(ProxyType::Socks5, "no-such-host.invalid", 3128);
        assert!(crate::proxy::plan(&bad, &caps()).is_ok());

        let http = direct_cell();
        let err = persist_then_reconfigure(&bad, |s| save_json(&file, s), http.clone(), caps())
            .await
            .expect_err("an unusable proxy must not report success");

        assert!(
            matches!(&err, SoneError::ProxyBlocked { reason } if reason.contains("no-such-host.invalid")),
            "the failure must name its cause, got: {err:?}"
        );
        assert!(
            same(&on_disk(&file), &bad),
            "the settings the user submitted must survive the transport failure"
        );
        // Saving first must not have loosened containment.
        assert!(
            http.client().is_err(),
            "a proxy that could not be built must leave egress blocked"
        );
    }

    /// Same property one step earlier: settings that form no plan at all.
    #[tokio::test]
    async fn a_proxy_that_forms_no_plan_is_still_saved() {
        let dir = tempfile::tempdir().unwrap();

        for bad in [
            enabled(ProxyType::Http, "127.0.0.1", 0),
            enabled(ProxyType::Http, "ho st", 3128),
            enabled(ProxyType::Http, "пример.рф", 3128),
        ] {
            let file = dir.path().join(format!("{}-{}.json", bad.host, bad.port));
            let http = direct_cell();
            let err = persist_then_reconfigure(&bad, |s| save_json(&file, s), http.clone(), caps())
                .await
                .expect_err("unplannable settings must not report success");

            assert!(
                matches!(&err, SoneError::ProxyBlocked { reason } if !reason.is_empty()),
                "{bad:?} must be refused with a reason, got: {err:?}"
            );
            assert!(same(&on_disk(&file), &bad), "{bad:?} must still be saved");
            assert!(http.client().is_err(), "{bad:?} must leave egress blocked");
        }
    }

    /// Pins the order itself, not merely that both things happened: at save
    /// time the cell must still hold the *previous* client. Put the save back
    /// after the transport swap and this fails.
    #[tokio::test]
    async fn the_save_lands_before_the_transports_move() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        let http = direct_cell();

        let observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let witness = (http.clone(), observed.clone());
        let bad = enabled(ProxyType::Socks5, "no-such-host.invalid", 3128);

        let err = persist_then_reconfigure(
            &bad,
            move |s| {
                let (cell, flag) = witness;
                flag.store(cell.client().is_ok(), Ordering::SeqCst);
                save_json(&file, s)
            },
            http.clone(),
            caps(),
        )
        .await
        .expect_err("the reconfiguration still has to fail for this to mean anything");

        assert!(matches!(err, SoneError::ProxyBlocked { .. }));
        assert!(
            observed.load(Ordering::SeqCst),
            "the transports were already reconfigured when the save ran — the \
             save must come first, or a failure to apply strands the user"
        );
        assert!(http.client().is_err(), "and the reconfiguration did happen");
    }

    /// The one case where the transports must NOT move: nothing was saved, so
    /// the cell must keep matching what a restart would read.
    #[tokio::test]
    async fn a_save_that_fails_leaves_the_transports_alone() {
        let http = direct_cell();
        let good = enabled(ProxyType::Http, "127.0.0.1", 3128);

        let err = persist_then_reconfigure(
            &good,
            |_| Err(SoneError::Io("disk full".into())),
            http.clone(),
            caps(),
        )
        .await
        .expect_err("a failed save must be reported");

        assert!(matches!(err, SoneError::Io(_)), "got: {err:?}");
        let c = http
            .client()
            .expect("the previous, working client must survive");
        assert!(
            !format!("{c:?}").contains("127.0.0.1:3128"),
            "settings that never reached disk must not reach the transports: {c:?}"
        );
    }

    #[tokio::test]
    async fn a_usable_proxy_is_saved_and_reported_as_applied() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        let http = direct_cell();
        let good = enabled(ProxyType::Http, "127.0.0.1", 3128);

        persist_then_reconfigure(&good, |s| save_json(&file, s), http.clone(), caps())
            .await
            .expect("a usable proxy must apply cleanly");

        assert!(same(&on_disk(&file), &good));
        let c = http.client().expect("a usable proxy must yield a client");
        assert!(
            format!("{c:?}").contains("All(http://127.0.0.1:3128)"),
            "the cell must carry the proxy that was just saved: {c:?}"
        );
    }

    /// The recovery the pre-login banner's button performs, end to end.
    ///
    /// Reachable state: a proxy that persists and cannot build, a logged-out
    /// user, and no settings screen — every request is refused, login
    /// included. The button sends the same settings with `enabled: false`, and
    /// that must both reach disk and unblock the cell while the cell is
    /// blocked. It does because the persist runs first and the reconfigure
    /// never reads the cell it is replacing.
    #[tokio::test]
    async fn disabling_a_blocked_proxy_saves_and_unblocks_the_cell() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        let http = direct_cell();

        let bad = enabled(ProxyType::Socks5, "no-such-host.invalid", 3128);
        persist_then_reconfigure(&bad, |s| save_json(&file, s), http.clone(), caps())
            .await
            .expect_err("this proxy cannot be applied");
        assert!(
            http.client().is_err(),
            "the cell must be blocked for this test to mean anything"
        );

        let mut off = bad.clone();
        off.enabled = false;
        persist_then_reconfigure(&off, |s| save_json(&file, s), http.clone(), caps())
            .await
            .expect("turning the proxy off must succeed from a blocked cell");

        assert!(
            !on_disk(&file).enabled,
            "the next start must read the proxy back as off"
        );
        http.client()
            .expect("disabling the proxy must restore a usable client");
    }

    /// The banner paints any `Ok` green, so "cannot report success" means the
    /// command must not return `Ok` at all for settings that would go direct.
    #[tokio::test]
    async fn a_blocked_plan_can_never_report_a_successful_connection() {
        // SOCKS5 resolves the proxy host at build time, so this blocks before a
        // single byte leaves the process — no network is touched by this test.
        let blocked = enabled(ProxyType::Socks5, "no-such-host.invalid", 3128);
        let err =
            test_proxy_connection_inner(blocked, crate::proxy::HostCaps::assume_all_present())
                .await
                .expect_err("an unresolvable proxy must not report success");
        assert!(
            err.contains("no-such-host.invalid"),
            "the block must name its cause, got: {err}"
        );
    }

    /// `PlanError::BadHost`'s whole message, spelled out rather than derived
    /// from the enum: a test that asked the type what it says would pass
    /// whatever it started saying, including the raw host again.
    const BAD_HOST: &str = "invalid proxy host: enter only a hostname or IP address, \
                            with no scheme, credentials, port or path";

    #[tokio::test]
    async fn settings_that_do_not_form_a_plan_are_refused_not_tested_directly() {
        // Each of these used to reach `build_http_client`, which silently
        // returned a proxy-LESS client — so the request went out direct and the
        // banner said "Connection successful".
        //
        // Asserting the specific refusal, not merely the absence of the word
        // "successful": this function can also return `Ok("… responded with
        // status …")`, which contains neither, so a weaker assertion would pass
        // on a request that actually went out.
        for (bad, expected) in [
            (
                enabled(ProxyType::Http, "127.0.0.1", 0),
                "proxy port must not be 0",
            ),
            (
                enabled(ProxyType::Http, "proxy.local:3128", 3128),
                "enter the host without a port; use the port field",
            ),
            (enabled(ProxyType::Http, "ho st", 3128), BAD_HOST),
            (
                enabled(ProxyType::Http, "[::1]", 3128),
                "enter an IPv6 address without brackets",
            ),
            (enabled(ProxyType::Http, "", 3128), BAD_HOST),
            // The whole reason `BAD_HOST` says nothing about what was typed:
            // this is a pasted SOCKS5 URI with a password in it, which is a
            // shape users really do paste into a host field, and the refusal
            // is rendered in a toast and written to the log.
            (
                enabled(ProxyType::Http, "socks5://user:secret@proxy.example", 3128),
                BAD_HOST,
            ),
            (
                enabled(ProxyType::Http, "пример.рф", 3128),
                "proxy host must be ASCII",
            ),
        ] {
            let err = test_proxy_connection_inner(
                bad.clone(),
                crate::proxy::HostCaps::assume_all_present(),
            )
            .await
            .expect_err("unplannable settings must never reach the network");
            assert_eq!(err, expected, "{bad:?} must be refused with its own reason");
            // No refusal echoes the host field back. It reaches a toast and the
            // log, and the field is where a pasted `scheme://user:pass@host`
            // lands, so echoing it publishes the password.
            assert!(
                bad.host.is_empty() || !err.contains(&bad.host),
                "the refusal for {bad:?} echoes the host field verbatim: {err}"
            );
            assert!(
                proxy_test_client(&bad, &caps()).is_err(),
                "unplannable settings {bad:?} must yield no client"
            );
        }
    }

    #[tokio::test]
    async fn a_disabled_proxy_is_refused_rather_than_tested_as_a_direct_connection() {
        // `plan()` maps a disabled proxy to `Direct`, whose client works fine.
        // Sending through it would always succeed and paint the banner green
        // for a connection that is not proxied at all.
        let mut off = enabled(ProxyType::Http, "127.0.0.1", 3128);
        off.enabled = false;
        assert!(matches!(
            crate::proxy::plan(&off, &caps()),
            Ok(crate::proxy::ProxyPlan::Direct)
        ));
        let err = test_proxy_connection_inner(off, crate::proxy::HostCaps::assume_all_present())
            .await
            .expect_err("a direct connection must never be reported as a working proxy");
        assert!(err.contains("No proxy configured"), "got: {err}");
    }

    #[test]
    fn a_usable_proxy_still_yields_a_client_to_test_with() {
        // The guard above must not have closed off the case the button exists
        // for. Built only — never sent, so no network.
        let good = enabled(ProxyType::Http, "127.0.0.1", 3128);
        let c = proxy_test_client(&good, &caps()).expect("a valid proxy must be testable");
        assert!(
            format!("{c:?}").contains("All(http://127.0.0.1:3128)"),
            "the test must go through the proxy it is testing: {c:?}"
        );
    }

    /// A proxy on loopback whose health a test can flip mid-life.
    ///
    /// Unhealthy means "accepts and says nothing", which is what a proxy that
    /// is not answering looks like to reqwest. The point is that the same
    /// address recovers without the settings being touched — the scenario the
    /// probe exists for, where the user fixes the proxy outside SONE and
    /// nothing in the app would otherwise ever notice.
    async fn controllable_proxy() -> (
        std::net::SocketAddr,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let healthy = Arc::new(AtomicBool::new(false));
        let flag = healthy.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = l.accept().await {
                let answer = flag.load(Ordering::SeqCst);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let _ = sock.read(&mut [0u8; 4096]).await;
                    if answer {
                        let _ = sock
                            .write_all(
                                b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                            )
                            .await;
                    }
                    let _ = sock.shutdown().await;
                });
            }
        });
        (addr, healthy)
    }

    /// The bug this command exists to close, end to end.
    ///
    /// The count behind `unreachable` is cleared only by an answered request,
    /// and once the banner is up the app is making none — so a proxy repaired
    /// externally left the bar standing forever. The probe is the request that
    /// was missing, and it has to work in both directions: raise the state
    /// when the proxy is silent, clear it the moment one answer comes back,
    /// with nothing saved and no client rebuilt in between.
    ///
    /// `http://` rather than `https://` so the stand-in sees a proxied request
    /// in absolute form instead of a CONNECT tunnel it would have to terminate
    /// with TLS. The host never resolves (RFC 6761) — the proxy is the only
    /// thing that could carry this.
    #[tokio::test]
    async fn the_probe_raises_the_unreachable_state_and_then_clears_it() {
        let (addr, healthy) = controllable_proxy().await;
        let settings = enabled(ProxyType::Http, "127.0.0.1", addr.port());
        let plan = crate::proxy::plan(&settings, &caps()).unwrap();
        let cell = crate::proxy_http::ProxiedHttp::from_plan(&plan, &caps());
        let url = "http://api.example.invalid/v1/ping";

        probe_proxy_reachability_inner(cell.clone(), url).await;
        assert!(
            !cell.unreachable(),
            "one unanswered request is a blip, not a verdict"
        );
        probe_proxy_reachability_inner(cell.clone(), url).await;
        assert!(
            cell.unreachable(),
            "two in a row is what the banner is allowed to report"
        );

        // The proxy comes back. Nothing is saved, nothing is rebuilt, and no
        // other request is made — this probe is the only thing that can notice.
        healthy.store(true, Ordering::SeqCst);
        probe_proxy_reachability_inner(cell.clone(), url).await;
        assert!(
            !cell.unreachable(),
            "an answered request must clear the count, or the banner is stuck"
        );
    }

    /// A blocked cell has no client, so there is nothing to send through and
    /// nothing `unanswered` could mean. The banner reports that state from the
    /// block reason instead, and probing it would put a packet on the wire for
    /// a question already answered.
    #[tokio::test]
    async fn a_blocked_cell_is_never_probed() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let dialed = Arc::new(AtomicBool::new(false));
        let witness = dialed.clone();
        tokio::spawn(async move {
            if l.accept().await.is_ok() {
                witness.store(true, Ordering::SeqCst);
            }
        });

        let cell = crate::proxy_http::ProxiedHttp::blocked("the settings form no plan");
        probe_proxy_reachability_inner(cell.clone(), &format!("http://{addr}/v1/ping")).await;
        tokio::task::yield_now().await;

        assert!(
            !dialed.load(Ordering::SeqCst),
            "a blocked cell must not reach the network"
        );
        assert!(
            !cell.unreachable(),
            "and a cell that sent nothing has observed nothing"
        );
    }
}
