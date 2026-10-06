//! Moving a login kept in a folder: Claude Desktop's, which is items of its data folder and
//! three keys of its `config.json`.
//!
//! The same two rules as a credential switch set the order of every step, kept the way a
//! folder can keep them. Nothing moves while the app has the folder open, so every move is
//! preceded by asking what is running. And what is parked is durable and recorded before
//! anything is installed in its place: until the outgoing account's park is in the state, a
//! run that dies is undone; after it, the run is finished. Nothing found in the data folder
//! is ever deleted: what is in the way of a move is set aside in the strays directory.

use super::tree_journal::{self, ConfigKeyNames, Item, Operation, TreeJournal};
use super::{Enrolled, Outcome, Settled, purge};
use crate::context::Context;
use crate::error::{Error, Result};
use crate::provider::desktop::TreeIdentity;
use crate::provider::desktop::config::{self, ConfigKeys};
use crate::provider::desktop::identity::{self, LiveOwner};
use crate::provider::desktop::paths;
use crate::provider::{self, ProviderId, TreeLogin};
use crate::service::Warning;
use crate::state::{self, Account, Detail, Key, Park, State};
use crate::store::tree as moves;
use crate::{atomic, fault, holder};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Component, Path, PathBuf};

/// The account's keys of `config.json`, as a park keeps them.
pub(super) const CONFIG_KEYS_FILE: &str = "config-keys.json";

/// What a park says it holds.
pub(super) const MANIFEST_FILE: &str = "manifest.json";

/// A parked login kept for less than this is said to be running out, since nothing can
/// renew it.
const EXPIRES_SOON: i64 = 7 * 86_400;

/// What a park holds, written beside it when it is made. The session is told by its
/// fingerprint, never by the cookie.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Manifest {
    pub account_uuid: String,
    pub fingerprint: String,
    pub items: Vec<String>,
    pub parked_at: i64,
}

/// A sign-out Pitboard made, waiting for somebody to sign in to another account in the app
/// and enrol it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Awaiting {
    /// The account that was parked, as its label; empty where Claude was already signed out
    /// and nobody was parked.
    pub from_label: String,
    pub started_at: i64,
}

/// The sign-out Pitboard made that no enrolment has followed yet, where there is one.
pub fn awaiting_sign_in(ctx: &Context) -> Option<Awaiting> {
    let raw = std::fs::read_to_string(awaiting_path(ctx)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn awaiting_path(ctx: &Context) -> PathBuf {
    paths::desktop_home(ctx).join("awaiting.json")
}

pub(super) fn write_awaiting(ctx: &Context, awaiting: &Awaiting) -> Result<()> {
    moves::ensure_private_dir(&paths::desktop_home(ctx))?;
    write_secret_json(&awaiting_path(ctx), awaiting)
}

pub(super) fn clear_awaiting(ctx: &Context) -> Result<()> {
    let path = awaiting_path(ctx);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        // Not a record of a switch: no file is half moved when it stays.
        Err(source) => Err(Error::HomeUnwritable { path, source }),
    }
}

/// Points a wait that names the account `from` at `to`, and returns what it said before,
/// where it changed. A rename follows it, so an add reopened afterwards does not offer to put
/// back an account that has no such name; forgetting the account empties it, as when nobody
/// was parked. A wait that names another account, or nobody, is left as it is.
pub(super) fn retarget_awaiting(ctx: &Context, from: &str, to: &str) -> Result<Option<Awaiting>> {
    match awaiting_sign_in(ctx) {
        Some(wait) if wait.from_label == from => {
            write_awaiting(
                ctx,
                &Awaiting {
                    from_label: to.to_string(),
                    ..wait.clone()
                },
            )?;
            Ok(Some(wait))
        }
        _ => Ok(None),
    }
}

/// Puts a wait back as `retarget_awaiting` found it, for a change that was not saved after
/// all. The error that made it necessary is the one worth reporting, so this one is not.
pub(super) fn restore_awaiting(ctx: &Context, before: Option<Awaiting>) {
    if let Some(wait) = before {
        let _ = write_awaiting(ctx, &wait);
    }
}

/// The tool's folder login, or a usage error for a tool whose login is a credential.
pub(super) fn tree_of(which: ProviderId) -> Result<&'static dyn TreeLogin> {
    provider::of(which).tree().ok_or_else(|| {
        Error::Usage(format!(
            "{} keeps its login as a credential, not in a folder",
            which.name()
        ))
    })
}

/// Refuses while anything has the tool's folder open: a process from its bundle, a lock in
/// the folder naming a live process, or a process list that could not be read, which counts
/// as open.
pub(super) fn quiet(ctx: &Context, which: ProviderId) -> Result<()> {
    let tree = tree_of(which)?;
    let open = |pids: Vec<u32>, detail: Option<String>| Error::AppStillOpen {
        tool: which,
        name: which.program().to_string(),
        pids,
        detail,
        midway: false,
    };
    match holder::find_within(ctx, tree) {
        None => {
            return Err(open(
                Vec::new(),
                Some("the process list could not be read".into()),
            ));
        }
        Some(holding) if !holding.is_empty() => {
            return Err(open(
                holding
                    .iter()
                    .flat_map(|h| h.pids.iter().copied())
                    .collect(),
                None,
            ));
        }
        Some(_) => {}
    }
    // The lock is a second sign beside the process list, for a build that keeps one; the
    // build measured keeps none (experiment E14), and its absence passes.
    if let Some(root) = tree.root(ctx) {
        match holder::lock_holder(ctx, tree, &root) {
            Ok(None) => {}
            Ok(Some(pid)) => return Err(open(vec![pid], None)),
            Err(e) => {
                return Err(open(
                    Vec::new(),
                    Some(format!(
                        "its lock in the data folder could not be read: {e}"
                    )),
                ));
            }
        }
    }
    Ok(())
}

/// [`quiet`], asked once the record of intent is written: a refusal now leaves part of the
/// move made, and says so rather than that nothing moved.
pub(super) fn still_quiet(ctx: &Context, which: ProviderId) -> Result<()> {
    quiet(ctx, which).map_err(|e| match e {
        Error::AppStillOpen {
            tool,
            name,
            pids,
            detail,
            ..
        } => Error::AppStillOpen {
            tool,
            name,
            pids,
            detail,
            midway: true,
        },
        other => other,
    })
}

/// The live data folder, made ready for a move: there, quiet, and on one volume with a
/// private parks directory, which is pinned where it was checked.
fn prepare(
    ctx: &Context,
    which: ProviderId,
) -> Result<(&'static dyn TreeLogin, PathBuf, Anchored)> {
    let tree = tree_of(which)?;
    let root = tree
        .root(ctx)
        .filter(|root| root.is_dir())
        .ok_or(Error::LiveCredentialAbsent { tool: which })?;
    quiet(ctx, which)?;
    moves::ensure_private_dir(&paths::desktop_home(ctx))?;
    let parks = paths::parks_dir(ctx);
    moves::ensure_private_dir(&parks)?;
    moves::same_device(ctx, &root, &parks)?;
    Ok((tree, root, Anchored::at(&parks)))
}

/// The inode at `path` in the data folder or a park, or why it could not be read.
pub(super) fn inode_at(path: &Path) -> Result<Option<u64>> {
    moves::inode(path).map_err(|source| Error::DesktopDataInaccessible {
        path: path.to_path_buf(),
        source,
    })
}

/// The inode of `item` in the data folder at `root`, once no folder on the way to it is a
/// link: a move would take what the link points at, which is not Claude's folder. The item
/// itself may be one, and is moved as it is.
pub(super) fn inode_of_live(root: &Path, item: &str) -> Result<Option<u64>> {
    let mut walked = root.to_path_buf();
    for part in Path::new(item)
        .parent()
        .into_iter()
        .flat_map(Path::components)
    {
        walked.push(part);
        if std::fs::symlink_metadata(&walked).is_ok_and(|found| found.file_type().is_symlink()) {
            return Err(Error::DesktopDataInaccessible {
                path: walked,
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "a folder holding a login's item is a link",
                ),
            });
        }
    }
    inode_at(&root.join(item))
}

/// A folder a run decides its steps under, with where it really was then.
pub(super) struct Anchored {
    pub(super) path: PathBuf,
    real: Option<PathBuf>,
    /// The device and inode of the folder itself: one deleted and made again at the same
    /// path resolves to the same path and is not the folder the steps were decided under.
    identity: Option<(u64, u64)>,
}

impl Anchored {
    pub(super) fn at(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            real: std::fs::canonicalize(path).ok(),
            identity: identity_of(path),
        }
    }

    /// Refuses a folder that is somewhere else now, as a link put in its place is.
    pub(super) fn still_there(&self) -> Result<()> {
        if std::fs::canonicalize(&self.path).ok() == self.real
            && identity_of(&self.path) == self.identity
        {
            return Ok(());
        }
        Err(Error::DesktopDataInaccessible {
            path: self.path.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the folder is not where it was when the steps were decided",
            ),
        })
    }
}

/// The device and inode of the folder at `path`, or of what a link at it points at.
fn identity_of(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .ok()
        .map(|found| (found.dev(), found.ino()))
}

/// Refuses `path` unless it is a folder and not a link to one: what a link points at is not
/// the folder Pitboard made.
fn refuse_linked_folder(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(found) if found.is_dir() => Ok(()),
        Ok(_) => Err(Error::DesktopDataInaccessible {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a folder Pitboard made is a link",
            ),
        }),
        Err(source) => Err(Error::DesktopDataInaccessible {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// One item moved by one rename. A move made and then not synced is an error too, though
/// the item is where it was moved to: it is not on disk yet, so the run stops with its
/// record, which settles by where each item is, rather than carry on to delete the record
/// and the park behind a move a power cut could take back.
pub(super) fn move_item(from: &Path, to: &Path) -> Result<u64> {
    moves::rename_durably(from, to).map_err(|e| Error::DesktopDataInaccessible {
        path: match moves::moved_not_synced(&e) {
            Some(moved) => moved.to.clone(),
            None => from.to_path_buf(),
        },
        source: e,
    })
}

/// `value` as JSON at `path`, readable by this user only.
pub(super) fn write_secret_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let body = serde_json::to_vec_pretty(value).expect("a record of Pitboard's serialises");
    atomic::write(path, &body, atomic::Perms::Secret).map_err(|source| Error::HomeUnwritable {
        path: path.to_path_buf(),
        source,
    })
}

/// Record the session `found` as the one `key`'s account was last seen with.
pub(super) fn note_session(state: &mut State, key: &Key, found: &TreeIdentity) {
    let Some(mut account) = state.get(key).cloned() else {
        return;
    };
    let organization_uuid = match &account.detail {
        Detail::Desktop {
            organization_uuid, ..
        } => organization_uuid.clone(),
        _ => None,
    };
    account.detail = Detail::Desktop {
        organization_uuid,
        session_fingerprint: found.fingerprint.clone(),
        session_expires_at: found.expires_at,
    };
    state.upsert(account);
}

/// Keep `organization` as the one a claude.ai reading of the Desktop account `uuid` was
/// asked with, so a jar that later names none still has one to ask with. Written only while
/// no other Pitboard run holds the lock and no switch waits to be finished, like a renewal:
/// it is a note, and never worth holding up or racing a change of the accounts.
pub(crate) fn note_organization(ctx: &Context, uuid: &str, organization: &str) {
    let Some(_exclusive) = super::try_exclusive(ctx) else {
        return;
    };
    if tree_journal::pending(ctx).is_some() {
        return;
    }
    let Ok(mut state) = state::load(ctx) else {
        return;
    };
    let Some(mut account) = state.by_uuid(ProviderId::Desktop, uuid).cloned() else {
        return;
    };
    let Detail::Desktop {
        organization_uuid, ..
    } = &mut account.detail
    else {
        return;
    };
    if organization_uuid.as_deref() == Some(organization) {
        return;
    }
    *organization_uuid = Some(organization.to_string());
    state.upsert(account);
    let _ = state::save(ctx, &state);
}

/// A name for a new park of the account `uuid`, free in the parks directory.
fn park_name(ctx: &Context, uuid: &str) -> String {
    let safe: String = uuid
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let parks = paths::parks_dir(ctx);
    let mut at = ctx.now_millis();
    loop {
        let name = format!("{}{safe}-{at}", paths::PARK_PREFIX);
        if std::fs::symlink_metadata(parks.join(&name)).is_err() {
            return name;
        }
        at += 1;
    }
}

/// The park `park` of `account`, checked before anything is written: one park's name, a
/// private directory, holding the account's session and keys.
pub(crate) fn check_park(
    ctx: &Context,
    label: &str,
    account: &Account,
    park: &Park,
) -> Result<PathBuf> {
    let corrupt = |detail: &str| Error::ParkedCredentialCorrupt {
        label: label.to_string(),
        detail: detail.to_string(),
    };
    let mut parts = Path::new(&park.service).components();
    let one_name = matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(_)), None)
    );
    if !one_name || !park.service.starts_with(paths::PARK_PREFIX) {
        return Err(corrupt("its name is not one of Pitboard's parks"));
    }
    let dir = paths::parks_dir(ctx).join(&park.service);
    let found = match std::fs::symlink_metadata(&dir) {
        Ok(found) => found,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::ParkedCredentialMissing {
                label: label.to_string(),
            });
        }
        Err(source) => return Err(Error::HomeUnwritable { path: dir, source }),
    };
    if found.file_type().is_symlink() || !found.is_dir() {
        return Err(corrupt("it is not a directory"));
    }
    if found.mode() & 0o077 != 0 {
        return Err(corrupt("others can reach it"));
    }
    let manifest: Manifest = std::fs::read_to_string(dir.join(MANIFEST_FILE))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .ok_or_else(|| corrupt("it says nothing of whose it is"))?;
    if manifest.account_uuid != account.account_uuid {
        return Err(Error::ParkedLoginBelongsElsewhere {
            label: label.to_string(),
            email: "another account".into(),
        });
    }
    if !dir.join(CONFIG_KEYS_FILE).is_file() {
        return Err(corrupt("the keys of its config are missing"));
    }
    // These keys are spliced in after the files move, and the run then checks the folder says
    // whose it is. Keys of another account, or of none, fail that check on every try.
    let keys = config::read_keys(&dir.join(CONFIG_KEYS_FILE))
        .map_err(|_| corrupt("the keys of its config cannot be read"))?;
    if keys
        .0
        .get(paths::LAST_KNOWN_ACCOUNT_KEY)
        .and_then(serde_json::Value::as_str)
        != Some(account.account_uuid.as_str())
    {
        return Err(corrupt("the keys of its config are not its account's"));
    }
    // Every item the manifest lists was in the park when it was made; one gone is a login
    // that would be installed incomplete.
    for item in &manifest.items {
        let inside = Path::new(item)
            .components()
            .all(|part| matches!(part, Component::Normal(_)));
        if !inside || std::fs::symlink_metadata(dir.join(item)).is_err() {
            return Err(corrupt("an item it holds is missing"));
        }
        // A link in the middle of the way would have the move take what it points at, which
        // is somewhere else. The item itself may be one: it is moved as it is.
        let mut walked = dir.clone();
        for part in Path::new(item)
            .parent()
            .into_iter()
            .flat_map(Path::components)
        {
            walked.push(part);
            if std::fs::symlink_metadata(&walked).is_ok_and(|found| found.file_type().is_symlink())
            {
                return Err(corrupt("an item it holds is behind a link"));
            }
        }
    }
    match identity::session_of(ctx, &dir)? {
        Some(session)
            if session.fingerprint == manifest.fingerprint
                && session.fingerprint == park.refresh_fingerprint => {}
        _ => return Err(corrupt("its session is not the one Pitboard parked")),
    }
    Ok(dir)
}

/// The keys of `config.json` an outgoing account has, by name only.
fn key_names(keys: &ConfigKeys) -> Vec<String> {
    keys.0.keys().cloned().collect()
}

/// Enrol the account signed in to the app now. Nothing moves, so no journal is written.
pub(super) fn enroll_current(
    ctx: &Context,
    key: &Key,
    state: &mut State,
) -> Result<(Enrolled, Vec<Warning>)> {
    let which = key.provider;
    let tree = tree_of(which)?;
    let root = tree
        .root(ctx)
        .ok_or(Error::LiveCredentialAbsent { tool: which })?;
    // Quit first, so what the app keeps in memory of the sign-in is on disk.
    quiet(ctx, which)?;
    let live = tree
        .identify(ctx, &root)?
        .ok_or(Error::LiveCredentialAbsent { tool: which })?;
    let owner = match identity::whose(state, Some(live.clone())) {
        // Enrolling under the label it is unconfirmed for is the confirmation.
        Err(Error::DesktopIdentityUnconfirmed { label })
            if label == key.label
                && state
                    .get(key)
                    .is_some_and(|a| a.account_uuid == live.account_uuid) =>
        {
            LiveOwner::Enrolled(key.clone())
        }
        other => other?,
    };
    let previous = state.active_for(which).map(str::to_string);
    let enrolled = match owner {
        LiveOwner::Nobody => return Err(Error::LiveCredentialAbsent { tool: which }),
        LiveOwner::Enrolled(found) if found != *key => {
            return Err(Error::AlreadyEnrolled {
                tool: which,
                email: "the account in Claude".into(),
                label: found.typed(),
            });
        }
        LiveOwner::Enrolled(_) => {
            // A park of the account signed in now is an older session of it, put aside
            // before somebody signed in to it again in the app: lapsed, or about to be. It
            // must not stay beside the login in use, so it goes.
            if let Some(old) = state.get(key).and_then(|a| a.parked.clone()) {
                state.discard(&old.service);
            }
            note_session(state, key, &live);
            state.used(key, ctx.now());
            Enrolled::InUse {
                email: String::new(),
                again: true,
            }
        }
        LiveOwner::NotEnrolled(found) => {
            if let Some(taken) = state.get(key)
                && taken.account_uuid != found.account_uuid
            {
                return Err(Error::LabelTaken {
                    label: key.typed(),
                    email: if taken.email.is_empty() {
                        "another account".into()
                    } else {
                        taken.email.clone()
                    },
                });
            }
            state.upsert(Account {
                label: key.label.clone(),
                account_uuid: found.account_uuid.clone(),
                email: String::new(),
                parked: None,
                last_used_at: Some(ctx.now()),
                detail: Detail::Desktop {
                    organization_uuid: None,
                    session_fingerprint: found.fingerprint.clone(),
                    session_expires_at: found.expires_at,
                },
            });
            Enrolled::Current {
                email: String::new(),
            }
        }
    };
    let mut warnings = Vec::new();
    if let Some(previous) = previous.filter(|label| *label != key.label) {
        let previous = Key::new(which, previous);
        if state.get(&previous).is_some_and(|a| a.parked.is_none()) {
            warnings.push(Warning::ReplacedOutsidePitboard {
                tool: which,
                label: state.typed(&previous),
            });
        }
    }
    state.set_active(which, Some(key.label.clone()));
    state::save(ctx, state)?;
    clear_awaiting(ctx)?;
    let pending = purge(ctx, state);
    warnings.extend((pending > 0).then_some(Warning::ParksPendingRemoval(pending)));
    Ok((enrolled, warnings))
}

/// The items `journal` says are the outgoing account's, moved into a new park, with its
/// keys and manifest written beside them, checked, and recorded in the state. Steps S2 to
/// S6 of a switch, and of a sign-out.
fn park_out(
    ctx: &Context,
    which: ProviderId,
    folders: [&Anchored; 2],
    state: &mut State,
    from_key: &Key,
    outgoing: &TreeIdentity,
    journal: &TreeJournal,
) -> Result<Park> {
    // The data folder and the parks directory, pinned where they were checked.
    let [anchored_root, parks_root] = folders;
    let root = anchored_root.path.as_path();
    let name = journal
        .from_park
        .clone()
        .expect("a journal with an outgoing account names its park");
    let parks = paths::parks_dir(ctx);
    let dir = parks.join(&name);
    let unwritable = |path: &Path| {
        let path = path.to_path_buf();
        move |source| Error::HomeUnwritable { path, source }
    };
    // The parks directory is the one that was checked: the park is made in it, not where a
    // link put in its place points.
    parks_root.still_there()?;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(unwritable(&dir))?;
    moves::fsync_dir(&parks).map_err(unwritable(&parks))?;
    // Pinned as soon as it is made: a link put in its place since is not the folder made, and
    // what is written to it, or anchored from it, would be where the link points.
    fault::point("tree.park_made");
    refuse_linked_folder(&dir)?;
    let dir_anchor = Anchored::at(&dir);

    // The keys of the config as the account left them, kept before anything moves: the app
    // may rewrite its config once the items are gone, and an undo puts these back.
    // The data folder is looked at again first: a link put in its place since it was
    // checked would give the keys of whatever it points at.
    anchored_root.still_there()?;
    let keys = config::read_keys(&paths::config_file(root))?;
    dir_anchor.still_there()?;
    write_secret_json(&dir.join(CONFIG_KEYS_FILE), &keys.0)?;

    let anchors = [anchored_root, parks_root, &dir_anchor];
    let mut moved = Vec::new();
    for item in journal.items.iter().filter(|i| i.from_inode.is_some()) {
        // Asked again before every move: the app may have been opened since the last.
        still_quiet(ctx, which)?;
        // A folder on the way may have become a link since the record was made, and a
        // rename follows it.
        inode_of_live(root, &item.path)?;
        fault::point("tree.park_move_checked");
        // The folders themselves are looked at again: a link put in the place of either is
        // not on the way to an item, and a rename follows it.
        anchors.iter().try_for_each(|anchor| anchor.still_there())?;
        // And the way inside the park: a folder made there for an earlier item can have
        // become a link, which the rename would put this one through.
        inode_of_live(&dir, &item.path)?;
        move_item(&root.join(&item.path), &dir.join(&item.path))?;
        moved.push(item.path.clone());
        if moved.len() == 1 {
            fault::point("tree.item_parked");
        }
    }
    fault::point("tree.live_parked");

    // The park itself, again: a link put in its place now would take the manifest to where
    // it points, with the login left in the folder it displaced.
    dir_anchor.still_there()?;
    write_secret_json(
        &dir.join(MANIFEST_FILE),
        &Manifest {
            account_uuid: journal.from_uuid.clone().unwrap_or_default(),
            fingerprint: outgoing.fingerprint.clone(),
            items: moved,
            parked_at: ctx.now(),
        },
    )?;
    fault::point("tree.park_stored");

    // The park, once more: a link put in its place now would make the checks below read the
    // folder it points at, and the account would be recorded under a link.
    dir_anchor.still_there()?;

    // The park is the session that was signed in, item for item, before it is recorded as
    // the account's.
    let parked_session = identity::session_of(ctx, &dir)?;
    let mut whole = parked_session.is_some_and(|s| s.fingerprint == outgoing.fingerprint);
    for item in journal.items.iter().filter(|i| i.from_inode.is_some()) {
        whole &= inode_at(&dir.join(&item.path))? == item.from_inode;
    }
    if !whole {
        return Err(Error::SwitchUnverified {
            tool: which,
            from: state.typed(from_key),
            to: journal
                .to_label
                .as_ref()
                .map(|label| state.typed(&Key::new(which, label.as_str())))
                .unwrap_or_default(),
            detail: "what was parked is not what was signed in".into(),
        });
    }

    // Recording the park is the point after which a run is finished rather than undone, so
    // the app is asked about once more first: opened since the last move, it may have
    // written to a folder whose items are gone, and that run must be undone.
    fault::point("tree.park_verified");
    still_quiet(ctx, which)?;
    // The park is looked at once more, as late as it can be: what was verified above is the
    // folder that is recorded, not one a link put there since.
    dir_anchor.still_there()?;
    note_session(state, from_key, outgoing);
    let park = Park {
        service: name,
        parked_at: ctx.now(),
        refresh_fingerprint: outgoing.fingerprint.clone(),
        access_expires_at: outgoing.expires_at,
        refresh_expires_at: outgoing.expires_at,
    };
    state.park(from_key, park.clone());
    state::save(ctx, state)?;
    fault::point("tree.park_recorded");
    Ok(park)
}

/// Every Claude Desktop park running out within a week, which nothing can renew. One that
/// has already lapsed is not running out: its row says so, and a switch refuses it.
pub(crate) fn expiring(ctx: &Context, state: &State, which: ProviderId) -> Vec<Warning> {
    state
        .accounts
        .iter()
        .filter(|a| a.provider() == which)
        .filter_map(|a| {
            let expires_at = a.parked.as_ref()?.refresh_expires_at?;
            (expires_at > ctx.now() && expires_at < ctx.now() + EXPIRES_SOON).then(|| {
                Warning::ParkExpiresSoon {
                    label: state.typed(&a.key()),
                    expires_at,
                }
            })
        })
        .collect()
}

/// Switch the app's folder to `key`'s account: the account signed in now is parked, and
/// `key`'s park is moved into its place.
pub(super) fn switch(settled: Settled, key: &Key) -> Result<(Outcome, Vec<Warning>)> {
    let Settled {
        _exclusive,
        mut state,
        ctx,
    } = settled;
    let ctx = &ctx;
    let which = key.provider;
    let target = state
        .get(key)
        .cloned()
        .ok_or_else(|| Error::AccountUnknown {
            label: key.typed(),
            enrolled: state.labels(which),
        })?;

    // A verified account already in use needs no move or repair. Reading its identity is
    // safe while the app is open; every path that writes still goes through prepare.
    if target.parked.is_none() && state.active_for(which) == Some(key.label.as_str()) {
        let tree = tree_of(which)?;
        if let Some(root) = tree.root(ctx)
            && matches!(
                identity::whose(&state, tree.identify(ctx, &root)?)?,
                LiveOwner::Enrolled(found) if found == *key
            )
        {
            // Signed in as this account, so a sign-out waiting for one is over.
            clear_awaiting(ctx)?;
            return Ok((
                Outcome::AlreadyActive {
                    label: state.typed(key),
                },
                Vec::new(),
            ));
        }
    }

    // S0: nothing written yet.
    let (tree, root, parks_anchor) = prepare(ctx, which)?;
    let live = tree.identify(ctx, &root)?;
    let from_key = match identity::whose(&state, live.clone())? {
        LiveOwner::Enrolled(found) if found == *key => {
            // A park of the account in use is an older copy of it, as when enrolling it
            // again: it must not stay beside the login in use, so it goes.
            let old = target.parked.as_ref().map(|p| p.service.clone());
            let mut warnings = Vec::new();
            if old.is_some() || state.active_for(which) != Some(key.label.as_str()) {
                if let Some(old) = &old {
                    state.discard(old);
                }
                if let Some(live) = &live {
                    note_session(&mut state, key, live);
                }
                state.set_active(which, Some(key.label.clone()));
                state.used(key, ctx.now());
                state::save(ctx, &state)?;
            }
            clear_awaiting(ctx)?;
            if old.is_some() {
                let pending = purge(ctx, &mut state);
                warnings.extend((pending > 0).then_some(Warning::ParksPendingRemoval(pending)));
            }
            return Ok((
                Outcome::AlreadyActive {
                    label: state.typed(key),
                },
                warnings,
            ));
        }
        LiveOwner::Enrolled(found) => Some(found),
        LiveOwner::Nobody => None,
        LiveOwner::NotEnrolled(_) => {
            return Err(Error::LiveAccountNotEnrolled {
                tool: which,
                email: "the account in Claude".into(),
            });
        }
    };
    let to = state.typed(key);
    let held = target.parked.clone().ok_or_else(|| Error::NothingParked {
        tool: which,
        label: to.clone(),
    })?;
    if !held.restorable_at(ctx.now()) {
        return Err(Error::ParkedLoginExpired { label: to });
    }
    let incoming = check_park(ctx, &to, &target, &held)?;
    // Pinned where the park was checked and the folder was read, before the record and the
    // outgoing account's move: a link put in the place of either since is not where they were.
    let root_anchor = Anchored::at(&root);
    let incoming_anchor = Anchored::at(&incoming);
    let incoming_keys = config::read_keys(&incoming.join(CONFIG_KEYS_FILE))?;
    let from = from_key.as_ref().map(|k| state.typed(k));
    let from_uuid = from_key
        .as_ref()
        .and_then(|k| state.get(k))
        .map(|a| a.account_uuid.clone());

    // S1: the record of intent, naming every item by the inode it has now.
    let mut items = Vec::new();
    for item in tree.items() {
        // Looked at with no one signed in as well: what the folder holds is set aside then.
        let live = inode_of_live(&root, item.path)?;
        let from_inode = from_key.as_ref().and(live);
        items.push(Item {
            path: item.path.to_string(),
            from_inode,
            to_inode: inode_at(&incoming.join(item.path))?,
        });
    }
    let live_keys = config::read_keys(&paths::config_file(&root))?;
    let journal = TreeJournal {
        operation: Operation::Switch,
        from_label: from_key.as_ref().map(|k| k.label.clone()),
        from_uuid: from_uuid.clone(),
        from_fingerprint: live
            .as_ref()
            .filter(|_| from_key.is_some())
            .map(|l| l.fingerprint.clone()),
        to_label: Some(key.label.clone()),
        to_uuid: Some(target.account_uuid.clone()),
        to_fingerprint: Some(held.refresh_fingerprint.clone()),
        from_park: from_uuid.as_deref().map(|uuid| park_name(ctx, uuid)),
        to_park: Some(held.service.clone()),
        items,
        config_keys: ConfigKeyNames {
            from: key_names(&live_keys),
            to: key_names(&incoming_keys),
        },
        ..TreeJournal::new(ctx, which, &root)?
    };
    tree_journal::write(ctx, &journal)?;
    fault::point("tree.journal_written");

    // S2 to S6: the outgoing account parked and recorded. After this, a run that dies is
    // finished rather than undone.
    let parked = match (&from_key, &live) {
        (Some(from_key), Some(outgoing)) => Some(park_out(
            ctx,
            which,
            [&root_anchor, &parks_anchor],
            &mut state,
            from_key,
            outgoing,
            &journal,
        )?),
        _ => None,
    };

    // S8: the incoming account's items into the folder, and whatever the app made there
    // since the outgoing account left set aside.
    let mut strays = 0;
    // Everything this switch sets aside shares one directory under the strays directory.
    let mut set_aside = moves::Strays::new();
    let mut installed = 0;
    let anchors = [root_anchor, incoming_anchor];
    for item in &journal.items {
        still_quiet(ctx, which)?;
        let there = root.join(&item.path);
        inode_of_live(&root, &item.path)?;
        fault::point("tree.install_checked");
        anchors.iter().try_for_each(Anchored::still_there)?;
        if inode_at(&there)?.is_some() {
            set_aside.set_aside(ctx, &there)?;
            strays += 1;
        }
        if item.to_inode.is_some() {
            // The park was checked before the record was written; a folder inside it that
            // became a link since would have the rename take what the link points at.
            inode_of_live(&incoming, &item.path)?;
            anchors.iter().try_for_each(Anchored::still_there)?;
            move_item(&incoming.join(&item.path), &there)?;
            installed += 1;
            if installed == 1 {
                fault::point("tree.item_installed");
            }
        }
    }
    fault::point("tree.installed");

    // S9: the account's keys of the config. The app rewrites its config as it runs, so it
    // is asked once more, as before every move. A folder nobody was signed in to can still
    // hold keys the app wrote since, which no park holds, so they are set aside with the
    // strays rather than written over.
    still_quiet(ctx, which)?;
    // The config is read and written under the folder, which is looked at once more.
    anchors.iter().try_for_each(Anchored::still_there)?;
    // With an outgoing account the park holds its keys, so what is left is only new when it
    // is not those either: a login the app made between the park and the install.
    {
        let left = config::read_keys(&paths::config_file(&root))?;
        if !left.0.is_empty() && left != incoming_keys && (from_key.is_none() || left != live_keys)
        {
            let slot = set_aside.dir(ctx)?;
            write_secret_json(&slot.join(CONFIG_KEYS_FILE), &left.0)?;
            strays += 1;
        }
    }
    anchors.iter().try_for_each(Anchored::still_there)?;
    config::splice(&paths::config_file(&root), &incoming_keys)?;
    fault::point("tree.config_spliced");

    // S10: the folder is the incoming account's. When it cannot be told, the record of
    // intent stays, and the next run finishes this.
    let unverified = |detail: String| Error::SwitchUnverified {
        tool: which,
        from: from.clone().unwrap_or_default(),
        to: to.clone(),
        detail,
    };
    let now_live = match tree.identify(ctx, &root) {
        Ok(Some(found))
            if found.account_uuid == target.account_uuid
                && found.fingerprint == held.refresh_fingerprint =>
        {
            found
        }
        Ok(_) => {
            return Err(unverified(
                "the data folder does not hold the session that was parked".into(),
            ));
        }
        Err(e) => return Err(unverified(e.to_string())),
    };

    // S11: recorded.
    state.set_active(which, Some(key.label.clone()));
    state.discard(&held.service);
    state.used(key, ctx.now());
    note_session(&mut state, key, &now_live);
    state::save(ctx, &state)?;
    fault::point("tree.recorded");

    // S12: done, and the incoming park's leftovers deleted. Somebody is signed in again, so
    // a sign-out waiting for one is over. The wait goes first: a wait that cannot go leaves
    // the record, which a retry finishes from, rather than a switch nobody can finish.
    clear_awaiting(ctx)?;
    tree_journal::clear(ctx)?;
    let pending = purge(ctx, &mut state);

    let warnings = (strays > 0)
        .then(|| Warning::StraysKept {
            path: paths::strays_dir(ctx),
            count: strays,
        })
        .into_iter()
        .chain(expiring(ctx, &state, which))
        .chain((pending > 0).then_some(Warning::ParksPendingRemoval(pending)))
        .collect();
    let adoption = provider::of(which).adoption();
    let outcome = match (from, parked) {
        (Some(from), Some(parked)) => Outcome::Switched {
            provider: which,
            from,
            to,
            parked,
            adoption,
        },
        _ => Outcome::Installed {
            provider: which,
            to,
            adoption,
        },
    };
    Ok((outcome, warnings))
}

/// Park the account signed in to the app and leave it signed out, so another account can be
/// signed in to there and enrolled, without signing out in the app, which may end the
/// session for good.
pub fn sign_out(settled: Settled, which: ProviderId) -> Result<(Outcome, Vec<Warning>)> {
    let Settled {
        _exclusive,
        mut state,
        ctx,
    } = settled;
    let ctx = &ctx;
    let (tree, root, parks_anchor) = prepare(ctx, which)?;
    let live = tree.identify(ctx, &root)?;
    let from_key = match identity::whose(&state, live.clone())? {
        LiveOwner::Nobody => {
            // Nobody to put back, but somebody is about to sign in and be enrolled, and a
            // restart of the app in between must find that waiting. A wait that already
            // names a parked account keeps it.
            if awaiting_sign_in(ctx).is_none() {
                write_awaiting(
                    ctx,
                    &Awaiting {
                        from_label: String::new(),
                        started_at: ctx.now(),
                    },
                )?;
            }
            return Ok((Outcome::AlreadySignedOut { provider: which }, Vec::new()));
        }
        LiveOwner::NotEnrolled(_) => {
            return Err(Error::LiveAccountNotEnrolled {
                tool: which,
                email: "the account in Claude".into(),
            });
        }
        LiveOwner::Enrolled(found) => found,
    };
    let outgoing = live.expect("an enrolled owner is somebody signed in");
    let from = state.typed(&from_key);
    let root_anchor = Anchored::at(&root);

    let mut items = Vec::new();
    for item in tree.items() {
        items.push(Item {
            path: item.path.to_string(),
            from_inode: inode_of_live(&root, item.path)?,
            to_inode: None,
        });
    }
    let live_keys = config::read_keys(&paths::config_file(&root))?;
    let journal = TreeJournal {
        operation: Operation::SignOut,
        from_label: Some(from_key.label.clone()),
        from_uuid: Some(outgoing.account_uuid.clone()),
        from_fingerprint: Some(outgoing.fingerprint.clone()),
        from_park: Some(park_name(ctx, &outgoing.account_uuid)),
        items,
        config_keys: ConfigKeyNames {
            from: key_names(&live_keys),
            to: Vec::new(),
        },
        ..TreeJournal::new(ctx, which, &root)?
    };
    tree_journal::write(ctx, &journal)?;
    fault::point("tree.journal_written");

    let parked = park_out(
        ctx,
        which,
        [&root_anchor, &parks_anchor],
        &mut state,
        &from_key,
        &outgoing,
        &journal,
    )?;
    still_quiet(ctx, which)?;
    root_anchor.still_there()?;
    config::splice(&paths::config_file(&root), &ConfigKeys::default())?;
    fault::point("tree.config_spliced");
    match tree.identify(ctx, &root) {
        Ok(None) => {}
        Ok(Some(_)) => {
            return Err(Error::SwitchUnverified {
                tool: which,
                from,
                to: String::new(),
                detail: "the data folder still holds a session".into(),
            });
        }
        Err(e) => {
            return Err(Error::SwitchUnverified {
                tool: which,
                from,
                to: String::new(),
                detail: e.to_string(),
            });
        }
    }

    state.set_active(which, None);
    state::save(ctx, &state)?;
    fault::point("tree.recorded");
    write_awaiting(
        ctx,
        &Awaiting {
            from_label: from_key.label.clone(),
            started_at: ctx.now(),
        },
    )?;
    tree_journal::clear(ctx)?;
    let pending = purge(ctx, &mut state);
    let warnings = (pending > 0)
        .then_some(Warning::ParksPendingRemoval(pending))
        .into_iter()
        .collect();
    Ok((
        Outcome::SignedOut {
            provider: which,
            from,
            parked,
            adoption: provider::of(which).adoption(),
        },
        warnings,
    ))
}

#[cfg(test)]
mod tests {
    use super::super::harness::{APP_PATH, DesktopMachine, NOW, desktop_machine, jar};
    use super::*;

    /// A refusal after the record of intent was written, which must not say that nothing
    /// moved: something did, and the next run finishes or undoes it.
    fn assert_stopped_partway(refused: &Error) {
        assert_eq!(refused.code(), "app_opened_midway", "{refused:?}");
        assert_eq!(refused.exit_code(), 3);
        let said = refused.to_string();
        assert!(!said.contains("nothing was moved"), "{said}");
        assert!(said.contains("partway"), "{said}");
    }

    fn here(m: &DesktopMachine) -> Key {
        m.key("here")
    }

    fn there(m: &DesktopMachine) -> Key {
        m.key("there")
    }

    fn switch_to(m: &DesktopMachine, label: &str) -> Result<(Outcome, Vec<Warning>)> {
        let (settled, _) = super::super::settle(&m.ctx, Some(ProviderId::Desktop))?;
        super::super::switch(settled, &m.key(label))
    }

    fn signing_out(m: &DesktopMachine) -> Result<(Outcome, Vec<Warning>)> {
        let (settled, _) = super::super::settle(&m.ctx, Some(ProviderId::Desktop))?;
        sign_out(settled, ProviderId::Desktop)
    }

    fn enrolling(m: &DesktopMachine, label: &str) -> Result<(Enrolled, Vec<Warning>)> {
        let (settled, _) = super::super::settle(&m.ctx, Some(ProviderId::Desktop))?;
        super::super::enroll(settled, &m.key(label), None)
    }

    #[test]
    fn a_switch_moves_only_the_allowlist() {
        let m = desktop_machine("allowlist");
        let support = m.support();
        let config_before: serde_json::Value =
            serde_json::from_slice(&std::fs::read(support.join("config.json")).unwrap()).unwrap();
        let untouched = [
            "claude_desktop_config.json",
            "plan-usage-history.json",
            "Cache/data_0",
            "IndexedDB/https_other.example_0.indexeddb.leveldb/000003.log",
        ];
        // Dated well before the switch, so a rewrite in it shows however coarse the clock.
        let long_ago = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        for path in untouched {
            std::fs::File::options()
                .write(true)
                .open(support.join(path))
                .unwrap()
                .set_modified(long_ago)
                .unwrap();
        }
        let seen = |path: &str| {
            let at = support.join(path);
            (
                moves::inode(&at).unwrap(),
                std::fs::metadata(&at).unwrap().modified().unwrap(),
                std::fs::read(&at).unwrap(),
            )
        };
        let before: Vec<_> = untouched.iter().map(|p| seen(p)).collect();

        let (outcome, warnings) = switch_to(&m, "there").expect("the switch");
        assert!(
            matches!(&outcome, Outcome::Switched { from, to, .. } if from == "desktop/here" && to == "desktop/there"),
            "{outcome:?}"
        );
        assert!(warnings.is_empty(), "{warnings:?}");

        for (path, before) in untouched.iter().zip(before) {
            assert_eq!(
                seen(path),
                before,
                "{path} belongs to the machine: same file, same contents, never written"
            );
        }
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(support.join("config.json")).unwrap()).unwrap();
        assert_eq!(config["lastKnownAccountUuid"], "there");
        assert_eq!(config["oauth:tokenCache"], "cache-there");
        assert_eq!(config["locale"], config_before["locale"]);

        let found = identity::identify_tree(&m.ctx, &support).unwrap().unwrap();
        assert_eq!(found.account_uuid, "there");
        let state = state::load(&m.ctx).unwrap();
        assert_eq!(state.active_for(ProviderId::Desktop), Some("there"));
        assert!(state.get(&there(&m)).unwrap().parked.is_none());
        let park = state.get(&here(&m)).unwrap().parked.clone().unwrap();
        let dir = paths::parks_dir(&m.ctx).join(&park.service);
        for item in [
            "Cookies",
            "Cookies-journal",
            "Local Storage",
            "Session Storage",
            "IndexedDB/https_claude.ai_0.indexeddb.leveldb",
            "WebStorage",
        ] {
            assert!(dir.join(item).exists(), "{item} is parked");
        }
        let keys = config::read_keys(&dir.join(CONFIG_KEYS_FILE)).unwrap();
        assert_eq!(keys.0["lastKnownAccountUuid"], "here");
        assert!(
            !paths::parks_dir(&m.ctx).join(m.there_park()).exists(),
            "the installed park is gone"
        );
        assert!(tree_journal::pending(&m.ctx).is_none());
    }

    #[test]
    fn nothing_moves_while_claude_runs() {
        let m = desktop_machine("runs");
        let before = m.inodes();
        let pid = m.mem.runs_within(APP_PATH);
        let refused = switch_to(&m, "there").expect_err("Claude is open");
        assert!(
            matches!(&refused, Error::AppStillOpen { pids, detail: None, .. } if *pids == vec![pid]),
            "{refused:?}"
        );
        assert_eq!(refused.code(), "app_still_open");
        assert!(
            refused.to_string().contains("nothing was moved"),
            "{refused}"
        );
        assert_eq!(m.inodes(), before);
        assert!(tree_journal::pending(&m.ctx).is_none());
    }

    #[test]
    fn nothing_moves_when_the_process_list_cannot_be_read() {
        let m = desktop_machine("no-list");
        let before = m.inodes();
        m.mem.process_list_fails();
        let refused = switch_to(&m, "there").expect_err("counted as open");
        assert!(
            matches!(
                &refused,
                Error::AppStillOpen {
                    detail: Some(_),
                    ..
                }
            ),
            "{refused:?}"
        );
        assert_eq!(m.inodes(), before);
        // Not knowing is not the same as Claude being open, and the app says it apart.
        assert_eq!(refused.code(), "app_state_unknown");
        assert!(
            refused
                .to_string()
                .contains("could not list what is running"),
            "{refused}"
        );
    }

    /// The data folder or the park can become a link between the look at the way to an item
    /// and the rename, which then takes the item to or from where the link points.
    #[test]
    fn a_park_that_became_a_link_before_a_move_is_not_moved_into() {
        let m = desktop_machine("park-linked-midway");
        let outside = m.support().with_file_name("outside-park");
        std::fs::create_dir_all(&outside).unwrap();
        let parks = paths::parks_dir(&m.ctx);
        let there = m.there_park();
        let (linked, in_parks) = (outside.clone(), parks.clone());
        let refused = fault::meanwhile(
            "tree.park_move_checked",
            move || {
                for entry in std::fs::read_dir(&in_parks).unwrap().flatten() {
                    if entry.file_name().to_string_lossy() != there {
                        std::fs::remove_dir_all(entry.path()).unwrap();
                        std::os::unix::fs::symlink(&linked, entry.path()).unwrap();
                    }
                }
            },
            || switch_to(&m, "there"),
        )
        .expect_err("the new park became a link");
        assert!(
            matches!(refused, Error::DesktopDataInaccessible { .. }),
            "{refused:?}"
        );
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing was moved to where the link points"
        );
    }

    #[test]
    fn an_incoming_park_that_became_a_link_before_a_move_is_not_moved_out_of() {
        let m = desktop_machine("incoming-linked-midway");
        let aside = m.support().with_file_name("incoming-aside");
        let incoming = paths::parks_dir(&m.ctx).join(m.there_park());
        let (moved_to, path) = (aside.clone(), incoming.clone());
        let refused = fault::meanwhile(
            "tree.install_checked",
            move || {
                std::fs::rename(&path, &moved_to).unwrap();
                std::os::unix::fs::symlink(&moved_to, &path).unwrap();
            },
            || switch_to(&m, "there"),
        )
        .expect_err("the incoming park became a link");
        assert!(
            matches!(refused, Error::DesktopDataInaccessible { .. }),
            "{refused:?}"
        );
    }

    /// The park the switch checked can be replaced by a link while the record is written and
    /// the outgoing account is parked, long before the first item is installed.
    #[test]
    fn an_incoming_park_that_became_a_link_before_the_record_is_not_moved_out_of() {
        let m = desktop_machine("incoming-linked-early");
        let aside = m.support().with_file_name("incoming-aside-early");
        let incoming = paths::parks_dir(&m.ctx).join(m.there_park());
        let (moved_to, path) = (aside.clone(), incoming.clone());
        let refused = fault::meanwhile(
            "tree.journal_written",
            move || {
                std::fs::rename(&path, &moved_to).unwrap();
                std::os::unix::fs::symlink(&moved_to, &path).unwrap();
            },
            || switch_to(&m, "there"),
        )
        .expect_err("the incoming park became a link");
        assert!(
            matches!(refused, Error::DesktopDataInaccessible { .. }),
            "{refused:?}"
        );
    }

    /// The data folder can be replaced by a link after the last item is installed, and the
    /// config of whatever the link points at is not Claude's.
    #[test]
    fn a_data_folder_that_became_a_link_before_the_config_is_not_written_through() {
        let m = desktop_machine("root-linked-before-config");
        let outside = m.support().with_file_name("outside-config");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("config.json"), "{\"locale\":\"elsewhere\"}").unwrap();
        let (support, linked) = (m.support(), outside.clone());
        let refused = fault::meanwhile(
            "tree.installed",
            move || {
                let aside = support.with_file_name("Claude-displaced");
                std::fs::rename(&support, &aside).unwrap();
                std::os::unix::fs::symlink(&linked, &support).unwrap();
            },
            || switch_to(&m, "there"),
        )
        .expect_err("the data folder became a link");
        assert!(
            matches!(refused, Error::DesktopDataInaccessible { .. }),
            "{refused:?}"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("config.json")).unwrap(),
            "{\"locale\":\"elsewhere\"}",
            "the config where the link points is left alone"
        );
    }

    /// A folder deleted and made again at the same path resolves to the same path.
    #[test]
    fn a_data_folder_made_again_before_a_move_is_not_moved_into() {
        let m = desktop_machine("root-made-again");
        let support = m.support();
        let aside = support.with_file_name("Claude-made-again-aside");
        let refused = fault::meanwhile(
            "tree.install_checked",
            move || {
                std::fs::rename(&support, &aside).unwrap();
                std::fs::create_dir(&support).unwrap();
            },
            || switch_to(&m, "there"),
        )
        .expect_err("the data folder is another folder now");
        assert!(
            matches!(refused, Error::DesktopDataInaccessible { .. }),
            "{refused:?}"
        );
        assert!(
            std::fs::read_dir(m.support()).unwrap().next().is_none(),
            "nothing was installed into the other folder"
        );
    }

    /// A folder made in the new park for an earlier item can become a link before a later
    /// one is moved through it.
    #[test]
    fn a_folder_in_the_new_park_that_became_a_link_is_not_moved_through() {
        let m = desktop_machine("park-parent-linked");
        let outside = m.support().with_file_name("outside-park-parent");
        std::fs::create_dir_all(&outside).unwrap();
        let parks = paths::parks_dir(&m.ctx);
        let there = m.there_park();
        let (linked, in_parks) = (outside.clone(), parks.clone());
        let refused = fault::meanwhile(
            "tree.park_move_checked",
            move || {
                for entry in std::fs::read_dir(&in_parks).unwrap().flatten() {
                    if entry.file_name().to_string_lossy() != there {
                        std::os::unix::fs::symlink(&linked, entry.path().join("IndexedDB"))
                            .unwrap();
                    }
                }
            },
            || switch_to(&m, "there"),
        )
        .expect_err("a folder in the park became a link");
        assert!(
            matches!(refused, Error::DesktopDataInaccessible { .. }),
            "{refused:?}"
        );
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing was moved to where the link points"
        );
    }

    /// A sign-out stopped after the account was parked, and Claude signed in to another
    /// account before recovery: that login's keys are set aside with its session, not lost.
    #[test]
    fn a_sign_out_recovery_keeps_the_keys_of_a_login_made_since() {
        let m = desktop_machine("sign-out-recovery-keys");
        let killed = fault::killing("tree.park_recorded", || signing_out(&m));
        assert_eq!(killed.unwrap_err(), "tree.park_recorded");
        m.plant_live("third", "v10third");
        m.recover().expect("recovered");
        let mut kept = Vec::new();
        super::super::harness::files_under(&paths::strays_dir(&m.ctx), &mut kept);
        assert!(
            kept.iter()
                .filter(|p| p.file_name().is_some_and(|n| n == CONFIG_KEYS_FILE))
                .map(|p| config::read_keys(p).unwrap())
                .any(|keys| keys.0.get("oauth:tokenCache")
                    == Some(&serde_json::json!("cache-third"))),
            "the keys are set aside"
        );
    }

    /// The strays directory can become a link between choosing where a stray goes and
    /// moving it there.
    #[test]
    fn a_strays_directory_that_became_a_link_is_not_moved_into() {
        let m = desktop_machine("strays-linked");
        let outside = m.support().with_file_name("outside-strays");
        std::fs::create_dir_all(&outside).unwrap();
        let strays = paths::strays_dir(&m.ctx);
        let linked = outside.clone();
        let stray = m.support().join("Cookies");
        let refused = fault::meanwhile(
            "tree.stray_target_chosen",
            move || {
                let _ = std::fs::remove_dir_all(&strays);
                std::os::unix::fs::symlink(&linked, &strays).unwrap();
            },
            || moves::Strays::new().set_aside(&m.ctx, &stray),
        )
        .expect_err("the strays directory became a link");
        assert!(
            matches!(refused, Error::HomeUnwritable { .. }),
            "{refused:?}"
        );
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing was moved to where the link points"
        );
        assert!(m.support().join("Cookies").exists(), "the login stays");
    }

    /// The parks directory can be replaced by a link between the check of it and the making of
    /// the outgoing park in it.
    #[test]
    fn a_parks_directory_that_became_a_link_is_not_made_a_park_in() {
        let m = desktop_machine("parks-linked");
        let outside = m.support().with_file_name("outside-parks");
        std::fs::create_dir_all(&outside).unwrap();
        let parks = paths::parks_dir(&m.ctx);
        let linked = outside.clone();
        let refused = fault::meanwhile(
            "tree.journal_written",
            move || {
                let aside = parks.with_file_name("parks-displaced");
                std::fs::rename(&parks, &aside).unwrap();
                std::os::unix::fs::symlink(&linked, &parks).unwrap();
            },
            || switch_to(&m, "there"),
        )
        .expect_err("the parks directory became a link");
        assert!(
            matches!(refused, Error::DesktopDataInaccessible { .. }),
            "{refused:?}"
        );
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "no park was made where the link points"
        );
    }

    /// The new park can be replaced by a link between making it and writing into it.
    #[test]
    fn a_new_park_that_became_a_link_is_not_written_into() {
        let m = desktop_machine("new-park-linked");
        let outside = m.support().with_file_name("outside-new-park");
        std::fs::create_dir_all(&outside).unwrap();
        let parks = paths::parks_dir(&m.ctx);
        let there = m.there_park();
        let linked = outside.clone();
        let refused = fault::meanwhile(
            "tree.park_made",
            move || {
                for entry in std::fs::read_dir(&parks).unwrap().flatten() {
                    if entry.file_name().to_string_lossy() != there {
                        std::fs::remove_dir_all(entry.path()).unwrap();
                        std::os::unix::fs::symlink(&linked, entry.path()).unwrap();
                    }
                }
            },
            || switch_to(&m, "there"),
        )
        .expect_err("the new park became a link");
        assert!(
            matches!(refused, Error::DesktopDataInaccessible { .. }),
            "{refused:?}"
        );
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing was written where the link points"
        );
    }

    /// The data folder can be replaced by a link between the check and the read of its keys,
    /// which would put another folder's keys in the park of this account.
    #[test]
    fn a_data_folder_that_became_a_link_gives_the_park_no_keys() {
        let m = desktop_machine("root-linked-before-keys");
        let outside = m.support().with_file_name("outside-keys");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            outside.join("config.json"),
            br#"{"lastKnownAccountUuid":"somebody-else"}"#,
        )
        .unwrap();
        let (support, moved, linked) = (
            m.support(),
            m.support().with_file_name("moved-support"),
            outside.clone(),
        );
        let refused = fault::meanwhile(
            "tree.park_made",
            move || {
                std::fs::rename(&support, &moved).unwrap();
                std::os::unix::fs::symlink(&linked, &support).unwrap();
            },
            || switch_to(&m, "there"),
        )
        .expect_err("the data folder became a link");
        assert!(
            matches!(refused, Error::DesktopDataInaccessible { .. }),
            "{refused:?}"
        );
        let there = m.there_park();
        for entry in std::fs::read_dir(paths::parks_dir(&m.ctx))
            .unwrap()
            .flatten()
        {
            if entry.file_name().to_string_lossy() != there {
                assert!(
                    !entry.path().join(CONFIG_KEYS_FILE).exists(),
                    "the keys of the folder the link points at were kept"
                );
            }
        }
    }

    /// The park is looked at again after the last item is in it: a link put there now would
    /// take the manifest to where it points, and leave the login in the folder it displaced.
    #[test]
    fn a_park_that_became_a_link_after_the_last_move_gets_no_manifest() {
        let m = desktop_machine("park-linked-late");
        let outside = m.support().with_file_name("outside-late-park");
        std::fs::create_dir_all(&outside).unwrap();
        let parks = paths::parks_dir(&m.ctx);
        let there = m.there_park();
        let linked = outside.clone();
        let refused = fault::meanwhile(
            "tree.live_parked",
            move || {
                for entry in std::fs::read_dir(&parks).unwrap().flatten() {
                    if entry.file_name().to_string_lossy() != there {
                        let displaced = entry.path().with_extension("displaced");
                        std::fs::rename(entry.path(), &displaced).unwrap();
                        std::os::unix::fs::symlink(&linked, entry.path()).unwrap();
                    }
                }
            },
            || switch_to(&m, "there"),
        )
        .expect_err("the park became a link");
        assert!(
            matches!(refused, Error::DesktopDataInaccessible { .. }),
            "{refused:?}"
        );
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing was written where the link points"
        );
    }

    /// The file that says an add is waiting is not a record of a switch: one that cannot be
    /// removed leaves no file half moved, so it is not said as one.
    #[test]
    fn an_awaiting_file_that_cannot_be_removed_is_not_a_recovery_failure() {
        let m = desktop_machine("awaiting-stays");
        let file = awaiting_path(&m.ctx);
        std::fs::create_dir_all(file.join("not-a-file")).unwrap();
        let refused = clear_awaiting(&m.ctx).expect_err("a directory is not removed as a file");
        assert!(
            matches!(refused, Error::HomeUnwritable { .. }),
            "{refused:?}"
        );
    }

    #[test]
    fn claude_starting_midway_stops_the_next_move() {
        let m = desktop_machine("midway");
        let before = m.inodes();
        let mem = std::sync::Arc::clone(&m.mem);
        let refused = fault::meanwhile(
            "tree.item_parked",
            move || {
                mem.runs_within(APP_PATH);
            },
            || switch_to(&m, "there"),
        )
        .expect_err("the app opened partway");
        assert!(matches!(refused, Error::AppStillOpen { .. }), "{refused:?}");
        assert_stopped_partway(&refused);
        // One item moved, and the record of intent says so.
        assert!(tree_journal::pending(&m.ctx).is_some());
        let waiting = super::super::settle(&m.ctx, Some(ProviderId::Desktop))
            .err()
            .expect("recovery waits while the app is open");
        assert!(
            matches!(waiting, Error::RecoveryWaiting { .. }),
            "{waiting:?}"
        );

        m.mem.quits_within();
        let recovered = m
            .recover()
            .expect("recovered")
            .expect("the interrupted switch was found");
        assert!(!recovered.finished, "undone, not finished");
        assert_eq!(m.inodes(), before, "every item is back where it was");
        assert_eq!(m.whole("here"), super::super::harness::Whole::Live);
        assert_eq!(m.whole("there"), super::super::harness::Whole::Parked);
    }

    /// Claude opened after the last item moved and before the config is written: the
    /// config is the app's while it runs, so it is left alone, and the run quit for. The
    /// next run once Claude is quit finishes the switch.
    #[test]
    fn claude_starting_before_the_config_is_written_leaves_it_alone() {
        let m = desktop_machine("before-config");
        let config = m.support().join("config.json");
        let before = std::fs::read(&config).unwrap();
        let mem = std::sync::Arc::clone(&m.mem);
        let refused = fault::meanwhile(
            "tree.installed",
            move || {
                mem.runs_within(APP_PATH);
            },
            || switch_to(&m, "there"),
        )
        .expect_err("the app opened before the config");
        assert!(matches!(refused, Error::AppStillOpen { .. }), "{refused:?}");
        assert_stopped_partway(&refused);
        assert_eq!(
            std::fs::read(&config).unwrap(),
            before,
            "the config is untouched"
        );
        assert!(tree_journal::pending(&m.ctx).is_some());

        m.mem.quits_within();
        let recovered = m
            .recover()
            .expect("recovered")
            .expect("the interrupted switch was found");
        assert!(recovered.finished, "finished, not undone");
        assert_eq!(m.whole("there"), super::super::harness::Whole::Live);
        assert_eq!(m.whole("here"), super::super::harness::Whole::Parked);
        let spliced: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        assert_eq!(spliced["lastKnownAccountUuid"], "there");
        assert_eq!(spliced["locale"], "en-US");
    }

    /// The same for a sign-out: Claude opened once the account is parked, so the config
    /// still names it, and the next run once Claude is quit finishes the sign-out.
    #[test]
    fn claude_starting_before_a_sign_out_writes_the_config_leaves_it_alone() {
        let m = desktop_machine("sign-out-before-config");
        let config = m.support().join("config.json");
        let before = std::fs::read(&config).unwrap();
        let mem = std::sync::Arc::clone(&m.mem);
        let refused = fault::meanwhile(
            "tree.park_recorded",
            move || {
                mem.runs_within(APP_PATH);
            },
            || signing_out(&m),
        )
        .expect_err("the app opened before the config");
        assert!(matches!(refused, Error::AppStillOpen { .. }), "{refused:?}");
        assert_stopped_partway(&refused);
        assert_eq!(
            std::fs::read(&config).unwrap(),
            before,
            "the config is untouched"
        );
        assert!(tree_journal::pending(&m.ctx).is_some());

        m.mem.quits_within();
        let recovered = m
            .recover()
            .expect("recovered")
            .expect("the interrupted sign-out was found");
        assert!(recovered.finished && recovered.signed_out, "{recovered:?}");
        assert_eq!(identity::identify_tree(&m.ctx, &m.support()).unwrap(), None);
        assert_eq!(m.whole("here"), super::super::harness::Whole::Parked);
        let spliced: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        for key in paths::CONFIG_KEYS {
            assert!(spliced.get(key).is_none(), "{key} is the account's");
        }
        assert_eq!(spliced["locale"], "en-US");
    }

    #[test]
    fn sign_out_leaves_claude_signed_out_and_the_account_parked() {
        let m = desktop_machine("sign-out");
        let (outcome, _) = signing_out(&m).expect("signed out");
        let Outcome::SignedOut { from, parked, .. } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(from, "desktop/here");
        assert_eq!(identity::identify_tree(&m.ctx, &m.support()).unwrap(), None);
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(m.support().join("config.json")).unwrap())
                .unwrap();
        for key in paths::CONFIG_KEYS {
            assert!(config.get(key).is_none(), "{key} is the account's");
        }
        assert_eq!(config["locale"], "en-US");
        let state = state::load(&m.ctx).unwrap();
        assert_eq!(state.active_for(ProviderId::Desktop), None);
        assert_eq!(state.get(&here(&m)).unwrap().parked.as_ref(), Some(&parked));
        assert_eq!(
            awaiting_sign_in(&m.ctx).map(|a| a.from_label),
            Some("here".to_string())
        );
        assert_eq!(m.whole("here"), super::super::harness::Whole::Parked);

        // Signed out already is nothing to do.
        let (again, _) = signing_out(&m).expect("nothing to do");
        assert!(
            matches!(
                again,
                Outcome::AlreadySignedOut {
                    provider: ProviderId::Desktop
                }
            ),
            "{again:?}"
        );

        // And a switch from signed out installs without parking anything.
        let (installed, _) = switch_to(&m, "there").expect("installed");
        assert!(
            matches!(&installed, Outcome::Installed { to, .. } if to == "desktop/there"),
            "{installed:?}"
        );
        assert_eq!(m.whole("there"), super::super::harness::Whole::Live);
        assert_eq!(m.whole("here"), super::super::harness::Whole::Parked);
    }

    #[test]
    fn enroll_refuses_while_claude_runs() {
        let m = desktop_machine("enroll-runs");
        m.mem.runs_within(APP_PATH);
        let refused = enrolling(&m, "here").expect_err("Claude is open");
        assert!(matches!(refused, Error::AppStillOpen { .. }), "{refused:?}");
    }

    #[test]
    fn enroll_records_uuid_and_fingerprint() {
        let m = desktop_machine("enroll-new");
        // Somebody signed in to a third account in the app.
        let support = m.support();
        m.plant_live("third", "v10third");
        let (enrolled, warnings) = enrolling(&m, "third").expect("enrolled");
        assert!(matches!(enrolled, Enrolled::Current { .. }), "{enrolled:?}");
        assert!(
            warnings
                .iter()
                .any(|w| matches!(w, Warning::ReplacedOutsidePitboard { label, .. } if label == "desktop/here")),
            "{warnings:?}"
        );
        let state = state::load(&m.ctx).unwrap();
        let account = state.get(&m.key("third")).unwrap();
        assert_eq!(account.account_uuid, "third");
        let live = identity::identify_tree(&m.ctx, &support).unwrap().unwrap();
        assert!(matches!(
            &account.detail,
            Detail::Desktop { session_fingerprint, .. } if *session_fingerprint == live.fingerprint
        ));
        assert_eq!(state.active_for(ProviderId::Desktop), Some("third"));

        // The same account under another label is refused, and so is a label in use.
        let refused = enrolling(&m, "again").expect_err("enrolled already");
        assert!(
            matches!(refused, Error::AlreadyEnrolled { .. }),
            "{refused:?}"
        );
        m.plant_live("fourth", "v10fourth");
        let refused = enrolling(&m, "there").expect_err("a label in use");
        assert!(matches!(refused, Error::LabelTaken { .. }), "{refused:?}");
    }

    #[test]
    fn an_unconfirmed_session_moves_nothing() {
        let m = desktop_machine("unconfirmed");
        // `there`'s parked session in the folder under `here`'s uuid: the uuid is stale or
        // the session moved, and either way it cannot be filed under one of them.
        m.plant_live("here", "v10there");
        let before = m.inodes();
        let refused = switch_to(&m, "there").expect_err("unconfirmed");
        assert!(
            matches!(&refused, Error::DesktopIdentityUnconfirmed { label } if label == "there"),
            "{refused:?}"
        );
        assert_eq!(m.inodes(), before);
        assert!(tree_journal::pending(&m.ctx).is_none());
    }

    /// Claude gave `here` a new session without signing it out, as it does after a
    /// `session_stale_relogin`. Setting it aside to add another account parks it as `here`'s
    /// and records the new session, and a switch back finds it whole.
    #[test]
    fn a_renewed_session_is_set_aside_as_its_account() {
        let m = desktop_machine("renewed");
        m.plant_live("here", "v10here-again");
        let renewed = identity::identify_tree(&m.ctx, &m.support())
            .expect("a readable folder")
            .expect("signed in");
        let (outcome, _) = signing_out(&m).expect("set aside");
        assert!(
            matches!(&outcome, Outcome::SignedOut { from, .. } if from == "desktop/here"),
            "{outcome:?}"
        );
        let state = state::load(&m.ctx).unwrap();
        assert!(
            matches!(
                &state.get(&here(&m)).unwrap().detail,
                Detail::Desktop { session_fingerprint, .. } if *session_fingerprint == renewed.fingerprint
            ),
            "the renewed session is the one recorded"
        );
        assert_eq!(m.whole("here"), super::super::harness::Whole::Parked);

        let (back, _) = switch_to(&m, "here").expect("restored");
        assert!(
            matches!(&back, Outcome::Installed { to, .. } if to == "desktop/here"),
            "{back:?}"
        );
        let live = identity::identify_tree(&m.ctx, &m.support())
            .unwrap()
            .unwrap();
        assert_eq!(live.fingerprint, renewed.fingerprint);
    }

    /// Switching to the account signed in now while it still has a park: the park is an
    /// older copy of it, and must not stay beside the login in use, as enrolling it again
    /// also drops it. The session in use is the one recorded.
    #[test]
    fn switching_to_the_account_in_use_drops_its_park() {
        let m = desktop_machine("in-use-parked");
        let park = m.there_park();
        // `there`'s parked session, signed in to the app as well.
        m.plant_live("there", "v10there");
        let live = identity::identify_tree(&m.ctx, &m.support())
            .expect("a readable folder")
            .expect("signed in");
        let (outcome, _) = switch_to(&m, "there").expect("already in use");
        assert!(
            matches!(&outcome, Outcome::AlreadyActive { label } if label == "desktop/there"),
            "{outcome:?}"
        );
        let state = state::load(&m.ctx).unwrap();
        assert_eq!(state.active_for(ProviderId::Desktop), Some("there"));
        let account = state.get(&there(&m)).expect("there is kept");
        assert_eq!(account.parked, None);
        assert!(
            matches!(
                &account.detail,
                Detail::Desktop { session_fingerprint, .. } if *session_fingerprint == live.fingerprint
            ),
            "{:?}",
            account.detail
        );
        assert!(
            !crate::provider::desktop::paths::parks_dir(&m.ctx)
                .join(&park)
                .exists(),
            "the old park is deleted"
        );
        assert!(expiring(&m.ctx, &state, ProviderId::Desktop).is_empty());
    }

    /// An add that parked `there` is waiting for a sign-in; signing back in to `there`
    /// directly and switching to it ends that wait, as enrolling or a completed switch does.
    #[test]
    fn switching_to_the_account_in_use_ends_a_sign_in_wait() {
        let m = desktop_machine("in-use-awaiting");
        m.there_park();
        m.plant_live("there", "v10there");
        write_awaiting(
            &m.ctx,
            &Awaiting {
                from_label: "desktop/there".into(),
                started_at: m.ctx.now(),
            },
        )
        .unwrap();
        let (outcome, _) = switch_to(&m, "there").expect("already in use");
        assert!(
            matches!(outcome, Outcome::AlreadyActive { .. }),
            "{outcome:?}"
        );
        assert_eq!(awaiting_sign_in(&m.ctx), None);
    }

    #[test]
    fn enrolling_the_same_label_confirms_it() {
        let m = desktop_machine("confirm");
        m.plant_live("here", "v10someone");
        let (enrolled, _) = enrolling(&m, "here").expect("confirmed");
        assert!(
            matches!(enrolled, Enrolled::InUse { again: true, .. }),
            "{enrolled:?}"
        );
        let (outcome, _) = switch_to(&m, "there").expect("now it moves");
        assert!(matches!(outcome, Outcome::Switched { .. }), "{outcome:?}");
    }

    /// `there`'s park lapsed, so somebody put `here` aside, signed in to `there` again in
    /// Claude, and enrolled it under its own label. The lapsed park is not kept beside the
    /// login now in use: nothing could install it, and it would be warned about forever.
    #[test]
    fn enrolling_again_over_a_lapsed_park_drops_the_park() {
        let m = desktop_machine("re-enroll-lapsed");
        let mut state = state::load(&m.ctx).unwrap();
        let mut account = state.get(&there(&m)).unwrap().clone();
        account.parked.as_mut().unwrap().refresh_expires_at = Some(NOW - 1);
        state.upsert(account);
        state::save(&m.ctx, &state).unwrap();
        signing_out(&m).expect("signed out");
        m.plant_live("there", "v10there-again");

        let (enrolled, _) = enrolling(&m, "there").expect("enrolled again");
        assert!(
            matches!(enrolled, Enrolled::InUse { again: true, .. }),
            "{enrolled:?}"
        );
        let state = state::load(&m.ctx).unwrap();
        assert_eq!(state.active_for(ProviderId::Desktop), Some("there"));
        assert_eq!(state.get(&there(&m)).unwrap().parked, None);
        assert!(expiring(&m.ctx, &state, ProviderId::Desktop).is_empty());
        assert!(state.discarded.is_empty(), "{:?}", state.discarded);
        assert!(
            !paths::parks_dir(&m.ctx).join(m.there_park()).exists(),
            "the lapsed park is deleted"
        );
        assert!(
            state.get(&here(&m)).unwrap().parked.is_some(),
            "here's is kept"
        );
    }

    #[test]
    fn what_claude_made_while_signed_out_goes_to_strays() {
        let m = desktop_machine("strays");
        signing_out(&m).expect("signed out");
        // The app opened signed out and made storage of its own.
        let made = m.support().join("Local Storage");
        std::fs::create_dir_all(&made).unwrap();
        std::fs::write(made.join("made-by-claude"), b"x").unwrap();
        let made_inode = moves::inode(&made.join("made-by-claude")).unwrap().unwrap();

        let (_, warnings) = switch_to(&m, "there").expect("installed");
        assert!(
            warnings
                .iter()
                .any(|w| matches!(w, Warning::StraysKept { count: 1, .. })),
            "{warnings:?}"
        );
        assert!(m.strays_hold(made_inode), "set aside, never deleted");
        assert_eq!(m.whole("there"), super::super::harness::Whole::Live);
    }

    /// Everything one switch sets aside goes into one directory under the strays directory,
    /// each item at its place in the data folder, rather than a directory per item: the lab
    /// run of 4 October 2026 found seven for one switch.
    #[test]
    fn one_switch_sets_its_strays_aside_in_one_directory() {
        let m = desktop_machine("strays-one-slot");
        signing_out(&m).expect("signed out");
        // The app opened signed out, made storage of its own and wrote keys to its config.
        let mut made = Vec::new();
        for item in ["Local Storage", "Session Storage", "WebStorage"] {
            let dir = m.support().join(item);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("made-by-claude"), b"x").unwrap();
            made.push(moves::inode(&dir.join("made-by-claude")).unwrap().unwrap());
        }
        let path = m.support().join("config.json");
        let mut config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        config["oauth:tokenCacheV2"] = serde_json::json!("cache-written-signed-out");
        std::fs::write(&path, config.to_string()).unwrap();

        let (_, warnings) = switch_to(&m, "there").expect("installed");
        assert!(
            warnings
                .iter()
                .any(|w| matches!(w, Warning::StraysKept { count: 4, .. })),
            "{warnings:?}"
        );
        let slots: Vec<_> = std::fs::read_dir(paths::strays_dir(&m.ctx))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(slots.len(), 1, "{slots:?}");
        let slot = &slots[0];
        for item in ["Local Storage", "Session Storage", "WebStorage"] {
            assert!(slot.join(item).join("made-by-claude").is_file(), "{item}");
        }
        assert!(slot.join(CONFIG_KEYS_FILE).is_file());
        for inode in made {
            assert!(m.strays_hold(inode), "set aside, never deleted");
        }
        assert_eq!(m.whole("there"), super::super::harness::Whole::Live);
    }

    /// Claude can write an account's keys into its config while Pitboard counts nobody as
    /// signed in. A switch writes the incoming account's keys over them, so they are set
    /// aside with the strays first, never lost.
    #[test]
    fn config_keys_claude_wrote_while_signed_out_are_set_aside() {
        let m = desktop_machine("strays-config");
        signing_out(&m).expect("signed out");
        let path = m.support().join("config.json");
        let mut config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        config["oauth:tokenCacheV2"] = serde_json::json!("cache-written-signed-out");
        std::fs::write(&path, config.to_string()).unwrap();

        let (_, warnings) = switch_to(&m, "there").expect("installed");
        assert!(
            warnings
                .iter()
                .any(|w| matches!(w, Warning::StraysKept { count: 1, .. })),
            "{warnings:?}"
        );
        let mut kept = Vec::new();
        super::super::harness::files_under(&paths::strays_dir(&m.ctx), &mut kept);
        let saved = kept
            .iter()
            .filter(|p| p.file_name().is_some_and(|n| n == CONFIG_KEYS_FILE))
            .map(|p| config::read_keys(p).unwrap())
            .find(|keys| keys.0.contains_key("oauth:tokenCacheV2"))
            .expect("the keys are set aside");
        assert_eq!(
            saved.0["oauth:tokenCacheV2"],
            serde_json::json!("cache-written-signed-out")
        );
        assert_eq!(m.whole("there"), super::super::harness::Whole::Live);

        // A folder that holds no keys of an account has nothing to set aside.
        let m = desktop_machine("strays-no-config");
        signing_out(&m).expect("signed out");
        let (_, warnings) = switch_to(&m, "there").expect("installed");
        assert!(
            !warnings
                .iter()
                .any(|w| matches!(w, Warning::StraysKept { .. })),
            "{warnings:?}"
        );
    }

    /// The park can be replaced by a link to itself between being written and being verified:
    /// the checks would pass through the link, and the account be recorded under it.
    #[test]
    fn a_park_that_became_a_link_after_it_was_stored_is_not_recorded() {
        let m = desktop_machine("park-linked-after-stored");
        let parks = paths::parks_dir(&m.ctx);
        let there = m.there_park();
        let displaced = m.support().with_file_name("displaced-park");
        let refused = fault::meanwhile(
            "tree.park_stored",
            move || {
                for entry in std::fs::read_dir(&parks).unwrap().flatten() {
                    if entry.file_name().to_string_lossy() != there {
                        std::fs::rename(entry.path(), &displaced).unwrap();
                        std::os::unix::fs::symlink(&displaced, entry.path()).unwrap();
                    }
                }
            },
            || switch_to(&m, "there"),
        )
        .expect_err("the park became a link");
        assert!(
            matches!(refused, Error::DesktopDataInaccessible { .. }),
            "{refused:?}"
        );
        let state = state::load(&m.ctx).unwrap();
        assert!(
            state
                .get(&Key::new(ProviderId::Desktop, "here"))
                .is_some_and(|account| account.parked.is_none()),
            "a link was recorded as the account's park"
        );
    }

    /// The park can be replaced by a link after it was verified, while the app is asked about
    /// once more: what is recorded must be the folder that was verified.
    #[test]
    fn a_park_that_became_a_link_after_it_was_verified_is_not_recorded() {
        let m = desktop_machine("park-linked-after-verified");
        let parks = paths::parks_dir(&m.ctx);
        let there = m.there_park();
        let displaced = m.support().with_file_name("displaced-verified-park");
        let refused = fault::meanwhile(
            "tree.park_verified",
            move || {
                for entry in std::fs::read_dir(&parks).unwrap().flatten() {
                    if entry.file_name().to_string_lossy() != there {
                        std::fs::rename(entry.path(), &displaced).unwrap();
                        std::os::unix::fs::symlink(&displaced, entry.path()).unwrap();
                    }
                }
            },
            || switch_to(&m, "there"),
        )
        .expect_err("the park became a link");
        assert!(
            matches!(refused, Error::DesktopDataInaccessible { .. }),
            "{refused:?}"
        );
        let state = state::load(&m.ctx).unwrap();
        assert!(
            state
                .get(&Key::new(ProviderId::Desktop, "here"))
                .is_some_and(|account| account.parked.is_none()),
            "a link was recorded as the account's park"
        );
    }

    /// Claude can sign in to another account and quit between the outgoing account being
    /// parked and the incoming one installed. Its keys are in the config, which the park does
    /// not hold, so a switch that writes the incoming keys over them sets them aside first.
    #[test]
    fn config_keys_claude_wrote_after_the_park_are_set_aside() {
        let m = desktop_machine("strays-config-after-park");
        let path = m.support().join("config.json");
        let rewritten = path.clone();
        let (_, warnings) = fault::meanwhile(
            "tree.park_stored",
            move || {
                let mut config: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&rewritten).unwrap()).unwrap();
                config["oauth:tokenCacheV2"] = serde_json::json!("cache-made-after-park");
                std::fs::write(&rewritten, config.to_string()).unwrap();
            },
            || switch_to(&m, "there"),
        )
        .expect("installed");
        assert!(
            warnings
                .iter()
                .any(|w| matches!(w, Warning::StraysKept { count: 1, .. })),
            "{warnings:?}"
        );
        let mut kept = Vec::new();
        super::super::harness::files_under(&paths::strays_dir(&m.ctx), &mut kept);
        assert!(
            kept.iter()
                .filter(|p| p.file_name().is_some_and(|n| n == CONFIG_KEYS_FILE))
                .map(|p| config::read_keys(p).unwrap())
                .any(|keys| keys.0.get("oauth:tokenCacheV2")
                    == Some(&serde_json::json!("cache-made-after-park"))),
            "the keys are set aside"
        );
        assert_eq!(m.whole("there"), super::super::harness::Whole::Live);
    }

    #[test]
    fn an_expired_park_is_not_installed() {
        let m = desktop_machine("expired");
        let mut state = state::load(&m.ctx).unwrap();
        let mut account = state.get(&there(&m)).unwrap().clone();
        account.parked.as_mut().unwrap().refresh_expires_at = Some(NOW - 1);
        state.upsert(account);
        state::save(&m.ctx, &state).unwrap();
        let before = m.inodes();
        let refused = switch_to(&m, "there").expect_err("expired");
        assert!(
            matches!(refused, Error::ParkedLoginExpired { .. }),
            "{refused:?}"
        );
        assert_eq!(m.inodes(), before);
    }

    #[test]
    fn a_park_naming_another_account_is_refused() {
        let m = desktop_machine("elsewhere");
        let dir = paths::parks_dir(&m.ctx).join(m.there_park());
        let mut manifest: Manifest =
            serde_json::from_slice(&std::fs::read(dir.join(MANIFEST_FILE)).unwrap()).unwrap();
        manifest.account_uuid = "someone-else".into();
        write_secret_json(&dir.join(MANIFEST_FILE), &manifest).unwrap();
        let before = m.inodes();
        let refused = switch_to(&m, "there").expect_err("not there's");
        assert!(
            matches!(refused, Error::ParkedLoginBelongsElsewhere { .. }),
            "{refused:?}"
        );
        assert_eq!(m.inodes(), before);

        // A session that is not the one parked is refused too.
        let m = desktop_machine("elsewhere-session");
        let dir = paths::parks_dir(&m.ctx).join(m.there_park());
        m.mem
            .plant_cookies(&dir.join("Cookies"), jar("v10other", NOW + 86_400));
        let refused = switch_to(&m, "there").expect_err("another session");
        assert!(
            matches!(refused, Error::ParkedCredentialCorrupt { .. }),
            "{refused:?}"
        );
    }

    /// A park that has lost an item its manifest lists would install an incomplete login, so
    /// it is refused before anything moves.
    #[test]
    fn a_park_missing_an_item_its_manifest_lists_is_refused() {
        let m = desktop_machine("missing-item");
        let dir = paths::parks_dir(&m.ctx).join(m.there_park());
        let manifest: Manifest =
            serde_json::from_slice(&std::fs::read(dir.join(MANIFEST_FILE)).unwrap()).unwrap();
        assert!(
            manifest.items.iter().any(|i| i == "Local Storage"),
            "{manifest:?}"
        );
        std::fs::remove_dir_all(dir.join("Local Storage")).unwrap();
        let before = m.inodes();
        let refused = switch_to(&m, "there").expect_err("an item is gone");
        assert!(
            matches!(refused, Error::ParkedCredentialCorrupt { .. }),
            "{refused:?}"
        );
        assert_eq!(m.inodes(), before);
    }

    /// The keys of a park's config say whose they are. Another account's, or none, would be
    /// spliced in after the files moved and fail the check at the end, every time, so the park
    /// is refused before anything moves.
    #[test]
    fn a_park_whose_config_keys_name_another_account_is_refused() {
        for (name, keys) in [
            (
                "keys-elsewhere",
                serde_json::json!({"lastKnownAccountUuid": "someone-else", "oauth:tokenCache": "cache"}),
            ),
            (
                "keys-no-account",
                serde_json::json!({"oauth:tokenCache": "cache"}),
            ),
        ] {
            let m = desktop_machine(name);
            let dir = paths::parks_dir(&m.ctx).join(m.there_park());
            write_secret_json(&dir.join(CONFIG_KEYS_FILE), &keys).unwrap();
            let before = m.inodes();
            let refused = switch_to(&m, "there").expect_err("keys of someone else");
            assert!(
                matches!(refused, Error::ParkedCredentialCorrupt { .. }),
                "{name}: {refused:?}"
            );
            assert_eq!(m.inodes(), before, "{name}");
        }
    }

    /// A move that was made but could not be synced is not on disk yet, so the run stops
    /// there with its record kept, rather than carry on to delete the record and the park
    /// behind a move a power cut could take back.
    #[test]
    fn a_move_that_cannot_be_synced_stops_the_switch_with_its_record() {
        use crate::store::tree::SYNC_FAILS;
        let m = desktop_machine("unsynced-move");
        let failed = fault::meanwhile(
            "tree.item_parked",
            || SYNC_FAILS.with(|fails| fails.set(true)),
            || switch_to(&m, "there"),
        );
        SYNC_FAILS.with(|fails| fails.set(false));
        let refused = failed.expect_err("the next move is not synced");
        assert!(
            matches!(&refused, Error::DesktopDataInaccessible { source, .. }
                if source.to_string().contains("could not be synced")),
            "{refused:?}"
        );
        assert!(
            crate::switch::tree_interrupted(&m.ctx).is_some(),
            "the record is kept for the next change"
        );
    }

    /// A nested item whose parent folder in the park is a link would be moved out of
    /// wherever the link points, so the park is refused before anything moves.
    #[test]
    fn a_park_whose_item_sits_behind_a_symlinked_folder_is_refused() {
        let m = desktop_machine("symlinked-parent");
        let dir = paths::parks_dir(&m.ctx).join(m.there_park());
        let mut manifest: Manifest =
            serde_json::from_slice(&std::fs::read(dir.join(MANIFEST_FILE)).unwrap()).unwrap();
        manifest.items.push("Nested/inner".into());
        std::fs::write(
            dir.join(MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let outside = dir.with_file_name("outside-the-park");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("inner"), "not the park's").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("Nested")).unwrap();
        let before = m.inodes();
        let refused = switch_to(&m, "there").expect_err("a linked parent");
        assert!(
            matches!(refused, Error::ParkedCredentialCorrupt { .. }),
            "{refused:?}"
        );
        assert_eq!(m.inodes(), before);
        assert!(
            outside.join("inner").exists(),
            "what the link points at stays"
        );
    }

    /// `IndexedDB` in the live data folder being a link would have the park take the two
    /// stores out of wherever it points, which is not Claude's folder, so nothing moves.
    #[test]
    fn a_live_item_behind_a_symlinked_folder_is_refused() {
        let m = desktop_machine("live-symlinked-parent");
        let outside = m.support().with_file_name("outside-the-data-folder");
        let store = outside.join("https_claude.ai_0.indexeddb.leveldb");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("CURRENT"), "not Claude's").unwrap();
        let indexed = m.support().join("IndexedDB");
        let _ = std::fs::remove_dir_all(&indexed);
        std::os::unix::fs::symlink(&outside, &indexed).unwrap();
        let before = m.inodes();
        let refused = switch_to(&m, "there").expect_err("a linked parent");
        assert!(
            matches!(refused, Error::DesktopDataInaccessible { .. }),
            "{refused:?}"
        );
        assert_eq!(m.inodes(), before);
        assert!(
            store.join("CURRENT").exists(),
            "what the link points at stays"
        );
    }

    /// A folder on the way to an item can become a link after the checks the record was made
    /// from, and a rename follows it. Every move looks again, so nothing is taken from where
    /// the link points.
    #[test]
    fn a_folder_that_became_a_link_after_the_record_is_not_moved_through() {
        let m = desktop_machine("live-linked-after-record");
        let outside = m.support().with_file_name("outside-after-record");
        let store = outside.join("https_claude.ai_0.indexeddb.leveldb");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("CURRENT"), "not Claude's").unwrap();
        let indexed = m.support().join("IndexedDB");
        let linked = outside.clone();
        let refused = fault::meanwhile(
            "tree.item_parked",
            move || {
                let _ = std::fs::remove_dir_all(&indexed);
                std::os::unix::fs::symlink(&linked, &indexed).unwrap();
            },
            || switch_to(&m, "there"),
        )
        .expect_err("a linked parent");
        assert!(
            matches!(refused, Error::DesktopDataInaccessible { .. }),
            "{refused:?}"
        );
        assert!(
            store.join("CURRENT").exists(),
            "what the link points at stays"
        );
    }

    /// The park the incoming account waits in is checked once, before the record is written.
    /// A folder inside it that became a link since is not followed: a rename from there would
    /// take what the link points at into Claude's folder.
    #[test]
    fn a_park_folder_that_became_a_link_is_not_moved_through() {
        let m = desktop_machine("park-folder-linked-after-check");
        let outside = m.support().with_file_name("outside-park-folder");
        let store = outside.join("https_claude.ai_0.indexeddb.leveldb");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("CURRENT"), "not the park's").unwrap();
        let indexed = paths::parks_dir(&m.ctx)
            .join(m.there_park())
            .join("IndexedDB");
        std::fs::create_dir_all(indexed.join("https_claude.ai_0.indexeddb.leveldb")).unwrap();
        std::fs::write(
            indexed.join("https_claude.ai_0.indexeddb.leveldb/000003.log"),
            "the park's",
        )
        .unwrap();
        let linked = outside.clone();
        let refused = fault::meanwhile(
            "tree.journal_written",
            move || {
                let _ = std::fs::remove_dir_all(&indexed);
                std::os::unix::fs::symlink(&linked, &indexed).unwrap();
            },
            || switch_to(&m, "there"),
        )
        .expect_err("a linked folder in the park");
        assert!(
            matches!(refused, Error::DesktopDataInaccessible { .. }),
            "{refused:?}"
        );
        assert!(
            store.join("CURRENT").exists(),
            "what the link points at stays"
        );
    }

    #[test]
    fn two_tools_with_one_uuid_keep_their_parks_apart() {
        let m = desktop_machine("two-tools");
        // Claude Code's account of the same uuid, parked in the vault.
        let service = crate::park::reserve(&m.ctx, "there").unwrap();
        let parked = crate::park::store_at(
            &m.ctx,
            ProviderId::Claude,
            &service,
            &super::super::harness::oauth("code-refresh", 30),
        )
        .unwrap();
        let mut state = state::load(&m.ctx).unwrap();
        state.accounts.push(super::super::harness::account(
            "there",
            "there",
            Some(parked.clone()),
        ));
        state::save(&m.ctx, &state).unwrap();

        switch_to(&m, "there").expect("the Desktop switch");
        let state = state::load(&m.ctx).unwrap();
        assert_eq!(
            state
                .get(&Key::new(ProviderId::Claude, "there"))
                .unwrap()
                .parked
                .as_ref(),
            Some(&parked),
            "Claude Code's park is not Claude Desktop's"
        );
        assert!(m.mem.vault().peek(&service).is_some());
        assert_ne!(service, m.there_park());

        // A Claude Code login of the same uuid that a run wrote down and never recorded. The
        // sweep gives it back to Claude Code's account, never to Claude Desktop's.
        let m = desktop_machine("two-tools-adopt");
        let mut state = state::load(&m.ctx).unwrap();
        state
            .accounts
            .push(super::super::harness::account("there", "there", None));
        state::save(&m.ctx, &state).unwrap();
        let service = crate::park::reserve(&m.ctx, "there").unwrap();
        crate::park::store_at(
            &m.ctx,
            ProviderId::Claude,
            &service,
            &super::super::harness::oauth("orphan-refresh", 30),
        )
        .unwrap();
        super::super::settle(&m.ctx, None).expect("swept");
        let state = state::load(&m.ctx).unwrap();
        let held = |key: &Key| {
            state
                .get(key)
                .and_then(|a| a.parked.as_ref())
                .map(|p| p.service.clone())
        };
        assert_eq!(held(&Key::new(ProviderId::Claude, "there")), Some(service));
        assert_eq!(held(&there(&m)), Some(m.there_park()));
        // The lookup both the sweep and doctor's check of held-back accounts go through.
        // Doctor names a held-back account by its service, and both tools' is Anthropic, so
        // which account the lookup answers is the whole of what can go wrong there.
        assert_eq!(
            state
                .vault_owner_of_park("there")
                .map(crate::state::Account::provider),
            Some(ProviderId::Claude)
        );
    }

    #[test]
    fn vault_code_never_sees_a_tree_name() {
        let m = desktop_machine("vault");
        m.mem
            .vault()
            .plant(&m.there_park(), "a vault item of the same name");

        // Neither the list of names Pitboard writes down nor asking the store itself takes
        // the folder park for a vault item, or the vault item for a park of an account's.
        let before = state::load(&m.ctx).unwrap();
        super::super::settle(&m.ctx, None).expect("swept");
        let (settled, _) = super::super::settle(&m.ctx, None).unwrap();
        super::super::repair(settled).expect("repaired");
        assert!(m.mem.vault().peek(&m.there_park()).is_some());
        assert!(paths::parks_dir(&m.ctx).join(m.there_park()).is_dir());
        let after = state::load(&m.ctx).unwrap();
        let parks = |state: &State| {
            state
                .accounts
                .iter()
                .map(|a| (a.key(), a.parked.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(parks(&after), parks(&before), "nobody's park changed hands");
        assert!(after.discarded.is_empty(), "nothing listed for deletion");

        switch_to(&m, "there").expect("switched");
        assert!(
            m.mem.vault().peek(&m.there_park()).is_some(),
            "a tree park's name is deleted from the parks directory, never the vault"
        );
        assert!(!paths::parks_dir(&m.ctx).join(m.there_park()).exists());
    }

    /// An add begun while Claude is already signed out still waits for a sign-in, with
    /// nobody to put back, so a restart of the app can find it and carry on.
    /// A wait that is still on disk after the sign-in it waited for would reopen a finished
    /// add at the next launch, so a record that cannot be removed is said, not dropped.
    #[test]
    fn an_awaiting_record_that_cannot_be_removed_is_an_error() {
        let m = desktop_machine("awaiting-stuck");
        std::fs::create_dir_all(awaiting_path(&m.ctx).join("not-a-file")).unwrap();
        assert!(clear_awaiting(&m.ctx).is_err());
        std::fs::remove_dir_all(awaiting_path(&m.ctx)).unwrap();
        clear_awaiting(&m.ctx).expect("a record that is not there is already cleared");
    }

    /// The record of a switch is what a retry resumes from, so a wait that cannot be
    /// removed must leave it in place: with it gone, the account is switched and the wait
    /// stays, to offer the account already in use at the next launch.
    #[test]
    fn a_switch_whose_wait_cannot_be_cleared_keeps_its_record_for_a_retry() {
        let m = desktop_machine("switch-wait-stuck");
        std::fs::create_dir_all(awaiting_path(&m.ctx).join("not-a-file")).unwrap();
        switch_to(&m, "there").expect_err("the wait cannot be removed");
        assert!(
            tree_journal::pending(&m.ctx).is_some(),
            "the record is kept so that a retry can finish"
        );
        std::fs::remove_dir_all(awaiting_path(&m.ctx)).unwrap();
        m.recover().expect("recovered once the wait can go");
        assert!(tree_journal::pending(&m.ctx).is_none());
        assert!(awaiting_sign_in(&m.ctx).is_none());
    }

    /// Enrolling has no journal, so what finishes it after a wait that cannot be removed is
    /// asking again: the account is already recorded, and the second run takes the same
    /// path as an account signed in again, which removes the wait.
    #[test]
    fn enrolling_again_finishes_an_add_whose_wait_could_not_be_removed() {
        let m = desktop_machine("enrol-wait-stuck");
        signing_out(&m).expect("signed out");
        m.plant_live("new", "v10new");
        let wait = awaiting_path(&m.ctx);
        std::fs::remove_file(&wait).expect("the wait signing out wrote");
        std::fs::create_dir_all(wait.join("not-a-file")).unwrap();
        enrolling(&m, "new").expect_err("the wait cannot be removed");
        assert!(
            state::load(&m.ctx)
                .unwrap()
                .get(&Key::new(ProviderId::Desktop, "new"))
                .is_some(),
            "the account is recorded"
        );

        std::fs::remove_dir_all(&wait).unwrap();
        enrolling(&m, "new").expect("asked again");
        assert!(awaiting_sign_in(&m.ctx).is_none());
    }

    /// A parked account renamed while an add waits is still the one to put back, under its
    /// new name: the old one is in no account's name any more.
    #[test]
    fn renaming_the_account_an_add_waits_to_put_back_renames_it_in_the_wait() {
        let m = desktop_machine("rename-awaiting");
        signing_out(&m).expect("signed out");
        let (settled, _) = super::super::settle(&m.ctx, Some(ProviderId::Desktop)).unwrap();
        super::super::rename(settled, &m.key("here"), "renamed").expect("renamed");
        assert_eq!(
            awaiting_sign_in(&m.ctx).map(|a| a.from_label),
            Some("renamed".to_string())
        );
        // An account the wait does not name leaves it as it is.
        let (settled, _) = super::super::settle(&m.ctx, Some(ProviderId::Desktop)).unwrap();
        m.there_park();
        super::super::rename(settled, &m.key("there"), "elsewhere").expect("renamed");
        assert_eq!(
            awaiting_sign_in(&m.ctx).map(|a| a.from_label),
            Some("renamed".to_string())
        );
    }

    /// The state is saved after the wait is pointed at the new name, and the wait goes back
    /// if that save fails: a rename that did not happen must not leave the wait naming an
    /// account that is not there.
    #[test]
    fn a_rename_whose_save_fails_leaves_the_wait_as_it_was() {
        let m = desktop_machine("rename-awaiting-fails");
        signing_out(&m).expect("signed out");
        let (settled, _) = super::super::settle(&m.ctx, Some(ProviderId::Desktop)).unwrap();
        let file = crate::home::dir(&m.ctx).join("state.json");
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir_all(&file).unwrap();
        let refused = super::super::rename(settled, &m.key("here"), "renamed")
            .expect_err("the state cannot be written");
        assert!(
            matches!(refused, Error::StateWriteFailed { .. }),
            "{refused:?}"
        );
        assert_eq!(
            awaiting_sign_in(&m.ctx).map(|a| a.from_label),
            Some("here".to_string())
        );
    }

    /// The other way a rename can half happen: the wait cannot be written. Nothing is saved
    /// then, so the account keeps its name and the wait still names it.
    #[test]
    fn a_rename_whose_wait_cannot_be_written_renames_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let m = desktop_machine("rename-wait-fails");
        signing_out(&m).expect("signed out");
        let home = paths::desktop_home(&m.ctx);
        let (settled, _) = super::super::settle(&m.ctx, Some(ProviderId::Desktop)).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o500)).unwrap();
        let refused = super::super::rename(settled, &m.key("here"), "renamed");
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        refused.expect_err("the wait cannot be written");
        assert_eq!(
            awaiting_sign_in(&m.ctx).map(|a| a.from_label),
            Some("here".to_string())
        );
        let state = crate::state::load(&m.ctx).unwrap();
        assert!(state.get(&m.key("here")).is_some(), "still named `here`");
        assert!(state.get(&m.key("renamed")).is_none());
    }

    /// Forgetting the account an add waits to put back leaves the wait with nobody to put
    /// back, as when Claude was already signed out, instead of offering an account that is
    /// gone.
    #[test]
    fn forgetting_the_account_an_add_waits_to_put_back_empties_the_wait() {
        let m = desktop_machine("forget-awaiting");
        signing_out(&m).expect("signed out");
        let (settled, _) = super::super::settle(&m.ctx, Some(ProviderId::Desktop)).unwrap();
        super::super::forget(settled, &m.key("here")).expect("forgotten");
        let wait = awaiting_sign_in(&m.ctx).expect("still waiting for a sign-in");
        assert_eq!(wait.from_label, "");
    }

    #[test]
    fn signing_out_while_signed_out_still_waits_for_a_sign_in() {
        let m = desktop_machine("sign-out-twice");
        signing_out(&m).expect("signed out");
        clear_awaiting(&m.ctx).unwrap();
        assert_eq!(awaiting_sign_in(&m.ctx), None);

        let (again, _) = signing_out(&m).expect("nothing to move");
        assert!(
            matches!(again, Outcome::AlreadySignedOut { .. }),
            "{again:?}"
        );
        let waiting = awaiting_sign_in(&m.ctx).expect("waiting for a sign-in");
        assert_eq!(waiting.from_label, "", "nobody was parked by this run");

        // One that already names a parked account is not replaced by the empty one.
        write_awaiting(
            &m.ctx,
            &Awaiting {
                from_label: "here".into(),
                started_at: NOW,
            },
        )
        .unwrap();
        signing_out(&m).expect("nothing to move");
        assert_eq!(
            awaiting_sign_in(&m.ctx).map(|a| a.from_label),
            Some("here".to_string())
        );
    }

    /// Log out in Claude leaves `lastKnownAccountUuid` in the config but removes the
    /// session. Nobody is signed in then, so the account it names can be forgotten.
    #[test]
    fn forgetting_the_account_left_in_the_config_by_log_out_is_allowed() {
        let m = desktop_machine("forget-after-log-out");
        m.mem.plant_cookies(
            &m.support().join("Cookies"),
            crate::provider::desktop::types::CookieTable {
                meta_version: 24,
                rows: Vec::new(),
            },
        );
        let (settled, _) = super::super::settle(&m.ctx, Some(ProviderId::Desktop)).unwrap();
        super::super::forget(settled, &here(&m)).expect("nobody is signed in to it");
        assert!(state::load(&m.ctx).unwrap().get(&here(&m)).is_none());
    }

    /// Claude opened after the last item moved, while the park was being written down: the
    /// outgoing account is not recorded as parked, so the next run undoes the move rather
    /// than finishing a switch over a folder the app may have written to.
    #[test]
    fn claude_starting_before_the_park_is_recorded_undoes_the_switch() {
        let m = desktop_machine("before-park-recorded");
        let before = m.inodes();
        let mem = std::sync::Arc::clone(&m.mem);
        let refused = fault::meanwhile(
            "tree.park_stored",
            move || {
                mem.runs_within(APP_PATH);
            },
            || switch_to(&m, "there"),
        )
        .expect_err("the app opened before the park was recorded");
        assert!(matches!(refused, Error::AppStillOpen { .. }), "{refused:?}");
        assert_stopped_partway(&refused);
        assert_eq!(
            state::load(&m.ctx).unwrap().get(&here(&m)).unwrap().parked,
            None,
            "the park was not recorded"
        );

        m.mem.quits_within();
        let recovered = m
            .recover()
            .expect("recovered")
            .expect("the interrupted switch was found");
        assert!(!recovered.finished, "undone, not finished");
        assert_eq!(m.inodes(), before, "every item is back where it was");
        assert_eq!(m.whole("here"), super::super::harness::Whole::Live);
    }

    /// Claude opened after the last item moved and rewrote its config, emptying the account's
    /// keys: undoing the switch puts the items back, and with them the keys that were there
    /// before, or the returned session would sit beside a token cache of nobody's.
    #[test]
    fn undoing_a_park_restores_the_config_keys_the_app_rewrote() {
        let m = desktop_machine("undo-config-keys");
        let config = paths::config_file(&m.support());
        let original = config::read_keys(&config).unwrap();
        assert!(!original.0.is_empty(), "the account has keys to lose");
        let mem = std::sync::Arc::clone(&m.mem);
        let rewritten = config.clone();
        let refused = fault::meanwhile(
            "tree.live_parked",
            move || {
                mem.runs_within(APP_PATH);
                config::splice(&rewritten, &ConfigKeys::default()).unwrap();
            },
            || switch_to(&m, "there"),
        )
        .expect_err("the app opened before the park was recorded");
        assert!(matches!(refused, Error::AppStillOpen { .. }), "{refused:?}");
        assert_eq!(config::read_keys(&config).unwrap(), ConfigKeys::default());

        m.mem.quits_within();
        let recovered = m
            .recover()
            .expect("recovered")
            .expect("the interrupted switch was found");
        assert!(!recovered.finished, "undone, not finished");
        assert_eq!(config::read_keys(&config).unwrap(), original);
    }
}
