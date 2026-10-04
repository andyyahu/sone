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
