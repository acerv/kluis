// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::path::Path;
use std::time::Duration;

use common::p;
use harness_exec::{ReadTool, DEFAULT_READ_LIMIT, MAX_IMAGE_BYTES, MAX_READ_BYTES};

fn call(path: &Path, offset: Option<usize>, limit: Option<usize>) -> ReadTool {
    ReadTool {
        path: p(path),
        offset,
        limit,
    }
}

#[test]
fn definition() {
    let tool = ReadTool::definition();
    assert_eq!(tool.name.as_ref(), "read");
    assert_eq!(tool.schema, Some(ReadTool::schema()));
}

#[tokio::test]
async fn whole_file() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, "one\ntwo\nthree\n").unwrap();

    let out = call(&f, None, None).run().await.unwrap();
    assert_eq!(out.content, "one\ntwo\nthree\n");
    assert_eq!(out.total_lines, 3);
    assert!(!out.truncated);
}

#[tokio::test]
async fn window() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, "1\n2\n3\n4\n5\n").unwrap();

    let out = call(&f, Some(2), Some(2)).run().await.unwrap();
    assert_eq!(out.content, "2\n3\n");
    assert_eq!(out.total_lines, 5);
    assert!(out.truncated);
    assert_eq!(out.next_offset, Some(4));

    let out = call(&f, Some(4), Some(10)).run().await.unwrap();
    assert_eq!(out.content, "4\n5\n");
    assert!(!out.truncated);
    assert_eq!(out.next_offset, None);

    // Window ends exactly at the last line.
    let out = call(&f, Some(4), Some(2)).run().await.unwrap();
    assert_eq!(out.content, "4\n5\n");
    assert!(!out.truncated);

    let out = call(&f, None, Some(5)).run().await.unwrap();
    assert_eq!(out.total_lines, 5);
    assert!(!out.truncated);
}

#[tokio::test]
async fn byte_limit_ends_at_a_line() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    let line_len = 64usize;
    let per_page = MAX_READ_BYTES / line_len;
    let extra = 100usize;
    let line = format!("{}\n", "x".repeat(line_len - 1));
    std::fs::write(&f, line.repeat(per_page + extra)).unwrap();

    let out = call(&f, None, None).run().await.unwrap();
    assert_eq!(out.content.len(), MAX_READ_BYTES);
    assert_eq!(out.content.lines().count(), per_page);
    assert!(out.truncated);
    assert_eq!(out.next_offset, Some(per_page + 1));

    // Continuing from next_offset picks up the following lines.
    let out = call(&f, Some(per_page + 1), None).run().await.unwrap();
    assert_eq!(out.content.lines().count(), extra);
    assert_eq!(out.next_offset, None);
}

#[tokio::test]
async fn line_over_byte_limit() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    let long = "x".repeat(MAX_READ_BYTES + 1);
    std::fs::write(&f, format!("short\n{long}\nend\n")).unwrap();

    // The window stops before the long line.
    let out = call(&f, None, None).run().await.unwrap();
    assert_eq!(out.content, "short\n");
    assert_eq!(out.next_offset, Some(2));

    // Starting at it points to bash.
    let err = call(&f, Some(2), None).run().await.unwrap_err();
    assert!(err.message.contains("line 2"), "{err}");
    assert!(err.message.contains("sed -n '2p'"), "{err}");
}

#[tokio::test]
async fn trailing_blank_lines() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, "a\n\n\n").unwrap();

    let out = call(&f, None, None).run().await.unwrap();
    assert_eq!(out.content, "a\n\n\n");
    assert_eq!(out.total_lines, 3);

    let out = call(&f, Some(3), None).run().await.unwrap();
    assert_eq!(out.content, "\n");
    assert!(!out.truncated);
}

// The only test in this binary that changes $HOME.
#[tokio::test]
async fn tilde_expands_to_home() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("t.txt"), "home\n").unwrap();
    std::env::set_var("HOME", home.path());

    let out = call(Path::new("~/t.txt"), None, None).run().await.unwrap();
    assert_eq!(out.content, "home\n");

    for dir in ["~", "~/"] {
        let err = call(Path::new(dir), None, None).run().await.unwrap_err();
        assert!(err.message.contains("directory"), "{dir}: {err}");
    }

    // `~user` and a `~` past the start are not expanded.
    for literal in ["~nobody/t.txt", "a/~/b"] {
        let err = call(Path::new(literal), None, None)
            .run()
            .await
            .unwrap_err();
        assert!(err.message.contains(literal), "{literal}: {err}");
    }

    // Without $HOME (or when empty), `~/...` is left untouched.
    let cwd = tempfile::tempdir().unwrap();
    std::fs::create_dir(cwd.path().join("~")).unwrap();
    std::fs::write(cwd.path().join("~/t.txt"), "local\n").unwrap();

    std::env::set_var("HOME", "");
    let out = call(Path::new("~/t.txt"), None, None)
        .run_in(cwd.path())
        .await
        .unwrap();
    assert_eq!(out.content, "local\n");

    std::env::remove_var("HOME");
    let out = call(Path::new("~/t.txt"), None, None)
        .run_in(cwd.path())
        .await
        .unwrap();
    assert_eq!(out.content, "local\n");
}

#[tokio::test]
async fn default_limit() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("big.txt");
    let text: String = (1..=2500).map(|i| format!("{i}\n")).collect();
    std::fs::write(&f, text).unwrap();

    let out = call(&f, None, None).run().await.unwrap();
    assert_eq!(out.content.lines().count(), DEFAULT_READ_LIMIT);
    assert_eq!(out.total_lines, 2500);
    assert!(out.truncated);
}

#[tokio::test]
async fn last_line_and_huge_limit() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, "1\n2\n3\n").unwrap();

    let out = call(&f, Some(3), None).run().await.unwrap();
    assert_eq!(out.content, "3\n");
    assert!(!out.truncated);

    let out = call(&f, Some(2), Some(usize::MAX)).run().await.unwrap();
    assert_eq!(out.content, "2\n3\n");
    assert!(!out.truncated);
}

#[tokio::test]
async fn zero_limit_fails() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, "1\n").unwrap();

    let err = call(&f, None, Some(0)).run().await.unwrap_err();
    assert!(err.message.contains("limit"));
}

#[tokio::test]
async fn exact_text() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");

    for (text, lines) in [
        ("a\r\nb\r\n", 2),
        ("a\nb", 2),
        ("a\r\nb\nc", 3),
        ("a\rb\n", 1),
        ("\n", 1),
        ("\n\n", 2),
    ] {
        std::fs::write(&f, text).unwrap();
        let out = call(&f, None, None).run().await.unwrap();
        assert_eq!(out.content, text, "{text:?}");
        assert_eq!(out.total_lines, lines, "{text:?}");
    }

    // A window keeps the endings of its own lines.
    std::fs::write(&f, "a\r\nb\nc").unwrap();
    let out = call(&f, Some(2), None).run().await.unwrap();
    assert_eq!(out.content, "b\nc");
    let out = call(&f, Some(1), Some(1)).run().await.unwrap();
    assert_eq!(out.content, "a\r\n");
}

#[tokio::test]
async fn bad_offsets() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, "1\n2\n").unwrap();

    assert!(call(&f, Some(0), None).run().await.is_err());
    assert!(call(&f, Some(3), None).run().await.is_err());
}

#[tokio::test]
async fn missing_and_directory() {
    let dir = tempfile::tempdir().unwrap();

    let err = call(&dir.path().join("nope"), None, None)
        .run()
        .await
        .unwrap_err();
    assert!(err.message.contains("nope"));

    let err = call(dir.path(), None, None).run().await.unwrap_err();
    assert!(err.message.contains("directory"));

    assert!(call(Path::new(""), None, None).run().await.is_err());
}

#[tokio::test]
async fn special_files_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("fifo");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(status.success());

    for path in [
        fifo.as_path(),
        Path::new("/dev/zero"),
        Path::new("/dev/null"),
    ] {
        let r = tokio::time::timeout(Duration::from_secs(5), call(path, None, None).run())
            .await
            .unwrap_or_else(|_| panic!("{} hung", path.display()));
        let err = r.unwrap_err();
        assert!(err.message.contains("not a regular file"), "{}", err);
    }
}

#[tokio::test]
async fn binary_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("bin");

    // NUL at the start, and far into the file.
    let mut late = vec![b'a'; 100_000];
    late.push(0);
    for bytes in [vec![0x7f, b'E', b'L', b'F', 0x02, 0x01, 0x01, 0x00], late] {
        std::fs::write(&f, &bytes).unwrap();
        let err = call(&f, None, None).run().await.unwrap_err();
        assert!(err.message.contains("binary file"), "{err}");
        assert!(err.message.contains("bin"));
    }
}

#[tokio::test]
async fn invalid_utf8() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("bin");
    std::fs::write(&f, [0x66, 0xff, 0xfe, 0x0a, 0x67]).unwrap();

    let out = call(&f, None, None).run().await.unwrap();
    assert_eq!(out.total_lines, 2);
    assert!(out.content.contains('\u{FFFD}'));
}

#[tokio::test]
async fn empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("empty");
    std::fs::write(&f, "").unwrap();

    let out = call(&f, None, None).run().await.unwrap();
    assert_eq!(out.content, "");
    assert_eq!(out.total_lines, 0);
    assert!(!out.truncated);

    // Offset is not validated against an empty file.
    let out = call(&f, Some(5), None).run().await.unwrap();
    assert_eq!(out.content, "");
    assert_eq!(out.total_lines, 0);
}

#[tokio::test]
async fn relative_path_starts_at_cwd() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "inside\n").unwrap();

    let out = call(Path::new("a.txt"), None, None)
        .run_in(dir.path())
        .await
        .unwrap();
    assert_eq!(out.content, "inside\n");
}

#[tokio::test]
async fn absolute_path_ignores_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, "here\n").unwrap();

    let out = call(&f, None, None).run_in(other.path()).await.unwrap();
    assert_eq!(out.content, "here\n");
}

#[tokio::test]
async fn multibyte_utf8_across_chunk_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("utf8.txt");

    // Put a 3-byte UTF-8 character (`€`) right across the 64 KiB boundary.
    let mut data = vec![b'a'; 64 * 1024 - 2];
    data.push(b'\n');
    data.extend_from_slice("€tail".as_bytes());
    std::fs::write(&f, &data).unwrap();

    let out = call(&f, Some(2), None).run().await.unwrap();
    assert_eq!(out.content, "€tail");
    assert_eq!(out.total_lines, 2);
    assert!(!out.truncated);
}

#[tokio::test]
async fn invalid_utf8_expansion_hits_byte_limit() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("bad.txt");

    // 20 KiB of invalid UTF-8 bytes fits raw, but expands to 60 KiB of U+FFFD.
    let mut data = b"ok\n".to_vec();
    data.extend(std::iter::repeat_n(0xff, 20 * 1024));
    data.push(b'\n');
    std::fs::write(&f, &data).unwrap();

    let out = call(&f, None, None).run().await.unwrap();
    assert_eq!(out.content, "ok\n");
    assert_eq!(out.total_lines, 2);
    assert!(out.truncated);
    assert_eq!(out.next_offset, Some(2));

    let err = call(&f, Some(2), None).run().await.unwrap_err();
    assert!(err.message.contains("line 2"), "{err}");
    assert!(err.message.contains("larger than"), "{err}");
}

#[tokio::test]
async fn streams_large_file_across_chunks() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("large.txt");

    // ~2 MiB across many 64 KiB chunks, with no trailing newline.
    let total = 20_000usize;
    let mut file = std::fs::File::create(&f).unwrap();
    let pad = "x".repeat(90);
    for i in 1..=total {
        if i < total {
            writeln!(file, "{i:06}:{pad}").unwrap();
        } else {
            write!(file, "{i:06}:{pad}").unwrap();
        }
    }
    drop(file);

    let out = call(&f, Some(15_000), Some(3)).run().await.unwrap();
    assert_eq!(
        out.content,
        format!("015000:{pad}\n015001:{pad}\n015002:{pad}\n")
    );
    assert_eq!(out.total_lines, total);
    assert!(out.truncated);
    assert_eq!(out.next_offset, Some(15_003));

    let out = call(&f, Some(total), Some(5)).run().await.unwrap();
    assert_eq!(out.content, format!("{total:06}:{pad}"));
    assert_eq!(out.total_lines, total);
    assert!(!out.truncated);
    assert_eq!(out.next_offset, None);

    // A single huge line without newlines is rejected without buffering it all.
    std::fs::write(&f, vec![b'z'; 256 * 1024]).unwrap();
    let err = call(&f, None, None).run().await.unwrap_err();
    assert!(err.message.contains("line 1"), "{err}");
    assert!(err.message.contains("larger than"), "{err}");
}

/// The first bytes of a PNG file, then some that are not text.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

#[tokio::test]
async fn image_comes_whole_and_encoded() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("shot.png");
    std::fs::write(&f, PNG).unwrap();

    let out = call(&f, Some(2), Some(1)).run().await.unwrap();

    let image = out.image.expect("an image");
    assert_eq!(image.mime, "image/png");
    assert_eq!(image.data, "iVBORw0KGgoAAAANSUhEUg==");
    assert!(out.content.contains("attached"), "{}", out.content);
}

#[tokio::test]
async fn image_too_large_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("big.png");
    let mut data = PNG.to_vec();
    data.resize(MAX_IMAGE_BYTES as usize + 1, 0);
    std::fs::write(&f, data).unwrap();

    let err = call(&f, None, None).run().await.unwrap_err();
    assert!(err.to_string().contains("image larger than"), "{err}");
}
