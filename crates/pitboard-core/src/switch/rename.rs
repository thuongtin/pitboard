//! Changing the label an account is enrolled under.

use super::{Result, Settled, tree};
use crate::provider::ProviderId;
use crate::state::{self, Key};

/// Returns the account's email.
pub fn rename(settled: Settled, from: &Key, to: &str) -> Result<String> {
    let Settled {
        _exclusive,
        mut state,
        ctx,
    } = settled;
    let email = state.relabel(from, to)?.email.clone();
    // The wait first: one that cannot follow the rename stops it, where a wait left naming
    // the old label would offer an account that is no longer there. If the save then fails,
    // the wait goes back.
    let waited = if from.provider == ProviderId::Desktop {
        tree::retarget_awaiting(&ctx, &from.label, to)?
    } else {
        None
    };
    if let Err(e) = state::save(&ctx, &state) {
        tree::restore_awaiting(&ctx, waited);
        return Err(e);
    }
    Ok(email)
}
