//! Domain model, traits and errors shared by every crate.
//! No UI code and no source-specific code lives here.

pub mod domain;
pub mod exec;
#[cfg(feature = "fake")]
pub mod fake;
pub mod log;
pub mod orchestrator;
pub mod source;
pub mod version;

pub use domain::*;
pub use source::{
    ApplyReport, ProgressEvent, ProgressSender, SourceError, TrackedStore, UpdateSource,
};
