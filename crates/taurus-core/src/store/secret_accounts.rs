//! The accounts Taurus wrote to the secret store (settings key
//! `secret_accounts`, a JSON array of strings), so every one of them can be
//! forgotten; and the jobs that sign in.

use crate::errors::{CoreError, ErrorKind};

use super::{JobId, Store, db_err};

const SECRET_ACCOUNTS: &str = "secret_accounts";

impl Store {
    /// The accounts recorded, in the order first recorded; none if the list
    /// was never made.
    pub fn secret_accounts(&self) -> Result<Vec<String>, CoreError> {
        match self.setting(SECRET_ACCOUNTS)? {
            None => Ok(Vec::new()),
            Some(json) => serde_json::from_str(&json).map_err(|e| CoreError {
                kind: ErrorKind::Unknown,
                detail: format!("the list of saved passwords is unreadable: {e}"),
            }),
        }
    }

    /// Records `account`, once.
    pub fn add_secret_account(&self, account: &str) -> Result<(), CoreError> {
        let mut accounts = self.secret_accounts()?;
        if accounts.iter().any(|a| a == account) {
            return Ok(());
        }
        accounts.push(account.to_owned());
        self.set_secret_accounts(&accounts)
    }

    /// Forgets `account`, if it is recorded.
    pub fn remove_secret_account(&self, account: &str) -> Result<(), CoreError> {
        let mut accounts = self.secret_accounts()?;
        let before = accounts.len();
        accounts.retain(|a| a != account);
        if accounts.len() == before {
            return Ok(());
        }
        self.set_secret_accounts(&accounts)
    }

    /// Replaces the list.
    pub fn set_secret_accounts(&self, accounts: &[String]) -> Result<(), CoreError> {
        let json = serde_json::to_string(accounts).map_err(|e| CoreError {
            kind: ErrorKind::Unknown,
            detail: format!("could not write the list of saved passwords: {e}"),
        })?;
        self.put_setting(SECRET_ACCOUNTS, &json)
    }

    /// Makes the list with `accounts` if there is none yet (a database of
    /// a version that kept no list); an existing list is left as it is.
    pub fn init_secret_accounts(&self, accounts: &[String]) -> Result<(), CoreError> {
        if self.setting(SECRET_ACCOUNTS)?.is_some() {
            return Ok(());
        }
        self.set_secret_accounts(accounts)
    }

    /// The jobs that have a user name to sign in with, by id.
    pub fn signed_in_jobs(&self) -> Result<Vec<JobId>, CoreError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT id FROM jobs WHERE auth_user IS NOT NULL ORDER BY id")
            .map_err(db_err)?;
        stmt.query_map([], |row| row.get(0).map(JobId))
            .map_err(db_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_err)
    }
}

#[cfg(test)]
mod tests {
    use crate::store::test_support::{new_job, open_temp};

    #[test]
    fn accounts_are_kept_once_in_order() {
        let (_dir, store) = open_temp();
        assert!(store.secret_accounts().unwrap().is_empty());
        store.add_secret_account("job:2:t").unwrap();
        store.add_secret_account("job:1:t").unwrap();
        store.add_secret_account("job:2:t").unwrap();
        assert_eq!(store.secret_accounts().unwrap(), ["job:2:t", "job:1:t"]);
        store.remove_secret_account("job:2:t").unwrap();
        store.remove_secret_account("job:9:t").unwrap();
        assert_eq!(store.secret_accounts().unwrap(), ["job:1:t"]);
        // An existing list, even an empty one, is not made again.
        store.init_secret_accounts(&["job:5:t".into()]).unwrap();
        assert_eq!(store.secret_accounts().unwrap(), ["job:1:t"]);
        store.set_secret_accounts(&[]).unwrap();
        store.init_secret_accounts(&["job:5:t".into()]).unwrap();
        assert!(store.secret_accounts().unwrap().is_empty());
    }

    #[test]
    fn a_missing_list_is_made_once() {
        let (_dir, store) = open_temp();
        store.init_secret_accounts(&["job:5:t".into()]).unwrap();
        assert_eq!(store.secret_accounts().unwrap(), ["job:5:t"]);
    }

    #[test]
    fn jobs_that_sign_in() {
        let (_dir, store) = open_temp();
        let a = store.insert_job(&new_job("a.bin", "/d")).unwrap();
        let b = store.insert_job(&new_job("b.bin", "/d")).unwrap();
        assert!(store.signed_in_jobs().unwrap().is_empty());
        store.set_auth_user(b, Some("ann")).unwrap();
        assert_eq!(store.signed_in_jobs().unwrap(), [b]);
        store.set_auth_user(a, Some("bob")).unwrap();
        assert_eq!(store.signed_in_jobs().unwrap(), [a, b]);
    }
}
