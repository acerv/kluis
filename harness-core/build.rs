// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Lists the skills that come with `kl`: each folder of `skills/` at the
//! root of the repository, built in with its `SKILL.md`.

use std::path::Path;
use std::{env, fs};

fn main() {
    let root = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("../skills");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut skills: Vec<_> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("{}: {e}", root.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    skills.sort();
    let mut code = String::from("&[\n");
    for dir in skills {
        let name = dir.file_name().unwrap().to_str().unwrap();
        let file = dir.join("SKILL.md").canonicalize().unwrap_or_else(|e| {
            panic!("{}/SKILL.md: {e}", dir.display());
        });
        code.push_str(&format!("    ({name:?}, include_str!({:?})),\n", file));
    }
    code.push(']');
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("bundled_skills.rs");
    fs::write(out, code).unwrap();
}
