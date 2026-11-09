//! AWS configuration loading and caller identity.
//!
//! Profiles (SSO sessions, assume-role chains) are resolved by the SDK's own
//! credential chain. Authentication failures are rewritten into an actionable
//! message instead of a raw SDK error.

use anyhow::anyhow;
use aws_config::{BehaviorVersion, Region, SdkConfig};
use ss_core::config::Environment;

/// The resolved caller, feeding the account guardrail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    /// The AWS account the credentials resolve to.
    pub account: String,
}

/// Builds the SDK configuration for `profile` and `region`. An empty value
/// leaves that setting to the SDK's default chain.
pub(crate) async fn load(profile: &str, region: &str) -> SdkConfig {
    let mut loader = aws_config::defaults(BehaviorVersion::latest());
    if !profile.is_empty() {
        loader = loader.profile_name(profile);
    }
    if !region.is_empty() {
        loader = loader.region(Region::new(region.to_owned()));
    }
    loader.load().await
}

/// Calls `sts:GetCallerIdentity`, turning authentication failures into
/// actionable errors.
pub(crate) async fn check_identity(cfg: &SdkConfig, profile: &str) -> anyhow::Result<Identity> {
    let out = aws_sdk_sts::Client::new(cfg)
        .get_caller_identity()
        .send()
        .await
        .map_err(|e| friendly(anyhow::Error::from(e), profile))?;
    Ok(Identity {
        account: out.account.unwrap_or_default(),
    })
}

/// Reports whether an error chain looks like an expired or absent SSO session.
pub(crate) fn is_expired_sso(chain: &str) -> bool {
    [
        "SSOProviderInvalidToken",
        "token has expired",
        "InvalidGrantException",
        "ExpiredToken",
        "failed to refresh cached credentials",
    ]
    .iter()
    .any(|needle| chain.contains(needle))
}

/// Adds the login command to an expired-SSO error; other errors pass through
/// unchanged.
pub(crate) fn friendly(err: anyhow::Error, profile: &str) -> anyhow::Error {
    if !is_expired_sso(&format!("{err:#}")) {
        return err;
    }
    let p = if profile.is_empty() {
        "<profile>"
    } else {
        profile
    };
    err.context(format!(
        "SSO session expired — run `aws sso login --profile {p}`"
    ))
}

/// Refuses to continue when the credentials resolve to a different account
/// than the environment expects. An environment with no expected account
/// accepts any.
pub(crate) fn check_account(id: &Identity, env: &Environment) -> anyhow::Result<()> {
    if env.account_id.is_empty() || id.account == env.account_id {
        return Ok(());
    }
    Err(anyhow!(
        "account mismatch: profile {:?} resolves to account {}, but env {:?} expects {}",
        env.profile,
        id.account,
        env.name,
        env.account_id
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_sso_shapes_are_recognised() {
        for needle in [
            "SSOProviderInvalidToken",
            "token has expired",
            "InvalidGrantException",
            "ExpiredToken",
            "failed to refresh cached credentials",
        ] {
            assert!(
                is_expired_sso(&format!("dispatch failure: {needle}: details")),
                "{needle}"
            );
        }
        assert!(!is_expired_sso("AccessDenied: not authorized"));
    }

    #[test]
    fn friendly_names_the_login_command() {
        let e = friendly(anyhow!("ExpiredToken: the token is expired"), "dev");
        assert_eq!(
            format!("{e:#}"),
            "SSO session expired — run `aws sso login --profile dev`: ExpiredToken: the token is expired"
        );
        let e = friendly(anyhow!("ExpiredToken"), "");
        assert!(e.to_string().contains("--profile <profile>"));
        assert_eq!(
            format!("{:#}", friendly(anyhow!("AccessDenied"), "dev")),
            "AccessDenied"
        );
    }

    #[test]
    fn account_guardrail() {
        let env = Environment {
            name: "dev".into(),
            profile: "dev-sso".into(),
            account_id: "111111111111".into(),
            ..Default::default()
        };
        let id = |a: &str| Identity { account: a.into() };
        assert!(check_account(&id("111111111111"), &env).is_ok());
        assert!(
            check_account(
                &id("222222222222"),
                &Environment {
                    account_id: String::new(),
                    ..env.clone()
                }
            )
            .is_ok()
        );
        assert_eq!(
            check_account(&id("222222222222"), &env)
                .expect_err("mismatch")
                .to_string(),
            r#"account mismatch: profile "dev-sso" resolves to account 222222222222, but env "dev" expects 111111111111"#
        );
    }
}
