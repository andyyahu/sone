use serde::Serialize;
use tauri::State;

use crate::error::SoneError;
use crate::overlay::OverlayTrackState;
use crate::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlayConnectionInfo {
    pub enabled: bool,
    pub url: Option<String>,
    pub port: Option<u16>,
    pub host: String,
}

#[tauri::command]
pub async fn overlay_get_connection_info(
    state: State<'_, AppState>,
) -> Result<OverlayConnectionInfo, SoneError> {
    let handle = state.overlay_handle.lock().await;
    let settings_host = state
        .load_settings()
        .map(|s| s.overlay_host)
        .unwrap_or_else(|| "127.0.0.1".to_string());
    Ok(match handle.as_ref() {
        Some(h) => OverlayConnectionInfo {
            enabled: true,
            url: Some(h.url()),
            port: Some(h.port),
            host: h.host.clone(),
        },
        None => OverlayConnectionInfo {
            enabled: false,
            url: None,
            port: None,
            host: settings_host,
        },
    })
}

#[tauri::command(rename_all = "camelCase")]
pub async fn overlay_publish_state(
    state: State<'_, AppState>,
    track: Option<OverlayTrackState>,
) -> Result<(), SoneError> {
    let mut s = state.overlay_state.write().await;
    s.track = track.clone();

    // Broadcast to all SSE listeners
    if let Some(t) = track {
        let json = serde_json::to_string(&t).unwrap_or_default();
        let _ = s.tx.send(json);
    } else {
        // Send empty state so overlay hides itself
        let empty = serde_json::to_string(&OverlayTrackState::default()).unwrap_or_default();
        let _ = s.tx.send(empty);
    }

    Ok(())
}

/// Stop the running server (if any) and wait for the port to be released.
async fn stop_server(state: &AppState) {
    let mut guard = state.overlay_handle.lock().await;
    if let Some(handle) = guard.take() {
        handle.shutdown().await;
    }
}

/// Stop the running server (if any), then start one with `settings`.
/// With `only_if_running`, no-op unless a server was running. Holds the
/// handle lock across stop→bind→store so concurrent callers cannot
/// double-bind. On failure the old server is already gone — callers decide
/// whether to roll back.
async fn restart_server(
    state: &AppState,
    settings: &crate::Settings,
    only_if_running: bool,
) -> Result<bool, SoneError> {
    let mut guard = state.overlay_handle.lock().await;
    if only_if_running && guard.is_none() {
        return Ok(false);
    }
    if let Some(handle) = guard.take() {
        handle.shutdown().await;
    }
    let tx = {
        let s = state.overlay_state.read().await;
        s.tx.clone()
    };
    let h = crate::overlay::start_server(
        state.overlay_state.clone(),
        tx,
        &settings.overlay_host,
        settings.overlay_port,
    )
    .await?;
    *guard = Some(h);
    Ok(true)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn overlay_set_enabled(
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<OverlayConnectionInfo, SoneError> {
    let _settings_guard = state.overlay_settings_lock.lock().await;
    let old_settings = state.settings_store.snapshot()?;
    let mut settings = old_settings.clone();
    settings.overlay_enabled = enabled;

    if enabled {
        // Start first — only persist enabled=true if it worked, so a bad
        // config never sticks across launches.
        restart_server(&state, &settings, false).await?;
    } else {
        stop_server(&state).await;
    }
    if let Err(error) = state.update_settings(|current| {
        current.overlay_enabled = settings.overlay_enabled;
        Ok(())
    }) {
        if old_settings.overlay_enabled {
            let _ = restart_server(&state, &old_settings, false).await;
        } else {
            stop_server(&state).await;
        }
        return Err(error);
    }
    drop(_settings_guard);

    overlay_get_connection_info(state).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn overlay_set_port(
    state: State<'_, AppState>,
    port: u16,
) -> Result<OverlayConnectionInfo, SoneError> {
    if port < 1024 {
        return Err(SoneError::Io(
            "Port must be between 1024 and 65535".to_string(),
        ));
    }

    let _settings_guard = state.overlay_settings_lock.lock().await;
    let old_settings = state.settings_store.snapshot()?;
    let mut settings = old_settings.clone();
    settings.overlay_port = port;

    if let Err(e) = restart_server(&state, &settings, true).await {
        // The old server is already stopped — best-effort rollback so a
        // working server isn't left dead.
        let _ = restart_server(&state, &old_settings, false).await;
        return Err(e);
    }
    if let Err(error) = state.update_settings(|current| {
        current.overlay_port = settings.overlay_port;
        Ok(())
    }) {
        if old_settings.overlay_enabled {
            let _ = restart_server(&state, &old_settings, false).await;
        } else {
            stop_server(&state).await;
        }
        return Err(error);
    }
    drop(_settings_guard);

    overlay_get_connection_info(state).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn overlay_set_host(
    state: State<'_, AppState>,
    host: String,
) -> Result<OverlayConnectionInfo, SoneError> {
    let trimmed = host.trim().to_string();
    if trimmed.is_empty() {
        return Err(SoneError::Io("Host cannot be empty".to_string()));
    }
    // IP literals only; IPv6 must be bracketed, e.g. "[::1]".
    let test = format!("{trimmed}:0");
    if test.parse::<std::net::SocketAddr>().is_err() {
        return Err(SoneError::Io(format!("Invalid host address: {trimmed}")));
    }

    let _settings_guard = state.overlay_settings_lock.lock().await;
    let old_settings = state.settings_store.snapshot()?;
    let mut settings = old_settings.clone();
    settings.overlay_host = trimmed;

    if let Err(e) = restart_server(&state, &settings, true).await {
        let _ = restart_server(&state, &old_settings, false).await;
        return Err(e);
    }
    if let Err(error) = state.update_settings(|current| {
        current.overlay_host = settings.overlay_host;
        Ok(())
    }) {
        if old_settings.overlay_enabled {
            let _ = restart_server(&state, &old_settings, false).await;
        } else {
            stop_server(&state).await;
        }
        return Err(error);
    }
    drop(_settings_guard);

    overlay_get_connection_info(state).await
}

/// Receive the full CSS variable block from the frontend (already computed by
/// `themeToCssVars`) and store it so `/overlay/theme.css` serves it, then
/// notify connected overlays via SSE so they hot-reload the stylesheet.
#[tauri::command(rename_all = "camelCase")]
pub async fn overlay_publish_theme(
    state: State<'_, AppState>,
    css: String,
) -> Result<(), SoneError> {
    let mut s = state.overlay_state.write().await;
    s.theme_css = css;
    // Signal all connected overlays to reload theme.css
    let _ = s.theme_tx.send("reload".to_string());
    Ok(())
}
