//! Environment allowlist enforcement.

use std::fmt;

/// Blast radius of an operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    /// Changes nothing.
    Read,
    /// Changes state.
    Mutate,
    /// Changes state in a way that is hard to undo.
    Destructive,
}

impl Class {
    /// The stable lowercase name of the class.
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Read => "read",
            Class::Mutate => "mutate",
            Class::Destructive => "destructive",
        }
    }
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The operation asking for authorization.
#[derive(Clone, Debug)]
pub struct Request {
    /// Identifier of the operation.
    pub op_id: String,
    /// Blast radius of the operation.
    pub class: Class,
    /// Name the operator must type back to confirm a destructive operation.
    pub resource: String,
}

/// Why an operation was not authorized.
#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    /// The environment is not in the allowlist; there is no override.
    #[error("environment {env:?} is not in the configured allowlist")]
    EnvNotAllowed {
        /// Operation that was refused.
        op: String,
        /// Environment that is not allowed.
        env: String,
    },
    /// The operator declined the confirmation.
    #[error("operation not confirmed")]
    NotConfirmed {
        /// Operation that was declined.
        op: String,
    },
    /// Confirmation was required but nothing could ask for it.
    #[error("no confirmer available: operation not confirmed")]
    NoConfirmer {
        /// Operation that could not be confirmed.
        op: String,
    },
}

impl GuardError {
    /// True when the operation was declined at confirmation rather than
    /// refused by policy.
    pub fn is_decline(&self) -> bool {
        matches!(self, Self::NotConfirmed { .. } | Self::NoConfirmer { .. })
    }
}

/// Authorizes an operation before it runs.
pub trait Guard: Send + Sync {
    /// Returns `Ok` when the operation may proceed.
    fn authorize(&self, req: &Request) -> Result<(), GuardError>;
}

/// Permits every operation.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAll;

impl Guard for AllowAll {
    fn authorize(&self, _req: &Request) -> Result<(), GuardError> {
        Ok(())
    }
}

/// Refuses mutating and destructive operations outright unless the resolved
/// environment is allowlisted. Nothing overrides it, neither a flag nor a
/// confirmation.
#[derive(Clone, Debug)]
pub struct EnvAllowlist {
    /// Resolved environment name.
    pub name: String,
    /// Whether the environment is in the allowlist.
    pub allowed: bool,
}

impl Guard for EnvAllowlist {
    fn authorize(&self, req: &Request) -> Result<(), GuardError> {
        if req.class == Class::Read || self.allowed {
            return Ok(());
        }
        Err(GuardError::EnvNotAllowed {
            op: req.op_id.clone(),
            env: self.name.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(class: Class) -> Request {
        Request {
            op_id: "x".into(),
            class,
            resource: "r".into(),
        }
    }

    fn list(name: &str, allowed: bool) -> EnvAllowlist {
        EnvAllowlist {
            name: name.into(),
            allowed,
        }
    }

    #[test]
    fn read_always_passes() {
        assert!(list("prod", false).authorize(&req(Class::Read)).is_ok());
    }

    #[test]
    fn env_allowlist_is_a_hard_stop() {
        for class in [Class::Mutate, Class::Destructive] {
            let err = list("prod", false).authorize(&req(class)).unwrap_err();
            assert!(
                matches!(&err, GuardError::EnvNotAllowed { op, env } if op == "x" && env == "prod")
            );
            assert!(!err.is_decline());
            assert_eq!(
                err.to_string(),
                r#"environment "prod" is not in the configured allowlist"#
            );
        }
    }

    #[test]
    fn allowlisted_env_passes_every_class() {
        for class in [Class::Read, Class::Mutate, Class::Destructive] {
            assert!(list("dev", true).authorize(&req(class)).is_ok());
            assert!(AllowAll.authorize(&req(class)).is_ok());
        }
    }

    #[test]
    fn class_names_are_stable() {
        assert_eq!(Class::Read.to_string(), "read");
        assert_eq!(Class::Mutate.to_string(), "mutate");
        assert_eq!(Class::Destructive.as_str(), "destructive");
    }

    #[test]
    fn confirmation_failures_are_declines() {
        let not_confirmed = GuardError::NotConfirmed { op: "x".into() };
        let no_confirmer = GuardError::NoConfirmer { op: "x".into() };
        assert!(not_confirmed.is_decline());
        assert!(no_confirmer.is_decline());
        assert_eq!(not_confirmed.to_string(), "operation not confirmed");
        assert_eq!(
            no_confirmer.to_string(),
            "no confirmer available: operation not confirmed"
        );
    }
}
