// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Files the user names in a prompt with `@path`.

/// Most files a prompt attaches.
const MAX_MENTIONS: usize = 10;

/// The paths `text` names with `@`, each once: the words starting with `@`,
/// without the punctuation that may end a sentence after them.
pub(crate) fn mentions(text: &str) -> Vec<&str> {
    let mut paths = Vec::new();
    for word in text.split_whitespace() {
        let Some(path) = word.strip_prefix('@') else {
            continue;
        };
        let path = path.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '"', '\'']);
        if !path.is_empty() && !paths.contains(&path) {
            paths.push(path);
            if paths.len() == MAX_MENTIONS {
                break;
            }
        }
    }
    paths
}
