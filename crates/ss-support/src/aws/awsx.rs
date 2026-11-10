//! AWS configuration loading and caller identity.
//!
//! Profiles (SSO sessions, assume-role chains) are resolved by the SDK's own
//! credential chain. Authentication failures are rewritten into an actionable
//! message instead of a raw SDK error.

use anyhow::anyhow;
use aws_config::{BehaviorVersion, Region, SdkConfig};
use aws_sdk_s3::config::ProvideCredentials;
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

/// Loads the SDK configuration for `env` and applies the guardrails every
/// service adapter shares. With a custom `endpoint` only the credentials are
/// resolved; otherwise the caller's account is verified against the
/// environment first. `profile` names the login command in authentication
/// hints.
///
/// # Errors
///
/// Returns an error when credentials cannot be resolved, the caller identity
/// cannot be read, or the account does not match the environment.
pub(crate) async fn sdk_config(
    env: &Environment,
    endpoint: Option<&str>,
    profile: &str,
) -> anyhow::Result<SdkConfig> {
    let cfg = load(&env.profile, &env.region).await;
    if endpoint.is_some() {
        check_credentials(&cfg, profile).await?;
    } else {
        let id = check_identity(&cfg, profile).await?;
        check_account(&id, env)?;
    }
    Ok(cfg)
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

/// Resolves the configured credentials once, turning authentication failures
/// into actionable errors. A configuration without a credentials provider
/// passes.
pub(crate) async fn check_credentials(cfg: &SdkConfig, profile: &str) -> anyhow::Result<()> {
    let Some(provider) = cfg.credentials_provider() else {
        return Ok(());
    };
    provider
        .provide_credentials()
        .await
        .map_err(|e| friendly(anyhow::Error::from(e), profile))?;
    Ok(())
}

/// Reports whether an error chain looks like an expired or absent SSO session.
pub(crate) fn is_expired_sso(chain: &str) -> bool {
    [
        "SSOProviderInvalidToken",
        "token has expired",
        "InvalidGrantException",
        "ExpiredToken",
        "failed to refresh cached credentials",
        "failed to load the cached SSO token",
        "UnauthorizedException",
    ]
    .iter()
    .any(|needle| chain.contains(needle))
}

/// Reports whether an error chain says no credential source produced
/// credentials at all.
pub(crate) fn is_missing_credentials(chain: &str) -> bool {
    chain.contains("no providers in chain provided credentials")
}

/// Adds the login command to an expired-SSO or missing-credentials error;
/// other errors pass through unchanged. The whole error chain is searched.
pub(crate) fn friendly(err: anyhow::Error, profile: &str) -> anyhow::Error {
    let chain = format!("{err:#}");
    let p = if profile.is_empty() {
        "<profile>"
    } else {
        profile
    };
    if is_expired_sso(&chain) {
        err.context(format!(
            "SSO session expired — run `aws sso login --profile {p}`"
        ))
    } else if is_missing_credentials(&chain) {
        err.context(format!(
            "no AWS credentials for profile {p} — run `aws sso login --profile {p}` or set AWS_PROFILE"
        ))
    } else {
        err
    }
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
            "failed to load the cached SSO token",
            "UnauthorizedException",
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
    fn a_never_logged_in_sso_session_names_the_login_command() {
        let e = friendly(anyhow!("failed to load the cached SSO token"), "dev");
        assert_eq!(
            format!("{e:#}"),
            "SSO session expired — run `aws sso login --profile dev`: failed to load the cached SSO token"
        );
    }

    #[test]
    fn an_unauthorized_legacy_sso_token_names_the_login_command() {
        let e = friendly(
            anyhow!("UnauthorizedException: Session token not found or invalid"),
            "dev",
        );
        assert_eq!(
            format!("{e:#}"),
            "SSO session expired — run `aws sso login --profile dev`: UnauthorizedException: Session token not found or invalid"
        );
    }

    #[test]
    fn the_whole_error_chain_is_searched() {
        let nested = anyhow!("failed to load the cached SSO token")
            .context("an error occurred while loading credentials")
            .context("dispatch failure");
        let e = friendly(nested, "dev");
        assert_eq!(
            e.to_string(),
            "SSO session expired — run `aws sso login --profile dev`"
        );
        assert!(
            format!("{e:#}").ends_with(": dispatch failure: an error occurred while loading credentials: failed to load the cached SSO token"),
            "{e:#}"
        );
    }

    #[test]
    fn missing_credentials_name_the_login_command_and_aws_profile() {
        let raw =
            || anyhow!("no providers in chain provided credentials").context("dispatch failure");
        assert_eq!(
            format!("{:#}", friendly(raw(), "dev")),
            "no AWS credentials for profile dev — run `aws sso login --profile dev` or set AWS_PROFILE: dispatch failure: no providers in chain provided credentials"
        );
        assert_eq!(
            friendly(raw(), "").to_string(),
            "no AWS credentials for profile <profile> — run `aws sso login --profile <profile>` or set AWS_PROFILE"
        );
    }

    // aws-config exposes in-memory profile files only through deprecated
    // aliases; they keep this test off the real `~/.aws` files.
    #[allow(deprecated)]
    #[tokio::test]
    async fn check_credentials_resolves_or_explains() {
        use aws_config::meta::credentials::CredentialsProviderChain;
        use aws_config::profile::ProfileFileCredentialsProvider;
        use aws_config::profile::profile_file::{ProfileFileKind, ProfileFiles};
        use aws_sdk_s3::config::{Credentials, SharedCredentialsProvider};

        let with = |provider: SharedCredentialsProvider| {
            SdkConfig::builder().credentials_provider(provider).build()
        };
        let fixed = Credentials::new("key", "secret", None, None, "static");
        check_credentials(&with(SharedCredentialsProvider::new(fixed)), "dev")
            .await
            .expect("static credentials resolve");

        let no_profiles = ProfileFileCredentialsProvider::builder()
            .profile_files(
                ProfileFiles::builder()
                    .with_contents(ProfileFileKind::Config, "")
                    .with_contents(ProfileFileKind::Credentials, "")
                    .build(),
            )
            .build();
        let empty = CredentialsProviderChain::first_try("profile", no_profiles);
        let err = check_credentials(&with(SharedCredentialsProvider::new(empty)), "dev")
            .await
            .expect_err("nothing provides credentials");
        assert_eq!(
            err.to_string(),
            "no AWS credentials for profile dev — run `aws sso login --profile dev` or set AWS_PROFILE"
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
