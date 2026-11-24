// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The skills that come with `kl`, installed in `~/.kluis/skills` only when
//! the user asks. A manifest there records the hash of each one as last
//! installed, so an update replaces a skill the user left untouched and
//! keeps one they edited.

use std::collections::BTreeMap;
use std::fs;
use std::hash::Hasher;
use std::io::{self, ErrorKind, Write};
use std::path::Path;

use fnv::FnvHasher;

use crate::private_file::{create_dir, read_capped, replace, sibling};
use crate::skills::{MAX_SKILL_BYTES, SKILL_FILE};

/// The bundled skills, sorted by name: the name of each folder of `skills/`
/// in the repository and its `SKILL.md` (see `build.rs`).
pub const BUNDLED_SKILLS: &[(&str, &str)] =
    include!(concat!(env!("OUT_DIR"), "/bundled_skills.rs"));

/// The manifest, in the skills folder. A file, so not taken for a skill.
const MANIFEST: &str = ".bundled";

/// Largest manifest read.
const MAX_MANIFEST_BYTES: usize = 16 * 1024;

/// What installing a bundled skill did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    /// It was missing and is now there.
    Added,
    /// It was as last installed and is now the new version.
    Updated,
    /// It already was the bundled version.
    Current,
    /// The user changed it: it is kept, the new version is beside it in
    /// `SKILL.md.new`.
    Kept,
}

/// Install the bundled skills in `dir` (e.g. `~/.kluis/skills`), returning
/// what was done with each one, or why it could not be installed.
pub fn install_skills(dir: &Path) -> io::Result<Vec<(&'static str, io::Result<Installed>)>> {
    refuse_symlink(dir)?;
    create_dir(dir)?;
    let mut manifest = read_manifest(dir).unwrap_or_default();
    let mut done = Vec::with_capacity(BUNDLED_SKILLS.len());
    for &(name, text) in BUNDLED_SKILLS {
        let res = install(&dir.join(name), text, manifest.get(name).copied());
        if res.is_ok() {
            manifest.insert(name.to_owned(), hash(text.as_bytes()));
        }
        done.push((name, res));
    }
    write_manifest(dir, &manifest)?;
    Ok(done)
}

/// The bundled skills newer than those last installed in `dir`, none if
/// they never were.
pub fn outdated_skills(dir: &Path) -> Vec<&'static str> {
    let Some(manifest) = read_manifest(dir) else {
        return Vec::new();
    };
    BUNDLED_SKILLS
        .iter()
        .filter(|(name, text)| manifest.get(*name) != Some(&hash(text.as_bytes())))
        .map(|&(name, _)| name)
        .collect()
}

/// Install `text` as the `SKILL.md` of the skill folder `dir`, last
/// installed with the hash `installed`.
fn install(dir: &Path, text: &str, installed: Option<u64>) -> io::Result<Installed> {
    refuse_symlink(dir)?;
    let path = dir.join(SKILL_FILE);
    let current = match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(e),
        Ok(_) => {
            let (bytes, _) = read_capped(&path, MAX_SKILL_BYTES)
                .ok_or_else(|| io::Error::other(format!("cannot read {}", path.display())))?;
            Some(hash(&bytes))
        }
    };
    let write = |path: &Path| replace(path, |file| file.write_all(text.as_bytes()));
    match current {
        None => {
            create_dir(dir)?;
            write(&path)?;
            Ok(Installed::Added)
        }
        Some(h) if h == hash(text.as_bytes()) => Ok(Installed::Current),
        Some(h) if Some(h) == installed => {
            write(&path)?;
            Ok(Installed::Updated)
        }
        Some(_) => {
            write(&sibling(&path, ".new"))?;
            Ok(Installed::Kept)
        }
    }
}

/// Fail when `path` is a symlink, which could lead anywhere.
fn refuse_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_symlink() => {
            Err(io::Error::other(format!("{} is a symlink", path.display())))
        }
        _ => Ok(()),
    }
}

fn hash(bytes: &[u8]) -> u64 {
    let mut hasher = FnvHasher::default();
    hasher.write(bytes);
    hasher.finish()
}

/// The manifest of `dir`, if any: one `<hash> <name>` line for each skill.
fn read_manifest(dir: &Path) -> Option<BTreeMap<String, u64>> {
    let (bytes, _) = read_capped(&dir.join(MANIFEST), MAX_MANIFEST_BYTES)?;
    let manifest = String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| {
            let (hash, name) = line.split_once(' ')?;
            Some((name.to_owned(), u64::from_str_radix(hash, 16).ok()?))
        })
        .collect();
    Some(manifest)
}

fn write_manifest(dir: &Path, manifest: &BTreeMap<String, u64>) -> io::Result<()> {
    let text: String = manifest
        .iter()
        .map(|(name, hash)| format!("{hash:016x} {name}\n"))
        .collect();
    replace(&dir.join(MANIFEST), |file| file.write_all(text.as_bytes()))
}
