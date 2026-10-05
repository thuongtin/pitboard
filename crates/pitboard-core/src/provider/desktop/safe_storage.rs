//! Claude's key in the keychain, `Claude Safe Storage`, which Chromium encrypts the app's
//! cookies with.
//!
//! The item's access list trusts Claude and nothing else, so reading its password makes
//! macOS ask the person at the Mac whether pitboard may. That question is asked once, when
//! somebody turns live usage on, and never again from a refresh: a refresh that would have
//! to ask gives up after ten seconds and says approval is needed. Giving up does not take
//! the question away: experiment U-K3 found macOS keeps it on screen after `security` is
//! killed, so a refresh that gave up never asks again for the same key. That holds within
//! one process: `enable` asks every time it runs, and two processes refreshing at once can
//! each ask, so each of those can leave a question behind. After Always Allow a background
//! shell that could not ask before reads the key without asking (experiment U-K1b), so
//! refreshes from there need nobody at the Mac; a read over SSH or from a scheduled job has
//! not been measured. Reading the item's attributes
//! asks nothing (the register's `security_attributes_never_prompt`, experiment U-K5), which
//! is how a key Claude made again is noticed without its password being read.
//!
//! Every read goes through `/usr/bin/security`, like every other keychain read pitboard
//! makes, and is never written: the item is Claude's.

use crate::context::Context;
use std::path::Path;
use std::time::Duration;
use zeroize::Zeroizing;

pub(crate) use super::types::ItemStamp;

/// The item Claude Desktop keeps its key in.
pub(crate) const ITEM_SERVICE: &str = "Claude Safe Storage";
/// The item's account. Experiment E5 read "Claude Key", and Swivel says the same; left
/// unnamed all the same, since the service alone finds the item and naming the account would
/// miss an install that used another.
pub(crate) const ITEM_ACCOUNT: Option<&str> = None;

/// Why the password is being read, which decides how long a question may wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyRead {
    /// Somebody turned live usage on and is at the Mac to answer macOS.
    Approve,
    /// A refresh, which nobody is watching.
    Refresh,
}

impl KeyRead {
    /// How long `security` may wait on macOS's question before it is killed. Killing it
    /// does not close the question (experiment U-K3): the dialog stays on screen until
    /// somebody answers it, and an answer then reaches nobody.
    pub(crate) fn limit(self) -> Duration {
        match self {
            KeyRead::Approve => Duration::from_secs(120),
            KeyRead::Refresh => Duration::from_secs(10),
        }
    }
}

/// What reading the item came to, by `security`'s exit status, which is the low byte of
/// the `OSStatus`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum KeyReadError {
    /// `errSecInteractionNotAllowed`: macOS would have to ask, and cannot from here.
    #[error("macOS can only ask about Claude's key on the Mac's own screen")]
    NoGui,
    /// `errSecUserCanceled`: somebody said no, or typed a password that was not accepted
    /// and then chose Allow, which experiment U-K2 found exits 128 as well.
    #[error("macOS did not let pitboard read Claude's key")]
    Denied,
    /// `errSecAuthFailed`. Experiment U-K2 saw no answer at the question give it, a wrong
    /// password included, which is [`KeyReadError::Denied`]; kept for a keychain that does.
    #[error("macOS did not accept the password given for Claude's key")]
    AuthFailed,
    /// Nobody answered in time. The question may still be on screen (experiment U-K3).
    #[error("macOS's question about Claude's key was not answered in time")]
    TimedOut,
    /// `errSecItemNotFound`.
    #[error("Claude's key is not in the keychain")]
    Missing,
    #[error("Claude's key could not be read: {0}")]
    Other(String),
}

impl KeyReadError {
    /// The stable code live usage records for this, as `LiveUsageNotAllowed` reads it.
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            KeyReadError::NoGui => "no_gui",
            KeyReadError::Denied => "denied",
            KeyReadError::AuthFailed => "auth_failed",
            KeyReadError::TimedOut => "timed_out",
            KeyReadError::Missing => "item_missing",
            KeyReadError::Other(_) => "other",
        }
    }
}

/// Claude's key, as a seam: `security` in every real context, a script in the tests,
/// which never read the real item.
pub(crate) trait SafeStorage: Send + Sync + std::fmt::Debug {
    /// When the item was made and last changed, read without its password.
    fn stamp(&self, ctx: &Context) -> Result<ItemStamp, KeyReadError>;
    /// The item's password, which may make macOS ask.
    fn password(&self, ctx: &Context, how: KeyRead) -> Result<Zeroizing<Vec<u8>>, KeyReadError>;
}

/// The keychain, through `/usr/bin/security`.
#[derive(Debug)]
pub(crate) struct SecurityCli;

impl SafeStorage for SecurityCli {
    fn stamp(&self, _ctx: &Context) -> Result<ItemStamp, KeyReadError> {
        never_from_a_test();
        stamp_with(Path::new(crate::store::SECURITY))
    }

    fn password(&self, _ctx: &Context, how: KeyRead) -> Result<Zeroizing<Vec<u8>>, KeyReadError> {
        never_from_a_test();
        password_with(Path::new(crate::store::SECURITY), how.limit())
    }
}

/// The item is somebody's real key, so a test that reaches it has forgotten to script it,
/// and stops before `security` runs. That covers the integration tests and the command line
/// they run, which build this crate with `test-support` rather than as a test.
fn never_from_a_test() {
    #[cfg(any(test, feature = "test-support"))]
    panic!("a test reached Claude's real key; script it with ScriptedSafeStorage");
}

/// `security` exits with the low byte of the `OSStatus`.
const INTERACTION_NOT_ALLOWED: i32 = 36;
const ITEM_NOT_FOUND: i32 = 44;
const AUTH_FAILED: i32 = 51;
const USER_CANCELED: i32 = 128;

/// Reading the attributes never asks: the register's `security_attributes_never_prompt`,
/// measured by experiment U-K5. Five seconds is the ceiling for a locked keychain.
const STAMP_LIMIT: Duration = Duration::from_secs(5);

fn find(with_password: bool) -> Vec<&'static str> {
    let mut args = vec!["find-generic-password"];
    if with_password {
        args.push("-w");
    }
    args.extend(["-s", ITEM_SERVICE]);
    if let Some(account) = ITEM_ACCOUNT {
        args.extend(["-a", account]);
    }
    args
}

#[cfg(any(not(target_os = "linux"), test))]
fn run(
    program: &Path,
    args: &[&str],
    limit: Duration,
) -> Result<std::process::Output, KeyReadError> {
    let mut command = std::process::Command::new(program);
    command.args(args);
    crate::process::output_within(command, b"", limit).map_err(|e| unanswered(program, &e))
}

/// How `security` exited, what it printed, wiped when dropped, and what it said on stderr.
type Secret = (std::process::ExitStatus, Zeroizing<Vec<u8>>, Vec<u8>);

/// [`run`] for the password, whose output is read into memory that is wiped when dropped.
#[cfg(any(not(target_os = "linux"), test))]
fn run_secret(program: &Path, args: &[&str], limit: Duration) -> Result<Secret, KeyReadError> {
    let mut command = std::process::Command::new(program);
    command.args(args);
    crate::process::secret_within(command, limit).map_err(|e| unanswered(program, &e))
}

/// What a `security` that could not be run, or did not finish in time, comes to.
#[cfg(any(not(target_os = "linux"), test))]
fn unanswered(program: &Path, e: &std::io::Error) -> KeyReadError {
    if e.kind() == std::io::ErrorKind::TimedOut {
        KeyReadError::TimedOut
    } else {
        KeyReadError::Other(format!("{} did not answer: {e}", program.display()))
    }
}

/// Claude Desktop does not run on Linux, so there is no key to read.
#[cfg(all(target_os = "linux", not(test)))]
fn run_secret(_program: &Path, _args: &[&str], _limit: Duration) -> Result<Secret, KeyReadError> {
    Err(KeyReadError::Missing)
}

/// Claude Desktop does not run on Linux, so there is no key to read.
#[cfg(all(target_os = "linux", not(test)))]
fn run(
    _program: &Path,
    _args: &[&str],
    _limit: Duration,
) -> Result<std::process::Output, KeyReadError> {
    Err(KeyReadError::Missing)
}

fn classify(status: std::process::ExitStatus, stderr: &[u8]) -> Result<(), KeyReadError> {
    match status.code() {
        Some(0) => Ok(()),
        Some(INTERACTION_NOT_ALLOWED) => Err(KeyReadError::NoGui),
        Some(USER_CANCELED) => Err(KeyReadError::Denied),
        Some(AUTH_FAILED) => Err(KeyReadError::AuthFailed),
        Some(ITEM_NOT_FOUND) => Err(KeyReadError::Missing),
        Some(code) => Err(KeyReadError::Other(format!(
            "security exited {code}: {}",
            String::from_utf8_lossy(stderr).trim()
        ))),
        None => Err(KeyReadError::Other("security exited on a signal".into())),
    }
}

fn stamp_with(program: &Path) -> Result<ItemStamp, KeyReadError> {
    let out = run(program, &find(false), STAMP_LIMIT)?;
    classify(out.status, &out.stderr)?;
    let listing = String::from_utf8_lossy(&out.stdout);
    match (date_of(&listing, "cdat"), date_of(&listing, "mdat")) {
        (Some(cdat), Some(mdat)) => Ok(ItemStamp { cdat, mdat }),
        _ => Err(KeyReadError::Other(
            "security listed the item without its dates".into(),
        )),
    }
}

fn password_with(program: &Path, limit: Duration) -> Result<Zeroizing<Vec<u8>>, KeyReadError> {
    let (status, mut password, stderr) = run_secret(program, &find(true), limit)?;
    classify(status, &stderr)?;
    // `-w` ends the password with a newline, which is not part of it.
    if password.last() == Some(&b'\n') {
        password.pop();
    }
    if password.is_empty() {
        return Err(KeyReadError::Other("security gave an empty key".into()));
    }
    Ok(password)
}

/// One date attribute of a `find-generic-password` listing, whose line reads
/// `    "cdat"<timedate>=0x3230...  "20260727084307Z\000"`.
fn date_of(listing: &str, attribute: &str) -> Option<String> {
    let label = format!("\"{attribute}\"<timedate>=");
    let line = listing
        .lines()
        .map(str::trim_start)
        .find(|line| line.starts_with(&label))?;
    let quoted = line.rsplit_once("  \"").map(|(_, rest)| rest)?;
    let date = quoted.strip_suffix('"')?;
    let date = date.strip_suffix("\\000").unwrap_or(date);
    (!date.is_empty()).then(|| date.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A stand-in for `security` in a scratch directory, which prints `stdout` and exits
    /// `code`. The real program is never run here: the item is somebody's real key.
    struct Fake(PathBuf);

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    fn fake(name: &str, script: &str) -> Fake {
        let dir = std::env::temp_dir().join(format!(
            "pitboard-safe-storage-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("security");
        // Written by a child, never this process: a descriptor open for writing here would
        // be inherited by a fork on another test's thread, and running the program while
        // that child lives fails with ETXTBSY, as `write_program` in the CLI's tests found.
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", "printf %s \"$2\" > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(&program)
            .arg(format!("#!/bin/sh\n{script}"))
            .status()
            .unwrap();
        assert!(status.success(), "could not write {}", program.display());
        Fake(program)
    }

    const LISTING: &str = r#"keychain: "/Users/someone/Library/Keychains/login.keychain-db"
version: 512
class: "genp"
attributes:
    0x00000007 <blob>="Claude Safe Storage"
    "acct"<blob>="Claude Key"
    "cdat"<timedate>=0x32303236303732373038343330375A00  "20260727084307Z\000"
    "mdat"<timedate>=0x32303236303732373038343330375A00  "20260801120000Z\000"
    "svce"<blob>="Claude Safe Storage"
"#;

    #[test]
    fn the_stamp_is_read_from_the_attributes() {
        let program = fake("stamp", &format!("cat <<'EOF'\n{LISTING}EOF\n"));
        assert_eq!(
            stamp_with(&program.0).unwrap(),
            ItemStamp {
                cdat: "20260727084307Z".into(),
                mdat: "20260801120000Z".into(),
            }
        );
    }

    /// The listing is asked for without `-w` or `-g`, so it never makes macOS ask.
    #[test]
    fn the_stamp_never_asks_for_the_password() {
        assert!(!find(false).contains(&"-w"));
        assert!(!find(false).contains(&"-g"));
        assert!(find(true).contains(&"-w"));
        assert_eq!(&find(false)[1..3], ["-s", "Claude Safe Storage"]);
    }

    #[test]
    fn the_password_loses_only_its_newline() {
        let program = fake("password", "printf 'not-a-real-password\\n'\n");
        let password = password_with(&program.0, Duration::from_secs(5)).unwrap();
        assert_eq!(password.as_slice(), b"not-a-real-password");
    }

    /// Each exit `security` is known to give is read as what it means, and 36 is not a
    /// refusal: it says only that nobody could have been asked.
    #[test]
    fn each_exit_means_what_security_says() {
        for (code, expected) in [
            (36, KeyReadError::NoGui),
            (128, KeyReadError::Denied),
            (51, KeyReadError::AuthFailed),
            (44, KeyReadError::Missing),
        ] {
            let program = fake(&format!("exit-{code}"), &format!("exit {code}\n"));
            assert_eq!(
                password_with(&program.0, Duration::from_secs(5)).unwrap_err(),
                expected,
                "exit {code}"
            );
            assert_eq!(stamp_with(&program.0).unwrap_err(), expected, "exit {code}");
        }
        let program = fake("exit-1", "echo 'something else' >&2\nexit 1\n");
        assert!(matches!(
            password_with(&program.0, Duration::from_secs(5)),
            Err(KeyReadError::Other(detail)) if detail.contains("something else")
        ));
    }

    /// A question nobody answers is closed when its time is up, and the read gives up.
    #[test]
    fn a_question_nobody_answers_times_out() {
        let program = fake("silent", "exec sleep 30\n");
        let started = std::time::Instant::now();
        assert_eq!(
            password_with(&program.0, Duration::from_millis(300)).unwrap_err(),
            KeyReadError::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_refresh_waits_ten_seconds_and_an_approval_two_minutes() {
        assert_eq!(KeyRead::Refresh.limit(), Duration::from_secs(10));
        assert_eq!(KeyRead::Approve.limit(), Duration::from_secs(120));
    }
}
