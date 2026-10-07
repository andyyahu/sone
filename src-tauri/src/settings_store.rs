//! Serialized settings transactions. Never hold this lock across network work.
use crate::{crypto::Crypto, Settings, SoneError};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct SettingsStore {
    path: PathBuf,
    crypto: Arc<Crypto>,
    // A corrupt/unreadable file stays an error. Falling back to defaults here
    // would let the next volume change silently erase credentials and prefs.
    current: Mutex<Result<Settings, String>>,
}

impl SettingsStore {
    pub fn open(path: PathBuf, crypto: Arc<Crypto>) -> Self {
        let current = Self::read(&path, &crypto).map_err(|e| e.to_string());
        if let Err(error) = &current {
            log::error!("Settings could not be loaded; writes disabled: {error}");
        }
        Self {
            path,
            crypto,
            current: Mutex::new(current),
        }
    }

    fn read(path: &Path, crypto: &Crypto) -> Result<Settings, SoneError> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Settings::default())
            }
            Err(error) => return Err(error.into()),
        };
        Ok(serde_json::from_slice(&crypto.decrypt(&bytes)?)?)
    }

    pub fn snapshot(&self) -> Result<Settings, SoneError> {
        self.current
            .lock()
            .map_err(|_| SoneError::Io("settings lock poisoned".into()))?
            .as_ref()
            .cloned()
            .map_err(|error| SoneError::Io(error.clone()))
    }

    /// Read, modify, encrypt and publish as one transaction. Only modified
    /// fields should be assigned; never replace this value with a stale copy.
    pub fn update<R>(
        &self,
        edit: impl FnOnce(&mut Settings) -> Result<R, SoneError>,
    ) -> Result<R, SoneError> {
        self.update_with_apply(edit, |_| Ok(()))
    }

    /// Lock the store before the runtime gate, and retain both through rollback.
    /// Other transactions may wait for the audio actor while holding the store;
    /// acquiring the playback gate first would prevent that actor from replying.
    pub fn update_with_apply_guarded<R, G>(
        &self,
        acquire: impl FnOnce() -> G,
        edit: impl FnOnce(&mut Settings) -> Result<R, SoneError>,
        apply: impl Fn(&Settings) -> Result<(), SoneError>,
    ) -> Result<R, SoneError> {
        let mut guard = None;
        let result = self.update_with_apply(
            |settings| {
                guard = Some(acquire());
                edit(settings)
            },
            apply,
        );
        drop(guard);
        result
    }

    /// Prepare durable bytes before touching runtime state. Apply and rollback
    /// run under the transaction lock; callbacks must not re-enter this store.
    pub fn update_with_apply<R>(
        &self,
        edit: impl FnOnce(&mut Settings) -> Result<R, SoneError>,
        apply: impl Fn(&Settings) -> Result<(), SoneError>,
    ) -> Result<R, SoneError> {
        let mut current = self
            .current
            .lock()
            .map_err(|_| SoneError::Io("settings lock poisoned".into()))?;
        let mut next = current
            .as_ref()
            .map_err(|error| SoneError::Io(error.clone()))?
            .clone();
        let result = edit(&mut next)?;
        let encrypted = self.crypto.encrypt(&serde_json::to_vec_pretty(&next)?)?;
        let prepared = prepare_write(&self.path, |file| {
            file.write_all(&encrypted)?;
            file.sync_all()
        })?;
        if let Err(error) = apply(&next).and_then(|()| prepared.commit(&self.path)) {
            if let Err(rollback) = apply(current.as_ref().expect("validated above")) {
                return Err(SoneError::Io(format!(
                    "{error}; could not restore runtime settings: {rollback}"
                )));
            }
            return Err(error);
        }
        *current = Ok(next);
        Ok(result)
    }
}

struct PreparedWrite {
    path: PathBuf,
    committed: bool,
}

impl PreparedWrite {
    fn commit(mut self, path: &Path) -> Result<(), SoneError> {
        fs::rename(&self.path, path)?;
        self.committed = true;
        // Rename has committed. An unsupported directory sync must not leave
        // memory/runtime claiming the preceding file is still current.
        if let Some(parent) = path.parent() {
            if let Err(error) = File::open(parent).and_then(|dir| dir.sync_all()) {
                log::warn!("Could not sync settings directory: {error}");
            }
        }
        Ok(())
    }
}

impl Drop for PreparedWrite {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn prepare_write(
    path: &Path,
    write: impl FnOnce(&mut File) -> std::io::Result<()>,
) -> Result<PreparedWrite, SoneError> {
    let parent = path
        .parent()
        .ok_or_else(|| SoneError::Io("settings path has no parent".into()))?;
    let temporary = parent.join(format!(".settings-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // Do not clean up a name we did not create (even a UUID collision must
    // leave another writer's temporary file intact).
    let mut file = options.open(&temporary)?;
    let prepared = PreparedWrite {
        path: temporary,
        committed: false,
    };
    write(&mut file)?;
    Ok(prepared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    fn fixture() -> (tempfile::TempDir, Arc<SettingsStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SettingsStore::open(
            dir.path().join("settings.json"),
            Arc::new(Crypto::for_tests()),
        ));
        (dir, store)
    }

    #[test]
    fn waiting_output_transaction_does_not_block_the_current_settings_actor() {
        use std::sync::mpsc;
        use std::time::Duration;
        let (_dir, store) = fixture();
        let gate = Arc::new(Mutex::new(()));
        let (in_apply, apply_started) = mpsc::channel();
        let (reply, actor_reply) = mpsc::channel();
        let first_store = store.clone();
        let first = std::thread::spawn(move || {
            first_store.update_with_apply(
                |_| Ok(()),
                |_| {
                    in_apply.send(()).unwrap();
                    actor_reply.recv_timeout(Duration::from_secs(2)).unwrap();
                    Ok(())
                },
            )
        });
        apply_started.recv_timeout(Duration::from_secs(2)).unwrap();
        let next_store = store.clone();
        let next_gate = gate.clone();
        let (attempting, attempted) = mpsc::channel();
        let (acquired, observed) = mpsc::channel();
        let second = std::thread::spawn(move || {
            attempting.send(()).unwrap();
            next_store.update_with_apply_guarded(
                || {
                    let guard = next_gate.lock().unwrap();
                    acquired.send(()).unwrap();
                    guard
                },
                |settings| {
                    settings.bit_perfect = true;
                    Ok(())
                },
                |_| Ok(()),
            )
        });
        attempted.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(observed.recv_timeout(Duration::from_millis(50)).is_err());
        // A PlayUrl already ahead of the first setter's actor command can
        // still take its snapshot, allowing that command to complete.
        drop(
            gate.try_lock()
                .expect("pending setter took the gate too early"),
        );
        reply.send(()).unwrap();
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
        observed.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(store.snapshot().unwrap().bit_perfect);
    }

    #[test]
    fn guarded_reader_only_sees_rollback_after_failed_rename() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::time::Duration;
        let (dir, store) = fixture();
        fs::create_dir(dir.path().join("settings.json")).unwrap();
        let gate = Arc::new(Mutex::new(()));
        let runtime = Arc::new(AtomicBool::new(false));
        let (applied, apply_seen) = mpsc::channel();
        let (proceed, continue_commit) = mpsc::channel();
        let writer_gate = gate.clone();
        let writer_runtime = runtime.clone();
        let writer = std::thread::spawn(move || {
            store.update_with_apply_guarded(
                || writer_gate.lock().unwrap(),
                |settings| {
                    settings.bit_perfect = true;
                    Ok(())
                },
                |settings| {
                    writer_runtime.store(settings.bit_perfect, Ordering::SeqCst);
                    if settings.bit_perfect {
                        applied.send(()).unwrap();
                        continue_commit
                            .recv_timeout(Duration::from_secs(2))
                            .unwrap();
                    }
                    Ok(())
                },
            )
        });
        apply_seen.recv_timeout(Duration::from_secs(2)).unwrap();
        let (read, read_result) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let _guard = gate.lock().unwrap();
            read.send(runtime.load(Ordering::SeqCst)).unwrap();
        });
        assert!(read_result.recv_timeout(Duration::from_millis(50)).is_err());
        proceed.send(()).unwrap();
        assert!(writer.join().unwrap().is_err());
        assert!(!read_result.recv_timeout(Duration::from_secs(2)).unwrap());
        reader.join().unwrap();
    }

    #[test]
    fn concurrent_fields_and_increments_survive_and_reload_encrypted() {
        let (dir, store) = fixture();
        let barrier = Arc::new(Barrier::new(3));
        let workers: Vec<_> = [false, true]
            .into_iter()
            .map(|auth| {
                let store = store.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..10 {
                        store
                            .update(|settings| {
                                settings.legacy_auth_notice_count += 1;
                                if auth {
                                    settings.auth_tokens = Some(crate::tidal_api::AuthTokens {
                                        access_token: "refreshed".into(),
                                        refresh_token: "refresh".into(),
                                        expires_in: 3600,
                                        token_type: "Bearer".into(),
                                        user_id: Some(42),
                                    });
                                } else {
                                    settings.volume = 0.25;
                                }
                                Ok(())
                            })
                            .unwrap();
                    }
                })
            })
            .collect();
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        let path = dir.path().join("settings.json");
        assert!(crate::crypto::is_encrypted(&fs::read(&path).unwrap()));
        let loaded = SettingsStore::open(path, Arc::new(Crypto::for_tests()))
            .snapshot()
            .unwrap();
        assert_eq!(loaded.legacy_auth_notice_count, 20);
        assert_eq!(loaded.volume, 0.25);
        assert_eq!(loaded.auth_tokens.unwrap().access_token, "refreshed");
    }

    #[test]
    fn legacy_plaintext_is_read_and_transactionally_migrated() {
        let (dir, _) = fixture();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            br#"{"auth_tokens":null,"last_track_id":17,"volume":0.4}"#,
        )
        .unwrap();
        let store = SettingsStore::open(path.clone(), Arc::new(Crypto::for_tests()));
        assert_eq!(store.snapshot().unwrap().last_track_id, Some(17));
        store
            .update(|s| {
                s.gapless = false;
                Ok(())
            })
            .unwrap();
        assert!(crate::crypto::is_encrypted(&fs::read(&path).unwrap()));
        let loaded = SettingsStore::open(path, Arc::new(Crypto::for_tests()))
            .snapshot()
            .unwrap();
        assert_eq!(loaded.volume, 0.4);
        assert_eq!(loaded.last_track_id, Some(17));
        assert!(!loaded.gapless);
    }

    #[test]
    fn rejected_transaction_does_not_modify_disk_or_memory() {
        let (dir, store) = fixture();
        store
            .update(|s| {
                s.volume = 0.25;
                Ok(())
            })
            .unwrap();
        let before = fs::read(dir.path().join("settings.json")).unwrap();
        let result: Result<(), SoneError> = store.update(|s| {
            s.volume = 0.75;
            Err(SoneError::Parse("invalid settings".into()))
        });
        assert!(result.is_err());
        assert_eq!(store.snapshot().unwrap().volume, 0.25);
        assert_eq!(fs::read(dir.path().join("settings.json")).unwrap(), before);
    }

    #[test]
    fn preparation_failure_never_changes_runtime_settings() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        let store =
            SettingsStore::open(parent.join("settings.json"), Arc::new(Crypto::for_tests()));
        fs::write(&parent, b"not a directory").unwrap();
        let result = store.update_with_apply(
            |s| {
                s.bit_perfect = true;
                Ok(())
            },
            |_| panic!("runtime must not change before durable bytes are prepared"),
        );
        assert!(result.is_err());
        assert!(!store.snapshot().unwrap().bit_perfect);
    }

    #[test]
    fn a_failed_runtime_apply_restores_runtime_and_never_publishes() {
        let (dir, store) = fixture();
        store.update(|_| Ok(())).unwrap();
        let before = fs::read(dir.path().join("settings.json")).unwrap();
        let runtime = std::cell::Cell::new(false);
        let result = store.update_with_apply(
            |s| {
                s.bit_perfect = true;
                Ok(())
            },
            |s| {
                runtime.set(s.bit_perfect);
                if s.bit_perfect {
                    Err(SoneError::Audio("injected apply failure".into()))
                } else {
                    Ok(())
                }
            },
        );
        assert!(result.is_err());
        assert!(!runtime.get());
        assert!(!store.snapshot().unwrap().bit_perfect);
        assert_eq!(fs::read(dir.path().join("settings.json")).unwrap(), before);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_failed_rename_restores_runtime_and_removes_the_prepared_file() {
        let (dir, store) = fixture();
        store.update(|_| Ok(())).unwrap();
        let path = dir.path().join("settings.json");
        let before = fs::read(&path).unwrap();
        let backup = dir.path().join("previous.json");
        fs::rename(&path, &backup).unwrap();
        fs::create_dir(&path).unwrap();
        let calls = std::cell::RefCell::new(vec![]);
        let result = store.update_with_apply(
            |s| {
                s.bit_perfect = true;
                Ok(())
            },
            |s| {
                calls.borrow_mut().push(s.bit_perfect);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(*calls.borrow(), [true, false]);
        assert!(!store.snapshot().unwrap().bit_perfect);
        assert_eq!(fs::read(backup).unwrap(), before);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn concurrent_runtime_updates_finish_with_the_same_persisted_value() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (dir, store) = fixture();
        let runtime = Arc::new(AtomicBool::new(false));
        let workers: Vec<_> = (0..8)
            .map(|n| {
                let store = store.clone();
                let runtime = runtime.clone();
                std::thread::spawn(move || {
                    store
                        .update_with_apply(
                            |s| {
                                s.bit_perfect = n % 2 == 0;
                                Ok(())
                            },
                            |s| {
                                runtime.store(s.bit_perfect, Ordering::SeqCst);
                                Ok(())
                            },
                        )
                        .unwrap()
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let stored = SettingsStore::open(
            dir.path().join("settings.json"),
            Arc::new(Crypto::for_tests()),
        );
        assert_eq!(
            stored.snapshot().unwrap().bit_perfect,
            runtime.load(Ordering::SeqCst)
        );
        assert_eq!(
            stored.snapshot().unwrap().bit_perfect,
            store.snapshot().unwrap().bit_perfect
        );
    }

    #[test]
    fn corrupt_file_is_not_overwritten_by_update() {
        let (dir, _) = fixture();
        let path = dir.path().join("settings.json");
        fs::write(&path, b"SONEbroken").unwrap();
        let store = SettingsStore::open(path.clone(), Arc::new(Crypto::for_tests()));
        assert!(store.snapshot().is_err());
        assert!(store
            .update(|s| {
                s.volume = 0.5;
                Ok(())
            })
            .is_err());
        assert_eq!(fs::read(path).unwrap(), b"SONEbroken");
    }

    #[test]
    fn output_rollback_restores_native_choice_without_touching_active_track() {
        use crate::audio_output::{AudioOutputConfig, AudioOutputRoute};
        let (dir, store) = fixture();
        store
            .update(|s| {
                s.bit_perfect = true;
                s.exclusive_mode = true;
                s.volume = 0.3;
                Ok(())
            })
            .unwrap();
        let original = AudioOutputConfig::from_settings(&store.snapshot().unwrap());
        let active = original.effective();
        let configured = std::cell::RefCell::new(original.clone());
        let mut next = original.clone();
        next.route = AudioOutputRoute::Hqplayer;
        let before = fs::read(dir.path().join("settings.json")).unwrap();
        let result = store.update_with_apply(
            |settings| {
                next.write_settings(settings);
                Ok(())
            },
            |settings| {
                let config = AudioOutputConfig::from_settings(settings);
                *configured.borrow_mut() = config.clone();
                if config.route == AudioOutputRoute::Hqplayer {
                    Err(SoneError::Audio("injected configuration failure".into()))
                } else {
                    Ok(())
                }
            },
        );
        assert!(result.is_err());
        assert_eq!(*configured.borrow(), original);
        assert_eq!(active.route, AudioOutputRoute::Native);
        assert!(active.bit_perfect);
        assert_eq!(store.snapshot().unwrap().volume, 0.3);
        assert_eq!(fs::read(dir.path().join("settings.json")).unwrap(), before);
    }

    #[test]
    fn legacy_audio_fields_survive_unrelated_settings_updates_and_reload() {
        use crate::audio_output::{AudioOutputConfig, AudioOutputRoute};
        let (dir, _) = fixture();
        let path = dir.path().join("settings.json");
        fs::write(&path, br#"{"auth_tokens":null,"last_track_id":null,"hqplayer":true,"hqplayer_host":"127.0.0.1","hqplayer_port":4322,"bit_perfect":true,"camilla_fir":true,"camilla_config":"room.yml"}"#).unwrap();
        let store = SettingsStore::open(path.clone(), Arc::new(Crypto::for_tests()));
        let config = AudioOutputConfig::from_settings(&store.snapshot().unwrap());
        assert_eq!(config.route, AudioOutputRoute::Hqplayer);
        assert!(config.bit_perfect);
        store
            .update(|s| {
                s.volume = 0.2;
                Ok(())
            })
            .unwrap();
        let restored = SettingsStore::open(path, Arc::new(Crypto::for_tests()));
        assert_eq!(
            AudioOutputConfig::from_settings(&restored.snapshot().unwrap()),
            config
        );
    }

    #[test]
    fn partial_write_failure_preserves_old_file_and_removes_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, b"previous").unwrap();
        let result = prepare_write(&path, |file| {
            file.write_all(b"incomplete")?;
            Err(std::io::Error::other("disk full"))
        });
        assert!(result.is_err());
        assert_eq!(fs::read(path).unwrap(), b"previous");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_commit_does_not_publish_modified_memory() {
        let (dir, store) = fixture();
        store
            .update(|s| {
                s.volume = 0.25;
                Ok(())
            })
            .unwrap();
        let path = dir.path().join("settings.json");
        let previous = fs::read(&path).unwrap();
        fs::rename(&path, dir.path().join("previous.json")).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(store
            .update(|s| {
                s.volume = 0.75;
                Ok(())
            })
            .is_err());
        assert_eq!(store.snapshot().unwrap().volume, 0.25);
        assert_eq!(
            fs::read(dir.path().join("previous.json")).unwrap(),
            previous
        );
    }
}
