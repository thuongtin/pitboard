//! Selects an existing Claude Code access grant from decrypted Desktop cache text.
//! This reader neither retains refresh credentials nor renews a grant.

use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::fmt;
use zeroize::Zeroizing;

pub(crate) struct CodeAccess {
    pub(crate) token: Zeroizing<String>,
    pub(crate) scopes: Vec<String>,
    /// Desktop stores this as epoch milliseconds.
    pub(crate) expires_at: i64,
    pub(crate) organization_uuid: String,
}

impl fmt::Debug for CodeAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodeAccess")
            .field("token", &"[redacted]")
            .field("scopes", &self.scopes)
            .field("expires_at", &self.expires_at)
            .field("organization_uuid", &self.organization_uuid)
            .finish()
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum CodeCacheError {
    #[error("the Desktop token cache has an unknown format")]
    Malformed,
    #[error("the Desktop token cache has no Claude Code access grant for this account")]
    Unavailable,
    #[error("the Desktop Claude Code access grant has expired")]
    Expired,
    #[error("the Desktop token cache has more than one current Claude Code access grant")]
    Ambiguous,
}

/// Picks one unexpired grant for the exact account, client, API host and required scopes.
/// All parse failures have fixed messages; JSON and credentials never enter an error.
pub(crate) fn select_access(
    plaintext: &str,
    account_uuid: &str,
    now_seconds: i64,
) -> Result<CodeAccess, CodeCacheError> {
    let mut deserializer = serde_json::Deserializer::from_str(plaintext);
    let result = deserializer
        .deserialize_map(CacheVisitor {
            account_uuid,
            now_millis: now_seconds.saturating_mul(1_000),
        })
        .map_err(|_| CodeCacheError::Malformed)?;
    deserializer.end().map_err(|_| CodeCacheError::Malformed)?;
    result
}

/// Unknown fields, including refreshToken, are skipped without retaining their values.
#[derive(Deserialize)]
struct Grant {
    #[serde(deserialize_with = "secret_string")]
    token: Zeroizing<String>,
    #[serde(rename = "expiresAt")]
    expires_at: i64,
}

fn secret_string<'de, D>(deserializer: D) -> Result<Zeroizing<String>, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map(Zeroizing::new)
}

struct CacheVisitor<'a> {
    account_uuid: &'a str,
    now_millis: i64,
}

impl<'de> Visitor<'de> for CacheVisitor<'_> {
    type Value = Result<CodeAccess, CodeCacheError>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Desktop token cache object")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut selected = None;
        let mut expired = false;
        let mut ambiguous = false;
        while let Some(key) = map.next_key::<String>()? {
            let Some((organization_uuid, scopes)) = target_key(&key, self.account_uuid) else {
                map.next_value::<IgnoredAny>()?;
                continue;
            };
            let grant = map.next_value::<Grant>()?;
            if grant.token.trim().is_empty() || grant.expires_at <= 0 {
                return Ok(Err(CodeCacheError::Malformed));
            }
            if grant.expires_at <= self.now_millis {
                expired = true;
                continue;
            }
            if selected.is_some() {
                ambiguous = true;
                continue;
            }
            selected = Some(CodeAccess {
                token: grant.token,
                scopes,
                expires_at: grant.expires_at,
                organization_uuid: organization_uuid.to_owned(),
            });
        }
        Ok(if ambiguous {
            Err(CodeCacheError::Ambiguous)
        } else if let Some(access) = selected {
            Ok(access)
        } else if expired {
            Err(CodeCacheError::Expired)
        } else {
            Err(CodeCacheError::Unavailable)
        })
    }
}

fn target_key<'a>(key: &'a str, account_uuid: &str) -> Option<(&'a str, Vec<String>)> {
    let (account, grant) = key.strip_prefix("acct:")?.split_once('|')?;
    if account != account_uuid || account.is_empty() {
        return None;
    }
    let (client, grant) = grant.split_once(':')?;
    if client != crate::api::oauth_client() {
        return None;
    }
    let (organization, grant) = grant.split_once(':')?;
    if organization.trim().is_empty() {
        return None;
    }
    let scope_text = grant.strip_prefix("https://api.anthropic.com:")?;
    let mut scopes = Vec::new();
    for scope in scope_text.split_whitespace() {
        if !scopes.iter().any(|seen| seen == scope) {
            scopes.push(scope.to_owned());
        }
    }
    let required = [
        "user:inference",
        "user:profile",
        "user:sessions:claude_code",
    ];
    required
        .iter()
        .all(|scope| scopes.iter().any(|found| found == scope))
        .then_some((organization, scopes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const ACCOUNT: &str = "fixture-account";
    const CLIENT: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
    const SCOPES: &str = "user:inference user:profile user:sessions:claude_code";
    const NOW: i64 = 1_800_000_000;
    const FRESH: i64 = NOW * 1_000 + 60_000;

    fn key(account: &str, client: &str, org: &str, scopes: &str) -> String {
        format!("acct:{account}|{client}:{org}:https://api.anthropic.com:{scopes}")
    }

    fn grant(expires: i64) -> Value {
        json!({"token": "fixture-access-secret", "expiresAt": expires,
            "refreshToken": "fixture-refresh-must-not-be-retained"})
    }

    fn one(entry: Value) -> String {
        json!({key(ACCOUNT, CLIENT, "fixture-org", SCOPES): entry}).to_string()
    }

    #[test]
    fn selects_code_grant_amid_other_accounts_clients_and_cowork() {
        let input = json!({
            key("other-account", CLIENT, "other-org", SCOPES): grant(FRESH),
            key(ACCOUNT, "other-client", "other-org", SCOPES): grant(FRESH),
            key(ACCOUNT, CLIENT, "fixture-org", "user:profile user:inference"): grant(FRESH),
            key(ACCOUNT, CLIENT, "fixture-org", SCOPES): grant(FRESH),
        });
        let access = select_access(&input.to_string(), ACCOUNT, NOW).unwrap();
        assert!(access.token.as_str() == "fixture-access-secret");
        assert_eq!(access.expires_at, FRESH);
        assert_eq!(access.organization_uuid, "fixture-org");
        assert_eq!(access.scopes, SCOPES.split_whitespace().collect::<Vec<_>>());
        assert!(!format!("{access:?}").contains("fixture-refresh"));
    }

    #[test]
    fn rejects_expired_and_boundary_grants_in_milliseconds() {
        for expires in [NOW * 1_000 - 1, NOW * 1_000] {
            assert_eq!(
                select_access(&one(grant(expires)), ACCOUNT, NOW).unwrap_err(),
                CodeCacheError::Expired
            );
        }
        assert!(select_access(&one(grant(NOW * 1_000 + 1)), ACCOUNT, NOW).is_ok());
    }

    #[test]
    fn rejects_two_fresh_organization_grants() {
        let input = json!({
            key(ACCOUNT, CLIENT, "first-org", SCOPES): grant(FRESH),
            key(ACCOUNT, CLIENT, "second-org", SCOPES): grant(FRESH),
        });
        assert_eq!(
            select_access(&input.to_string(), ACCOUNT, NOW).unwrap_err(),
            CodeCacheError::Ambiguous
        );
    }

    #[test]
    fn a_fresh_grant_wins_over_an_expired_one() {
        let input = json!({
            key(ACCOUNT, CLIENT, "old-org", SCOPES): grant(NOW * 1_000 - 1),
            key(ACCOUNT, CLIENT, "current-org", SCOPES): grant(FRESH),
        });
        assert_eq!(
            select_access(&input.to_string(), ACCOUNT, NOW)
                .unwrap()
                .organization_uuid,
            "current-org"
        );
    }

    #[test]
    fn unrelated_or_lesser_grants_are_unavailable_even_if_malformed() {
        for candidate in [
            key("fixture-account-extra", CLIENT, "org", SCOPES),
            key(ACCOUNT, "other-client", "org", SCOPES),
            key(ACCOUNT, CLIENT, "org", "user:inference user:profile"),
            key(ACCOUNT, CLIENT, "", SCOPES),
        ] {
            let input = json!({candidate: {"token": ""}});
            assert_eq!(
                select_access(&input.to_string(), ACCOUNT, NOW).unwrap_err(),
                CodeCacheError::Unavailable
            );
        }
    }

    #[test]
    fn malformed_target_entries_fail_without_echoing_secrets() {
        for entry in [
            json!({"expiresAt": FRESH}),
            json!({"token": "fixture-secret"}),
            json!({"accessToken": "fixture-secret", "expiresAt": FRESH}),
            json!({"token": "", "expiresAt": FRESH}),
            json!({"token": " \t ", "expiresAt": FRESH}),
            json!({"token": "fixture-secret", "expiresAt": "1800000060000"}),
            json!({"token": "fixture-secret", "expiresAt": 1.5}),
            json!({"token": "fixture-secret", "expiresAt": 0}),
            json!({"token": "fixture-secret", "expiresAt": -1}),
            json!(null),
        ] {
            let error = select_access(&one(entry), ACCOUNT, NOW).unwrap_err();
            assert_eq!(error, CodeCacheError::Malformed);
            assert!(!error.to_string().contains("fixture-secret"));
        }
        for input in ["not json", "[]", "null"] {
            assert_eq!(
                select_access(input, ACCOUNT, NOW).unwrap_err(),
                CodeCacheError::Malformed
            );
        }
    }

    #[test]
    fn scopes_keep_the_cache_order_without_duplicates() {
        let scopes =
            "user:profile user:inference user:profile user:sessions:claude_code user:extra";
        let input = json!({key(ACCOUNT, CLIENT, "org", scopes): grant(FRESH)});
        let access = select_access(&input.to_string(), ACCOUNT, NOW).unwrap();
        assert_eq!(
            access.scopes,
            [
                "user:profile",
                "user:inference",
                "user:sessions:claude_code",
                "user:extra"
            ]
        );
    }

    #[test]
    fn overflow_in_the_clock_cannot_make_an_expired_grant_fresh() {
        assert_eq!(
            select_access(&one(grant(i64::MAX)), ACCOUNT, i64::MAX).unwrap_err(),
            CodeCacheError::Expired
        );
    }

    #[test]
    fn debug_redacts_the_access_token() {
        let access = CodeAccess {
            token: Zeroizing::new("fixture-access-secret".into()),
            scopes: vec![],
            expires_at: FRESH,
            organization_uuid: "org".into(),
        };
        let shown = format!("{access:?}");
        assert!(!shown.contains("fixture-access-secret"));
        assert!(shown.contains("[redacted]"));
    }

    #[test]
    fn only_the_exact_api_host_and_required_scopes_match() {
        for candidate in [
            key(ACCOUNT, CLIENT, "org", SCOPES).replace(
                "https://api.anthropic.com:",
                "https://api.anthropic.com.other:",
            ),
            key(
                ACCOUNT,
                CLIENT,
                "org",
                "user:inference user:profile user:sessions:claude_code_extra",
            ),
        ] {
            let input = json!({candidate: grant(FRESH)});
            assert_eq!(
                select_access(&input.to_string(), ACCOUNT, NOW).unwrap_err(),
                CodeCacheError::Unavailable
            );
        }
    }

    #[test]
    fn refresh_fields_are_ignored_and_trailing_json_is_rejected() {
        let mut entry = grant(FRESH);
        entry["refreshToken"] = json!({"unexpected": [true, null, 123]});
        let input = one(entry);
        assert!(select_access(&input, ACCOUNT, NOW).is_ok());
        assert_eq!(
            select_access(&format!("{input} {{}}"), ACCOUNT, NOW).unwrap_err(),
            CodeCacheError::Malformed
        );
    }
}
