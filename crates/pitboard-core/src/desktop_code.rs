//! Launches one Code session with Desktop's existing access grant. Desktop owns renewal.

use crate::context::Context;
use crate::error::{Cause, Error};
use crate::provider::ProviderId;
use crate::provider::desktop::code_cache::CodeAccess;
use crate::provider::desktop::{
    code_cache, config, crypto, identity, paths, safe_storage::KeyRead,
};
use crate::state::{self, Key};
use base64::Engine;
use sha2::{Digest, Sha256};
use std::process::ExitStatus;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DesktopCodeError {
    #[error(transparent)]
    Account(#[from] Error),
    #[error("Desktop is switching accounts; finish that switch before opening Claude Code")]
    Switching,
    #[error("this Desktop login no longer belongs to the selected account")]
    IdentityChanged,
    #[error(
        "Desktop has no usable Claude Code grant ({reason}). Open this account in Claude Desktop, use its Code feature, then try again"
    )]
    Cache { reason: &'static str },
    #[error("macOS did not provide Claude's encryption key ({reason})")]
    Key { reason: &'static str },
    #[error("the Claude Code grant could not be verified ({cause:?})")]
    Verification { cause: Cause },
    #[error(
        "managed Claude Code settings can override this account; use a separately signed-in Code account under that policy"
    )]
    ManagedSettings,
    #[error("Claude Code could not be started: {0}")]
    Launch(#[source] std::io::Error),
}

impl DesktopCodeError {
    /// The exit status: a refusal the account's own error carries keeps its status, and
    /// every other failure of a session is an ordinary one.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Account(error) => error.exit_code(),
            _ => 1,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Account(error) => error.code(),
            Self::Switching => "desktop_code_switching",
            Self::IdentityChanged => "desktop_code_identity_changed",
            Self::Cache { .. } => "desktop_code_unavailable",
            Self::Key { .. } => "desktop_code_key_unavailable",
            Self::Verification { .. } => "desktop_code_verification_failed",
            Self::ManagedSettings => "desktop_code_managed_settings",
            Self::Launch(_) => "desktop_code_launch_failed",
        }
    }
}

/// A verified, short-lived grant. No token crosses the service's public boundary.
pub struct DesktopCodeSession {
    program: std::path::PathBuf,
    home: std::path::PathBuf,
    history_root: std::path::PathBuf,
    search_path: std::ffi::OsString,
    access: CodeAccess,
    clock: std::sync::Arc<dyn crate::time::Clock>,
}

impl std::fmt::Debug for DesktopCodeSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopCodeSession")
            .field("expires_at", &self.access.expires_at)
            .finish_non_exhaustive()
    }
}

impl DesktopCodeSession {
    pub fn expires_at(&self) -> i64 {
        self.access.expires_at / 1000
    }

    /// Runs interactively. The child receives only an access token, never a refresh token.
    pub fn run(self) -> Result<ExitStatus, DesktopCodeError> {
        if self.access.expires_at <= self.clock.now().saturating_mul(1000) {
            return Err(DesktopCodeError::Cache { reason: "expired" });
        }
        guard_managed(&crate::settings::managed_files())?;
        // Keep the conversation and files Code creates. Every launch gets a fresh config
        // so an earlier session cannot supply a different login on the next launch.
        // Every folder down to the root is looked at, up to Pitboard's desktop folder, not
        // only the last: a link at one above it would take the folders made below it to
        // where it points.
        let mut above: Vec<&std::path::Path> =
            self.history_root.ancestors().skip(1).take(2).collect();
        above.reverse();
        for folder in above {
            crate::store::tree::ensure_private_dir(folder)?;
        }
        crate::store::tree::ensure_private_dir(&self.history_root)?;
        let projects = self.history_root.join("projects");
        crate::store::tree::ensure_private_dir(&projects)?;
        let config = tempfile::Builder::new()
            .prefix("session-")
            .tempdir_in(&self.history_root)
            .map_err(DesktopCodeError::Launch)?
            .keep();
        link_projects(&config).map_err(DesktopCodeError::Launch)?;
        self.command(&config)
            .status()
            .map_err(DesktopCodeError::Launch)
    }

    // A Command's Debug includes its environment. Keep it behind this opaque boundary.
    fn command(&self, config: &std::path::Path) -> std::process::Command {
        let mut command = std::process::Command::new(&self.program);
        for name in AUTH_ENV
            .iter()
            .chain(crate::settings::OVERRIDING_ENV.iter())
        {
            command.env_remove(name);
        }
        command
            .env("HOME", &self.home)
            .env("PATH", &self.search_path);
        // A fresh config also gives Code a separate credential slot. User/project/local
        // settings can inject another token on startup and on reload (Code 2.1.289).
        command.env("CLAUDE_CONFIG_DIR", config);
        command.env("CLAUDE_SECURESTORAGE_CONFIG_DIR", config);
        command.args(["--setting-sources", ""]);
        command.env("CLAUDE_CODE_OAUTH_TOKEN", self.access.token.as_str());
        command.env("CLAUDE_CODE_OAUTH_SCOPES", self.access.scopes.join(" "));
        command
    }
}

const AUTH_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_SCOPES",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
    "CLAUDE_CODE_CUSTOM_OAUTH_URL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "ANTHROPIC_UNIX_SOCKET",
    "ANTHROPIC_CUSTOM_HEADERS",
    "CLAUDE_CODE_REMOTE_SESSION_ID",
    "CLAUDE_CODE_MANAGED_SETTINGS_PATH",
    "CLAUDE_CODE_REMOTE_SETTINGS_PATH",
    "CLAUDE_CODE_MOCK_REMOTE_SETTINGS",
];

fn guard_managed(paths: &[std::path::PathBuf]) -> Result<(), DesktopCodeError> {
    for path in paths {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(DesktopCodeError::ManagedSettings),
        };
        let settings: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| DesktopCodeError::ManagedSettings)?;
        if !settings.is_object()
            || settings
                .get("apiKeyHelper")
                .is_some_and(|value| !value.as_str().is_some_and(|text| text.trim().is_empty()))
        {
            return Err(DesktopCodeError::ManagedSettings);
        }
        if let Some(env) = settings.get("env") {
            let env = env.as_object().ok_or(DesktopCodeError::ManagedSettings)?;
            if AUTH_ENV
                .iter()
                .chain(crate::settings::OVERRIDING_ENV.iter())
                .any(|name| env.contains_key(*name))
            {
                return Err(DesktopCodeError::ManagedSettings);
            }
        }
    }
    Ok(())
}

pub(crate) fn prepare(ctx: &Context, label: &str) -> Result<DesktopCodeSession, DesktopCodeError> {
    // Read one coherent Pitboard snapshot without settling, moving or renewing anything.
    let _guard = crate::switch::exclusive(ctx)?;
    guard_managed(&crate::settings::managed_files())?;
    if crate::switch::tree_interrupted(ctx).is_some() {
        return Err(DesktopCodeError::Switching);
    }
    let state = state::load(ctx)?;
    let key = Key::new(ProviderId::Desktop, label);
    let account = state.get(&key).ok_or_else(|| Error::AccountUnknown {
        label: label.into(),
        enrolled: state.labels(ProviderId::Desktop),
    })?;
    let live = paths::support_dir(ctx).filter(|root| {
        // Where the live login is this account's own, the app has kept it fresh and the park
        // is the older copy. Whatever keeps the live folder from being read leaves the park.
        identity::identify_tree(ctx, root)
            .and_then(|found| identity::whose(&state, found))
            .is_ok_and(
                |owner| matches!(owner, identity::LiveOwner::Enrolled(owned) if owned == key),
            )
    });
    let path = if let Some(root) = live {
        paths::config_file(&root)
    } else if let Some(park) = &account.parked {
        crate::switch::check_tree_park(ctx, label, account, park)?.join("config-keys.json")
    } else {
        let root = paths::support_dir(ctx).ok_or(DesktopCodeError::IdentityChanged)?;
        if state.active_for(ProviderId::Desktop) != Some(label)
            || identity::identify_tree(ctx, &root)?
                .is_none_or(|owner| owner.account_uuid != account.account_uuid)
        {
            return Err(DesktopCodeError::IdentityChanged);
        }
        paths::config_file(&root)
    };
    let keys = config::read_keys(&path)?;
    if keys
        .0
        .get("lastKnownAccountUuid")
        .and_then(serde_json::Value::as_str)
        != Some(account.account_uuid.as_str())
    {
        return Err(DesktopCodeError::IdentityChanged);
    }
    let cache = keys
        .0
        .get("oauth:tokenCacheV2")
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or(DesktopCodeError::Cache { reason: "missing" })?;
    let encrypted = base64::engine::general_purpose::STANDARD
        .decode(cache)
        .map_err(|_| DesktopCodeError::Cache { reason: "format" })?;
    let password = ctx
        .safe_storage()
        .password(ctx, KeyRead::Approve)
        .map_err(|error| DesktopCodeError::Key {
            reason: error.reason(),
        })?;
    let encryption_key = crypto::derive_key(&password);
    let plaintext = crypto::decrypt_cache(&encryption_key, &encrypted).map_err(|_| {
        DesktopCodeError::Cache {
            reason: "decryption",
        }
    })?;
    let access = code_cache::select_access(&plaintext, &account.account_uuid, ctx.now()).map_err(
        |error| DesktopCodeError::Cache {
            reason: match error {
                code_cache::CodeCacheError::Malformed => "format",
                code_cache::CodeCacheError::Unavailable => "missing",
                code_cache::CodeCacheError::Expired => "expired",
                code_cache::CodeCacheError::Ambiguous => "ambiguous",
            },
        },
    )?;
    let owner = crate::api::owner(ctx, access.token.as_str()).map_err(|error| {
        DesktopCodeError::Verification {
            cause: Cause::of(&error),
        }
    })?;
    if owner.account_uuid != account.account_uuid
        || owner.organization_uuid != access.organization_uuid
    {
        return Err(DesktopCodeError::IdentityChanged);
    }
    Ok(DesktopCodeSession {
        program: ctx.claude_program.clone(),
        home: ctx.home.clone(),
        history_root: paths::desktop_home(ctx)
            .join("code-sessions")
            .join(hex::encode(Sha256::digest(account.account_uuid.as_bytes()))),
        search_path: ctx.search_path(),
        access,
        clock: ctx.clock.clone(),
    })
}

/// The shared `projects` folder, linked into the session's own config folder. The link is
/// resolved from the folder it sits in, so its target is written from there: a path spelled
/// relative to where Pitboard was started would point somewhere else once Claude Code reads it.
fn link_projects(config: &std::path::Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink("../projects", config.join("projects"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Owner;
    use crate::api::scripted::{Asked, ScriptedApi, ScriptedSafeStorage};
    use crate::provider::desktop::{crypto, paths};
    use crate::switch::harness::{NOW, desktop_machine};
    use base64::Engine;
    use serde_json::json;

    fn encrypted(account: &str, expiry: i64) -> String {
        use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
        let key = crypto::derive_key(b"fixture-password");
        let plaintext = json!({
            format!("acct:{account}|{}:fixture-org:https://api.anthropic.com:user:profile user:inference user:sessions:claude_code", crate::api::oauth_client()): {
                "token": "fixture-access", "refreshToken": "fixture-refresh", "expiresAt": expiry
            }
        }).to_string();
        let mut data = plaintext.into_bytes();
        let padding = 16 - data.len() % 16;
        data.extend(std::iter::repeat_n(padding as u8, padding));
        let cipher = aes::Aes128::new(GenericArray::from_slice(key.as_ref()));
        let mut previous = [b' '; 16];
        for block in data.chunks_exact_mut(16) {
            for (byte, old) in block.iter_mut().zip(previous) {
                *byte ^= old;
            }
            cipher.encrypt_block(GenericArray::from_mut_slice(block));
            previous.copy_from_slice(block);
        }
        let mut result = b"v10".to_vec();
        result.extend(data);
        base64::engine::general_purpose::STANDARD.encode(result)
    }

    fn plant(path: &std::path::Path, account: &str, expiry: i64) {
        let mut config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        config["oauth:tokenCacheV2"] = json!(encrypted(account, expiry));
        std::fs::write(path, config.to_string()).unwrap();
    }

    fn owned(api: &ScriptedApi, account: &str) {
        api.owned_by(
            "fixture-access",
            Owner {
                account_uuid: account.into(),
                email: "fixture@example.invalid".into(),
                organization_uuid: "fixture-org".into(),
            },
        );
    }

    /// The session folder is made inside the history folder, beside `projects`, so a link
    /// to its sibling reaches the shared history whatever the history folder was spelled as.
    #[test]
    fn the_session_reaches_the_shared_history_through_its_link() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("projects")).unwrap();
        std::fs::write(base.path().join("projects/conversation"), "shared").unwrap();
        let config = base.path().join("session-1");
        std::fs::create_dir(&config).unwrap();
        link_projects(&config).unwrap();
        let target = std::fs::read_link(config.join("projects")).unwrap();
        assert!(!target.is_absolute());
        assert_eq!(
            std::fs::read_to_string(config.join("projects/conversation")).unwrap(),
            "shared"
        );
    }

    #[test]
    fn live_and_parked_grants_verify_identity_without_writing_or_renewing() {
        for label in ["here", "there"] {
            let m = desktop_machine(&format!("code-{label}"));
            let path = if label == "here" {
                m.support().join("config.json")
            } else {
                paths::parks_dir(&m.ctx)
                    .join(m.there_park())
                    .join("config-keys.json")
            };
            plant(&path, label, (NOW + 3600) * 1000);
            let before = std::fs::read(&path).unwrap();
            let state_before = std::fs::read(crate::home::dir(&m.ctx).join("state.json")).unwrap();
            let api = ScriptedApi::new();
            owned(&api, label);
            let ctx = m
                .ctx
                .clone()
                .with_scripted_api(api.clone())
                .with_scripted_safe_storage(ScriptedSafeStorage::holding("fixture-password"));
            let session = prepare(&ctx, label).unwrap();
            assert_eq!(session.expires_at(), NOW + 3600);
            assert!(!format!("{session:?}").contains("fixture-access"));
            assert_eq!(api.asked(), vec![Asked::Owner("fixture-access".into())]);
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert_eq!(
                std::fs::read(crate::home::dir(&m.ctx).join("state.json")).unwrap(),
                state_before
            );
        }
    }

    /// An account whose park is older than the login the app now holds for it: the live one
    /// is the grant Desktop keeps renewing, so the stale park must not be what is read.
    #[test]
    fn a_parked_account_signed_in_live_again_uses_the_live_grant() {
        let m = desktop_machine("code-live-over-park");
        m.plant_live("there", "v10there");
        plant(
            &m.support().join("config.json"),
            "there",
            (NOW + 3600) * 1000,
        );
        plant(
            &paths::parks_dir(&m.ctx)
                .join(m.there_park())
                .join("config-keys.json"),
            "there",
            NOW * 1000,
        );
        let api = ScriptedApi::new();
        owned(&api, "there");
        let ctx = m
            .ctx
            .clone()
            .with_scripted_api(api)
            .with_scripted_safe_storage(ScriptedSafeStorage::holding("fixture-password"));

        let session = prepare(&ctx, "there").expect("the live grant is the fresh one");

        assert_eq!(session.expires_at(), NOW + 3600);
    }

    /// A refusal the account's own error carries is the command's refusal: exit 3, which a
    /// script tells from an ordinary failure.
    #[test]
    fn a_wrapped_account_refusal_keeps_its_exit_status() {
        let refusal = Error::DesktopFormatUnknown {
            what: "cookies".into(),
            found: "format 99".into(),
        };
        assert_eq!(refusal.exit_code(), 3);
        assert_eq!(DesktopCodeError::Account(refusal).exit_code(), 3);
        assert_eq!(DesktopCodeError::Switching.exit_code(), 1);
    }

    #[test]
    fn verified_owner_must_match_the_selected_account_and_organization() {
        let m = desktop_machine("code-owner");
        plant(
            &m.support().join("config.json"),
            "here",
            (NOW + 3600) * 1000,
        );
        let api = ScriptedApi::new();
        owned(&api, "somebody-else");
        let ctx = m
            .ctx
            .clone()
            .with_scripted_api(api)
            .with_scripted_safe_storage(ScriptedSafeStorage::holding("fixture-password"));
        assert!(matches!(
            prepare(&ctx, "here"),
            Err(DesktopCodeError::IdentityChanged)
        ));
    }

    #[test]
    fn an_expired_grant_never_requests_a_profile_or_refresh() {
        let m = desktop_machine("code-expired");
        plant(&m.support().join("config.json"), "here", NOW * 1000);
        let api = ScriptedApi::new();
        let ctx = m
            .ctx
            .clone()
            .with_scripted_api(api.clone())
            .with_scripted_safe_storage(ScriptedSafeStorage::holding("fixture-password"));
        assert!(matches!(
            prepare(&ctx, "here"),
            Err(DesktopCodeError::Cache { reason: "expired" })
        ));
        assert_eq!(api.calls(), 0);
    }

    /// A link where the folder of every account's Code history is kept would take the
    /// folders made below it to where it points.
    #[test]
    fn a_history_folder_above_the_root_that_is_a_link_is_not_used() {
        let m = desktop_machine("code-history-linked");
        plant(
            &m.support().join("config.json"),
            "here",
            (NOW + 3600) * 1000,
        );
        let api = ScriptedApi::new();
        owned(&api, "here");
        let ctx = m
            .ctx
            .clone()
            .with_scripted_api(api)
            .with_scripted_safe_storage(ScriptedSafeStorage::holding("fixture-password"));
        let outside = m.support().with_file_name("outside-code-history");
        std::fs::create_dir_all(&outside).unwrap();
        let home = paths::desktop_home(&ctx);
        std::fs::create_dir_all(&home).unwrap();
        std::os::unix::fs::symlink(&outside, home.join("code-sessions")).unwrap();
        let session = prepare(&ctx, "here").unwrap();
        assert!(session.run().is_err(), "a linked folder is refused");
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing was made where the link points"
        );
    }

    /// Every folder down to the history root is looked at, up to Pitboard's desktop folder.
    #[test]
    fn a_desktop_folder_that_is_a_link_holds_no_code_history() {
        let m = desktop_machine("code-history-desktop-linked");
        plant(
            &m.support().join("config.json"),
            "here",
            (NOW + 3600) * 1000,
        );
        let api = ScriptedApi::new();
        owned(&api, "here");
        let ctx = m
            .ctx
            .clone()
            .with_scripted_api(api)
            .with_scripted_safe_storage(ScriptedSafeStorage::holding("fixture-password"));
        let outside = m.support().with_file_name("outside-desktop-home");
        std::fs::create_dir_all(&outside).unwrap();
        let home = paths::desktop_home(&ctx);
        std::fs::remove_dir_all(&home).ok();
        std::os::unix::fs::symlink(&outside, &home).unwrap();
        let session = prepare(&ctx, "here").unwrap();
        assert!(session.run().is_err(), "a linked folder is refused");
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing was made where the link points"
        );
    }

    #[test]
    fn a_session_isolated_from_other_logins_runs_and_preserves_the_child_exit_code() {
        let m = desktop_machine("code-child");
        plant(
            &m.support().join("config.json"),
            "here",
            (NOW + 3600) * 1000,
        );
        let api = ScriptedApi::new();
        owned(&api, "here");
        let program = m.support().join("code-stand-in");
        let script = r#"#!/bin/sh
test "$1" = --setting-sources && test "$2" = '' || exit 71
test "$CLAUDE_CODE_OAUTH_TOKEN" = fixture-access || exit 72
test "$CLAUDE_CONFIG_DIR" = "$CLAUDE_SECURESTORAGE_CONFIG_DIR" || exit 73
test -d "$CLAUDE_CONFIG_DIR" || exit 74
printf %s "$CLAUDE_CONFIG_DIR" > "$0.config-path"
printf %s fixture-conversation > "$CLAUDE_CONFIG_DIR/projects/conversation"
exit 35
"#;
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", "printf %s \"$2\" > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(&program)
            .arg(script)
            .status()
            .unwrap();
        assert!(status.success());
        let ctx = m
            .ctx
            .clone()
            .with_claude_program(program)
            .with_scripted_api(api.clone())
            .with_scripted_safe_storage(ScriptedSafeStorage::holding("fixture-password"));
        let session = prepare(&ctx, "here").unwrap();
        let config = m.support().join("inspect-only-config");
        let command = session.command(&config);
        let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        for name in AUTH_ENV
            .iter()
            .chain(crate::settings::OVERRIDING_ENV.iter())
        {
            if matches!(
                *name,
                "CLAUDE_CODE_OAUTH_TOKEN" | "CLAUDE_CODE_OAUTH_SCOPES"
            ) {
                continue;
            }
            assert_eq!(env.get(std::ffi::OsStr::new(name)), Some(&None), "{name}");
        }
        assert!(
            !command
                .get_args()
                .any(|arg| arg.to_string_lossy().contains("fixture-access"))
        );
        assert_eq!(
            env.get(std::ffi::OsStr::new("HOME")),
            Some(&Some(ctx.home.as_os_str()))
        );
        assert_eq!(session.run().unwrap().code(), Some(35));
        let used_config = std::fs::read_to_string(
            ctx.claude_program
                .with_file_name("code-stand-in.config-path"),
        )
        .unwrap();
        assert!(std::path::Path::new(&used_config).exists());
        assert_eq!(
            std::fs::read_to_string(
                std::path::Path::new(&used_config).join("projects/conversation")
            )
            .unwrap(),
            "fixture-conversation"
        );
        assert_eq!(api.asked(), vec![Asked::Owner("fixture-access".into())]);
    }

    #[test]
    fn managed_settings_cannot_replace_or_redirect_the_selected_grant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("managed-settings.json");
        for value in [
            json!({"env": {"CLAUDE_CODE_OAUTH_TOKEN": "different-account"}}),
            json!({"env": {"ANTHROPIC_BASE_URL": "https://other.invalid"}}),
            json!({"env": {"ANTHROPIC_UNIX_SOCKET": "/tmp/another-socket"}}),
            json!({"env": {"ANTHROPIC_CUSTOM_HEADERS": "x-api-key: fixture-key"}}),
            json!({"env": {"CLAUDE_CODE_MANAGED_SETTINGS_PATH": "/tmp/another-policy"}}),
            json!({"apiKeyHelper": "echo another-key"}),
            json!([]),
        ] {
            std::fs::write(&path, value.to_string()).unwrap();
            assert!(matches!(
                guard_managed(std::slice::from_ref(&path)),
                Err(DesktopCodeError::ManagedSettings)
            ));
        }
        std::fs::write(
            &path,
            json!({"env": {"EDITOR": "vi"}, "permissions": {}}).to_string(),
        )
        .unwrap();
        assert!(guard_managed(&[path]).is_ok());
    }
}
