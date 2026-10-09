//! Collaboration status for agents (spike): peers, join state and per-pair
//! latency, so a demo can be measured through the control channel.

use crate::{CmdResult, CommandSpec, Session};

pub fn specs() -> Vec<CommandSpec> {
    vec![CommandSpec::new("collab.stats", "Collaboration Status", "Agents", |s, _| stats(s)).params("{}").pure()]
}

fn stats(s: &mut Session) -> CmdResult {
    Ok(s.collab_stats())
}
