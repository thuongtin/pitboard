//! What Chromium's cookie jar says about who is signed in to Claude Desktop.
//!
//! Only the ciphertext is read, never decrypted here: whose session it is comes from the
//! hash of the ciphertext, and the uuid comes from `config.json`. The jar is read by the
//! system's own `sqlite3`, opened as immutable, so reading it never takes a lock a running
//! Claude could be waiting on and never needs Claude's key; a jar with a write still in its
//! journal is refused rather than read around it.

pub(crate) use super::types::{CookieRow, CookieTable};
use sha2::{Digest, Sha256};
use std::path::Path;

/// The cookie database's `meta.version` on the build the register names.
pub(crate) const EXPECTED_META: u32 = 24;

/// The system's own `sqlite3`, which every Mac has, so pitboard links no SQLite of its own.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub(crate) const SQLITE: &str = "/usr/bin/sqlite3";

/// The prefix of every value Chromium encrypts with the key in the keychain.
const PREFIX: &[u8] = b"v10";

/// Seconds between 1601, Chromium's epoch, and 1970.
const CHROME_EPOCH_OFFSET: i64 = 11_644_473_600;

/// The session a jar holds: a handle on it, never the cookie itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Session {
    /// The SHA-256 of the `sessionKey` cookie's ciphertext, in hex.
    pub fingerprint: String,
    /// When the cookie expires, in epoch seconds. `None` for one that never does.
    pub expires_at: Option<i64>,
}

/// A jar in a form this pitboard was not written for.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum DesktopFormat {
    #[error("meta version {0}, where {expected} was expected", expected = EXPECTED_META)]
    Meta(u32),
    #[error("the `{0}` cookie is not encrypted the way pitboard knows")]
    Prefix(String),
}

/// The session `table` holds, if any. A jar of another version, or a value that is not
/// `v10` ciphertext, is refused rather than read as signed out.
pub(crate) fn session(table: &CookieTable) -> Result<Option<Session>, DesktopFormat> {
    if table.meta_version != EXPECTED_META {
        return Err(DesktopFormat::Meta(table.meta_version));
    }
    // Every row read shares the one key, so any of them in another form means all may be.
    if let Some(odd) = table
        .rows
        .iter()
        .find(|row| !row.encrypted_value.starts_with(PREFIX))
    {
        return Err(DesktopFormat::Prefix(odd.name.clone()));
    }
    Ok(cookie(table, "sessionKey").map(|row| Session {
        fingerprint: fingerprint(&row.encrypted_value),
        expires_at: (row.expires_utc != 0).then(|| chrome_to_unix(row.expires_utc)),
    }))
}

/// Whether `sessionKeyV3` holds another session than `sessionKey`, which doctor reports.
/// Where either is missing there is nothing to differ.
// Read by doctor, which says when the two copies of the session disagree.
#[allow(dead_code)]
pub(crate) fn twins_differ(table: &CookieTable) -> bool {
    match (cookie(table, "sessionKey"), cookie(table, "sessionKeyV3")) {
        (Some(plain), Some(v3)) => plain.encrypted_value != v3.encrypted_value,
        _ => false,
    }
}

/// Chromium's time, microseconds since 1601, as epoch seconds.
pub(crate) fn chrome_to_unix(micros: i64) -> i64 {
    micros.div_euclid(1_000_000) - CHROME_EPOCH_OFFSET
}

/// The cookie called `name` for claude.ai, the domain-wide one first, which is the one
/// Claude sets.
pub(crate) fn cookie<'a>(table: &'a CookieTable, name: &str) -> Option<&'a CookieRow> {
    [".claude.ai", "claude.ai"].iter().find_map(|host| {
        table
            .rows
            .iter()
            .find(|row| row.name == name && row.host_key == *host)
    })
}

fn fingerprint(ciphertext: &[u8]) -> String {
    hex::encode(Sha256::digest(ciphertext))
}

/// What pitboard reads of the jar.
#[cfg_attr(target_os = "linux", allow(dead_code))]
const QUERY: &str = "SELECT key, value FROM meta WHERE key='version'; \
     SELECT host_key, name, hex(encrypted_value) AS v, expires_utc FROM cookies \
     WHERE host_key IN ('.claude.ai','claude.ai') \
     AND name IN ('sessionKey','sessionKeyV3','lastActiveOrg');";

/// The jar at `db`, read by `sqlite3`. A jar that is not there is `NotFound`, which a
/// caller reads as signed out; anything else that goes wrong is an error, never an empty
/// jar.
///
/// The database is opened as immutable. Read-only is not enough: `mode=ro` still takes a
/// shared lock, and against a holder of an exclusive one, as Chromium is while it runs,
/// that waits and then answers "database is locked". Immutable takes no lock, and so also
/// ignores a journal a write is still in, which would let a half-written jar be read. So
/// the jar is read only while its journal and log are empty, as the register's
/// `desktop_cookie_journal_idle` says they are between writes, and only if neither the
/// journal nor the jar changed while it was read. A jar mid-write is `ResourceBusy`, and
/// asking again a moment later reads it.
#[cfg(not(target_os = "linux"))]
pub(crate) fn read_with_sqlite(db: &Path) -> std::io::Result<CookieTable> {
    use std::io;
    // Checked first, the jar included: `sqlite3` would make an empty database where there
    // is none.
    let before = quiet(db)?;
    let mut sqlite = std::process::Command::new(SQLITE);
    sqlite
        .args(["-readonly", "-json", "-init", "/dev/null"])
        .arg(format!("file:{}?immutable=1", uri_path(db)))
        .arg(QUERY);
    let out = crate::process::output_within(sqlite, b"", std::time::Duration::from_secs(5))?;
    if !out.status.success() {
        // What sqlite3 says names the file and the fault, and holds no cookie.
        return Err(io::Error::other(format!(
            "sqlite3 could not read {}: {}",
            db.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    if quiet(db)? != before {
        return Err(io::Error::new(
            io::ErrorKind::ResourceBusy,
            format!("{} was written to while it was read", db.display()),
        ));
    }
    parse(&out.stdout)
}

/// The jar's size and when it was last written, which change with every write that reaches
/// it.
#[cfg_attr(target_os = "linux", allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
struct Quiet {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

/// The jar at `db` as it stands, refused as `ResourceBusy` while its rollback journal or
/// write-ahead log holds anything, which is a write not yet finished. A jar that is not
/// there is `NotFound`.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn quiet(db: &Path) -> std::io::Result<Quiet> {
    use std::io;
    for suffix in ["-journal", "-wal"] {
        let mut side = db.as_os_str().to_owned();
        side.push(suffix);
        let side = Path::new(&side);
        match std::fs::symlink_metadata(side) {
            Ok(found) if found.len() > 0 => {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    format!("{} holds a write not yet finished", side.display()),
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    let jar = std::fs::metadata(db)?;
    Ok(Quiet {
        len: jar.len(),
        modified: jar.modified().ok(),
    })
}

/// Claude Desktop does not run on Linux, and there is no jar to read.
#[cfg(target_os = "linux")]
pub(crate) fn read_with_sqlite(_db: &Path) -> std::io::Result<CookieTable> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Claude Desktop's cookies are read on macOS only",
    ))
}

/// `path` for a `file:` URI: every byte but the unreserved ones and `/` escaped, so a `?`
/// or `#` in a folder's name is part of the path and not the start of something else.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn uri_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::new();
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// `sqlite3 -json`'s answer to [`QUERY`]: one array per statement that returned rows.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn parse(stdout: &[u8]) -> std::io::Result<CookieTable> {
    use serde_json::Value;
    use std::io;
    let bad = |what: String| io::Error::new(io::ErrorKind::InvalidData, what);
    let mut table = CookieTable::default();
    let mut saw_meta = false;
    for result in serde_json::Deserializer::from_slice(stdout).into_iter::<Vec<Value>>() {
        let rows = result.map_err(|e| bad(format!("sqlite3 answered something else: {e}")))?;
        for row in rows {
            if let Some(value) = row.get("value") {
                // The version is stored as text, though a number would do as well.
                table.meta_version = match value {
                    Value::String(text) => text.parse().ok(),
                    Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
                    _ => None,
                }
                .ok_or_else(|| bad("the cookie database's version is not a number".into()))?;
                saw_meta = true;
            } else {
                let text = |key: &str| {
                    row.get(key)
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .ok_or_else(|| bad(format!("a cookie row has no `{key}`")))
                };
                table.rows.push(CookieRow {
                    host_key: text("host_key")?,
                    name: text("name")?,
                    encrypted_value: hex::decode(text("v")?)
                        .map_err(|_| bad("a cookie value is not hex".into()))?,
                    expires_utc: row
                        .get("expires_utc")
                        .and_then(Value::as_i64)
                        .ok_or_else(|| bad("a cookie row has no `expires_utc`".into()))?,
                });
            }
        }
    }
    if !saw_meta {
        return Err(bad("the cookie database says no version".into()));
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(host_key: &str, name: &str, value: &[u8], expires_utc: i64) -> CookieRow {
        CookieRow {
            host_key: host_key.into(),
            name: name.into(),
            encrypted_value: value.to_vec(),
            expires_utc,
        }
    }

    fn jar(rows: Vec<CookieRow>) -> CookieTable {
        CookieTable {
            meta_version: EXPECTED_META,
            rows,
        }
    }

    /// 2026-10-30T00:00:00Z in Chromium's time.
    const EXPIRES: i64 = (1_793_318_400 + 11_644_473_600) * 1_000_000;

    #[test]
    fn a_signed_out_jar_has_no_session() {
        assert_eq!(session(&jar(Vec::new())), Ok(None));
        let other = jar(vec![row(".claude.ai", "lastActiveOrg", b"v10org", EXPIRES)]);
        assert_eq!(session(&other), Ok(None));
    }

    #[test]
    fn the_fingerprint_is_the_hash_of_the_ciphertext() {
        let ciphertext = b"v10\x01\x02\x03ciphertext";
        let found = session(&jar(vec![
            row(".claude.ai", "lastActiveOrg", b"v10org", EXPIRES),
            row(".claude.ai", "sessionKey", ciphertext, EXPIRES),
            row(".claude.ai", "sessionKeyV3", b"v10another", EXPIRES),
        ]))
        .unwrap()
        .expect("signed in");
        // printf 'v10\x01\x02\x03ciphertext' | shasum -a 256
        use sha2::{Digest, Sha256};
        assert_eq!(found.fingerprint, hex::encode(Sha256::digest(ciphertext)));
        assert_eq!(found.fingerprint.len(), 64);
        assert_eq!(found.expires_at, Some(1_793_318_400));

        // A session cookie with no expiry says so rather than 1601.
        let forever = session(&jar(vec![row("claude.ai", "sessionKey", ciphertext, 0)]))
            .unwrap()
            .unwrap();
        assert_eq!(forever.expires_at, None);
        assert_eq!(forever.fingerprint, found.fingerprint);
    }

    /// The two copies of the session are told apart, so doctor can say when they differ,
    /// and the plain one is the one that names the login.
    #[test]
    fn twins_that_differ_are_noticed() {
        let same = jar(vec![
            row(".claude.ai", "sessionKey", b"v10same", EXPIRES),
            row(".claude.ai", "sessionKeyV3", b"v10same", EXPIRES),
        ]);
        assert!(!twins_differ(&same));
        let apart = jar(vec![
            row(".claude.ai", "sessionKey", b"v10one", EXPIRES),
            row(".claude.ai", "sessionKeyV3", b"v10two", EXPIRES),
        ]);
        assert!(twins_differ(&apart));
        assert!(!twins_differ(&jar(vec![row(
            ".claude.ai",
            "sessionKey",
            b"v10one",
            EXPIRES
        )])));
    }

    #[test]
    fn another_meta_version_is_refused() {
        let mut table = jar(vec![row(".claude.ai", "sessionKey", b"v10x", EXPIRES)]);
        table.meta_version = 25;
        assert_eq!(session(&table), Err(DesktopFormat::Meta(25)));
        table.meta_version = 0;
        assert_eq!(session(&table), Err(DesktopFormat::Meta(0)));
    }

    #[test]
    fn a_value_not_v10_is_refused() {
        for value in [&b"v11abc"[..], b"plain", b""] {
            let table = jar(vec![row(".claude.ai", "sessionKey", value, EXPIRES)]);
            assert_eq!(
                session(&table),
                Err(DesktopFormat::Prefix("sessionKey".into()))
            );
        }
        // Any of the rows read, not only the session, since they share one key.
        let table = jar(vec![
            row(".claude.ai", "sessionKey", b"v10ok", EXPIRES),
            row(".claude.ai", "lastActiveOrg", b"v20org", EXPIRES),
        ]);
        assert_eq!(
            session(&table),
            Err(DesktopFormat::Prefix("lastActiveOrg".into()))
        );
    }

    #[test]
    fn chrome_time_converts() {
        assert_eq!(chrome_to_unix(11_644_473_600 * 1_000_000), 0);
        assert_eq!(chrome_to_unix(EXPIRES), 1_793_318_400);
        assert_eq!(chrome_to_unix(EXPIRES + 999_999), 1_793_318_400);
    }

    /// Opened as immutable, `sqlite3` ignores a journal, so a jar with a write still in
    /// its journal or log is refused rather than read half-written. An empty journal is
    /// what the running app leaves between writes, and is no write.
    #[test]
    fn a_jar_mid_write_is_refused() {
        let dir = std::env::temp_dir().join(format!(
            "pitboard-cookies-quiet-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("Cookies");

        assert_eq!(
            quiet(&db).unwrap_err().kind(),
            std::io::ErrorKind::NotFound,
            "no jar is not found, which a caller reads as signed out"
        );
        std::fs::write(&db, b"rows").unwrap();
        let alone = quiet(&db).expect("no journal at all");
        std::fs::write(dir.join("Cookies-journal"), b"").unwrap();
        assert_eq!(quiet(&db).expect("an empty journal"), alone);

        std::fs::write(dir.join("Cookies-journal"), b"pages not yet written back").unwrap();
        let busy = quiet(&db).unwrap_err();
        assert_eq!(busy.kind(), std::io::ErrorKind::ResourceBusy, "{busy}");
        std::fs::write(dir.join("Cookies-journal"), b"").unwrap();
        std::fs::write(dir.join("Cookies-wal"), b"frames").unwrap();
        assert_eq!(
            quiet(&db).unwrap_err().kind(),
            std::io::ErrorKind::ResourceBusy
        );
        std::fs::remove_file(dir.join("Cookies-wal")).unwrap();

        // A jar that changed while it was read is told apart from one that did not.
        std::fs::write(&db, b"more rows").unwrap();
        assert_ne!(quiet(&db).unwrap(), alone);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The query goes through the system's own `sqlite3`, against a database made the way
    /// Chromium's is laid out, in a folder whose name needs escaping in a URI.
    #[cfg(target_os = "macos")]
    #[test]
    fn reads_a_real_database_through_sqlite3() {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!(
            "pitboard-cookies #1 ?{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("Cookies");
        let made = Command::new(SQLITE)
            .arg(&db)
            .arg(
                "CREATE TABLE meta(key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR);\
                 INSERT INTO meta VALUES('version','24'),('last_compatible_version','24');\
                 CREATE TABLE cookies(creation_utc INTEGER NOT NULL, host_key TEXT NOT NULL, \
                   name TEXT NOT NULL, value TEXT NOT NULL, encrypted_value BLOB NOT NULL, \
                   expires_utc INTEGER NOT NULL);\
                 INSERT INTO cookies VALUES(1,'.claude.ai','sessionKey','',X'763130AABB00',\
                   13437792000000000);\
                 INSERT INTO cookies VALUES(1,'.claude.ai','lastActiveOrg','',X'763130CC',0);\
                 INSERT INTO cookies VALUES(1,'.claude.ai','cf_clearance','',X'763130DD',0);\
                 INSERT INTO cookies VALUES(1,'example.com','sessionKey','',X'763130EE',0);",
            )
            .status()
            .unwrap();
        assert!(made.success());

        let table = read_with_sqlite(&db).expect("read");
        assert_eq!(table.meta_version, 24);
        let mut rows = table.rows.clone();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(
            rows,
            vec![
                row(".claude.ai", "lastActiveOrg", &[0x76, 0x31, 0x30, 0xcc], 0),
                row(
                    ".claude.ai",
                    "sessionKey",
                    &[0x76, 0x31, 0x30, 0xaa, 0xbb, 0x00],
                    13_437_792_000_000_000
                ),
            ]
        );
        assert!(session(&table).unwrap().is_some());

        // Read while another process holds the database locked, the way a running Claude
        // can, without waiting and without taking a lock of its own.
        let mut holder = Command::new(SQLITE)
            .arg(&db)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::{BufRead, Write};
            let mut stdin = holder.stdin.take().unwrap();
            stdin
                .write_all(b"BEGIN EXCLUSIVE;\nSELECT 'locked';\n")
                .unwrap();
            let mut line = String::new();
            std::io::BufReader::new(holder.stdout.as_mut().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line.trim(), "locked");
            let started = std::time::Instant::now();
            let held = read_with_sqlite(&db).expect("read under a lock");
            assert_eq!(held, table);
            assert!(started.elapsed() < std::time::Duration::from_secs(2));
            drop(stdin);
        }
        let _ = holder.kill();
        let _ = holder.wait();

        // A write still in the journal is never read around.
        std::fs::write(dir.join("Cookies-journal"), b"a write in progress").unwrap();
        let busy = read_with_sqlite(&db).unwrap_err();
        assert_eq!(busy.kind(), std::io::ErrorKind::ResourceBusy, "{busy}");
        std::fs::write(dir.join("Cookies-journal"), b"").unwrap();
        assert_eq!(read_with_sqlite(&db).expect("an empty journal"), table);

        // No jar at all is "not found", which a caller reads as signed out.
        let missing = read_with_sqlite(&dir.join("Nothing")).unwrap_err();
        assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
        // Something that is not a database is an error, never an empty jar.
        std::fs::write(
            dir.join("Garbage"),
            b"not a database at all, not even close",
        )
        .unwrap();
        assert!(read_with_sqlite(&dir.join("Garbage")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
