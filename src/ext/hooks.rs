//! HTTP hooks: a service the hub calls in the `request`, `attempt` and `complete` phases. They
//! sit under `hooks:` in the rules file. See `docs/extensions.md`.

mod hook;
mod hook_reply;
mod hook_set;
mod hook_spec;

pub(crate) use hook_set::HookSet;
pub(crate) use hook_spec::HookSpec;
