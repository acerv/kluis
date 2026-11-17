// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::path::Path;

pub fn p(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
