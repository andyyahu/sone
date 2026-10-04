use serde::Serialize;
use tauri::State;

use crate::error::SoneError;
use crate::mcp::{NowPlayingSnapshot, QueueTrackSnapshot};
use crate::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpConnectionInfo {
    pub enabled: bool,
    pub url: Option<String>,
    pub port: Option<u16>,
}

#[tauri::command]
pub async fn mcp_get_connection_info(
    state: State<'_, AppState>,
) -> Result<McpConnectionInfo, SoneError> {
    let handle = state.mcp_handle.lock().await;
    Ok(match handle.as_ref() {
        Some(h) => McpConnectionInfo {
            enabled: true,
            url: Some(h.url()),
            port: Some(h.port),
        },
        None => McpConnectionInfo {
            enabled: false,
            url: None,
            port: None,
        },
    })
}

#[tauri::command(rename_all = "camelCase")]
pub async fn mcp_publish_state(
    state: State<'_, AppState>,
    now_playing: Option<NowPlayingSnapshot>,
    queue: Option<Vec<QueueTrackSnapshot>>,
) -> Result<(), SoneError> {
    let mut s = state.mcp_state.write().await;
    if let Some(np) = now_playing {
        s.now_playing = Some(np);
    }
    if let Some(q) = queue {
        s.queue = q;
    }
    Ok(())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn mcp_set_enabled(
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
    enabled: bool,
) -> Result<McpConnectionInfo, SoneError> {
    let mut guard = state.mcp_handle.lock().await;
    let old_settings = state.settings_store.snapshot()?;
    let mut settings = old_settings.clone();

    settings.mcp_enabled = enabled;

    if enabled && settings.mcp_token.is_empty() {
        settings.mcp_token = uuid::Uuid::new_v4().simple().to_string();
    }

    {
        // Hold the guard across stop→bind→store so this cannot race the
        // startup spawn (or a concurrent regenerate) into a double bind.
        if let Some(handle) = guard.take() {
            handle.shutdown().await;
        }
        if enabled {
            let h = crate::mcp::start_server(
                app_handle.clone(),
                settings.mcp_port,
                settings.mcp_token.clone(),
            )
            .await?;
            *guard = Some(h);
        }
    }
    // Persist only after the server matches — a failed enable must not
    // stick across launches.
    let persisted = state.update_settings(|current| {
        current.mcp_enabled = enabled;
        current.mcp_token = settings.mcp_token.clone();
        Ok(())
    });
    if let Err(error) = persisted {
        if let Some(handle) = guard.take() {
            handle.shutdown().await;
        }
        if old_settings.mcp_enabled {
            *guard = crate::mcp::start_server(
                app_handle.clone(),
                old_settings.mcp_port,
                old_settings.mcp_token,
            )
            .await
            .ok();
        }
        return Err(error);
    }
    drop(guard);

    mcp_get_connection_info(state).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn mcp_regenerate_token(
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<McpConnectionInfo, SoneError> {
    let mut guard = state.mcp_handle.lock().await;
    let old_settings = state.settings_store.snapshot()?;
    let mut settings = old_settings.clone();
    settings.mcp_token = uuid::Uuid::new_v4().simple().to_string();

    {
        if let Some(handle) = guard.take() {
            handle.shutdown().await;
        }
        if settings.mcp_enabled {
            match crate::mcp::start_server(
                app_handle.clone(),
                settings.mcp_port,
                settings.mcp_token.clone(),
            )
            .await
            {
                Ok(h) => *guard = Some(h),
                Err(e) => {
                    // Best-effort rollback with the old token so a working
                    // server isn't left dead.
                    if let Ok(h) = crate::mcp::start_server(
                        app_handle.clone(),
                        old_settings.mcp_port,
                        old_settings.mcp_token.clone(),
                    )
                    .await
                    {
                        *guard = Some(h);
                    }
                    return Err(e);
                }
            }
        }
    }
    if let Err(error) = state.update_settings(|current| {
        current.mcp_token = settings.mcp_token;
        Ok(())
    }) {
        if let Some(handle) = guard.take() {
            handle.shutdown().await;
        }
        if old_settings.mcp_enabled {
            *guard = crate::mcp::start_server(
                app_handle.clone(),
                old_settings.mcp_port,
                old_settings.mcp_token,
            )
            .await
            .ok();
        }
        return Err(error);
    }
    drop(guard);

    mcp_get_connection_info(state).await
}
