use tauri::State;

use crate::cache::{CacheResult, CacheTier};
use crate::error::SoneError;
use crate::tidal_api::FeedResponse;
use crate::AppState;

/// Invalidation tag for every cached feed entry. Intentionally undiscriminated —
/// `mark_feed_seen` drops the whole tag, so it must cover all users' entries.
///
/// Request tickets fence both misses and future refreshes against mark_feed_seen,
/// so a response that started before invalidation cannot resurrect unread state.
const FEED_CACHE_TAG: &str = "feed";

/// Cache key for the activity feed. Scoped per user, matching the
/// convention of every other user-scoped cache in this codebase
/// (`user-playlists:{id}`, `fav-albums:{id}`, …) — account identity must remain explicit even when cached data
/// is being refreshed during an account switch.
fn feed_cache_key(user_id: u64) -> String {
    format!("feed:activities:{}", user_id)
}

/// Fetch the activity feed.
///
/// Uses a plain TTL cache rather than the stale-while-revalidate pattern in
/// `get_page_section`: a background refresh would race the optimistic badge
/// zeroing in `mark_feed_seen` and resurrect the unread dot. At a 15-minute
/// `UserContent` TTL on a handful of rows, SWR buys nothing.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_feed(state: State<'_, AppState>, user_id: u64) -> Result<FeedResponse, SoneError> {
    log::debug!("[get_feed] user_id={}", user_id);

    let fetch_ticket = state.disk_cache.begin_fetch().await;
    let cache_key = feed_cache_key(user_id);
    if let CacheResult::Fresh(bytes) = state
        .disk_cache
        .get(&cache_key, CacheTier::UserContent)
        .await
    {
        if let Ok(feed) = serde_json::from_slice::<FeedResponse>(&bytes) {
            return Ok(feed);
        }
    }

    let mut client = crate::client_timing::lock(&state.tidal_client, "get_feed").await;
    let feed = client.fetch_feed(user_id).await?;
    drop(client);

    if let Ok(json) = serde_json::to_vec(&feed) {
        state
            .disk_cache
            .put_if_current(
                &fetch_ticket,
                &cache_key,
                &json,
                CacheTier::UserContent,
                &[FEED_CACHE_TAG],
            )
            .await
            .ok();
    }

    Ok(feed)
}

/// Mark all feed activities seen, then drop the cached feed so the next
/// `get_feed` refetches instead of replaying a body with a nonzero count.
#[tauri::command(rename_all = "camelCase")]
pub async fn mark_feed_seen(state: State<'_, AppState>, user_id: u64) -> Result<(), SoneError> {
    let client = crate::client_timing::lock(&state.tidal_client, "mark_feed_seen").await;
    let result = client.mark_feed_seen(user_id).await;
    drop(client);

    state.disk_cache.invalidate_tag(FEED_CACHE_TAG).await;

    result
}
