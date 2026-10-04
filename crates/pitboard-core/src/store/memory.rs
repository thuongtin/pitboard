//! Credential stores that live in memory and fail when they are told to.
//!
//! The interesting half of pitboard is what happens when a step fails partway through, and
//! against a real keychain none of it can be produced: a write cannot be made to fail, an
//! item cannot be made to disappear between two calls, and a read cannot be made to say it
//! could not tell. So the tests that mattered most were the ones that could not be written.
//!
//! This is not a simulation of the keychain. It keeps the one rule every real backend has
//! to keep, that a write reads its own result back and reports anything else, and otherwise
//! does exactly what it is told.

use super::{Backend, Error, Host, RawStore};
use crate::context::Context;
use crate::provider::desktop::types::CookieTable;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// What a store does instead of working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// The write fails outright and nothing is stored, the way a refused keychain write does.
    FailWrite(String),
    /// The write lands, but the store holds something else afterwards. A backend that lies
    /// about what it wrote is the failure `NotDurable` exists for.
    CorruptWrite(String),
    /// Every read says it could not tell. Never the same answer as "nothing is there":
    /// reading a locked keychain as empty is what would tell someone to sign in again.
    Unreadable(String),
    /// The item is gone from this call onwards, as if something else had deleted it.
    Vanish,
    /// Reads work until the first write is attempted; the write fails, and every read after
    /// it says it could not tell. A keychain that locks partway through a change, which is
    /// what a screen lock does, and which cannot be produced by counting calls without
    /// pinning a test to the exact number the code happens to make.
    LocksOnWrite,
    /// The write lands, and the item is gone by the time anything reads it back. Claude
    /// Code's `/logout` deletes the credential with no lock held once it has given up
    /// waiting, which is the one write pitboard cannot exclude.
    DeletedAfterWrite,
    /// The write lands and the store locks before the write can read it back, so the write
    /// says it could not tell, and so does every read after it. The same screen lock as
    /// `LocksOnWrite`, a moment later.
    LocksAfterWrite,
}

/// One store. Plant, peek and enumerate without going through the store's own rules, so a
/// test can set up a machine state and then assert on it.
#[derive(Debug)]
pub struct MemoryStore {
    kind: Backend,
    items: Mutex<HashMap<String, String>>,
    faults: Mutex<HashMap<String, (usize, Fault)>>,
    blanket: Mutex<Option<Fault>>,
    /// Services whose `LocksOnWrite` has fired.
    locked: Mutex<std::collections::HashSet<String>>,
    /// How many bytes a write can take the cheap way, where the store has a ceiling.
    ceiling: Mutex<Option<usize>>,
}

impl MemoryStore {
    /// An empty store that reports itself as `kind`.
    pub fn of(kind: Backend) -> Arc<MemoryStore> {
        Arc::new(MemoryStore {
            kind,
            items: Mutex::new(HashMap::new()),
            faults: Mutex::new(HashMap::new()),
            blanket: Mutex::new(None),
            locked: Mutex::new(std::collections::HashSet::new()),
            ceiling: Mutex::new(None),
        })
    }

    /// From now on a write costs what it would in a keychain whose `security` reads `limit`
    /// bytes from its standard input, with the argument line allowed above that.
    pub fn takes_on_stdin(&self, limit: usize) {
        *self
            .ceiling
            .lock()
            .expect("a poisoned test store is a failed test") = Some(limit);
    }

    /// Put something there without a write, to set a machine up.
    pub fn plant(&self, service: &str, contents: &str) {
        self.items
            .lock()
            .expect("a poisoned test store is a failed test")
            .insert(service.into(), contents.into());
    }

    /// What is there, ignoring any fault. This is the assertion a test makes.
    pub fn peek(&self, service: &str) -> Option<String> {
        self.items
            .lock()
            .expect("a poisoned test store is a failed test")
            .get(service)
            .cloned()
    }

    /// Every service this store holds, which no real vault on macOS can answer.
    pub fn services(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .items
            .lock()
            .expect("a poisoned test store is a failed test")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// From now on, this service misbehaves in this way.
    pub fn fault(&self, service: &str, fault: Fault) {
        self.fault_after(service, 0, fault);
    }

    /// Misbehave only once this service has been touched `calls` more times. A keychain
    /// that locks in the middle of a change is an ordinary thing and cannot be produced
    /// any other way.
    pub fn fault_after(&self, service: &str, calls: usize, fault: Fault) {
        self.faults
            .lock()
            .expect("a poisoned test store is a failed test")
            .insert(service.into(), (calls, fault));
    }

    /// From now on, every service misbehaves in this way, including ones that do not
    /// exist yet. A name reserved at the moment of the write cannot be named in advance.
    pub fn fault_all(&self, fault: Fault) {
        *self
            .blanket
            .lock()
            .expect("a poisoned test store is a failed test") = Some(fault);
    }

    /// Empty the store, as a Claude Code that keeps its login somewhere pitboard has never
    /// heard of would look from here.
    pub fn delete_everything(&self) {
        self.items
            .lock()
            .expect("a poisoned test store is a failed test")
            .clear();
    }

    /// Stop misbehaving everywhere.
    pub fn heal_all(&self) {
        *self
            .blanket
            .lock()
            .expect("a poisoned test store is a failed test") = None;
        self.faults
            .lock()
            .expect("a poisoned test store is a failed test")
            .clear();
        self.locked
            .lock()
            .expect("a poisoned test store is a failed test")
            .clear();
    }

    /// Stop misbehaving.
    pub fn heal(&self, service: &str) {
        self.faults
            .lock()
            .expect("a poisoned test store is a failed test")
            .remove(service);
    }

    fn has_locked(&self, service: &str) -> bool {
        self.locked
            .lock()
            .expect("a poisoned test store is a failed test")
            .contains(service)
    }

    fn lock_now(&self, service: &str) {
        self.locked
            .lock()
            .expect("a poisoned test store is a failed test")
            .insert(service.to_string());
    }

    /// What this service does on this call, counting the call down towards its fault.
    fn fault_for(&self, service: &str) -> Option<Fault> {
        let mut faults = self
            .faults
            .lock()
            .expect("a poisoned test store is a failed test");
        if let Some((countdown, fault)) = faults.get_mut(service) {
            if *countdown == 0 {
                return Some(fault.clone());
            }
            *countdown -= 1;
            return None;
        }
        drop(faults);
        self.blanket
            .lock()
            .expect("a poisoned test store is a failed test")
            .clone()
    }
}

impl RawStore for Arc<MemoryStore> {
    fn kind(&self) -> Backend {
        self.kind
    }

    fn contains(&self, service: &str) -> Result<bool, Error> {
        Ok(self.read(service)?.is_some())
    }

    fn read(&self, service: &str) -> Result<Option<String>, Error> {
        if self.has_locked(service) {
            return Err(Error::Unreadable("the keychain is locked".into()));
        }
        match self.fault_for(service) {
            Some(Fault::Unreadable(why)) => Err(Error::Unreadable(why)),
            Some(Fault::Vanish) => Ok(None),
            _ => Ok(self.peek(service)),
        }
    }

    fn write(&self, service: &str, contents: &str) -> Result<(), Error> {
        if self.has_locked(service) {
            return Err(Error::Write("the keychain is locked".into()));
        }
        match self.fault_for(service) {
            Some(Fault::FailWrite(why)) => return Err(Error::Write(why)),
            Some(Fault::Unreadable(why)) => return Err(Error::Unreadable(why)),
            Some(Fault::LocksOnWrite) => {
                self.lock_now(service);
                return Err(Error::Write("the keychain is locked".into()));
            }
            Some(Fault::LocksAfterWrite) => {
                self.plant(service, contents);
                self.lock_now(service);
                return Err(Error::Unreadable("the keychain is locked".into()));
            }
            Some(Fault::CorruptWrite(instead)) => self.plant(service, &instead),
            Some(Fault::DeletedAfterWrite) => {
                // The write lands and whoever else is writing gets there before the
                // read-back, so the store reports the item as absent, not as unreadable.
                self.items
                    .lock()
                    .expect("a poisoned test store is a failed test")
                    .remove(service);
                self.heal(service);
                return Ok(());
            }
            _ => self.plant(service, contents),
        }
        // The rule every backend keeps: believe the store, not the call that wrote to it.
        match self.peek(service) {
            Some(back) if back == contents => Ok(()),
            _ => Err(Error::NotDurable(format!(
                "{service} holds different bytes"
            ))),
        }
    }

    fn delete(&self, service: &str) -> Result<(), Error> {
        self.items
            .lock()
            .expect("a poisoned test store is a failed test")
            .remove(service);
        Ok(())
    }

    fn list(&self) -> Result<Option<Vec<String>>, Error> {
        Ok(Some(
            self.services()
                .into_iter()
                .filter(|s| crate::park::is_park_name(s))
                .collect(),
        ))
    }

    fn cost(&self, _service: &str, contents: &str) -> Option<super::Cost> {
        let limit = *self
            .ceiling
            .lock()
            .expect("a poisoned test store is a failed test");
        limit.map(|limit| super::Cost {
            needs: contents.len(),
            limit,
            second_route: true,
        })
    }
}

/// The vault as one context opens it. A real keychain takes `PITBOARD_NO_ARGV` from the
/// context it is opened with, so above its ceiling it writes on the argument line or
/// refuses; this one does the same.
struct Vault {
    store: Arc<MemoryStore>,
    argument_line: bool,
}

impl RawStore for Vault {
    fn kind(&self) -> Backend {
        RawStore::kind(&self.store)
    }

    fn contains(&self, service: &str) -> Result<bool, Error> {
        RawStore::contains(&self.store, service)
    }

    fn read(&self, service: &str) -> Result<Option<String>, Error> {
        RawStore::read(&self.store, service)
    }

    fn write(&self, service: &str, contents: &str) -> Result<(), Error> {
        if let Some(cost) = self.cost(service, contents)
            && cost.refused()
        {
            return Err(Error::Write(format!(
                "this credential is {} bytes, past the {}-byte command limit",
                cost.needs, cost.limit
            )));
        }
        RawStore::write(&self.store, service, contents)
    }

    fn delete(&self, service: &str) -> Result<(), Error> {
        RawStore::delete(&self.store, service)
    }

    fn list(&self) -> Result<Option<Vec<String>>, Error> {
        RawStore::list(&self.store)
    }

    fn cost(&self, service: &str, contents: &str) -> Option<super::Cost> {
        RawStore::cost(&self.store, service, contents).map(|cost| super::Cost {
            second_route: self.argument_line,
            ..cost
        })
    }
}

/// A machine whose keychain and filesystem are both in memory.
///
/// It fakes the two things a real host offers and nothing above them, so the code under
/// test is the real one. Before this it faked `read_signin` with a map keyed by directory,
/// which meant no test ever ran Claude Code's own slot hashing on the sign-in path: the
/// double answered the question the code was supposed to answer.
#[derive(Debug)]
pub struct MemoryHost {
    keychain: Arc<MemoryStore>,
    vault: Arc<MemoryStore>,
    files: Mutex<HashMap<PathBuf, Arc<MemoryStore>>>,
    running: Mutex<HashMap<String, Vec<crate::process::Process>>>,
    /// What runs from inside a bundle, as `ps` would list it, every one started by launchd.
    within: Mutex<Vec<crate::process::Process>>,
    /// Whether the process list cannot be read.
    list_fails: std::sync::atomic::AtomicBool,
    /// Cookie jars, by the file they were planted on: its inode where it exists, so a jar
    /// moved by rename is still the same jar, and its path where it does not.
    cookies: Mutex<HashMap<JarKey, CookieTable>>,
    /// How every jar's read fails, where a test says it does.
    jar_fault: Mutex<Option<std::io::ErrorKind>>,
    /// Devices a test says paths are on, where they are not on the real one.
    devices: Mutex<HashMap<PathBuf, u64>>,
    /// Whether every home parks in `vault`, the way every home on macOS parks in the login
    /// keychain. So by default, because that is where the rules about another pitboard's
    /// parks are needed.
    shared_vault: std::sync::atomic::AtomicBool,
}

impl Default for MemoryHost {
    fn default() -> MemoryHost {
        MemoryHost {
            // Keychain, because that is the chain the interesting rules are written for.
            keychain: MemoryStore::of(Backend::Keychain),
            vault: MemoryStore::of(Backend::Keychain),
            files: Mutex::new(HashMap::new()),
            running: Mutex::new(HashMap::new()),
            within: Mutex::new(Vec::new()),
            list_fails: std::sync::atomic::AtomicBool::new(false),
            cookies: Mutex::new(HashMap::new()),
            jar_fault: Mutex::new(None),
            devices: Mutex::new(HashMap::new()),
            shared_vault: std::sync::atomic::AtomicBool::new(true),
        }
    }
}

impl MemoryHost {
    pub fn new() -> Arc<MemoryHost> {
        Arc::new(MemoryHost::default())
    }

    /// The keychain, where a tool that uses one keeps its live credential.
    pub fn live(&self) -> &Arc<MemoryStore> {
        &self.keychain
    }

    /// Where pitboard's parked logins are.
    pub fn vault(&self) -> &Arc<MemoryStore> {
        &self.vault
    }

    /// The file at `path`, where a tool that keeps its login in a file keeps it.
    pub fn file_at(&self, path: PathBuf) -> Arc<MemoryStore> {
        Arc::clone(
            self.files
                .lock()
                .expect("a poisoned test host is a failed test")
                .entry(path)
                .or_insert_with(|| MemoryStore::of(Backend::File)),
        )
    }

    /// From now on the vault belongs to one home alone, the way pitboard's vault of files
    /// does off macOS.
    pub fn vault_of_its_own(&self) {
        self.shared_vault
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Say that `count` processes are running `program`, each started by its bare name, the
    /// way a shell starts one from `PATH`.
    pub fn runs(&self, program: &str, count: usize) {
        self.runs_at(program, &vec![program; count]);
    }

    /// Say that a process is running `program` from each of `paths`.
    pub fn runs_at(&self, program: &str, paths: &[&str]) {
        let processes = (1..)
            .zip(paths)
            .map(|(pid, path)| crate::process::Process {
                pid,
                path: PathBuf::from(path),
            })
            .collect();
        self.running
            .lock()
            .expect("a poisoned test host is a failed test")
            .insert(program.to_string(), processes);
    }

    /// Say that a process is running from `path`, inside whatever bundle that is in, and
    /// answer its pid. The first is 100, apart from the pids [`MemoryHost::runs_at`] gives.
    pub fn runs_within(&self, path: &str) -> u32 {
        let mut within = self
            .within
            .lock()
            .expect("a poisoned test host is a failed test");
        let pid = 100 + u32::try_from(within.len()).expect("a test runs few processes");
        within.push(crate::process::Process {
            pid,
            path: PathBuf::from(path),
        });
        pid
    }

    /// Everything [`MemoryHost::runs_within`] said was running has quit.
    pub fn quits_within(&self) {
        self.within
            .lock()
            .expect("a poisoned test host is a failed test")
            .clear();
    }

    /// From now on every cookie jar there is fails to read this way, as a jar the app is
    /// writing, or one `sqlite3` gave up waiting on, does.
    pub fn jar_fails(&self, kind: std::io::ErrorKind) {
        *self
            .jar_fault
            .lock()
            .expect("a poisoned test host is a failed test") = Some(kind);
    }

    /// From now on the process list cannot be read.
    pub fn process_list_fails(&self) {
        self.list_fails
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Say that the cookie database at `path` holds `table`. Planted on the file's inode
    /// where it exists, so the jar travels with it when it is moved.
    // `CookieTable` is the crate's own, so a test outside the crate cannot plant one.
    #[allow(dead_code)]
    pub(crate) fn plant_cookies(&self, path: &Path, table: CookieTable) {
        self.cookies
            .lock()
            .expect("a poisoned test host is a failed test")
            .insert(jar_key(path), table);
    }

    /// Say that `path` is on device `dev`.
    pub fn device(&self, path: &Path, dev: u64) {
        self.devices
            .lock()
            .expect("a poisoned test host is a failed test")
            .insert(path.to_path_buf(), dev);
    }
}

/// Which jar a cookie database is: its inode where the file exists.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum JarKey {
    Inode(u64, u64),
    Path(PathBuf),
}

fn jar_key(path: &Path) -> JarKey {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(found) => JarKey::Inode(found.dev(), found.ino()),
        Err(_) => JarKey::Path(path.to_path_buf()),
    }
}

impl Host for MemoryHost {
    fn foreign_keychain(&self, _ctx: &Context, _account: &str) -> Option<Box<dyn RawStore>> {
        Some(Box::new(Arc::clone(&self.keychain)))
    }

    fn file(&self, path: PathBuf) -> Box<dyn RawStore> {
        Box::new(self.file_at(path))
    }

    fn vault(&self, ctx: &Context) -> Box<dyn RawStore> {
        Box::new(Vault {
            store: Arc::clone(&self.vault),
            argument_line: ctx.argv_fallback(),
        })
    }

    fn vault_is_shared(&self) -> bool {
        self.shared_vault.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// What a test said is running, and nothing on the machine running the tests.
    fn processes(&self, program: &str) -> Option<Vec<crate::process::Process>> {
        Some(
            self.running
                .lock()
                .expect("a poisoned test host is a failed test")
                .get(program)
                .cloned()
                .unwrap_or_default(),
        )
    }

    /// What a test said runs inside a bundle, by the rule the real list is read by, every
    /// process started by launchd.
    fn processes_within(
        &self,
        bundle: crate::process::Bundle<'_>,
        excluded: &[&str],
    ) -> Option<Vec<crate::process::Process>> {
        if self.list_fails.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        let listed: Vec<(u32, u32, PathBuf)> = self
            .within
            .lock()
            .expect("a poisoned test host is a failed test")
            .iter()
            .map(|p| (p.pid, 1, p.path.clone()))
            .collect();
        Some(crate::process::within_listed(&listed, bundle, excluded))
    }

    /// Whether a test said the process runs, and nothing about the machine's own.
    fn pid_alive(&self, pid: u32) -> bool {
        let running = self
            .running
            .lock()
            .expect("a poisoned test host is a failed test")
            .values()
            .flatten()
            .any(|p| p.pid == pid);
        running
            || self
                .within
                .lock()
                .expect("a poisoned test host is a failed test")
                .iter()
                .any(|p| p.pid == pid)
    }

    /// What a test said process `pid` runs, and nothing about the machine's own.
    fn program_of(&self, pid: u32) -> Option<PathBuf> {
        let running = self
            .running
            .lock()
            .expect("a poisoned test host is a failed test")
            .values()
            .flatten()
            .find(|p| p.pid == pid)
            .map(|p| p.path.clone());
        running.or_else(|| {
            self.within
                .lock()
                .expect("a poisoned test host is a failed test")
                .iter()
                .find(|p| p.pid == pid)
                .map(|p| p.path.clone())
        })
    }

    /// Where a test said, or the real device.
    fn device_of(&self, path: &Path) -> std::io::Result<u64> {
        use std::os::unix::fs::MetadataExt;
        if let Some(dev) = self
            .devices
            .lock()
            .expect("a poisoned test host is a failed test")
            .get(path)
        {
            return Ok(*dev);
        }
        std::fs::metadata(path).map(|m| m.dev())
    }

    /// The jar a test planted on the file. A file that is not there is not found, as
    /// `sqlite3` would find it; one there with nothing planted cannot be read.
    fn cookie_table(&self, path: &Path) -> std::io::Result<CookieTable> {
        std::fs::symlink_metadata(path)?;
        if let Some(kind) = *self
            .jar_fault
            .lock()
            .expect("a poisoned test host is a failed test")
        {
            return Err(std::io::Error::new(kind, "a jar a test made fail"));
        }
        let cookies = self
            .cookies
            .lock()
            .expect("a poisoned test host is a failed test");
        cookies
            .get(&jar_key(path))
            .or_else(|| cookies.get(&JarKey::Path(path.to_path_buf())))
            .cloned()
            .ok_or_else(|| std::io::Error::other("nothing planted in this jar"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<MemoryStore> {
        MemoryStore::of(Backend::Keychain)
    }

    #[test]
    fn a_write_that_does_not_read_back_is_reported_rather_than_believed() {
        let s = store();
        s.fault("svc", Fault::CorruptWrite("something else".into()));
        assert!(matches!(s.write("svc", "after"), Err(Error::NotDurable(_))));
        assert_eq!(s.peek("svc").as_deref(), Some("something else"));
    }

    #[test]
    fn a_refused_write_leaves_what_was_there() {
        let s = store();
        s.plant("svc", "before");
        s.fault("svc", Fault::FailWrite("told to".into()));
        assert!(matches!(s.write("svc", "after"), Err(Error::Write(_))));
        assert_eq!(s.peek("svc").as_deref(), Some("before"));
    }

    #[test]
    fn an_unreadable_store_never_answers_absent() {
        let s = store();
        s.plant("svc", "before");
        s.fault("svc", Fault::Unreadable("the keychain is locked".into()));
        assert!(matches!(s.read("svc"), Err(Error::Unreadable(_))));
        assert!(matches!(s.contains("svc"), Err(Error::Unreadable(_))));
    }

    #[test]
    fn something_that_vanished_reads_as_gone_but_is_still_there_to_assert_on() {
        let s = store();
        s.plant("svc", "before");
        s.fault("svc", Fault::Vanish);
        assert_eq!(s.read("svc").expect("vanishing is not a failure"), None);
        assert_eq!(s.peek("svc").as_deref(), Some("before"));
        s.heal("svc");
        assert_eq!(s.read("svc").expect("healed"), Some("before".into()));
    }

    #[test]
    fn a_store_that_locks_on_a_write_reads_fine_until_then() {
        let s = store();
        s.plant("svc", "before");
        s.fault("svc", Fault::LocksOnWrite);

        assert_eq!(
            s.read("svc").expect("still readable"),
            Some("before".into())
        );
        assert!(matches!(s.write("svc", "after"), Err(Error::Write(_))));
        assert!(
            matches!(s.read("svc"), Err(Error::Unreadable(_))),
            "once it is locked it cannot answer at all"
        );
        assert_eq!(
            s.peek("svc").as_deref(),
            Some("before"),
            "and nothing moved"
        );

        s.heal_all();
        assert_eq!(s.read("svc").expect("unlocked"), Some("before".into()));
    }

    #[test]
    fn a_fault_can_wait_for_a_few_calls_first() {
        let s = store();
        s.plant("svc", "before");
        s.fault_after("svc", 2, Fault::Vanish);
        assert_eq!(s.read("svc").expect("first"), Some("before".into()));
        assert_eq!(s.read("svc").expect("second"), Some("before".into()));
        assert_eq!(s.read("svc").expect("third"), None, "now it is gone");
    }

    #[test]
    fn a_vault_can_be_enumerated_which_no_real_one_on_macos_can() {
        let s = store();
        s.plant("pitboard-park-b", "2");
        s.plant("pitboard-park-a", "1");
        assert_eq!(s.services(), vec!["pitboard-park-a", "pitboard-park-b"]);
    }
}
