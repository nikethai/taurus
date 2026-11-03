//! Where the engine keeps the jobs' passwords: `EngineConfig::secrets`
//! (the OS keychain by default), written in order off the async threads,
//! with memory standing in while a write is under way or when the store
//! fails. Every account written is listed in the database (settings key
//! `secret_accounts`) as it is written, so all of them can be forgotten.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::errors::{CoreError, ErrorKind};
use crate::secrets::SecretStore;
use crate::store::{JobId, Store};

use super::detail::PasswordStore;

/// How long `flush` waits for the store: a locked keyring may wait on an
/// unlock prompt nobody answers, and must not hang `shutdown`, `add`,
/// `remove` or `set_credentials`.
const FLUSH_LIMIT: Duration = Duration::from_secs(10);
/// How long `forget_all` waits for the store to delete every password.
const FORGET_LIMIT: Duration = Duration::from_secs(30);

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
///
/// With a ledger (the engine's database), the writer lists each account
/// there before it writes it, and takes it off once deleted or when the
/// store refused it (memory alone has it then).
#[derive(Clone)]
pub(super) struct Secrets {
    store: Arc<dyn SecretStore>,
    token: Arc<str>,
    pending: Pending,
    next_write: Arc<AtomicU64>,
    writes: mpsc::UnboundedSender<Write>,
    ledger: Option<Store>,
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
    /// Deletes every listed account and `extra`, and the passwords held in
    /// memory since before write `before`; answers how many were removed.
    ForgetAll {
        extra: Vec<String>,
        before: u64,
        reply: oneshot::Sender<u32>,
    },
}

/// What the writer works with.
struct Writer {
    store: Arc<dyn SecretStore>,
    pending: Pending,
    ledger: Option<Store>,
}

impl Secrets {
    /// `token` is the database's own (see `Store::database_token`). With a
    /// `ledger` that has no list of accounts yet (a database of an earlier
    /// version), the list starts with the accounts of the jobs that sign
    /// in. Starts the writer, so it must run in a Tokio runtime.
    pub(super) fn new(store: Arc<dyn SecretStore>, token: &str, ledger: Option<Store>) -> Secrets {
        let (writes, queue) = mpsc::unbounded_channel();
        let pending = Pending::default();
        let writer = Writer {
            store: store.clone(),
            pending: pending.clone(),
            ledger: ledger.clone(),
        };
        tokio::spawn(write_in_order(writer, queue));
        let secrets = Secrets {
            store,
            token: token.into(),
            pending,
            next_write: Arc::new(AtomicU64::new(0)),
            writes,
            ledger,
        };
        if let Some(ledger) = &secrets.ledger {
            let listed = secrets
                .signed_in_accounts(ledger)
                .and_then(|accounts| ledger.init_secret_accounts(&accounts));
            if let Err(e) = listed {
                tracing::warn!("could not list the saved passwords: {e}");
            }
        }
        secrets
    }

    /// The accounts of the jobs that have a user name.
    fn signed_in_accounts(&self, ledger: &Store) -> Result<Vec<String>, CoreError> {
        Ok(ledger
            .signed_in_jobs()?
            .into_iter()
            .map(|id| self.account(id))
            .collect())
    }

    /// Whether a password of the job is kept: in memory, or listed as
    /// written to the store (without reading the store). Without a ledger,
    /// only one in memory counts.
    #[cfg(test)]
    fn is_kept(&self, id: JobId) -> bool {
        self.kept_in(id).is_some()
    }

    /// Where the job's password is kept: in memory while the store has
    /// not taken it (refused, or a write still on its way), else in the
    /// store when listed as written there.
    pub(super) fn kept_in(&self, id: JobId) -> Option<PasswordStore> {
        let account = self.account(id);
        if lock(&self.pending).contains_key(&account) {
            return Some(PasswordStore::Memory);
        }
        let listed = self.ledger.as_ref().is_some_and(|ledger| {
            ledger
                .secret_accounts()
                .is_ok_and(|accounts| accounts.contains(&account))
        });
        listed.then_some(PasswordStore::Keychain)
    }

    /// Deletes every password this database's engine ever wrote (as
    /// listed, plus those of the jobs that sign in, which an earlier
    /// version did not list) and those held in memory, after every write
    /// asked for so far, then empties the list; answers how many it
    /// removed. An account the store fails to delete is only warned about
    /// (the next call finds it again if its job still signs in).
    pub(super) async fn forget_all(&self) -> Result<u32, CoreError> {
        let extra = match &self.ledger {
            Some(ledger) => self.signed_in_accounts(ledger)?,
            None => Vec::new(),
        };
        let before = self.next_write.fetch_add(1, Ordering::Relaxed);
        let (reply, removed) = oneshot::channel();
        let gone = || CoreError {
            kind: ErrorKind::Unknown,
            detail: "the password store stopped".to_string(),
        };
        self.writes
            .send(Write::ForgetAll {
                extra,
                before,
                reply,
            })
            .map_err(|_| gone())?;
        match tokio::time::timeout(FORGET_LIMIT, removed).await {
            Ok(removed) => removed.map_err(|_| gone()),
            Err(_) => Err(CoreError {
                kind: ErrorKind::Timeout,
                detail: "the keychain has not answered".to_string(),
            }),
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
async fn write_in_order(writer: Writer, mut queue: mpsc::UnboundedReceiver<Write>) {
    let writer = Arc::new(writer);
    while let Some(write) = queue.recv().await {
        if let Write::Flush(done) = write {
            let _ = done.send(());
            continue;
        }
        let writer = writer.clone();
        let _ = tokio::task::spawn_blocking(move || writer.apply(write)).await;
    }
}

impl Writer {
    fn apply(&self, write: Write) {
        match write {
            Write::Set {
                id,
                account,
                password,
                number,
            } => {
                // Listed first: a crash right after the write leaves it
                // listed, so it can still be forgotten.
                self.list(&account, true);
                match self.store.set(&account, &password) {
                    Ok(()) => {
                        let mut pending = lock(&self.pending);
                        // Unless a newer password is on its way.
                        if pending.get(&account).is_some_and(|(n, _)| *n == number) {
                            pending.remove(&account);
                        }
                    }
                    Err(e) => {
                        // Not in the store: only memory has it.
                        self.list(&account, false);
                        tracing::warn!(
                            job = id.0,
                            "keeping the password in memory until the engine stops: {e}"
                        );
                    }
                }
            }
            Write::Delete { id, account } => match self.store.delete(&account) {
                Ok(()) => self.list(&account, false),
                Err(e) => tracing::warn!(job = id.0, "could not delete the password: {e}"),
            },
            Write::Flush(done) => {
                let _ = done.send(());
            }
            Write::ForgetAll {
                extra,
                before,
                reply,
            } => {
                let _ = reply.send(self.forget_all(extra, before));
            }
        }
    }

    /// Lists (`listed`) or unlists the account in the ledger, if any.
    fn list(&self, account: &str, listed: bool) {
        let Some(ledger) = &self.ledger else {
            return;
        };
        let done = if listed {
            ledger.add_secret_account(account)
        } else {
            ledger.remove_secret_account(account)
        };
        if let Err(e) = done {
            tracing::warn!("could not update the list of saved passwords: {e}");
        }
    }

    fn forget_all(&self, extra: Vec<String>, before: u64) -> u32 {
        let listed = match &self.ledger {
            Some(ledger) => ledger.secret_accounts().unwrap_or_else(|e| {
                tracing::warn!("{e}");
                Vec::new()
            }),
            None => Vec::new(),
        };
        let mut accounts: Vec<String> = Vec::new();
        for account in listed.into_iter().chain(extra) {
            if !accounts.contains(&account) {
                accounts.push(account);
            }
        }
        let mut removed: HashSet<String> = HashSet::new();
        for account in accounts {
            match self.store.remove(&account) {
                Ok(true) => {
                    removed.insert(account);
                }
                Ok(false) => {}
                // A job that still signs in is found again next time.
                Err(e) => tracing::warn!("could not delete a saved password: {e}"),
            }
        }
        // Held in memory only (the store did not take them), from before
        // this was asked for.
        lock(&self.pending).retain(|account, (number, _)| {
            let older = *number < before;
            if older {
                removed.insert(account.clone());
            }
            !older
        });
        if let Some(ledger) = &self.ledger
            && let Err(e) = ledger.set_secret_accounts(&[])
        {
            tracing::warn!("could not update the list of saved passwords: {e}");
        }
        u32::try_from(removed.len()).unwrap_or(u32::MAX)
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
        let secrets = Secrets::new(Arc::new(NoKeychain), "t0", None);
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
        let secrets = Secrets::new(store.clone(), "abc", None);
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
        let secrets = Secrets::new(Arc::new(Stuck(Mutex::new(wait))), "t1", None);
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
        let secrets = Secrets::new(Arc::new(NoKeychain), "t2", None);
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
        let a = Secrets::new(store.clone(), "aaaa", None);
        let b = Secrets::new(store.clone(), "bbbb", None);
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

    fn ledger() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("taurus.db")).unwrap();
        (dir, store)
    }

    fn signed_in_job(ledger: &Store, name: &str) -> JobId {
        let id = ledger
            .insert_job(&crate::store::NewJob {
                url: format!("http://example.test/{name}"),
                source_url: format!("http://example.test/{name}"),
                dest_dir: "/d".into(),
                file_name: name.into(),
                connections: 1,
                mirrors: Vec::new(),
                checksum: None,
                referer: None,
                queue_pos: 0,
                kind: crate::store::JobKind::Http,
                info_hash: None,
                torrent_meta: None,
                only_files: None,
            })
            .unwrap();
        ledger.set_auth_user(id, Some("ann")).unwrap();
        id
    }

    #[tokio::test]
    async fn written_accounts_are_listed_and_all_are_forgotten() {
        let (_dir, ledger) = ledger();
        let store = Arc::new(MemoryStore::new());
        // An earlier version's password: written, never listed.
        let old = signed_in_job(&ledger, "old.bin");
        store.set(&format!("job:{}:tok", old.0), "old-pw").unwrap();
        let secrets = Secrets::new(store.clone(), "tok", Some(ledger.clone()));
        // The jobs that sign in start the list of a database without one.
        assert_eq!(
            ledger.secret_accounts().unwrap(),
            [format!("job:{}:tok", old.0)]
        );
        assert!(secrets.is_kept(old));

        secrets.keep(JobId(40), "pw-40");
        secrets.keep(JobId(41), "pw-41");
        secrets.flush().await;
        assert_eq!(ledger.secret_accounts().unwrap().len(), 3);
        assert!(secrets.is_kept(JobId(41)));
        assert_eq!(secrets.kept_in(JobId(41)), Some(PasswordStore::Keychain));
        // A deleted password is taken off the list.
        secrets.forget(JobId(41));
        secrets.flush().await;
        assert!(!secrets.is_kept(JobId(41)));
        assert_eq!(ledger.secret_accounts().unwrap().len(), 2);
        // Not listed (the list was lost), but the job signs in: found too.
        ledger
            .set_secret_accounts(&["job:40:tok".to_string()])
            .unwrap();

        assert_eq!(secrets.forget_all().await.unwrap(), 2);
        assert!(store.is_empty());
        assert!(ledger.secret_accounts().unwrap().is_empty());
        assert!(!secrets.is_kept(old) && !secrets.is_kept(JobId(40)));
        assert_eq!(secrets.forget_all().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn forgetting_without_a_keychain_drops_what_memory_holds() {
        let (_dir, ledger) = ledger();
        let secrets = Secrets::new(Arc::new(NoKeychain), "t3", Some(ledger.clone()));
        secrets.keep(JobId(1), "pw");
        secrets.flush().await;
        // In memory only: kept, not listed (gone after a restart).
        assert!(secrets.is_kept(JobId(1)));
        assert_eq!(secrets.kept_in(JobId(1)), Some(PasswordStore::Memory));
        assert_eq!(secrets.kept_in(JobId(2)), None);
        assert!(ledger.secret_accounts().unwrap().is_empty());
        assert_eq!(secrets.forget_all().await.unwrap(), 1);
        assert_eq!(secrets.password(JobId(1)), None);
        assert!(!secrets.is_kept(JobId(1)));
    }

    #[tokio::test]
    async fn forgetting_waits_for_earlier_writes() {
        let store = Arc::new(MemoryStore::new());
        let (_dir, ledger) = ledger();
        let secrets = Secrets::new(store.clone(), "t4", Some(ledger));
        secrets.keep(JobId(1), "pw");
        // No flush: the write is still queued, and runs first.
        assert_eq!(secrets.forget_all().await.unwrap(), 1);
        assert!(store.is_empty());
        assert_eq!(secrets.password(JobId(1)), None);
    }
}
