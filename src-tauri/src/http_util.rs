use std::time::Duration;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Spawn an axum server that shuts down gracefully when `cancel` fires.
/// The returned task completes only after the listener is closed and all
/// in-flight connections have drained — await it before rebinding the same
/// address, or the bind races the old socket teardown (EADDRINUSE).
pub fn spawn_with_shutdown(
    listener: TcpListener,
    app: axum::Router,
    cancel: CancellationToken,
    name: &'static str,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(cancel.cancelled_owned())
            .await
        {
            log::error!("{name} server exited with error: {e}");
        }
        log::info!("{name} server shut down");
    })
}

/// Cancel the server and wait for it to drain, bounded. A client dribbling
/// bytes can hold graceful drain open forever — after the timeout, abort:
/// the listener was already dropped at the shutdown signal, so the port is
/// free for rebind either way.
pub async fn shutdown_bounded(cancel: CancellationToken, task: JoinHandle<()>, name: &'static str) {
    cancel.cancel();
    let mut task = task;
    match tokio::time::timeout(Duration::from_secs(3), &mut task).await {
        Ok(Err(e)) => log::error!("{name} server task failed: {e}"),
        Ok(Ok(())) => {}
        Err(_) => {
            log::warn!("{name} server drain timed out; aborting");
            task.abort();
            let _ = (&mut task).await;
        }
    }
}

/// Byte-bounded preview that never cuts through a UTF-8 scalar. Callers keep
/// their existing redaction rules; this helper only controls length.
pub(crate) fn bounded_preview(text: &str, max_bytes: usize) -> &str {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod preview_tests {
    use super::bounded_preview;

    #[test]
    fn every_boundary_of_multilingual_response_is_safe_and_bounded() {
        let text = "API 音樂🎧 café response";
        for limit in 0..=text.len() + 1 {
            let preview = bounded_preview(text, limit);
            assert!(preview.len() <= limit);
            assert!(text.starts_with(preview));
            if let Some(next) = text[preview.len()..].chars().next() {
                assert!(preview.len() + next.len_utf8() > limit);
            }
        }
        assert_eq!(bounded_preview(text, usize::MAX), text);
        assert_eq!(bounded_preview("", 0), "");
    }
}
