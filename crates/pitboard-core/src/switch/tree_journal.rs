//! Finishing or undoing what an interrupted move of a folder login started.
//!
//! A tree switch writes this record before it moves anything, naming every item it may
//! move by the inode it has on each side. Which way recovery goes is decided by the state
//! alone: until the outgoing account's park is recorded there, the run is undone, item for
//! item back where it was; once it is, the run is finished. Every item is judged before any
//! is moved, by its inode, so an item found where neither side put it stops recovery with
//! nothing changed, and is never guessed at. Whatever the app made in the meantime is set
//! aside in the strays directory, never deleted, and recovery waits while the app is open.

use super::tree::{self, CONFIG_KEYS_FILE, MANIFEST_FILE};
use super::{Abandoned, Error, Recovered, Result};
use crate::atomic;
use crate::context::Context;
use crate::provider::ProviderId;
use crate::provider::desktop::config::{self, ConfigKeys};
use crate::provider::desktop::{identity, paths};
use crate::service::Warning;
use crate::state::{self, Key, State};
use crate::store::tree as moves;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

/// What kind of record this is, so one is never read as another's.
const KIND: &str = "tree";

/// The form of the record this Pitboard writes.
const VERSION: u32 = 1;

/// What the interrupted run was doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Operation {
    /// One account parked, if one was signed in, and another installed.
    Switch,
    /// The account signed in parked, and the app left signed out.
    SignOut,
}

/// One item the run may move, by its inode on each side. `None` where that side had none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Item {
    pub(super) path: String,
    pub(super) from_inode: Option<u64>,
    pub(super) to_inode: Option<u64>,
}

/// The names of the keys of `config.json` on each side. Names only: the values are the
/// account's, and stay in its park.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ConfigKeyNames {
    pub(super) from: Vec<String>,
    pub(super) to: Vec<String>,
}

/// The record of intent of a tree switch or sign-out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct TreeJournal {
    pub(super) kind: String,
    pub(super) version: u32,
    pub(super) provider: ProviderId,
    pub(super) operation: Operation,
    pub(super) started_at: i64,
    /// The data folder the run moved items of. Recovered only against that same folder.
    pub(super) support_dir: PathBuf,
    /// The volume it was on.
    pub(super) device: Option<u64>,
    /// The folder itself, by its inode on that volume, where a link to it is followed. The
    /// same path and volume can hold another folder, made again or reached by a link that
    /// now points elsewhere.
    #[serde(default)]
    pub(super) support_inode: Option<u64>,
    pub(super) from_label: Option<String>,
    pub(super) from_uuid: Option<String>,
    pub(super) from_fingerprint: Option<String>,
    pub(super) to_label: Option<String>,
    pub(super) to_uuid: Option<String>,
    pub(super) to_fingerprint: Option<String>,
    /// The park the outgoing account's items go into, made by the run.
    pub(super) from_park: Option<String>,
    /// The park the incoming account's items come from.
    pub(super) to_park: Option<String>,
    pub(super) items: Vec<Item>,
    pub(super) config_keys: ConfigKeyNames,
}

impl TreeJournal {
    /// A record of nothing yet, for the data folder `root` of `which`.
    pub(super) fn new(ctx: &Context, which: ProviderId, root: &Path) -> Result<TreeJournal> {
        Ok(TreeJournal {
            kind: KIND.into(),
            version: VERSION,
            provider: which,
            operation: Operation::Switch,
            started_at: ctx.now(),
            support_dir: root.to_path_buf(),
            device: Some(ctx.host().device_of(root).map_err(|source| {
                Error::DesktopDataInaccessible {
                    path: root.to_path_buf(),
                    source,
                }
            })?),
            support_inode: Some(std::fs::metadata(root).map(|found| found.ino()).map_err(
                |source| Error::DesktopDataInaccessible {
                    path: root.to_path_buf(),
                    source,
                },
            )?),
            from_label: None,
            from_uuid: None,
            from_fingerprint: None,
            to_label: None,
            to_uuid: None,
            to_fingerprint: None,
            from_park: None,
            to_park: None,
            items: Vec::new(),
            config_keys: ConfigKeyNames::default(),
        })
    }

    fn side(&self, label: &Option<String>) -> Option<Key> {
        label
            .as_ref()
            .map(|label| Key::new(self.provider, label.as_str()))
    }

    /// The two sides, as a sentence names them; empty for a side there is none of.
    fn named(&self, state: &State) -> (String, String) {
        let name = |key: Option<Key>| key.map(|k| state.typed(&k)).unwrap_or_default();
        (
            name(self.side(&self.from_label)),
            name(self.side(&self.to_label)),
        )
    }

    /// Everything a record must say for recovery to act on it. A record written by this
    /// Pitboard always does; one that does not was changed afterwards.
    fn check(&self) -> std::result::Result<(), String> {
        if self.kind != KIND || self.version != VERSION {
            return Err(format!(
                "it is a {} record of version {}",
                self.kind, self.version
            ));
        }
        let tree = crate::provider::of(self.provider)
            .tree()
            .ok_or("it names a tool whose login is not a folder")?;
        // The volume is what tells the folder this run moved items of from another one at the
        // same path, so a record that does not name it has nothing to check against.
        if self.device.is_none() {
            return Err("it does not say which volume it ran on".into());
        }
        if self.support_inode.is_none() {
            return Err("it does not say which folder it ran on".into());
        }
        let from = [
            self.from_label.is_some(),
            self.from_uuid.is_some(),
            self.from_fingerprint.is_some(),
            self.from_park.is_some(),
        ];
        if from.iter().any(|&b| b) && !from.iter().all(|&b| b) {
            return Err("it names part of an outgoing account".into());
        }
        let to = [
            self.to_label.is_some(),
            self.to_uuid.is_some(),
            self.to_fingerprint.is_some(),
            self.to_park.is_some(),
        ];
        match self.operation {
            Operation::SignOut if to.iter().any(|&b| b) || !from[0] => {
                return Err("a sign-out names an incoming account, or no outgoing one".into());
            }
            Operation::Switch if !to.iter().all(|&b| b) => {
                return Err("a switch names no incoming account".into());
            }
            _ => {}
        }
        for park in [&self.from_park, &self.to_park].into_iter().flatten() {
            if !one_park(park) {
                return Err(format!("`{park}` is not the name of a park of Pitboard's"));
            }
        }
        // Every item of the account is listed once: one left out would be neither moved nor
        // checked, and purged with the park it was in.
        if self.items.len() != tree.items().len() {
            return Err("it does not list each item of the account's once".into());
        }
        for known in tree.items() {
            if !self.items.iter().any(|item| item.path == known.path) {
                return Err(format!("it does not list `{}`", known.path));
            }
        }
        for item in &self.items {
            if !tree.items().iter().any(|known| known.path == item.path) {
                return Err(format!("`{}` is not an item of the account's", item.path));
            }
            if self.operation == Operation::SignOut && item.to_inode.is_some() {
                return Err("a sign-out installs nothing".into());
            }
        }
        Ok(())
    }
}

/// Whether `name` is one park's name, inside the parks directory.
fn one_park(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(_)), None)
    ) && name.starts_with(paths::PARK_PREFIX)
}

/// How recovery treats a record it cannot act on now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reconcile {
    /// The command that follows is about this tool, and cannot go ahead until it is done.
    Required,
    /// The command is about no tool in particular. A folder login it cannot settle now is
    /// left for later, so the app being open never stops anything else.
    IfQuiet,
}

fn path(ctx: &Context) -> PathBuf {
    paths::desktop_home(ctx).join("journal.json")
}

/// Durable before anything moves. Refused when a record is there already: that one is for
/// recovery, and is never written over.
pub(super) fn write(ctx: &Context, journal: &TreeJournal) -> Result<()> {
    moves::ensure_private_dir(&paths::desktop_home(ctx))?;
    let path = path(ctx);
    if std::fs::symlink_metadata(&path).is_ok() {
        return Err(Error::RecoveryFailed {
            path,
            source: std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "an interrupted switch's record is already there",
            ),
        });
    }
    let body = serde_json::to_vec_pretty(journal).expect("a record of Pitboard's serialises");
    atomic::write(&path, &body, atomic::Perms::Secret)
        .map_err(|source| Error::RecoveryFailed { path, source })
}

/// The run reached a state the state file fully describes. A record that stays is replayed
/// by every later command and refuses the next run, so one that cannot be removed is an
/// error; one already gone is not.
pub(super) fn clear(ctx: &Context) -> Result<()> {
    let path = path(ctx);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::RecoveryFailed { path, source }),
    }
}

/// The interrupted folder switch the next command about its tool will finish or undo, as
/// the two sides it names; empty for a side there is none of.
pub fn pending(ctx: &Context) -> Option<(String, String)> {
    let journal = read(ctx).ok().flatten()?;
    let name = |key: Option<Key>| key.map(|k| k.typed()).unwrap_or_default();
    Some((
        name(journal.side(&journal.from_label)),
        name(journal.side(&journal.to_label)),
    ))
}

/// Why the record of an interrupted folder switch cannot be read, where there is a record
/// and it cannot be: every command about its tool refuses until it is dealt with.
pub fn unreadable(ctx: &Context) -> Option<String> {
    read(ctx).err().map(|error| error.to_string())
}

/// The switch a run left recorded when it failed partway, said as a warning for that
/// failure. Names are as typed, the way `pending` says them.
pub fn unfinished(ctx: &Context) -> Option<Warning> {
    let journal = read(ctx).ok().flatten()?;
    let name = |key: Option<Key>| key.map(|k| k.typed()).unwrap_or_default();
    Some(Warning::SwitchUnfinished {
        tool: journal.provider,
        from: name(journal.side(&journal.from_label)),
        to: name(journal.side(&journal.to_label)),
    })
}

/// An interrupted folder switch left waiting because its app is open, said the way a
/// command that goes ahead says it, for a report that only reads and so never settles it.
/// Nothing is moved and the record is kept.
pub fn waiting(ctx: &Context, state: &State) -> Option<Warning> {
    let journal = read(ctx).ok().flatten()?;
    let tool = journal.provider;
    tree::quiet(ctx, tool).is_err().then(|| {
        let (from, to) = journal.named(state);
        Warning::RecoveryWaiting { tool, from, to }
    })
}

/// The keys kept in a park at `file`, once they are known to be the ones the journal named.
/// A park that lost one is damaged, and splicing it would replace a complete login with an
/// incomplete one that is then purged with its park, so the names are compared first.
fn kept_keys(file: &Path, named: &[String]) -> std::result::Result<config::ConfigKeys, String> {
    let keys = config::read_keys(file).map_err(|e| e.to_string())?;
    let mut expected = named.to_vec();
    expected.sort();
    if keys.0.keys().cloned().collect::<Vec<_>>() != expected {
        return Err("the keys of the config kept in the park are not the ones recorded".into());
    }
    Ok(keys)
}

/// The record, where there is one. Written atomically, so a record that does not parse, or
/// does not say what every record says, was damaged afterwards and says nothing of how far
/// its run got.
fn read(ctx: &Context) -> Result<Option<TreeJournal>> {
    let path = path(ctx);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(Error::RecoveryFailed { path, source }),
    };
    let journal: TreeJournal =
        serde_json::from_slice(&raw).map_err(|source| Error::RecoveryRecordCorrupt {
            path: path.clone(),
            source,
        })?;
    journal
        .check()
        .map_err(|why| Error::RecoveryRecordCorrupt {
            path,
            source: <serde_json::Error as serde::de::Error>::custom(why),
        })?;
    Ok(Some(journal))
}

/// One step of putting things back, decided before any is taken.
enum Step {
    /// Set aside what is at this path.
    Stray(PathBuf),
    /// Move this item from the first path to the second.
    Move(PathBuf, PathBuf),
}

/// The tool whose folder switch was interrupted, where a record of one can be read.
pub(super) fn tool(ctx: &Context) -> Option<ProviderId> {
    read(ctx).ok().flatten().map(|journal| journal.provider)
}

/// Finish or undo the interrupted run, if there was one: `Warning::Recovered` for a run
/// settled now, and in `IfQuiet` mode `Warning::RecoveryWaiting` for one left for later
/// because the app is open.
///
/// Only the app being open leaves a run for later. A record that cannot be read, one from
/// another data folder, an item in neither place or a move that fails is an error in either
/// mode: the command that follows must not go ahead as if every item were accounted for,
/// and uninstalling or repairing past an unfinished run could delete the only copy of a
/// login.
pub(super) fn reconcile(
    ctx: &Context,
    state: &mut State,
    mode: Reconcile,
) -> Result<Option<Warning>> {
    let Some(journal) = read(ctx)? else {
        return Ok(None);
    };
    match settle_journal(ctx, state, &journal) {
        Ok(recovered) => Ok(Some(Warning::Recovered(recovered))),
        Err(Error::RecoveryWaiting { tool, from, to }) if mode == Reconcile::IfQuiet => {
            Ok(Some(Warning::RecoveryWaiting { tool, from, to }))
        }
        // The app opened between two moves. What has moved is where the record says it
        // goes, so the next run picks up from there.
        Err(Error::AppStillOpen { tool, .. }) if mode == Reconcile::IfQuiet => {
            let (from, to) = journal.named(state);
            Ok(Some(Warning::RecoveryWaiting { tool, from, to }))
        }
        Err(e) => Err(e),
    }
}

fn settle_journal(ctx: &Context, state: &mut State, journal: &TreeJournal) -> Result<Recovered> {
    let which = journal.provider;
    let tree = tree::tree_of(which)?;
    let (from, to) = journal.named(state);

    // Another data folder than the one the run moved items of: the items it names are not
    // here to judge.
    let root = tree.root(ctx);
    if root.as_deref() != Some(journal.support_dir.as_path()) {
        return Err(Error::RecoveryElsewhere {
            tool: which,
            from,
            to,
            slot: journal.support_dir.display().to_string(),
        });
    }
    let root = journal.support_dir.clone();
    // The same path on another volume is another folder: an inode is only the same item on
    // one device. A volume that cannot be read now is no proof it is the same one.
    if let Some(recorded) = journal.device
        && ctx.host().device_of(&root).ok() != Some(recorded)
    {
        return Err(Error::RecoveryElsewhere {
            tool: which,
            from,
            to,
            slot: root.display().to_string(),
        });
    }
    // The same path and volume can still be another folder: one deleted and made again, or a
    // link now pointing elsewhere. A folder that cannot be read now is no proof it is the same.
    if let Some(recorded) = journal.support_inode
        && std::fs::metadata(&root).ok().map(|found| found.ino()) != Some(recorded)
    {
        return Err(Error::RecoveryElsewhere {
            tool: which,
            from,
            to,
            slot: root.display().to_string(),
        });
    }
    if tree::quiet(ctx, which).is_err() {
        return Err(Error::RecoveryWaiting {
            tool: which,
            from,
            to,
        });
    }

    let undetermined = |detail: String| Error::RecoveryUndetermined {
        tool: which,
        from: from.clone(),
        to: to.clone(),
        detail,
    };

    // Recorded as parked is the point after which the run is finished rather than undone.
    let parked = match (&journal.from_uuid, &journal.from_park) {
        (Some(uuid), Some(park)) => state
            .by_uuid(which, uuid)
            .and_then(|a| a.parked.as_ref())
            .is_some_and(|p| p.service == *park),
        _ => false,
    };
    let forward = journal.from_uuid.is_none() || parked;
    let parks = paths::parks_dir(ctx);

    if !forward {
        let from_park = parks.join(journal.from_park.as_deref().unwrap_or_default());
        refuse_linked_park(&from_park).map_err(undetermined)?;
        let mut steps = Vec::new();
        for item in &journal.items {
            let parked = tree::inode_of_live(&from_park, &item.path)?;
            let there = root.join(&item.path);
            let live = tree::inode_of_live(&root, &item.path)?;
            match (parked, live) {
                (Some(p), live) if Some(p) == item.from_inode => {
                    if live.is_some() {
                        steps.push(Step::Stray(there.clone()));
                    }
                    steps.push(Step::Move(from_park.join(&item.path), there));
                }
                (None, live) if live == item.from_inode => {}
                (None, Some(_)) if item.from_inode.is_none() => steps.push(Step::Stray(there)),
                _ => {
                    return Err(undetermined(format!(
                        "`{}` is in neither place the switch put it",
                        item.path
                    )));
                }
            }
        }
        // The keys are kept before the first item moves, so with an item to move back they
        // are there. Gone, nothing is moved: the config the app may have rewritten since
        // could not be put back, and a retry would find the items home and finish as though
        // it could.
        let kept = from_park.join(CONFIG_KEYS_FILE);
        if steps.iter().any(|step| matches!(step, Step::Move(..))) && !kept.is_file() {
            return Err(undetermined(
                "the config keys kept before the first move are gone".into(),
            ));
        }
        let config = paths::config_file(&root);
        let original = if kept.is_file() {
            Some(kept_keys(&kept, &journal.config_keys.from).map_err(undetermined)?)
        } else {
            None
        };
        let mut strays = moves::Strays::new();
        if let Some(original) = &original {
            keep_keys_of_strays(ctx, which, &mut strays, &config, &steps, original)?;
        }
        crate::fault::point("tree.recovery_steps_decided");
        take(ctx, which, &[&from_park, &root], steps, &mut strays)?;
        // The app may have rewritten its config once the items were gone, so the keys kept
        // before the first move go back with them, unless it is as it was.
        if let Some(original) = original {
            tree::still_quiet(ctx, which)?;
            if config::read_keys(&config)? != original {
                config::splice(&config, &original)?;
            }
        }
        // What is left of the park the run began is Pitboard's own records of it, and the
        // folders the moves made. Anything else in it is set aside, never deleted.
        let _ = std::fs::remove_file(from_park.join(CONFIG_KEYS_FILE));
        let _ = std::fs::remove_file(from_park.join(MANIFEST_FILE));
        remove_empty(&from_park);
        if std::fs::symlink_metadata(&from_park).is_ok() {
            strays.set_aside(ctx, &from_park)?;
        }
        clear(ctx)?;
        return Ok(Recovered {
            from,
            to,
            finished: false,
            signed_out: journal.operation == Operation::SignOut,
        });
    }

    let to_park = journal.to_park.as_deref().map(|name| parks.join(name));
    if let Some(dir) = &to_park {
        refuse_linked_park(dir).map_err(undetermined)?;
    }
    let mut steps = Vec::new();
    let mut jar_installed = false;
    for item in &journal.items {
        let there = root.join(&item.path);
        let live = tree::inode_of_live(&root, &item.path)?;
        if item.from_inode.is_some() && live == item.from_inode {
            return Err(undetermined(format!(
                "`{}` is still signed in after it was parked",
                item.path
            )));
        }
        let incoming = match &to_park {
            Some(dir) => tree::inode_of_live(dir, &item.path)?,
            None => None,
        };
        match (incoming, live) {
            (Some(i), live) if Some(i) == item.to_inode => {
                if live.is_some() {
                    steps.push(Step::Stray(there.clone()));
                }
                let from = to_park.as_ref().expect("an inode was read there");
                steps.push(Step::Move(from.join(&item.path), there));
            }
            (None, Some(l)) if Some(l) == item.to_inode => {
                jar_installed |= item.path == "Cookies";
            }
            (None, None) if item.to_inode.is_none() => {}
            (None, Some(_)) if item.to_inode.is_none() => steps.push(Step::Stray(there)),
            _ => {
                return Err(undetermined(format!(
                    "`{}` is in neither place the switch put it",
                    item.path
                )));
            }
        }
    }
    let config = paths::config_file(&root);
    if jar_installed
        && journal.operation == Operation::Switch
        && another_login_made_the_jar_theirs(ctx, journal, &root, &config, &parks)?
    {
        return Err(undetermined(
            "another account signed in while recovery was stopped".into(),
        ));
    }
    let to_key = journal.side(&journal.to_label);
    let keys = to_park.as_ref().map(|dir| dir.join(CONFIG_KEYS_FILE));
    let incoming = match (&journal.operation, keys.filter(|keys| keys.is_file())) {
        (Operation::Switch, Some(keys)) => {
            Some(kept_keys(&keys, &journal.config_keys.to).map_err(undetermined)?)
        }
        _ => None,
    };
    let mut strays = moves::Strays::new();
    if let Some(incoming) = &incoming {
        keep_keys_of_strays(ctx, which, &mut strays, &config, &steps, incoming)?;
    }
    crate::fault::point("tree.recovery_steps_decided");
    let mut bases: Vec<&Path> = vec![&root];
    bases.extend(to_park.as_deref());
    take(ctx, which, &bases, steps, &mut strays)?;
    crate::fault::point("tree.recovery_moved");

    // The app rewrites its config as it runs, so it is asked once more, as before every move.
    tree::still_quiet(ctx, which)?;
    match journal.operation {
        Operation::Switch => {
            match incoming {
                Some(incoming) => config::splice(&config, &incoming)?,
                // Spliced and the park deleted already, when the folder says so.
                None if verified(ctx, journal, &root)? => {}
                None => {
                    return Err(undetermined(
                        "the incoming account's keys of the config are gone".into(),
                    ));
                }
            }
            if !verified(ctx, journal, &root)? {
                return Err(Error::SwitchUnverified {
                    tool: which,
                    from: from.clone(),
                    to: to.clone(),
                    detail: "the data folder does not hold the session that was parked".into(),
                });
            }
            let to_key = to_key.expect("a switch names its incoming account");
            if state.get(&to_key).is_some() {
                state.set_active(which, Some(to_key.label.clone()));
                state.used(&to_key, ctx.now());
                if let Some(found) = tree.identify(ctx, &root)? {
                    tree::note_session(state, &to_key, &found);
                }
            }
            if let Some(park) = &journal.to_park {
                state.discard(park);
            }
            state::save(ctx, state)?;
        }
        Operation::SignOut => {
            config::splice(&config, &ConfigKeys::default())?;
            if tree.identify(ctx, &root)?.is_some() {
                return Err(Error::SwitchUnverified {
                    tool: which,
                    from: from.clone(),
                    to: String::new(),
                    detail: "the data folder still holds a session".into(),
                });
            }
            state.set_active(which, None);
            state::save(ctx, state)?;
            tree::write_awaiting(
                ctx,
                &tree::Awaiting {
                    from_label: journal.from_label.clone().unwrap_or_default(),
                    started_at: journal.started_at,
                },
            )?;
        }
    }
    if journal.operation == Operation::Switch {
        // Somebody is signed in again, so a sign-out waiting for one is over. Before the
        // record goes: the record is what a retry finishes this by, and a wait left behind
        // with no record would be offered for ever.
        tree::clear_awaiting(ctx)?;
    }
    clear(ctx)?;
    Ok(Recovered {
        from,
        to,
        finished: true,
        signed_out: journal.operation == Operation::SignOut,
    })
}

/// A park that is a link now is not the folder the run made: reading from it or cleaning it
/// would reach what the link points at, outside Pitboard's parks.
fn refuse_linked_park(park: &Path) -> std::result::Result<(), String> {
    match std::fs::symlink_metadata(park) {
        Ok(found) if found.file_type().is_symlink() => Err(format!(
            "the park at {} is a link, not the folder the switch made",
            park.display()
        )),
        _ => Ok(()),
    }
}

/// A jar found already installed is told by its inode, and a rewrite in place keeps that. A
/// session that is not the one the switch installed, under a config naming an account that
/// is neither side of the switch, is another login's: putting the incoming account's keys
/// over it would file one account's keys under another's session. The record is kept, and
/// `abandon` stays available.
///
/// A config that names the outgoing account is what a crash between the install and the
/// splice leaves, but also what the outgoing account signing in again writes. The park kept
/// the outgoing keys, so only a config that still holds exactly those is the crash's.
fn another_login_made_the_jar_theirs(
    ctx: &Context,
    journal: &TreeJournal,
    root: &Path,
    config: &Path,
    parks: &Path,
) -> Result<bool> {
    let Some(session) = identity::session_of(ctx, root)? else {
        return Ok(false);
    };
    if Some(&session.fingerprint) == journal.to_fingerprint.as_ref() {
        return Ok(false);
    }
    let keys = config::read_keys(config)?;
    let named = keys
        .0
        .get(paths::LAST_KNOWN_ACCOUNT_KEY)
        .and_then(serde_json::Value::as_str)
        .filter(|uuid| !uuid.is_empty());
    let Some(uuid) = named else {
        return Ok(false);
    };
    if Some(uuid) == journal.to_uuid.as_deref() {
        return Ok(false);
    }
    if Some(uuid) != journal.from_uuid.as_deref() {
        return Ok(true);
    }
    // The outgoing account's own name: its keys are told apart from a login it made since.
    let Some(park) = journal.from_park.as_deref() else {
        return Ok(false);
    };
    let kept = parks.join(park).join(CONFIG_KEYS_FILE);
    Ok(match kept_keys(&kept, &journal.config_keys.from) {
        Ok(original) => original != keys,
        Err(_) => false,
    })
}

/// The keys of the config a recovery is about to write over, set aside with the strays
/// before the first one is moved, when it sets a login's items aside. They belong to a login
/// the app made since the crash, and a session without them is only half of one. Kept first,
/// so a recovery stopped after the moves finds them kept; nothing is kept when nothing is
/// set aside, so the keys of an app that only rewrote its config are not.
fn keep_keys_of_strays(
    ctx: &Context,
    which: ProviderId,
    strays: &mut moves::Strays,
    config: &Path,
    steps: &[Step],
    incoming: &ConfigKeys,
) -> Result<()> {
    if !steps.iter().any(|step| matches!(step, Step::Stray(_))) {
        return Ok(());
    }
    tree::still_quiet(ctx, which)?;
    let left = config::read_keys(config)?;
    if !left.0.is_empty() && left != *incoming {
        let slot = strays.dir(ctx)?;
        tree::write_secret_json(&slot.join(CONFIG_KEYS_FILE), &left.0)?;
    }
    Ok(())
}

/// Whether the folder at `root` holds the incoming account's session: by its fingerprint,
/// or by its cookie jar being the very file that was parked, which the app may since have
/// written a renewed session into.
fn verified(ctx: &Context, journal: &TreeJournal, root: &Path) -> Result<bool> {
    let tree = tree::tree_of(journal.provider)?;
    let Some(found) = tree.identify(ctx, root)? else {
        return Ok(false);
    };
    if Some(&found.account_uuid) != journal.to_uuid.as_ref() {
        return Ok(false);
    }
    if Some(&found.fingerprint) == journal.to_fingerprint.as_ref() {
        return Ok(true);
    }
    let jar = journal.items.iter().find(|i| i.path == "Cookies");
    let live = tree::inode_at(&paths::cookies_db(root))?;
    Ok(jar.is_some_and(|i| i.to_inode.is_some() && i.to_inode == live))
}

/// Take the steps, each one after asking again whether the app has opened. What is set
/// aside goes into the recovery's one directory of strays.
///
/// The steps were decided from what each folder held, and a folder on the way to an item can
/// become a link before the step is taken. Every path is looked at again under the folder it
/// was decided under (`bases`), so nothing is moved to or from where a link points.
fn take(
    ctx: &Context,
    which: ProviderId,
    bases: &[&Path],
    steps: Vec<Step>,
    strays: &mut moves::Strays,
) -> Result<()> {
    let recheck = |path: &Path| -> Result<()> {
        for base in bases {
            if let Ok(relative) = path.strip_prefix(base) {
                tree::inode_of_live(base, &relative.to_string_lossy())?;
            }
        }
        Ok(())
    };
    for step in steps {
        tree::still_quiet(ctx, which)?;
        match step {
            Step::Stray(path) => {
                recheck(&path)?;
                strays.set_aside(ctx, &path)?;
            }
            Step::Move(from, to) => {
                recheck(&from)?;
                recheck(&to)?;
                tree::move_item(&from, &to)?;
            }
        }
    }
    Ok(())
}

/// Removes every empty directory under `dir`, and `dir` itself if it ends up empty. A
/// directory with anything in it is left as it is.
fn remove_empty(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            remove_empty(&entry.path());
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// Throws away a record that cannot be finished, moving nothing. Every item stays where it
/// is, in the data folder or a park, so the worst case is a park nothing names, which is
/// never deleted. A record that cannot be read is thrown away too: it was damaged after it
/// was written, and recovery would refuse it for ever.
pub(super) fn abandon(ctx: &Context, state: &mut State) -> Result<Option<Abandoned>> {
    let path = path(ctx);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(Error::RecoveryFailed { path, source }),
    }
    let Ok(Some(journal)) = read(ctx) else {
        clear(ctx)?;
        return Ok(Some(Abandoned {
            from: String::new(),
            to: String::new(),
            kept: 0,
        }));
    };
    let parks = paths::parks_dir(ctx);
    let mut kept = 0;
    for park in [&journal.from_park, &journal.to_park].into_iter().flatten() {
        let dir = parks.join(park);
        // A login, however many items it is made of, is kept once.
        kept += usize::from(
            journal
                .items
                .iter()
                .any(|item| std::fs::symlink_metadata(dir.join(&item.path)).is_ok()),
        );
    }
    let (from, to) = journal.named(state);
    clear(ctx)?;
    Ok(Some(Abandoned { from, to, kept }))
}

#[cfg(test)]
mod tests {
    use super::super::harness::{APP_PATH, NOW, TREE_POINTS, Whole, desktop_machine, jar};
    use super::*;
    use crate::fault;
    use std::collections::BTreeSet;

    /// What must be true after a killed Claude Desktop switch has been recovered, wherever
    /// it died.
    fn hold(m: &super::super::harness::DesktopMachine, at: &str, before: &BTreeSet<u64>) {
        assert!(pending(&m.ctx).is_none(), "{at}: the record is gone");
        let here = m.whole("here");
        let there = m.whole("there");
        assert!(
            matches!(
                (&here, &there),
                (Whole::Live, Whole::Parked) | (Whole::Parked, Whole::Live)
            ),
            "{at}: one account signed in and one parked, all of each, got {here:?} and {there:?}"
        );
        let after: BTreeSet<u64> = m.inodes().into_values().collect();
        assert!(
            before.is_subset(&after),
            "{at}: nothing that was there is gone: {:?}",
            before.difference(&after).collect::<Vec<_>>()
        );
        let state = state::load(&m.ctx).unwrap();
        let live = crate::provider::desktop::identity::identify_tree(&m.ctx, &m.support())
            .unwrap()
            .expect("somebody signed in");
        let active = state
            .active_for(ProviderId::Desktop)
            .expect("an active account");
        assert_eq!(
            state.get(&m.key(active)).unwrap().account_uuid,
            live.account_uuid,
            "{at}: the state says who is signed in"
        );
    }

    #[test]
    fn every_tree_point_recovers_to_one_whole_account() {
        for point in TREE_POINTS {
            let m = desktop_machine(&format!("recovers-{}", point.replace('.', "-")));
            let before: BTreeSet<u64> = m.inodes().into_values().collect();
            assert_eq!(
                m.crash_at(point).unwrap_err(),
                point,
                "the switch must reach {point}, or the case proves nothing"
            );
            let recovered = m
                .recover()
                .unwrap_or_else(|e| panic!("{point}: recovery refused: {e}"));
            assert!(recovered.is_some(), "{point}: an interrupted switch found");
            hold(&m, point, &before);
            m.recover()
                .unwrap_or_else(|e| panic!("{point}: the second recovery refused: {e}"));
            hold(&m, &format!("{point}, recovered twice"), &before);

            // And the machine still switches.
            let (settled, _) = super::super::settle(&m.ctx, Some(ProviderId::Desktop)).unwrap();
            let back = if m.whole("here") == Whole::Live {
                "there"
            } else {
                "here"
            };
            super::super::switch(settled, &m.key(back))
                .unwrap_or_else(|e| panic!("{point}: the next switch refused: {e}"));
            hold(&m, &format!("{point}, switched again"), &before);
        }
    }

    #[test]
    fn every_tree_point_waits_while_claude_runs() {
        for point in TREE_POINTS {
            let m = desktop_machine(&format!("waits-{}", point.replace('.', "-")));
            assert_eq!(m.crash_at(point).unwrap_err(), point);
            let during = m.inodes();
            m.mem.runs_within(APP_PATH);
            let waiting = m.recover().expect_err("Claude is open");
            assert!(
                matches!(waiting, Error::RecoveryWaiting { .. }),
                "{point}: {waiting:?}"
            );
            assert_eq!(m.inodes(), during, "{point}: nothing moved");
            assert!(pending(&m.ctx).is_some(), "{point}: the record is kept");

            // A command about no tool in particular is not held up by it.
            assert!(super::super::settle(&m.ctx, None).is_ok(), "{point}");
            assert!(pending(&m.ctx).is_some());

            m.mem.quits_within();
            m.recover().expect("recovered once Claude quit");
            assert!(pending(&m.ctx).is_none());
        }
    }

    #[test]
    fn every_tree_point_sends_what_claude_made_to_strays() {
        for point in TREE_POINTS {
            if point == "tree.recorded" || point == "tree.config_spliced" {
                continue;
            }
            let m = desktop_machine(&format!("strays-{}", point.replace('.', "-")));
            let before: BTreeSet<u64> = m.inodes().into_values().collect();
            assert_eq!(m.crash_at(point).unwrap_err(), point);
            // The app opened on whatever was left, and made a session of its own.
            let cookies = m.support().join("Cookies");
            if cookies.exists() {
                continue;
            }
            std::fs::write(&cookies, b"made by Claude").unwrap();
            m.mem.plant_cookies(&cookies, jar("v10made", NOW + 86_400));
            let made = moves::inode(&cookies).unwrap().unwrap();

            m.recover()
                .unwrap_or_else(|e| panic!("{point}: recovery refused: {e}"));
            assert!(m.strays_hold(made), "{point}: set aside, never deleted");
            hold(&m, point, &before);
        }
    }

    /// Claude opened on what a crash left and signed in to another account, writing that
    /// login's keys into the config as well as its session. Recovery sets the session aside
    /// and puts the keys of the config back, so the keys go aside with it: a session without
    /// the caches and the uuid is only half a login.
    #[test]
    fn every_tree_point_keeps_the_config_keys_of_a_login_claude_made() {
        for point in TREE_POINTS {
            if point == "tree.recorded" || point == "tree.config_spliced" {
                continue;
            }
            let m = desktop_machine(&format!("stray-keys-{}", point.replace('.', "-")));
            assert_eq!(m.crash_at(point).unwrap_err(), point);
            let cookies = m.support().join("Cookies");
            if cookies.exists() {
                continue;
            }
            std::fs::write(&cookies, b"made by Claude").unwrap();
            m.mem.plant_cookies(&cookies, jar("v10made", NOW + 86_400));
            let config = m.support().join("config.json");
            let mut written: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
            written["oauth:tokenCacheV2"] = serde_json::json!("cache-made-by-claude");
            std::fs::write(&config, written.to_string()).unwrap();

            m.recover()
                .unwrap_or_else(|e| panic!("{point}: recovery refused: {e}"));
            let mut kept = Vec::new();
            crate::switch::harness::files_under(
                &crate::provider::desktop::paths::strays_dir(&m.ctx),
                &mut kept,
            );
            assert!(
                kept.iter()
                    .filter(|path| path.file_name().is_some_and(|n| n == CONFIG_KEYS_FILE))
                    .any(|path| std::fs::read_to_string(path)
                        .unwrap()
                        .contains("cache-made-by-claude")),
                "{point}: the keys of the login Claude made are set aside"
            );
        }
    }

    /// A recovery stopped after it set a login's session aside, before the config was
    /// written, is retried with the session already out of the folder, so the keys are kept
    /// before the first move and not found missing by the retry.
    #[test]
    fn a_recovery_stopped_after_its_moves_has_kept_the_keys_of_a_login_claude_made() {
        let m = desktop_machine("stray-keys-restart");
        assert_eq!(
            m.crash_at("tree.park_recorded").unwrap_err(),
            "tree.park_recorded"
        );
        let cookies = m.support().join("Cookies");
        assert!(!cookies.exists(), "the live session is parked");
        std::fs::write(&cookies, b"made by Claude").unwrap();
        m.mem.plant_cookies(&cookies, jar("v10made", NOW + 86_400));
        let config = m.support().join("config.json");
        let mut written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        written["oauth:tokenCacheV2"] = serde_json::json!("cache-made-by-claude");
        std::fs::write(&config, written.to_string()).unwrap();

        let stopped = crate::fault::killing("tree.recovery_moved", || m.recover());
        assert_eq!(stopped.unwrap_err(), "tree.recovery_moved");
        m.recover().expect("the retry finishes");
        let mut kept = Vec::new();
        crate::switch::harness::files_under(
            &crate::provider::desktop::paths::strays_dir(&m.ctx),
            &mut kept,
        );
        assert!(
            kept.iter()
                .filter(|path| path.file_name().is_some_and(|n| n == CONFIG_KEYS_FILE))
                .any(|path| std::fs::read_to_string(path)
                    .unwrap()
                    .contains("cache-made-by-claude")),
            "the keys of the login Claude made are set aside"
        );
    }

    /// A jar rewritten in place keeps its inode, so the inode alone cannot say the session in
    /// it is the one the switch installed. Another account's login written over it while
    /// recovery was stopped is not put under the incoming account's keys.
    #[test]
    fn a_login_written_over_the_installed_jar_is_not_given_the_incoming_keys() {
        let m = desktop_machine("rewritten-jar");
        assert_eq!(m.crash_at("tree.installed").unwrap_err(), "tree.installed");
        let cookies = m.support().join("Cookies");
        let inode_before = std::fs::metadata(&cookies).unwrap().ino();
        std::fs::write(&cookies, b"made by Claude").unwrap();
        m.mem.plant_cookies(&cookies, jar("v10third", NOW + 86_400));
        assert_eq!(std::fs::metadata(&cookies).unwrap().ino(), inode_before);
        let config = m.support().join("config.json");
        let mut written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        written["lastKnownAccountUuid"] = serde_json::json!("third");
        written["oauth:tokenCache"] = serde_json::json!("cache-third");
        std::fs::write(&config, written.to_string()).unwrap();
        let before = std::fs::read(&config).unwrap();

        let refused = m.recover().expect_err("another account signed in");
        assert!(
            matches!(refused, Error::RecoveryUndetermined { .. }),
            "{refused:?}"
        );
        assert_eq!(
            std::fs::read(&config).unwrap(),
            before,
            "the other account's keys are left as they are"
        );
        assert!(pending(&m.ctx).is_some(), "the record is kept");
    }

    /// The account a switch left may sign in again by writing the installed jar in place. Its
    /// config then names the outgoing account too, so the name alone cannot tell it from the
    /// config the crash left: the keys it holds can, since the park kept the outgoing ones.
    #[test]
    fn the_outgoing_account_signing_back_in_is_not_given_the_incoming_keys() {
        let m = desktop_machine("rewritten-jar-by-outgoing");
        assert_eq!(m.crash_at("tree.installed").unwrap_err(), "tree.installed");
        let cookies = m.support().join("Cookies");
        std::fs::write(&cookies, b"made by Claude").unwrap();
        m.mem
            .plant_cookies(&cookies, jar("v10here-again", NOW + 86_400));
        let config = m.support().join("config.json");
        let mut written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        written["lastKnownAccountUuid"] = serde_json::json!("here");
        written["oauth:tokenCache"] = serde_json::json!("cache-here-again");
        std::fs::write(&config, written.to_string()).unwrap();
        let before = std::fs::read(&config).unwrap();

        let refused = m
            .recover()
            .expect_err("the outgoing account signed in again");
        assert!(
            matches!(refused, Error::RecoveryUndetermined { .. }),
            "{refused:?}"
        );
        assert_eq!(
            std::fs::read(&config).unwrap(),
            before,
            "the new login's keys are left as they are"
        );
        assert!(pending(&m.ctx).is_some(), "the record is kept");
    }

    /// Recovery decides its steps before it takes any, and a folder on the way to an item can
    /// become a link in between. Every move looks at the way again, so nothing is taken to or
    /// from where the link points.
    #[test]
    fn a_folder_that_became_a_link_while_recovery_ran_is_not_moved_through() {
        let m = desktop_machine("recovery-linked");
        let indexed = paths::parks_dir(&m.ctx)
            .join(m.there_park())
            .join("IndexedDB/https_claude.ai_0.indexeddb.leveldb");
        std::fs::create_dir_all(&indexed).unwrap();
        std::fs::write(indexed.join("000003.log"), "the park's").unwrap();
        assert_eq!(
            m.crash_at("tree.park_recorded").unwrap_err(),
            "tree.park_recorded"
        );
        let outside = m.support().with_file_name("outside-recovery");
        std::fs::create_dir_all(&outside).unwrap();
        let live_indexed = m.support().join("IndexedDB");
        let linked = outside.clone();
        let refused = fault::meanwhile(
            "tree.recovery_steps_decided",
            move || {
                let _ = std::fs::remove_dir_all(&live_indexed);
                std::os::unix::fs::symlink(&linked, &live_indexed).unwrap();
            },
            || m.recover(),
        )
        .expect_err("a linked folder on the way");
        assert!(
            matches!(refused, Error::DesktopDataInaccessible { .. }),
            "{refused:?}"
        );
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing was moved to where the link points"
        );
    }

    /// A folder deleted and made again at the same path, or a link pointed somewhere else, is
    /// on the same volume at the same path and is not the folder the run moved items of.
    #[test]
    fn a_data_folder_made_again_at_the_same_path_is_left_alone() {
        let m = desktop_machine("remade-folder");
        assert_eq!(
            m.crash_at("tree.item_parked").unwrap_err(),
            "tree.item_parked"
        );
        let aside = m.support().with_file_name("Claude-aside");
        std::fs::rename(m.support(), &aside).unwrap();
        std::fs::create_dir(m.support()).unwrap();
        let refused = super::super::settle(&m.ctx, Some(ProviderId::Desktop))
            .err()
            .expect("another folder at the same path");
        assert!(
            matches!(refused, Error::RecoveryElsewhere { .. }),
            "{refused:?}"
        );
        assert!(pending(&m.ctx).is_some(), "the record is kept");
        assert!(
            std::fs::read_dir(m.support()).unwrap().next().is_none(),
            "nothing was put into the other folder"
        );
    }

    #[test]
    fn a_tree_journal_from_elsewhere_is_left_alone() {
        let m = desktop_machine("elsewhere");
        assert_eq!(
            m.crash_at("tree.item_parked").unwrap_err(),
            "tree.item_parked"
        );
        let during = m.inodes();
        let ctx = m
            .ctx
            .clone()
            .with_desktop_dir(m.support().join("other").to_string_lossy().into_owned());
        let refused = super::super::settle(&ctx, Some(ProviderId::Desktop))
            .err()
            .expect("another data folder");
        assert!(
            matches!(refused, Error::RecoveryElsewhere { .. }),
            "{refused:?}"
        );
        assert_eq!(m.inodes(), during);
        assert!(pending(&m.ctx).is_some());
    }

    /// The same path on another volume is not the folder the run moved items of: inodes mean
    /// something only on one device, so what a path now holds is not judged by them.
    #[test]
    fn a_data_folder_on_another_volume_is_left_alone() {
        let m = desktop_machine("another-volume");
        m.mem.device(&m.support(), 7);
        m.mem.device(&paths::parks_dir(&m.ctx), 7);
        assert_eq!(
            m.crash_at("tree.item_parked").unwrap_err(),
            "tree.item_parked"
        );
        let during = m.inodes();
        m.mem.device(&m.support(), 8);
        let refused = super::super::settle(&m.ctx, Some(ProviderId::Desktop))
            .err()
            .expect("another volume");
        assert!(
            matches!(refused, Error::RecoveryElsewhere { .. }),
            "{refused:?}"
        );
        assert_eq!(m.inodes(), during);
        assert!(pending(&m.ctx).is_some());
    }

    #[test]
    fn a_damaged_tree_journal_is_corrupt_not_guessed() {
        let m = desktop_machine("damaged");
        assert_eq!(
            m.crash_at("tree.item_parked").unwrap_err(),
            "tree.item_parked"
        );
        let during = m.inodes();
        let record = path(&m.ctx);
        let mut journal: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();

        // An item outside the account's, which recovery would otherwise move.
        journal["items"][0]["path"] = "../../escape".into();
        std::fs::write(&record, journal.to_string()).unwrap();
        let refused = m.recover().expect_err("damaged");
        assert!(
            matches!(refused, Error::RecoveryRecordCorrupt { .. }),
            "{refused:?}"
        );

        std::fs::write(&record, b"{not json").unwrap();
        let refused = m.recover().expect_err("damaged");
        assert!(
            matches!(refused, Error::RecoveryRecordCorrupt { .. }),
            "{refused:?}"
        );
        assert_eq!(m.inodes(), during, "nothing moved on a guess");

        // Abandoning it throws the record away and moves nothing.
        let abandoned = super::super::abandon(&m.ctx).unwrap();
        assert!(abandoned.is_some());
        assert!(pending(&m.ctx).is_none());
        assert_eq!(m.inodes(), during);
    }

    /// A Desktop login is several items, so `kept` counts the logins the record names that
    /// still hold something, at most the two it moves between, not the items in them.
    #[test]
    fn abandoning_a_desktop_switch_counts_logins_not_items() {
        let m = desktop_machine("abandon-count");
        assert_eq!(
            m.crash_at("tree.item_parked").unwrap_err(),
            "tree.item_parked"
        );
        let abandoned = super::super::abandon(&m.ctx).unwrap().expect("a record");
        assert_eq!(abandoned.kept, 2);
        assert!(pending(&m.ctx).is_none());
    }

    /// A home copied from another Mac while a Desktop switch was unfinished: its record names
    /// that Mac's volume and parks, so adopting the home throws it away, with the wait that
    /// came with it, instead of leaving every later change to refuse.
    #[test]
    fn adopting_a_home_drops_the_unfinished_switch_it_brought() {
        let m = desktop_machine("adopt-journal");
        assert_eq!(
            m.crash_at("tree.item_parked").unwrap_err(),
            "tree.item_parked"
        );
        assert!(pending(&m.ctx).is_some());
        tree::write_awaiting(
            &m.ctx,
            &tree::Awaiting {
                from_label: "here".into(),
                started_at: m.ctx.now(),
            },
        )
        .unwrap();
        let record = crate::home::dir(&m.ctx).join("state.json");
        let mut whole: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
        whole["machine"] = "a hash from another computer".into();
        std::fs::write(&record, serde_json::to_vec(&whole).unwrap()).unwrap();

        super::super::adopt(&m.ctx)
            .expect("adopting")
            .expect("there was work to do");
        assert!(pending(&m.ctx).is_none(), "the record is gone");
        assert_eq!(tree::awaiting_sign_in(&m.ctx), None);
    }

    /// A wait for a sign-in that cannot be cleared keeps the record, so a retry clears it: with
    /// the record gone first, the wait stayed and the app offered to put back an account that
    /// was in use.
    #[test]
    fn a_wait_that_cannot_be_cleared_keeps_the_record_for_a_retry() {
        let m = desktop_machine("awaiting-stuck");
        assert_eq!(
            m.crash_at("tree.park_recorded").unwrap_err(),
            "tree.park_recorded"
        );
        let wait = crate::provider::desktop::paths::desktop_home(&m.ctx).join("awaiting.json");
        std::fs::create_dir_all(wait.join("stuck")).unwrap();
        m.recover().expect_err("the wait cannot be removed");
        assert!(pending(&m.ctx).is_some(), "the record is kept");
        std::fs::remove_dir_all(&wait).unwrap();
        m.recover().expect("recovered once the wait can go");
        assert!(pending(&m.ctx).is_none());
    }

    /// A recovery runs later than the switch it finishes, so a folder holding an item may
    /// have become a link in between. Recovery would move what the link points at, which is
    /// not Claude's folder, so it refuses before it moves anything, going back or forward.
    #[test]
    fn a_recovery_refuses_an_item_behind_a_folder_that_became_a_link() {
        for point in ["tree.live_parked", "tree.park_recorded"] {
            let m = desktop_machine(&format!("linked-{}", point.replace('.', "-")));
            assert_eq!(m.crash_at(point).unwrap_err(), point);
            let outside = m.support().with_file_name("outside-the-data-folder");
            let store = outside.join("https_claude.ai_0.indexeddb.leveldb");
            std::fs::create_dir_all(&store).unwrap();
            std::fs::write(store.join("CURRENT"), "not Claude's").unwrap();
            let indexed = m.support().join("IndexedDB");
            let _ = std::fs::remove_dir_all(&indexed);
            std::os::unix::fs::symlink(&outside, &indexed).unwrap();

            let refused = m.recover().expect_err("a linked folder");
            assert!(
                matches!(refused, Error::DesktopDataInaccessible { .. }),
                "{point}: {refused:?}"
            );
            assert!(pending(&m.ctx).is_some(), "{point}: the record is kept");
            assert!(
                store.join("CURRENT").exists(),
                "{point}: what the link points at stays"
            );
        }
    }

    /// A record that lost an item, or lists one twice, would have recovery neither move nor
    /// check that item, and then purge it with the park: it is corrupt, not recovered.
    #[test]
    fn a_tree_journal_missing_or_repeating_an_item_is_corrupt() {
        let m = desktop_machine("incomplete");
        assert_eq!(
            m.crash_at("tree.item_parked").unwrap_err(),
            "tree.item_parked"
        );
        let during = m.inodes();
        let record = path(&m.ctx);
        let whole: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
        let count = whole["items"].as_array().unwrap().len();
        assert!(count > 1, "a tree has several items");

        let mut lost = whole.clone();
        lost["items"].as_array_mut().unwrap().pop();
        let mut twice = whole.clone();
        let first = twice["items"][0].clone();
        *twice["items"].as_array_mut().unwrap().last_mut().unwrap() = first;
        for damaged in [lost, twice] {
            std::fs::write(&record, damaged.to_string()).unwrap();
            let refused = m.recover().expect_err("damaged");
            assert!(
                matches!(refused, Error::RecoveryRecordCorrupt { .. }),
                "{refused:?}"
            );
            assert_eq!(m.inodes(), during, "nothing moved on a guess");
        }
    }

    /// The volume is what lets recovery tell the folder the run moved items of from another one
    /// at the same path, so a record that does not name it is corrupt, not one to check
    /// nothing against.
    #[test]
    fn a_tree_journal_that_names_no_volume_is_corrupt() {
        let m = desktop_machine("no-volume");
        assert_eq!(
            m.crash_at("tree.item_parked").unwrap_err(),
            "tree.item_parked"
        );
        let during = m.inodes();
        let record = path(&m.ctx);
        let mut whole: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
        assert!(whole["device"].is_number(), "a run records its volume");
        whole["device"] = serde_json::Value::Null;
        std::fs::write(&record, whole.to_string()).unwrap();
        let refused = m.recover().expect_err("no volume to check against");
        assert!(
            matches!(refused, Error::RecoveryRecordCorrupt { .. }),
            "{refused:?}"
        );
        assert_eq!(m.inodes(), during, "nothing moved on a guess");
    }

    /// The record is what says a run is unfinished, so a run is not finished while it is still
    /// there: a record that cannot be removed is an error, and one already gone is not.
    #[test]
    fn a_record_that_cannot_be_removed_is_an_error() {
        let m = desktop_machine("clear-fails");
        assert!(clear(&m.ctx).is_ok(), "nothing to remove is not a failure");
        let record = path(&m.ctx);
        std::fs::create_dir_all(record.join("not-a-file")).unwrap();
        let refused = clear(&m.ctx).expect_err("a directory is not removed as a file");
        assert!(
            matches!(refused, Error::RecoveryFailed { .. }),
            "{refused:?}"
        );
    }

    /// A record that cannot be looked up is not a record that is not there: abandoning says
    /// so, rather than reporting nothing to abandon while the record goes on blocking.
    #[test]
    fn abandoning_a_record_that_cannot_be_looked_up_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let m = desktop_machine("abandon-lookup-fails");
        assert_eq!(
            m.crash_at("tree.item_parked").unwrap_err(),
            "tree.item_parked"
        );
        let home = paths::desktop_home(&m.ctx);
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o000)).unwrap();
        let refused = super::super::abandon(&m.ctx);
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        refused.expect_err("the record could not be looked up");
        assert!(path(&m.ctx).is_file(), "the record is still there");
    }

    /// The keys kept before the first move are what put the config back when a switch is
    /// undone. Gone, with an item still to move back, recovery would complete without them
    /// and pair the returned session with whatever the app wrote: it refuses, and moves
    /// nothing.
    #[test]
    fn undoing_a_park_whose_kept_config_keys_are_gone_is_refused() {
        let m = desktop_machine("undo-keys-gone");
        assert_eq!(
            m.crash_at("tree.item_parked").unwrap_err(),
            "tree.item_parked"
        );
        let during = m.inodes();
        let journal = read(&m.ctx).unwrap().expect("a journal");
        let kept = paths::parks_dir(&m.ctx)
            .join(journal.from_park.expect("an outgoing park"))
            .join(CONFIG_KEYS_FILE);
        std::fs::remove_file(&kept).unwrap();
        let refused = m.recover().expect_err("the keys are gone");
        assert!(
            matches!(refused, Error::RecoveryUndetermined { .. }),
            "{refused:?}"
        );
        assert_eq!(m.inodes(), during, "nothing moved on a guess");
        assert!(path(&m.ctx).is_file(), "the record is kept");
    }

    /// Somebody is signed in once a switch is recovered forward, so a sign-out that an add
    /// was waiting on is over, as it is when the switch runs through.
    #[test]
    fn finishing_a_switch_ends_the_sign_in_wait() {
        let m = desktop_machine("finish-ends-wait");
        assert_eq!(
            m.crash_at("tree.park_recorded").unwrap_err(),
            "tree.park_recorded"
        );
        tree::write_awaiting(
            &m.ctx,
            &tree::Awaiting {
                from_label: "here".into(),
                started_at: NOW,
            },
        )
        .unwrap();
        let recovered = m.recover().expect("recovered").expect("found");
        assert!(recovered.finished);
        assert_eq!(tree::awaiting_sign_in(&m.ctx), None);
    }

    #[test]
    fn an_item_in_neither_place_is_undetermined() {
        let m = desktop_machine("neither");
        assert_eq!(
            m.crash_at("tree.live_parked").unwrap_err(),
            "tree.live_parked"
        );
        // Something took `Session Storage` out of the park the run made.
        let journal = read(&m.ctx).unwrap().unwrap();
        let park = paths::parks_dir(&m.ctx).join(journal.from_park.unwrap());
        let gone = m.support().join("elsewhere");
        std::fs::rename(park.join("Session Storage"), &gone).unwrap();
        let during = m.inodes();
        let refused = m.recover().expect_err("undetermined");
        assert!(
            matches!(refused, Error::RecoveryUndetermined { .. }),
            "{refused:?}"
        );
        assert_eq!(
            m.inodes(),
            during,
            "nothing moved while one item is unknown"
        );
        assert!(pending(&m.ctx).is_some());
    }

    /// A park replaced by a link after the record was written is somewhere else: moving
    /// from it or cleaning it would take or delete what the link points at.
    #[test]
    fn a_park_replaced_by_a_link_is_not_read_or_cleaned() {
        let m = desktop_machine("linked-park");
        assert_eq!(
            m.crash_at("tree.live_parked").unwrap_err(),
            "tree.live_parked"
        );
        let journal = read(&m.ctx).unwrap().unwrap();
        let park = paths::parks_dir(&m.ctx).join(journal.from_park.unwrap());
        let outside = m.support().with_file_name("outside-park");
        std::fs::rename(&park, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &park).unwrap();
        let mut before = Vec::new();
        crate::switch::harness::files_under(&outside, &mut before);

        let refused = m.recover().expect_err("a linked park");
        assert!(
            matches!(refused, Error::RecoveryUndetermined { .. }),
            "{refused:?}"
        );
        let mut after = Vec::new();
        crate::switch::harness::files_under(&outside, &mut after);
        assert_eq!(after, before, "what the link points at is untouched");
        assert!(pending(&m.ctx).is_some(), "the record is kept");
    }

    #[test]
    fn a_desktop_interruption_never_blocks_claude_code() {
        let m = desktop_machine("never-blocks");
        assert_eq!(
            m.crash_at("tree.item_parked").unwrap_err(),
            "tree.item_parked"
        );
        m.mem.runs_within(APP_PATH);
        assert!(super::super::settle(&m.ctx, Some(ProviderId::Claude)).is_ok());
        assert!(super::super::settle(&m.ctx, Some(ProviderId::Codex)).is_ok());
        assert!(
            pending(&m.ctx).is_some(),
            "left for a Claude Desktop command"
        );
    }

    /// A command about no tool in particular goes ahead while Claude is open, and says the
    /// interrupted run is waiting rather than saying nothing.
    #[test]
    fn a_command_about_no_tool_warns_while_claude_runs() {
        let m = desktop_machine("no-tool-waiting");
        assert_eq!(
            m.crash_at("tree.item_parked").unwrap_err(),
            "tree.item_parked"
        );
        m.mem.runs_within(APP_PATH);
        let (_, found) =
            super::super::settle(&m.ctx, None).expect("a command about no tool goes ahead");
        assert!(
            matches!(
                found.as_slice(),
                [Warning::RecoveryWaiting { tool: ProviderId::Desktop, from, to }]
                    if from == "desktop/here" && to == "desktop/there"
            ),
            "{found:?}"
        );
        assert_eq!(
            pending(&m.ctx),
            Some(("desktop/here".to_string(), "desktop/there".to_string())),
            "what status reports as waiting"
        );
    }

    /// Recovery writes the config last, and Claude opening after the items are back in place
    /// leaves the config to Claude: the run waits for the next one, which finishes it.
    #[test]
    fn claude_starting_before_recovery_writes_the_config_leaves_it_alone() {
        let m = desktop_machine("recovery-before-config");
        assert_eq!(m.crash_at("tree.installed").unwrap_err(), "tree.installed");
        let config = m.support().join("config.json");
        let before = std::fs::read(&config).unwrap();
        let mem = std::sync::Arc::clone(&m.mem);
        let refused = crate::fault::meanwhile(
            "tree.recovery_moved",
            move || {
                mem.runs_within(APP_PATH);
            },
            || m.recover(),
        )
        .expect_err("the app opened before the config");
        assert!(matches!(refused, Error::AppStillOpen { .. }), "{refused:?}");
        assert_eq!(refused.code(), "app_opened_midway", "{refused:?}");
        assert!(
            !refused.to_string().contains("nothing was moved"),
            "{refused}"
        );
        assert_eq!(
            std::fs::read(&config).unwrap(),
            before,
            "the config is untouched"
        );
        assert!(
            pending(&m.ctx).is_some(),
            "the record stays for the next run"
        );

        m.mem.quits_within();
        let recovered = m
            .recover()
            .expect("recovered")
            .expect("the interrupted switch was found");
        assert!(recovered.finished, "finished, not undone");
        assert_eq!(m.whole("there"), Whole::Live);
        assert_eq!(m.whole("here"), Whole::Parked);
    }

    /// Only the app being open leaves a run for later. A record that cannot be trusted stops
    /// a command about no tool too, rather than letting it go ahead as if every item were
    /// accounted for.
    #[test]
    fn a_damaged_tree_journal_stops_a_command_about_no_tool() {
        let m = desktop_machine("no-tool-damaged");
        assert_eq!(
            m.crash_at("tree.item_parked").unwrap_err(),
            "tree.item_parked"
        );
        let during = m.inodes();
        std::fs::write(path(&m.ctx), b"{not json").unwrap();
        let refused = super::super::settle(&m.ctx, None)
            .err()
            .expect("a damaged record");
        assert!(
            matches!(refused, Error::RecoveryRecordCorrupt { .. }),
            "{refused:?}"
        );
        assert_eq!(m.inodes(), during);
    }

    /// An interrupted switch of Claude Code and one of Claude Desktop settled by the same
    /// command are both reported.
    #[test]
    fn both_interrupted_runs_are_reported() {
        let m = desktop_machine("both");
        assert_eq!(
            m.crash_at("tree.park_recorded").unwrap_err(),
            "tree.park_recorded"
        );
        // Claude Code killed switching from `a` to `b` before its park was stored, with `a`
        // still signed in.
        let live = super::super::harness::document("a-refresh");
        let claude = crate::provider::of(ProviderId::Claude);
        m.mem.live().plant(
            &crate::provider::claude::paths::live_service(&m.ctx),
            &live.to_string(),
        );
        let vault = super::super::journal::Journal {
            provider: ProviderId::Claude,
            started_at: NOW,
            from_label: "a".into(),
            from_uuid: "a".into(),
            to_label: "b".into(),
            to_uuid: "b".into(),
            park_service: "pitboard-a-1760000000000".into(),
            incoming_service: "pitboard-b-1759999999000".into(),
            from_fingerprint: claude.fingerprint(&live),
            to_fingerprint: "another".into(),
            slot: None,
        };
        super::super::journal::write_journal(&m.ctx, &vault).unwrap();

        let (_, found) = super::super::settle(&m.ctx, Some(ProviderId::Desktop)).unwrap();
        let recovered: Vec<_> = found
            .iter()
            .filter_map(|w| match w {
                Warning::Recovered(r) => Some((r.to.as_str(), r.finished)),
                _ => None,
            })
            .collect();
        assert_eq!(
            recovered,
            [("b", false), ("desktop/there", true)],
            "{found:?}"
        );
        assert!(!super::super::interrupted(&m.ctx));
        assert!(pending(&m.ctx).is_none());
    }

    /// An interrupted sign-out is reported as one, with the account it was signing out as
    /// its subject, and its tool is known before it is recovered.
    #[test]
    fn an_interrupted_sign_out_is_reported_as_one() {
        let m = desktop_machine("sign-out-recovered");
        let killed = crate::fault::killing("tree.config_spliced", || {
            let (settled, _) = super::super::settle(&m.ctx, Some(ProviderId::Desktop))?;
            super::super::sign_out(settled, ProviderId::Desktop)
        });
        assert_eq!(killed.unwrap_err(), "tree.config_spliced");
        assert_eq!(
            super::super::interrupted_tool(&m.ctx),
            Some(ProviderId::Desktop)
        );

        let recovered = m.recover().unwrap().expect("an interrupted sign-out");
        assert!(recovered.signed_out && recovered.finished, "{recovered:?}");
        assert_eq!(recovered.subject(), "desktop/here -> signed out");
        assert_eq!(
            recovered.to_string(),
            "an earlier sign-out of `desktop/here` was interrupted; it had in fact finished, \
             and Pitboard has recorded that"
        );
        assert_eq!(super::super::interrupted_tool(&m.ctx), None);
        assert_eq!(m.whole("here"), super::super::harness::Whole::Parked);
    }

    /// A park whose kept keys lost one the journal recorded is damaged: splicing it in would
    /// replace the complete login with an incomplete one, and the park would then be purged.
    /// Recovery refuses, on either side, and leaves the park where it is.
    #[test]
    fn recovery_refuses_kept_config_keys_that_lost_one_the_journal_names() {
        let lose_a_key = |file: &Path| {
            let mut keys: serde_json::Value =
                serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
            let object = keys.as_object_mut().unwrap();
            let token_cache = object.keys().next_back().unwrap().clone();
            object.remove(&token_cache);
            std::fs::write(file, keys.to_string()).unwrap();
        };

        // Finishing: the incoming account's keys.
        let m = desktop_machine("finish-lost-key");
        assert_eq!(
            m.crash_at("tree.park_recorded").unwrap_err(),
            "tree.park_recorded"
        );
        let kept = paths::parks_dir(&m.ctx)
            .join(m.there_park())
            .join(CONFIG_KEYS_FILE);
        lose_a_key(&kept);
        let refused = m.recover().expect_err("the keys are incomplete");
        assert!(
            matches!(refused, Error::RecoveryUndetermined { .. }),
            "{refused:?}"
        );
        assert!(kept.is_file(), "the park is kept");

        // Undoing: the outgoing account's keys.
        let m = desktop_machine("undo-lost-key");
        assert_eq!(
            m.crash_at("tree.item_parked").unwrap_err(),
            "tree.item_parked"
        );
        let journal = read(&m.ctx).unwrap().expect("a journal");
        let kept = paths::parks_dir(&m.ctx)
            .join(journal.from_park.expect("an outgoing park"))
            .join(CONFIG_KEYS_FILE);
        lose_a_key(&kept);
        let refused = m.recover().expect_err("the keys are incomplete");
        assert!(
            matches!(refused, Error::RecoveryUndetermined { .. }),
            "{refused:?}"
        );
        assert!(kept.is_file(), "the park is kept");
    }
}
