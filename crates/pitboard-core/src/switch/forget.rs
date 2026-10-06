//! Dropping an account and the credentials parked for it.

use super::{Error, Result, Settled, purge, tree};
use crate::context::Context;
use crate::provider::{self, ProviderId};
use crate::service::Warning;
use crate::state::{self, Key};

/// What the tool's own files say of who is signed in.
enum Live {
    Account(String),
    /// A folder login with no session in it. Its config may still name the account Log out
    /// left behind, which nobody is signed in to.
    SignedOut,
    /// Nothing to go by, or a login that could not be read, which is never read as signed
    /// out.
    Unknown,
}

fn live_account(ctx: &Context, which: ProviderId) -> Live {
    let tool = provider::of(which);
    if let Some(tree) = tool.tree()
        && let Some(root) = tree.root(ctx)
        && let Ok(found) = tree.identify(ctx, &root)
    {
        return match found {
            Some(live) => Live::Account(live.account_uuid),
            None => Live::SignedOut,
        };
    }
    tool.recorded_identity(ctx)
        .map_or(Live::Unknown, |found| Live::Account(found.account_id))
}

/// Returns the account's email.
pub fn forget(settled: Settled, key: &Key) -> Result<(String, Vec<Warning>)> {
    let Settled {
        _exclusive,
        mut state,
        ctx,
    } = settled;
    // Who is signed in is a fact about the machine. Pitboard's record of its last switch
    // is stale the moment someone signs in with the tool's own login command, and
    // forgetting the account that is actually in use throws away the only record of it.
    // Asked of the tool's own files, so it answers offline: Claude Code's config, a Codex
    // login's own claims, or the session in Claude Desktop's data folder.
    let signed_in = match (live_account(&ctx, key.provider), state.get(key)) {
        (Live::SignedOut, _) => false,
        (Live::Account(uuid), Some(account)) => account.account_uuid == uuid,
        // No live identity to compare against, so Pitboard's own record of the last switch
        // is all there is.
        _ => state.active_for(key.provider) == Some(key.label.as_str()),
    };
    if signed_in {
        return Err(Error::CannotForgetActiveAccount { label: key.typed() });
    }
    let enrolled = state.labels(key.provider);
    let account = state.remove(key).ok_or_else(|| Error::AccountUnknown {
        label: key.typed(),
        enrolled,
    })?;
    // A sign-in wait that would put this account back has nobody to put back once it is
    // gone. Emptied first, like a rename, and put back if the save fails.
    let waited = if key.provider == ProviderId::Desktop {
        tree::retarget_awaiting(&ctx, &key.label, "")?
    } else {
        None
    };
    if let Err(e) = state::save(&ctx, &state) {
        tree::restore_awaiting(&ctx, waited);
        return Err(e);
    }
    crate::fault::point("forget.recorded");
    // Under the account's usage key, so forgetting Claude Desktop's account leaves what is
    // known about the same account in Claude Code, and the other way round.
    let usage_key = account.usage_key();
    crate::readings::forget(&ctx, &usage_key);
    crate::budget::forget(&ctx, &usage_key);
    crate::history::forget(&ctx, &usage_key);
    let pending = purge(&ctx, &mut state);
    Ok((
        account.email,
        (pending > 0)
            .then_some(Warning::ParksPendingRemoval(pending))
            .into_iter()
            .collect(),
    ))
}
