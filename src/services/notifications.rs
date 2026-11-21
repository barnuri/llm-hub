//! Generic notifications: the inbox every source writes to, and the mute rules that silence them.

pub mod budget;
pub mod budget_loop;
pub mod command_sink;
pub mod desktop;
pub mod dispatch;
pub mod inbox;
pub mod mute;
pub mod sources;
pub mod webhook;
