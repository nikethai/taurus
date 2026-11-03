//! Where the engine keeps the jobs' passwords: `EngineConfig::secrets`
//! (the OS keychain by default), written in order off the async threads,
//! with memory standing in while a write is under way or when the store
//! fails.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::secrets::SecretStore;
use crate::store::JobId;

/// How long `flush` waits for the store: a locked keyring may wait on an
/// unlock prompt nobody answers, and must not hang `shutdown`, `add`,
/// `remove` or `set_credentials`.
const FLUSH_LIMIT: Duration = Duration::from_secs(10);

/// Passwords not (yet) in the store, by account: `(write number, password)`.
type Pending = Arc<Mutex<HashMap<String, (u64, String)>>>;

/// The engine's passwords. Cheap to clone; clones share one writer.
///
/// The account of job `id` is `job:<id>:<database token>`: job ids are
/// unique only within one database, the keychain is shared by all of them.
///
/// `keep` and `forget` return at once: a password is readable (`password`)
/// from the moment it is kept, and the store's writes and deletes run one
/// after the other in a blocking thread, in the order asked. If the store
/// fails to take a password (no keychain), it stays in memory until the
/// engine stops (`clear_memory`), with a warning that does not hold it: a
/// job then asks for it again after a restart. A failed delete only warns.
#[derive(Clone)]
pub(super) struct Secrets {
    store: Arc<dyn SecretStore>,
    token: Arc<str>,
    pending: Pending,
    next_write: Arc<AtomicU64>,
    writes: mpsc::UnboundedSender<Write>,
}

enum Write {
    Set {
        id: JobId,
        account: String,
        password: String,
        number: u64,
    },
    Delete {
        id: JobId,
        account: String,
    },
    /// Answered once every earlier write is done.
    Flush(oneshot::Sender<()>),
}

impl Secrets {
    /// `token` is the database's own (see `Store::database_token`). Starts
    /// the writer, so it must run in a Tokio runtime.
    pub(super) fn new(store: Arc<dyn SecretStore>, token: &str) -> Secrets {
        let (writes, queue) = mpsc::unbounded_channel();
        let pending = Pending::default();
        tokio::spawn(write_in_order(store.clone(), pending.clone(), queue));
        Secrets {
            store,
            token: token.into(),
            pending,
            next_write: Arc::new(AtomicU64::new(0)),
            writes,
        }
    }

    fn account(&self, id: JobId) -> String {
        format!("job:{}:{}", id.0, self.token)
    }

    /// Keeps the job's password, replacing any earlier one.
    pub(super) fn keep(&self, id: JobId, password: &str) {
        let account = self.account(id);
        let number = self.next_write.fetch_add(1, Ordering::Relaxed);
        lock(&self.pending).insert(account.clone(), (number, password.to_owned()));
        let _ = self.writes.send(Write::Set {
            id,
            account,
            password: password.to_owned(),
            number,
        });
    }

    /// Deletes the job's password. Until the store's delete has run,
    /// `password` may still find it there: only a removed job is forgotten,
    /// and nothing reads its password (a job reusing the id reads one only
    /// once it has a user name, which keeps a new password first).
    pub(super) fn forget(&self, id: JobId) {
        let account = self.account(id);
        lock(&self.pending).remove(&account);
        let _ = self.writes.send(Write::Delete { id, account });
    }

    /// Returns once every write asked for so far is done, or after
    /// `FLUSH_LIMIT` (with a warning); the writes go on either way.
    pub(super) async fn flush(&self) {
        self.flush_within(FLUSH_LIMIT).await;
    }

    /// `flush`, waiting at most `limit`; whether every write was done.
    async fn flush_within(&self, limit: Duration) -> bool {
        let (done, wait) = oneshot::channel();
        if self.writes.send(Write::Flush(done)).is_err() {
            return true;
        }
        match tokio::time::timeout(limit, wait).await {
            Ok(_) => true,
            Err(_) => {
                tracing::warn!(
                    "the keychain has not answered in {limit:?}; going on without waiting for it"
                );
                false
            }
        }
    }

    /// Forgets the passwords held in memory (those the store has not
    /// taken), as the engine stops.
    pub(super) fn clear_memory(&self) {
        lock(&self.pending).clear();
    }

    /// The job's password, if one is kept. Blocks for the store.
    pub(super) fn password(&self, id: JobId) -> Option<String> {
        let account = self.account(id);
        if let Some((_, password)) = lock(&self.pending).get(&account) {
            return Some(password.clone());
        }
        self.store.get(&account).unwrap_or_else(|e| {
            tracing::warn!(job = id.0, "could not read the password: {e}");
            None
        })
    }
}

/// Runs the writes one at a time, each in a blocking thread, until every
/// `Secrets` is dropped.
async fn write_in_order(
    store: Arc<dyn SecretStore>,
    pending: Pending,
    mut queue: mpsc::UnboundedReceiver<Write>,
) {
    while let Some(write) = queue.recv().await {
        if let Write::Flush(done) = write {
            let _ = done.send(());
            continue;
        }
        let (store, pending) = (store.clone(), pending.clone());
        let _ = tokio::task::spawn_blocking(move || apply(&*store, &pending, write)).await;
    }
}

fn apply(store: &dyn SecretStore, pending: &Pending, write: Write) {
    match write {
        Write::Set {
            id,
            account,
            password,
            number,
        } => match store.set(&account, &password) {
            Ok(()) => {
                let mut pending = lock(pending);
                // Unless a newer password is on its way.
                if pending.get(&account).is_some_and(|(n, _)| *n == number) {
                    pending.remove(&account);
                }
            }
            Err(e) => tracing::warn!(
                job = id.0,
                "keeping the password in memory until the engine stops: {e}"
            ),
        },
        Write::Delete { id, account } => {
            if let Err(e) = store.delete(&account) {
                tracing::warn!(job = id.0, "could not delete the password: {e}");
            }
        }
        Write::Flush(done) => {
            let _ = done.send(());
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing panics while holding the lock, so the map is still whole.
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::{CoreError, ErrorKind};
    use crate::secrets::MemoryStore;

    /// A keychain that is not there.
    struct NoKeychain;

    fn absent() -> CoreError {
        CoreError {
            kind: ErrorKind::Unknown,
            detail: "no keychain".into(),
        }
    }

    impl SecretStore for NoKeychain {
        fn set(&self, _: &str, _: &str) -> Result<(), CoreError> {
            Err(absent())
        }
        fn get(&self, _: &str) -> Result<Option<String>, CoreError> {
            Err(absent())
        }
        fn delete(&self, _: &str) -> Result<(), CoreError> {
            Err(absent())
        }
    }

    #[tokio::test]
    async fn without_a_keychain_passwords_live_in_memory() {
        let secrets = Secrets::new(Arc::new(NoKeychain), "t0");
        assert_eq!(secrets.password(JobId(1)), None);
        secrets.keep(JobId(1), "pw");
        secrets.flush().await;
        assert_eq!(secrets.password(JobId(1)).as_deref(), Some("pw"));
        assert_eq!(secrets.password(JobId(2)), None);
        secrets.forget(JobId(1));
        secrets.flush().await;
        assert_eq!(secrets.password(JobId(1)), None);
    }

    #[tokio::test]
    async fn a_working_store_holds_the_password_in_order() {
        let store = Arc::new(MemoryStore::new());
        let secrets = Secrets::new(store.clone(), "abc");
        secrets.keep(JobId(7), "old");
        // Readable before the store has it.
        assert_eq!(secrets.password(JobId(7)).as_deref(), Some("old"));
        secrets.keep(JobId(7), "new");
        secrets.flush().await;
        assert_eq!(store.get("job:7:abc").unwrap().as_deref(), Some("new"));
        assert!(lock(&secrets.pending).is_empty());
        assert_eq!(secrets.password(JobId(7)).as_deref(), Some("new"));
        // A delete asked after a write runs after it.
        secrets.keep(JobId(7), "newer");
        secrets.forget(JobId(7));
        secrets.flush().await;
        assert_eq!(store.get("job:7:abc").unwrap(), None);
        assert_eq!(secrets.password(JobId(7)), None);
    }

    /// A keychain that waits (a locked keyring's unlock prompt nobody
    /// answers) until the test lets it go on.
    struct Stuck(Mutex<std::sync::mpsc::Receiver<()>>);

    impl SecretStore for Stuck {
        fn set(&self, _: &str, _: &str) -> Result<(), CoreError> {
            let _ = lock(&self.0).recv();
            Ok(())
        }
        fn get(&self, _: &str) -> Result<Option<String>, CoreError> {
            Ok(None)
        }
        fn delete(&self, _: &str) -> Result<(), CoreError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_stuck_keychain_holds_a_flush_only_up_to_its_limit() {
        let (go_on, wait) = std::sync::mpsc::channel();
        let secrets = Secrets::new(Arc::new(Stuck(Mutex::new(wait))), "t1");
        secrets.keep(JobId(1), "pw");
        let started = std::time::Instant::now();
        assert!(!secrets.flush_within(Duration::from_millis(200)).await);
        assert!(started.elapsed() < Duration::from_secs(10));
        // The password is still there for the job meanwhile.
        assert_eq!(secrets.password(JobId(1)).as_deref(), Some("pw"));
        go_on.send(()).unwrap();
        assert!(secrets.flush_within(Duration::from_secs(30)).await);
    }

    #[tokio::test]
    async fn passwords_kept_in_memory_are_cleared_at_shutdown() {
        let secrets = Secrets::new(Arc::new(NoKeychain), "t2");
        secrets.keep(JobId(1), "pw");
        secrets.flush().await;
        assert_eq!(secrets.password(JobId(1)).as_deref(), Some("pw"));
        secrets.clear_memory();
        assert_eq!(secrets.password(JobId(1)), None);
        assert!(lock(&secrets.pending).is_empty());
    }

    #[tokio::test]
    async fn databases_do_not_share_accounts() {
        let store = Arc::new(MemoryStore::new());
        let a = Secrets::new(store.clone(), "aaaa");
        let b = Secrets::new(store.clone(), "bbbb");
        a.keep(JobId(1), "for-a");
        b.keep(JobId(1), "for-b");
        a.flush().await;
        b.flush().await;
        assert_eq!(a.password(JobId(1)).as_deref(), Some("for-a"));
        assert_eq!(b.password(JobId(1)).as_deref(), Some("for-b"));
        b.forget(JobId(1));
        b.flush().await;
        assert_eq!(a.password(JobId(1)).as_deref(), Some("for-a"));
    }
}
