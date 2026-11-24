// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Custom slash commands: markdown files whose text is a prompt template.
//! `/<name> [args]` sends the text of `<name>.md` with `$ARGUMENTS`
//! replaced by the arguments, or with them appended when it has none.

use std::path::Path;

use crate::front_matter::{clean_description, split_front_matter, valid_name, NAME_RULE};
use crate::prompt::read_capped;

/// What `$ARGUMENTS` in a template is replaced with.
pub const ARGUMENTS: &str = "$ARGUMENTS";

/// Most command files read.
pub const MAX_COMMANDS: usize = 100;

/// Largest command file read.
pub const MAX_COMMAND_BYTES: usize = 32 * 1024;

/// A prompt template `/<name>` sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub template: String,
}

impl Command {
    /// The prompt the command sends with `args`.
    pub fn expand(&self, args: &str) -> String {
        let args = args.trim();
        if self.template.contains(ARGUMENTS) {
            self.template.replace(ARGUMENTS, args)
        } else if args.is_empty() {
            self.template.clone()
        } else {
            format!("{}\n\n{args}", self.template)
        }
    }
}

/// Commands found, sorted by name, and warnings about the files that
/// could not be used.
#[derive(Debug, Default)]
pub struct Commands {
    pub commands: Vec<Command>,
    pub warnings: Vec<String>,
}

/// The prompt `text` sends when it runs one of `commands`, as in
/// `/<name> [args]`.
pub fn expand_command(commands: &[Command], text: &str) -> Option<String> {
    let text = text.trim_start().strip_prefix('/')?;
    let (name, args) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let command = commands.iter().find(|c| c.name == name)?;
    Some(command.expand(args))
}

/// Read the commands in the `<name>.md` files of `dir` (e.g.
/// `~/.kluis/commands`).
///
/// Only commands the user put there are trusted: neither `dir` nor a
/// command file may be a symlink, which could lead elsewhere.
pub fn find_commands(dir: &Path) -> Commands {
    let mut found = Commands::default();
    let real = std::fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir());
    let Some(entries) = std::fs::read_dir(dir).ok().filter(|_| real) else {
        return found;
    };
    // Bounded, as anything could create any number of files there.
    let mut files: Vec<_> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .take(MAX_COMMANDS + 1)
        .collect();
    if files.len() > MAX_COMMANDS {
        files.pop();
        found.warnings.push(format!(
            "{}: only {MAX_COMMANDS} command files are read",
            dir.display()
        ));
    }
    files.sort();
    for path in files {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let command = match read_capped(&path, MAX_COMMAND_BYTES) {
            None => continue,
            Some((_, true)) => Err("larger than 32 KiB"),
            Some(_) if !valid_name(name) => Err(NAME_RULE),
            Some((bytes, false)) => parse_command(name, &String::from_utf8_lossy(&bytes)),
        };
        match command {
            Ok(command) => found.commands.push(command),
            Err(err) => found
                .warnings
                .push(format!("command {}: {err}", path.display())),
        }
    }
    found
}

/// The command `name` whose file holds `text`: an optional front matter
/// describing it, then its template.
fn parse_command(name: &str, text: &str) -> Result<Command, &'static str> {
    let (description, body) = match split_front_matter(text)? {
        Some((front, body)) => (front.description, body),
        None => (None, text),
    };
    // Without control characters, which could drive the terminal showing
    // the prompt.
    let template: String = body
        .trim()
        .chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect();
    if template.is_empty() {
        return Err("no prompt");
    }
    // Else the first line of the prompt describes it.
    let description = description.unwrap_or_else(|| {
        let first = template.lines().next().unwrap_or_default();
        first.trim_start_matches('#').to_owned()
    });
    Ok(Command {
        name: name.to_owned(),
        description: clean_description(&description),
        template,
    })
}
