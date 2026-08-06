//! The command-line surface and the orchestration behind it.

mod cli;
mod completions;
mod console_events;
mod dispatch;
mod explain;
mod host;
mod list;
mod merge;
mod migrate;
mod run;
mod suppress;
mod when;

pub use cli::{
    Cli, Command, CompletionsArgs, ConfigArgs, ExplainArgs, FeatureArgs, ListArgs, ListKind, MergeArgs, MigrateArgs,
    RunArgs, SelectArgs, SuppressArgs,
};
pub use dispatch::{run, EXIT_CANNOT_PROCEED, EXIT_GATE_FAILED, EXIT_INTERNAL, EXIT_OK, EXIT_USAGE};
pub use host::Host;
pub use when::When;
