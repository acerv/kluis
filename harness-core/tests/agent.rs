// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use harness_core::Agent;

fn assert_send<T: Send>(_: T) {}

/// A UI runs the agent on its own task, which needs a `Send` future.
#[test]
fn prompt_future_is_send() {
    let mut agent = Agent::new("deepseek-chat").unwrap();
    assert_send(agent.prompt("hi", |_| {}));
}

#[test]
fn history_starts_empty() {
    let agent = Agent::new("deepseek-chat").unwrap();
    assert!(agent.history().is_empty());
}
