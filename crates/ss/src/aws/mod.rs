//! AWS-facing side of the binary: SDK configuration, caller identity, the
//! account guardrail, and the S3 adapter.

pub(crate) mod awsx;
pub(crate) mod s3;

use std::sync::Arc;

use ss_core::config::Environment;
use ss_core::svc::s3::{Lazy, S3Port};
use ss_core::vars;

use crate::services::S3Factory;

use self::s3::{AwsS3, Options};

/// The S3 source for real invocations. Nothing touches AWS until the first
/// S3 call: the port loads the SDK configuration then. With
/// `SS_S3_ENDPOINT` set the port talks to that S3-compatible endpoint and
/// skips the identity and account checks, resolving credentials only;
/// otherwise it verifies the caller's account against the environment first.
/// Either way, a credential failure names the command that fixes it.
pub(crate) fn s3_factory() -> S3Factory {
    Arc::new(|env: &Environment, vars: &vars::Vars| {
        let env = env.clone();
        let endpoint = vars::get(vars, "SS_S3_ENDPOINT").map(str::to_owned);
        let profile = effective_profile(&env, vars);
        Arc::new(Lazy::new(move || {
            let env = env.clone();
            let endpoint = endpoint.clone();
            let profile = profile.clone();
            Box::pin(async move {
                let cfg = awsx::load(&env.profile, &env.region).await;
                let opts = match endpoint {
                    Some(endpoint) => {
                        awsx::check_credentials(&cfg, &profile).await?;
                        Options {
                            endpoint: Some(endpoint),
                            ..Options::default()
                        }
                    }
                    None => {
                        let id = awsx::check_identity(&cfg, &profile).await?;
                        awsx::check_account(&id, &env)?;
                        Options::default()
                    }
                };
                let port: Arc<dyn S3Port> = Arc::new(AwsS3::new(&cfg, opts));
                Ok(port)
            })
        })) as Arc<dyn S3Port>
    })
}

/// The profile named in authentication hints: the environment's profile,
/// else `AWS_PROFILE`, else empty.
fn effective_profile(env: &Environment, vars: &vars::Vars) -> String {
    if env.profile.is_empty() {
        vars::get(vars, "AWS_PROFILE")
            .unwrap_or_default()
            .to_owned()
    } else {
        env.profile.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_effective_profile_falls_back_to_aws_profile() {
        let vars: vars::Vars = [("AWS_PROFILE".to_owned(), "from-env".to_owned())].into();
        let named = Environment {
            profile: "dev-sso".into(),
            ..Default::default()
        };
        assert_eq!(effective_profile(&named, &vars), "dev-sso");
        assert_eq!(
            effective_profile(&Environment::default(), &vars),
            "from-env"
        );
        assert_eq!(
            effective_profile(&Environment::default(), &vars::Vars::new()),
            ""
        );
        let blank: vars::Vars = [("AWS_PROFILE".to_owned(), String::new())].into();
        assert_eq!(effective_profile(&Environment::default(), &blank), "");
    }
}
