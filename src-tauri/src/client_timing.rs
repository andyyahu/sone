//! Separate command queue time from work performed with the shared API client.
//! Labels must be static operation names, never URLs, queries or account data.

use std::ops::{Deref, DerefMut};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, MutexGuard};

pub struct ClientGuard<'a, T> {
    guard: MutexGuard<'a, T>,
    operation: &'static str,
    waited: Duration,
    acquired: Instant,
}

pub async fn lock<'a, T>(client: &'a Mutex<T>, operation: &'static str) -> ClientGuard<'a, T> {
    let start = Instant::now();
    let guard = client.lock().await;
    ClientGuard {
        guard,
        operation,
        waited: start.elapsed(),
        acquired: Instant::now(),
    }
}

impl<T> Deref for ClientGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for ClientGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for ClientGuard<'_, T> {
    fn drop(&mut self) {
        log::debug!(
            "[client timing] operation={} queue_ms={:.2} held_ms={:.2}",
            self.operation,
            self.waited.as_secs_f64() * 1000.0,
            self.acquired.elapsed().as_secs_f64() * 1000.0
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn guard_preserves_mutation_and_releases_the_underlying_lock() {
        let client = Mutex::new(1);
        let mut held = lock(&client, "test").await;
        *held += 1;
        assert!(client.try_lock().is_err());
        drop(held);
        assert_eq!(*client.try_lock().unwrap(), 2);
    }
}
