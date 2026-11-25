// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The sandbox told in words matches what it is asked to do.

use harness_sandbox::{format_size, Sandbox};

/// The lines of `text` below the line `title`, up to the next blank one.
fn section<'a>(text: &'a str, title: &str) -> Vec<&'a str> {
    text.lines()
        .skip_while(|line| *line != title)
        .skip(1)
        .take_while(|line| !line.is_empty())
        .map(str::trim)
        .collect()
}

#[test]
fn tells_what_commands_can_change_read_and_not_see() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path().canonicalize().unwrap();
    let (project, docs, secret) = (dir.join("project"), dir.join("docs"), dir.join("secret"));
    for path in [&project, &docs, &secret] {
        std::fs::create_dir(path).unwrap();
    }
    std::fs::create_dir(project.join(".git")).unwrap();
    std::fs::write(project.join(".env"), "KEY=1").unwrap();

    let text = Sandbox::new()
        .allow_write(&project)
        .allow_read(&docs)
        .deny_read(&secret)
        .deny_read(project.join(".env"))
        .private_dir("/home/nobody")
        .max_tmp_size(512 << 20)
        .max_processes(64)
        .describe()
        .unwrap();

    let p = |path: &std::path::Path| path.to_str().unwrap().to_owned();
    assert_eq!(section(&text, "Commands can change:"), [p(&project)]);
    // The repository of a writable folder is kept read-only.
    assert_eq!(
        section(&text, "Commands can only read:"),
        [p(&docs), p(&project.join(".git"))]
    );
    // Only what a shown folder would show needs hiding.
    assert_eq!(
        section(&text, "Hidden from commands:"),
        [p(&project.join(".env"))]
    );
    assert_eq!(
        section(&text, "Empty, kept in memory:"),
        ["/tmp (up to 512.0 MiB)", "/home/nobody (up to 512.0 MiB)"]
    );
    assert!(
        text.contains("\nNetwork: none (no TCP connection)\n"),
        "{text}"
    );
    assert!(
        text.ends_with("Processes and threads: at most 64\n"),
        "{text}"
    );
}

#[test]
fn tells_the_network_commands_get() {
    let network = |sandbox: Sandbox| {
        let text = sandbox.describe().unwrap();
        let line = text.lines().find(|l| l.starts_with("Network: ")).unwrap();
        line.to_owned()
    };
    assert_eq!(network(Sandbox::new().no_network()), "Network: none");
    assert_eq!(
        network(Sandbox::new().allow_connect(5432).allow_connect(80)),
        "Network: TCP ports 5432, 80 on any host"
    );
    assert_eq!(network(Sandbox::new().allow_any_network()), "Network: any");
}

#[test]
fn fails_as_the_container_does() {
    let sandbox = Sandbox::new().allow_read("/no/such/folder");
    assert!(sandbox.container().is_err());
    assert!(sandbox.describe().is_err());
}

#[test]
fn sizes_use_the_largest_unit() {
    assert_eq!(format_size(512), "512 bytes");
    assert_eq!(format_size(1536), "1.5 KiB");
    assert_eq!(format_size(4 << 30), "4.0 GiB");
    assert_eq!(format_size(2048 << 30), "2048.0 GiB");
}
