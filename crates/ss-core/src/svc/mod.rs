//! Service ports: the narrow interfaces operations use to reach external
//! systems (S3, DynamoDB and Kubernetes), plus the error type they share.

pub mod ddb;
pub mod kubectl;
pub mod s3;

/// A failure reported by a service port.
#[derive(Debug, thiserror::Error)]
pub enum SvcError {
    /// The requested resource does not exist.
    #[error("not found: {what}")]
    NotFound {
        /// What was looked up, e.g. `bucket/key`.
        what: String,
        /// The underlying error, when one produced this.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },
    /// What a write was based on has changed since it was read, so the
    /// write was not made.
    #[error("changed since it was read: {what}")]
    Changed {
        /// What changed, e.g. `bucket/key`.
        what: String,
    },
    /// Any other failure.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl SvcError {
    /// A not-found error with no underlying cause.
    pub fn not_found(what: impl Into<String>) -> Self {
        Self::NotFound {
            what: what.into(),
            source: None,
        }
    }

    /// A not-found error that keeps the error which revealed it.
    pub fn not_found_from(
        what: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::NotFound {
            what: what.into(),
            source: Some(Box::new(source)),
        }
    }

    /// Whether this is a [`SvcError::NotFound`].
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. })
    }

    /// An error saying `what` has changed since it was read.
    pub fn changed(what: impl Into<String>) -> Self {
        Self::Changed { what: what.into() }
    }

    /// Whether this is a [`SvcError::Changed`].
    pub fn is_changed(&self) -> bool {
        matches!(self, Self::Changed { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_changed_error_says_what_changed_and_is_not_a_not_found() {
        let err = SvcError::changed("ss-config/cfg/acme.json");

        assert_eq!(
            err.to_string(),
            "changed since it was read: ss-config/cfg/acme.json"
        );
        assert!(err.is_changed());
        assert!(!err.is_not_found());
        assert!(!SvcError::not_found("x").is_changed());
        assert!(!SvcError::Other(anyhow::anyhow!("x")).is_changed());
    }
}
