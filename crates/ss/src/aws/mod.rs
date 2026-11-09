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
/// skips the identity and account checks; otherwise it verifies the caller's
/// account against the environment first.
pub(crate) fn s3_factory() -> S3Factory {
    Arc::new(|env: &Environment, vars: &vars::Vars| {
        let env = env.clone();
        let endpoint = vars::get(vars, "SS_S3_ENDPOINT").map(str::to_owned);
        Arc::new(Lazy::new(move || {
            let env = env.clone();
            let endpoint = endpoint.clone();
            Box::pin(async move {
                let cfg = awsx::load(&env.profile, &env.region).await;
                let opts = match endpoint {
                    Some(endpoint) => Options {
                        endpoint: Some(endpoint),
                        ..Options::default()
                    },
                    None => {
                        let id = awsx::check_identity(&cfg, &env.profile).await?;
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
