//! The [`Operation`] trait, its parameter declarations, and the shared
//! infrastructure handed to every operation.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;

use crate::cache::{self, Store};
use crate::config::{DiffDefaults, Environment, TraceConfig};
use crate::guard::Class;
use crate::svc::ddb::DdbPort;
use crate::svc::kubectl::KubectlPort;
use crate::svc::s3::S3Port;

use super::{Input, OpResult, Runner};

/// Static description of an operation.
#[derive(Clone, Copy, Debug)]
pub struct Meta {
    /// Stable identifier used on the command line and in the audit trail,
    /// e.g. `s3.version-diff`.
    pub id: &'static str,
    /// Short human name, e.g. `Compare object versions`.
    pub name: &'static str,
    /// One line shown in pickers and listings.
    pub summary: &'static str,
    /// Grouping shown in menus, e.g. `S3`.
    pub category: &'static str,
    /// Blast radius, which decides whether the run needs confirmation.
    pub class: Class,
}

/// The kind of value a parameter takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParamType {
    /// Free text.
    String,
    /// A whole number.
    Int,
    /// `true` or `false`.
    Bool,
    /// One of a fixed set of values.
    Enum,
    /// A length of time such as `30s`.
    Duration,
    /// An arbitrary JSON document.
    Json,
}

impl ParamType {
    /// The upper-case placeholder shown for the value in usage text.
    pub fn value_name(self) -> &'static str {
        match self {
            ParamType::String => "STRING",
            ParamType::Int => "INT",
            ParamType::Bool => "BOOL",
            ParamType::Enum => "ENUM",
            ParamType::Duration => "DURATION",
            ParamType::Json => "JSON",
        }
    }
}

impl fmt::Display for ParamType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ParamType::String => "string",
            ParamType::Int => "int",
            ParamType::Bool => "bool",
            ParamType::Enum => "enum",
            ParamType::Duration => "duration",
            ParamType::Json => "json",
        })
    }
}

/// One suggested value for a parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    /// The value to submit.
    pub value: String,
    /// A human label shown instead of the value, when set.
    pub label: Option<String>,
}

/// A boxed, sendable future.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Computes the suggested values for a parameter from the shared [`Deps`] and
/// the input collected so far, so later parameters can depend on earlier ones.
pub type SuggestFn =
    Arc<dyn Fn(Deps, Input) -> BoxFuture<anyhow::Result<Vec<Choice>>> + Send + Sync>;

/// One declared operation input. Front-ends build their flags and forms from
/// these declarations.
#[derive(Clone)]
pub struct Param {
    /// The parameter name, used as the flag name and the [`Input`] key.
    pub name: &'static str,
    /// A human label shown in forms; the name is shown when unset.
    pub label: Option<&'static str>,
    /// The kind of value the parameter takes.
    pub ty: ParamType,
    /// Whether the run cannot start without a value.
    pub required: bool,
    /// The value used when none is given.
    pub default: Option<&'static str>,
    /// One-line help text.
    pub help: Option<&'static str>,
    /// Suggests values for interactive pickers and completion.
    pub suggest: Option<SuggestFn>,
}

impl Param {
    /// An optional string parameter with no label, default, help, or suggestions.
    pub fn new(name: &'static str) -> Param {
        Param {
            name,
            label: None,
            ty: ParamType::String,
            required: false,
            default: None,
            help: None,
            suggest: None,
        }
    }

    /// Sets the human label.
    pub fn label(mut self, label: &'static str) -> Param {
        self.label = Some(label);
        self
    }

    /// Sets the value type.
    pub fn ty(mut self, ty: ParamType) -> Param {
        self.ty = ty;
        self
    }

    /// Sets whether a value is required.
    pub fn required(mut self, required: bool) -> Param {
        self.required = required;
        self
    }

    /// Sets the default value.
    pub fn default(mut self, default: &'static str) -> Param {
        self.default = Some(default);
        self
    }

    /// Sets the help text.
    pub fn help(mut self, help: &'static str) -> Param {
        self.help = Some(help);
        self
    }

    /// Sets the suggestion function.
    pub fn suggest(mut self, suggest: SuggestFn) -> Param {
        self.suggest = Some(suggest);
        self
    }

    /// The label, falling back to the name.
    pub fn display_label(&self) -> &str {
        self.label.unwrap_or(self.name)
    }
}

impl fmt::Debug for Param {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Param")
            .field("name", &self.name)
            .field("label", &self.label)
            .field("ty", &self.ty)
            .field("required", &self.required)
            .field("default", &self.default)
            .field("help", &self.help)
            .field("suggest", &self.suggest.as_ref().map(|_| "<fn>"))
            .finish()
    }
}

/// Shared infrastructure, built once at startup and cloned into every
/// operation.
#[derive(Clone)]
pub struct Deps {
    /// Cache for immutable fetched bodies.
    pub cache: Arc<dyn Store>,
    /// The resolved target environment.
    pub env: Environment,
    /// Defaults for diff-producing operations.
    pub diff: DiffDefaults,
    /// Trace configuration for cross-service lookups.
    pub trace: TraceConfig,
    /// The S3 port, when this invocation has one.
    pub s3: Option<Arc<dyn S3Port>>,
    /// The DynamoDB port, when this invocation has one.
    pub ddb: Option<Arc<dyn DdbPort>>,
    /// The Kubernetes port, when this invocation has one.
    pub kubectl: Option<Arc<dyn KubectlPort>>,
}

impl Default for Deps {
    fn default() -> Self {
        Deps {
            cache: Arc::new(cache::Nop),
            env: Environment::default(),
            diff: DiffDefaults::default(),
            trace: TraceConfig::default(),
            s3: None,
            ddb: None,
            kubectl: None,
        }
    }
}

/// A named, parameterized support diagnostic. Every registered operation is a
/// command-line subcommand and an interactive menu entry.
#[async_trait]
pub trait Operation: Send + Sync {
    /// Static description of the operation.
    fn meta(&self) -> Meta;
    /// The inputs the operation accepts, in display order.
    fn params(&self) -> Vec<Param>;
    /// Runs the operation, driving its steps through `r`.
    async fn run(&self, r: &mut Runner, input: &Input) -> anyhow::Result<OpResult>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_deps_have_no_ddb_port() {
        assert!(Deps::default().ddb.is_none());
    }

    #[test]
    fn default_deps_have_no_kubectl() {
        assert!(Deps::default().kubectl.is_none());
    }

    #[test]
    fn param_type_names_are_stable() {
        let all = [
            (ParamType::String, "string", "STRING"),
            (ParamType::Int, "int", "INT"),
            (ParamType::Bool, "bool", "BOOL"),
            (ParamType::Enum, "enum", "ENUM"),
            (ParamType::Duration, "duration", "DURATION"),
            (ParamType::Json, "json", "JSON"),
        ];
        for (ty, display, value_name) in all {
            assert_eq!(ty.to_string(), display);
            assert_eq!(ty.value_name(), value_name);
        }
    }

    #[test]
    fn display_label_falls_back_to_name() {
        let p = Param::new("bucket");
        assert_eq!(p.display_label(), "bucket");
        assert_eq!(p.label("Bucket name").display_label(), "Bucket name");
    }
}
