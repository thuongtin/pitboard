//! Running a helper program with a deadline. A helper that never answers must not hold
//! pitboard's lock, or an app's worker, forever.

// Linux reads its process list from /proc and has no keychain to call, so there only the
// tests run a helper.
use std::path::{Path, PathBuf};
#[cfg(any(not(target_os = "linux"), test))]
use std::{
    io::{self, Read, Write},
    process::{Command, ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
#[cfg(any(not(target_os = "linux"), test))]
use zeroize::Zeroizing;

/// `command`'s output, with `input` on its stdin. Past `limit` the process is killed and the
/// answer is `TimedOut`. Its output is drained on other threads, so a chatty helper cannot
/// fill a pipe and stall.
#[cfg(any(not(target_os = "linux"), test))]
pub fn output_within(command: Command, input: &[u8], limit: Duration) -> io::Result<Output> {
    let (status, stdout, stderr) = run_within(command, input, limit, |pipe| {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    })?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// The most of a secret [`secret_within`] keeps. A password is far shorter; anything past
/// this is read and thrown away, so the helper never stalls on a full pipe.
#[cfg(any(not(target_os = "linux"), test))]
const SECRET_LIMIT: usize = 4096;

/// [`output_within`] for a helper that prints a secret, with nothing on its stdin. Its
/// stdout is read into one buffer sized up front, which never grows and so leaves no copy
/// behind, and which is wiped when it is dropped.
#[cfg(any(not(target_os = "linux"), test))]
pub(crate) fn secret_within(
    command: Command,
    limit: Duration,
) -> io::Result<(ExitStatus, Zeroizing<Vec<u8>>, Vec<u8>)> {
    run_within(command, b"", limit, |pipe| {
        let mut bytes = Zeroizing::new(Vec::with_capacity(SECRET_LIMIT));
        let _ = pipe.take(SECRET_LIMIT as u64).read_to_end(&mut bytes);
        let _ = io::copy(pipe, &mut io::sink());
        bytes
    })
}

/// Runs `command` with `input` on its stdin, past `limit` kills it, and reads its stdout
/// with `read`, on another thread like its stderr.
#[cfg(any(not(target_os = "linux"), test))]
fn run_within<T: Default + Send + 'static>(
    mut command: Command,
    input: &[u8],
    limit: Duration,
    read: fn(&mut std::process::ChildStdout) -> T,
) -> io::Result<(ExitStatus, T, Vec<u8>)> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<Vec<u8>> {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    }
    let stdout = child.stdout.take();
    let stdout = thread::spawn(move || stdout.map(|mut pipe| read(&mut pipe)).unwrap_or_default());
    let stderr = drain(child.stderr.take());
    // Dropping stdin closes it, which is how a helper reading commands learns there are no
    // more. A helper that exits without reading is not an error.
    if let Some(mut stdin) = child.stdin.take() {
        match stdin.write_all(input) {
            Err(e) if e.kind() != io::ErrorKind::BrokenPipe => return Err(e),
            _ => {}
        }
    }

    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no answer within {}s", limit.as_secs()),
            ));
        }
        thread::sleep(Duration::from_millis(5));
    };
    Ok((
        status,
        stdout.join().unwrap_or_default(),
        stderr.join().unwrap_or_default(),
    ))
}

/// One process this user is running, and where its program runs from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    /// The program's path where the system says one, or its bare name where it does not.
    /// macOS gives what the program was started as, which is its full path when whatever
    /// started it named one; Linux gives the file it runs, where this user may read that.
    pub path: PathBuf,
}

/// Every process this user is running whose program is called `program`. `None` where the
/// process list could not be read, which is not the same as none running.
///
/// What is compared is the program's own name, so a script or a shell that merely mentions
/// the program is not counted. Only this user's: another user's sessions use another
/// user's login, which a switch here never touches.
#[cfg(target_os = "linux")]
pub fn processes(program: &str) -> Option<Vec<Process>> {
    use std::os::unix::fs::MetadataExt;
    // Linux says it in /proc, which every Linux has, where `/bin/ps` is not on every one:
    // a slim container or NixOS has none, and the warning this feeds would quietly vanish.
    let me = std::fs::metadata("/proc/self").ok()?.uid();
    let mut found: Vec<Process> = std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            if entry.metadata().ok()?.uid() != me {
                return None;
            }
            let name = std::fs::read_to_string(entry.path().join("comm")).ok()?;
            (name.trim() == program).then(|| Process {
                pid,
                path: std::fs::read_link(entry.path().join("exe"))
                    .unwrap_or_else(|_| PathBuf::from(program)),
            })
        })
        .collect();
    found.sort_unstable_by_key(|p| p.pid);
    Some(found)
}

#[cfg(not(target_os = "linux"))]
pub fn processes(program: &str) -> Option<Vec<Process>> {
    // `-x` with no other selection is every process this user owns, with or without a
    // terminal.
    let mut ps = Command::new("/bin/ps");
    ps.args(["-x", "-o", "pid=,comm="]);
    let out = output_within(ps, b"", Duration::from_secs(5)).ok()?;
    if !out.status.success() {
        return None;
    }
    Some(named(&String::from_utf8_lossy(&out.stdout), program))
}

/// The processes in a `ps -o pid=,comm=` listing whose program is called `program`. A path
/// can have spaces in it, so everything after the pid is the path.
#[cfg(any(not(target_os = "linux"), test))]
fn named(listing: &str, program: &str) -> Vec<Process> {
    listing
        .lines()
        .filter_map(|line| {
            let (pid, path) = line.trim_start().split_once(char::is_whitespace)?;
            let path = PathBuf::from(path.trim());
            (path.file_name()? == program).then_some(Process {
                pid: pid.parse().ok()?,
                path,
            })
        })
        .collect()
}

/// One line of a process list that also says each process's parent.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Listed {
    pid: u32,
    ppid: u32,
    path: PathBuf,
}

/// Which app bundle [`processes_within`] asks about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bundle<'a> {
    /// Every bundle of this name, wherever it is: the outermost one a program is in.
    Named(&'a str),
    /// The bundle at exactly this path, and no other, whatever it is called.
    At(&'a Path),
}

impl Bundle<'_> {
    /// The bundle `path` runs from, if it runs from one this asks about. By name, that is
    /// the outermost bundle of that name, or of the copies in `renamed`.
    fn root_of<'p>(&self, path: &'p Path, renamed: &[&Path]) -> Option<&'p Path> {
        match *self {
            Bundle::Named(name) => path
                .ancestors()
                .skip(1)
                .filter(|dir| {
                    dir.file_name().is_some_and(|dir| dir == name) || renamed.contains(dir)
                })
                .last(),
            Bundle::At(root) => path.ancestors().skip(1).find(|dir| *dir == root),
        }
    }

    /// The program the bundle's app runs, which a Mac app is named for: `Claude` for
    /// `Claude.app`. Renaming the bundle in the Finder leaves it as it was.
    pub fn program(&self) -> Option<&str> {
        let name = match *self {
            Bundle::Named(name) => name,
            Bundle::At(root) => root.file_name()?.to_str()?,
        };
        name.strip_suffix(".app")
    }

    /// The bundles of `listed`, by name, that are a copy of this one under another name:
    /// the outermost bundle around a process running this one's program, as its main
    /// program in `Contents/MacOS` or as one of its helpers in `Contents/Frameworks`.
    fn renamed<'p>(&self, listed: &'p [Listed]) -> Vec<&'p Path> {
        let (Bundle::Named(_), Some(program)) = (self, self.program()) else {
            return Vec::new();
        };
        let helper = format!("{program} Helper");
        let mut found: Vec<&Path> = listed
            .iter()
            .filter_map(|process| {
                let app = process
                    .path
                    .ancestors()
                    .skip(1)
                    .filter(|dir| dir.extension().is_some_and(|ext| ext == "app"))
                    .last()?;
                let rest = process.path.strip_prefix(app).ok()?;
                let parts: Vec<&str> = rest
                    .components()
                    .map(|c| c.as_os_str().to_str())
                    .collect::<Option<_>>()?;
                let ours = match parts.as_slice() {
                    ["Contents", "MacOS", main] => *main == program,
                    ["Contents", "Frameworks", inner, ..] => {
                        inner.starts_with(&helper) && inner.ends_with(".app")
                    }
                    _ => false,
                };
                ours.then_some(app)
            })
            .collect();
        found.sort_unstable();
        found.dedup();
        found
    }
}

/// Every process this user is running from inside `bundle`, wherever in it the program is,
/// except those at a path in `excluded` (relative to the bundle) that the bundle's own
/// processes did not start. `None` where the process list could not be read,
/// which is not the same as none running.
///
/// An excluded program counts when the app itself started it: Claude's
/// `chrome-native-host` is started by Chrome for the browser extension and holds nothing of
/// the app's data folder, but one the app started is the app's.
#[cfg(target_os = "linux")]
pub fn processes_within(bundle: Bundle<'_>, excluded: &[&str]) -> Option<Vec<Process>> {
    use std::os::unix::fs::MetadataExt;
    let me = std::fs::metadata("/proc/self").ok()?.uid();
    let listed: Vec<Listed> = std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            if entry.metadata().ok()?.uid() != me {
                return None;
            }
            // The parent is the second field after the name, which is in parentheses and
            // may itself hold spaces and parentheses.
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            let (_, rest) = stat.rsplit_once(')')?;
            let ppid = rest.split_whitespace().nth(1)?.parse().ok()?;
            let path = std::fs::read_link(entry.path().join("exe")).ok()?;
            Some(Listed { pid, ppid, path })
        })
        .collect();
    Some(within(&listed, bundle, excluded))
}

#[cfg(not(target_os = "linux"))]
pub fn processes_within(bundle: Bundle<'_>, excluded: &[&str]) -> Option<Vec<Process>> {
    let mut ps = Command::new("/bin/ps");
    ps.args(["-x", "-o", "pid=,ppid=,comm="]);
    let out = output_within(ps, b"", Duration::from_secs(5)).ok()?;
    if !out.status.success() {
        return None;
    }
    Some(within(
        &listed(&String::from_utf8_lossy(&out.stdout)),
        bundle,
        excluded,
    ))
}

/// The lines of a `ps -o pid=,ppid=,comm=` listing. A path can have spaces in it, so
/// everything after the parent is the path; a line that is not three fields is skipped.
#[cfg(any(not(target_os = "linux"), test))]
fn listed(listing: &str) -> Vec<Listed> {
    listing
        .lines()
        .filter_map(|line| {
            let (pid, rest) = line.trim_start().split_once(char::is_whitespace)?;
            let (ppid, path) = rest.trim_start().split_once(char::is_whitespace)?;
            Some(Listed {
                pid: pid.parse().ok()?,
                ppid: ppid.parse().ok()?,
                path: PathBuf::from(path.trim()),
            })
        })
        .collect()
}

/// The processes of `listed` inside `bundle`, by the rule [`processes_within`] gives, in
/// pid order.
fn within(listed: &[Listed], bundle: Bundle<'_>, excluded: &[&str]) -> Vec<Process> {
    // Where in its bundle each process runs from, for those inside one. By name, the
    // outermost bundle of that name is the app's: a helper is a bundle of its own inside it.
    let renamed = bundle.renamed(listed);
    let inside: Vec<(&Listed, &Path)> = listed
        .iter()
        .filter_map(|process| {
            let root = bundle.root_of(&process.path, &renamed)?;
            Some((process, process.path.strip_prefix(root).ok()?))
        })
        .collect();
    let is_excluded = |rest: &Path| excluded.iter().any(|path| rest.starts_with(path));
    let own: std::collections::HashSet<u32> = inside
        .iter()
        .filter(|(_, rest)| !is_excluded(rest))
        .map(|(process, _)| process.pid)
        .collect();
    let mut found: Vec<Process> = inside
        .into_iter()
        .filter(|(process, rest)| !is_excluded(rest) || own.contains(&process.ppid))
        .map(|(process, _)| Process {
            pid: process.pid,
            path: process.path.clone(),
        })
        .collect();
    found.sort_unstable_by_key(|p| p.pid);
    found
}

/// [`within`] over `(pid, ppid, path)`, for a host that fakes the list.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn within_listed(
    listed: &[(u32, u32, PathBuf)],
    bundle: Bundle<'_>,
    excluded: &[&str],
) -> Vec<Process> {
    let listed: Vec<Listed> = listed
        .iter()
        .map(|(pid, ppid, path)| Listed {
            pid: *pid,
            ppid: *ppid,
            path: path.clone(),
        })
        .collect();
    within(&listed, bundle, excluded)
}

/// Whether a process `pid` may be running. One this user may not signal is counted, since
/// it is there.
pub fn pid_alive(pid: u32) -> bool {
    crate::atomic::may_be_running(pid)
}

/// The program process `pid` runs, as `ps` names it: its full path when whatever started
/// it named one. `None` where that cannot be told, as for a process that has gone.
#[cfg(not(target_os = "linux"))]
pub fn program_of(pid: u32) -> Option<PathBuf> {
    let mut ps = Command::new("/bin/ps");
    ps.args(["-p", &pid.to_string(), "-o", "comm="]);
    let out = output_within(ps, b"", Duration::from_secs(5)).ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !path.is_empty()).then(|| PathBuf::from(path))
}

/// The file process `pid` runs, where this user may read that.
#[cfg(target_os = "linux")]
pub fn program_of(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// The process Chromium's `SingletonLock` at `path` names: a link whose target is
/// `<hostname>-<pid>`. `None` where there is no lock. Something there that names no
/// process is an error, never "nobody", since a lock pitboard cannot read may still be
/// held.
pub fn singleton_pid(path: &Path) -> std::io::Result<Option<u32>> {
    let target = match std::fs::read_link(path) {
        Ok(target) => target,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    // A hostname can have dashes in it, and a pid cannot.
    target
        .to_str()
        .and_then(|target| target.rsplit_once('-'))
        .and_then(|(_, pid)| pid.parse::<u32>().ok())
        .filter(|&pid| pid != 0)
        .map(Some)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} names no process", path.display()),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A secret is kept whole and no larger than the buffer sized for it, and a helper that
    /// prints more than that still finishes rather than stalling on a full pipe.
    #[test]
    fn a_secret_is_read_into_one_buffer_that_never_grows() {
        let mut short = Command::new("/bin/sh");
        short.args(["-c", "printf 'not-a-real-password'; echo oops >&2; exit 3"]);
        let (status, secret, stderr) = secret_within(short, Duration::from_secs(5)).unwrap();
        assert_eq!(status.code(), Some(3));
        assert_eq!(secret.as_slice(), b"not-a-real-password");
        assert_eq!(secret.capacity(), SECRET_LIMIT);
        assert_eq!(stderr, b"oops\n");

        let mut long = Command::new("/bin/sh");
        long.args(["-c", "head -c 200000 /dev/zero"]);
        let (status, secret, _) = secret_within(long, Duration::from_secs(5)).unwrap();
        assert!(status.success());
        assert_eq!(secret.len(), SECRET_LIMIT);
        assert_eq!(secret.capacity(), SECRET_LIMIT);
    }

    #[test]
    fn a_program_is_found_by_its_own_name_and_nothing_else() {
        let listing = "  412 /Users/a/.codex/packages/standalone/bin/codex\n\
                       7031 codex\n\
                       7032 /bin/zsh\n\
                         88 /usr/bin/codex-helper\n\
                       9001 /Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex\n\
                       9002 /Users/a/Codex Things/bin/codex\n\
                       node\n";
        let found = named(listing, "codex");
        assert_eq!(
            found.iter().map(|p| p.pid).collect::<Vec<_>>(),
            [412, 7031, 9001, 9002]
        );
        assert_eq!(
            found[3].path,
            PathBuf::from("/Users/a/Codex Things/bin/codex")
        );
        assert!(named(listing, "gemini").is_empty());
    }

    /// The list is readable on every machine these tests run on, and this test's own
    /// process is in it, found by its own name, with where it runs from.
    #[test]
    fn this_users_processes_can_be_read() {
        assert_eq!(processes("definitely-not-a-program-name"), Some(Vec::new()));
        let me = std::env::current_exe().expect("this test's own binary");
        let name = me.file_name().unwrap().to_string_lossy().into_owned();
        // Linux keeps fifteen characters of a process's name, and test binaries are longer.
        let name: String = if cfg!(target_os = "linux") {
            name.chars().take(15).collect()
        } else {
            name
        };
        let found = processes(&name).expect("readable");
        let mine = found
            .iter()
            .find(|p| p.pid == std::process::id())
            .unwrap_or_else(|| panic!("{name} is running: {found:?}"));
        assert_eq!(mine.path.file_name(), me.file_name());
    }

    /// Another user's processes use another user's login, which a switch here never
    /// touches. The first process of every machine these tests run on is the system's.
    #[test]
    fn another_users_processes_are_not_listed() {
        let first = if cfg!(target_os = "linux") {
            std::fs::read_to_string("/proc/1/comm")
                .expect("the first process")
                .trim()
                .to_string()
        } else {
            "launchd".to_string()
        };
        let owner = |pid: u32| {
            let mut ps = Command::new("ps");
            ps.args(["-o", "uid=", "-p", &pid.to_string()]);
            output_within(ps, b"", Duration::from_secs(5))
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                .unwrap_or_default()
        };
        if owner(1) == owner(std::process::id()) {
            // Run as the system's own user, as in some containers, where it is ours.
            return;
        }
        let found = processes(&first).expect("readable");
        assert!(found.iter().all(|p| p.pid != 1), "{first}: {found:?}");
    }

    /// A sample of `ps -x -o pid=,ppid=,comm=` from a Mac running Claude, with Chrome's
    /// native host for the browser extension both as Chrome starts it and as Claude does.
    const CLAUDE_LISTING: &str = "\
  501     1 /Applications/Claude.app/Contents/MacOS/Claude
  502   501 /Applications/Claude.app/Contents/Frameworks/Claude Helper (Renderer).app/Contents/MacOS/Claude Helper (Renderer)
  503   501 /Applications/Claude.app/Contents/Frameworks/Electron Framework.framework/Helpers/chrome_crashpad_handler
  590     1 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome
  600   590 /Applications/Claude.app/Contents/Helpers/chrome-native-host
  601   501 /Applications/Claude.app/Contents/Helpers/chrome-native-host
  700     1 /Applications/Claude Other.app/Contents/MacOS/Claude
  701     1 /Users/a/Downloads/Claude.app
  702     1 /usr/local/bin/claude
  703     1 Claude
  704   700 /Applications/Claude Other.app/Contents/Frameworks/Electron Framework.framework/Helpers/chrome_crashpad_handler
  705     1 /Applications/Claude 2.app/Contents/Frameworks/Claude Helper (GPU).app/Contents/MacOS/Claude Helper (GPU)
  706     1 /Applications/Notes.app/Contents/MacOS/Notes
  707     1 /Applications/Other.app/Contents/MacOS/Claude Helper
garbage
";

    #[test]
    fn a_helper_counts_and_chrome_native_host_does_not() {
        let found = within(
            &listed(CLAUDE_LISTING),
            Bundle::Named("Claude.app"),
            &["Contents/Helpers/chrome-native-host"],
        );
        // The app, its helpers wherever in the bundle they are, and a native host the app
        // itself started; never one Chrome started, which holds nothing of Claude's folder.
        // A copy renamed in the Finder is found by the program it runs, which keeps its
        // name: its main program, or a helper left after it quit, and with either everything
        // else inside it.
        assert_eq!(
            found.iter().map(|p| p.pid).collect::<Vec<_>>(),
            [501, 502, 503, 601, 700, 704, 705]
        );
        assert_eq!(
            found[1].path,
            PathBuf::from(
                "/Applications/Claude.app/Contents/Frameworks/Claude Helper (Renderer).app/\
                 Contents/MacOS/Claude Helper (Renderer)"
            )
        );
        // With nothing excluded, every process inside the bundle counts.
        assert_eq!(
            within(&listed(CLAUDE_LISTING), Bundle::Named("Claude.app"), &[]).len(),
            8
        );
        assert!(within(&listed(CLAUDE_LISTING), Bundle::Named("ChatGPT.app"), &[]).is_empty());
    }

    /// A bundle a test or an app put somewhere of its own is asked about by that path: the
    /// person's own Claude, and any other copy, run from elsewhere and do not count. Within
    /// it the same rule holds for Chrome's native host.
    #[test]
    fn a_bundle_at_a_path_counts_only_what_runs_from_there() {
        const LISTING: &str = "\
  501     1 /Applications/Claude.app/Contents/MacOS/Claude
  502   501 /Applications/Claude.app/Contents/Frameworks/Claude Helper.app/Contents/MacOS/Claude Helper
  590     1 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome
  800     1 /scratch/t/Claude.app/Contents/MacOS/Claude
  801   800 /scratch/t/Claude.app/Contents/Frameworks/Claude Helper.app/Contents/MacOS/Claude Helper
  810   590 /scratch/t/Claude.app/Contents/Helpers/chrome-native-host
  811   800 /scratch/t/Claude.app/Contents/Helpers/chrome-native-host
  820     1 /scratch/t/Claude.app.old/Contents/MacOS/Claude
  821     1 /scratch/Claude.app/Contents/MacOS/Claude
";
        let excluded = &["Contents/Helpers/chrome-native-host"];
        let pids = |bundle| {
            within(&listed(LISTING), bundle, excluded)
                .iter()
                .map(|p| p.pid)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            pids(Bundle::At(Path::new("/scratch/t/Claude.app"))),
            [800, 801, 811]
        );
        assert!(pids(Bundle::At(Path::new("/nowhere/Claude.app"))).is_empty());
        // By name, every bundle called that counts, wherever it is.
        assert_eq!(
            pids(Bundle::Named("Claude.app")),
            [501, 502, 800, 801, 811, 821]
        );
    }

    /// Read from the machine running the tests: every test binary runs from Cargo's `deps`.
    #[test]
    fn this_users_processes_within_a_folder_can_be_read() {
        assert_eq!(
            processes_within(Bundle::Named("definitely-not-a-bundle.app"), &[]),
            Some(Vec::new())
        );
        let found = processes_within(Bundle::Named("deps"), &[]).expect("readable");
        assert!(
            found.iter().any(|p| p.pid == std::process::id()),
            "{found:?}"
        );
    }

    /// Read from the machine running the tests: this test runs its own binary, and a
    /// process that has gone runs nothing.
    #[test]
    fn the_program_a_process_runs_can_be_read() {
        let me = program_of(std::process::id()).expect("readable");
        let exe = std::env::current_exe().unwrap();
        assert_eq!(me.file_name(), exe.file_name(), "{me:?}");

        let mut gone = Command::new("true").spawn().unwrap();
        let dead = gone.id();
        gone.wait().unwrap();
        assert_eq!(program_of(dead), None);
    }

    #[test]
    fn a_dead_singleton_lock_does_not_count() {
        let dir = std::env::temp_dir().join(format!(
            "pitboard-singleton-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("SingletonLock");
        assert_eq!(singleton_pid(&lock).unwrap(), None, "no lock, nobody");

        let mut gone = Command::new("true").spawn().unwrap();
        let dead = gone.id();
        gone.wait().unwrap();
        std::os::unix::fs::symlink(format!("my-mac.local-{dead}"), &lock).unwrap();
        assert_eq!(singleton_pid(&lock).unwrap(), Some(dead));
        assert!(!pid_alive(dead));

        std::fs::remove_file(&lock).unwrap();
        std::os::unix::fs::symlink(format!("my-mac-2-{}", std::process::id()), &lock).unwrap();
        assert_eq!(singleton_pid(&lock).unwrap(), Some(std::process::id()));
        assert!(pid_alive(std::process::id()));

        // Something there that names no process is not read as nobody.
        for target in ["nonsense", "host-", "host-12x"] {
            std::fs::remove_file(&lock).unwrap();
            std::os::unix::fs::symlink(target, &lock).unwrap();
            assert!(singleton_pid(&lock).is_err(), "{target}");
        }
        std::fs::remove_file(&lock).unwrap();
        std::fs::write(&lock, b"").unwrap();
        assert!(singleton_pid(&lock).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_prompt_answer_is_returned_whole() {
        let mut cat = Command::new("cat");
        cat.arg("-");
        let out = output_within(cat, b"hello", Duration::from_secs(5)).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"hello");
    }

    #[test]
    fn a_helper_that_never_answers_is_stopped_at_the_deadline() {
        let mut sleep = Command::new("sleep");
        sleep.arg("30");
        let started = Instant::now();
        let err = output_within(sleep, b"", Duration::from_millis(200)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_large_answer_cannot_stall_the_helper() {
        let mut yes = Command::new("head");
        yes.args(["-c", "1000000", "/dev/zero"]);
        let out = output_within(yes, b"", Duration::from_secs(10)).unwrap();
        assert_eq!(out.stdout.len(), 1_000_000);
    }
}
