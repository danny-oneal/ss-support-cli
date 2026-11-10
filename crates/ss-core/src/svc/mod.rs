//! Service ports: the narrow interfaces operations use to reach external
//! systems (S3 and DynamoDB), plus the error type they share.

pub mod ddb;
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
}
