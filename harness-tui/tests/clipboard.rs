// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Pasting the image of the clipboard, with a `wl-paste` of the test.
//! Alone in its binary: it changes the environment.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use harness_core::tools::MAX_IMAGE_BYTES;
use harness_tui::paste_image;

/// The first bytes of a PNG file.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

/// A `wl-paste` in `dir` offering the `types` and, for an image type, the
/// file `image`.
fn fake_wl_paste(dir: &Path, types: &str) {
    let script = format!(
        "#!/bin/sh\n\
         if [ \"$1\" = --list-types ]; then printf '{types}'; exit 0; fi\n\
         [ \"$1\" = --no-newline ] && [ \"$2\" = --type ] || exit 1\n\
         case \"$3\" in image/*) cat '{}/image' ;; *) exit 1 ;; esac\n",
        dir.display()
    );
    let path = dir.join("wl-paste");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn paste_image_reads_the_image_of_the_clipboard() {
    let dir = tempfile::tempdir().unwrap();
    // A Wayland display the test can reach.
    let socket = dir.path().join("wayland-0");
    std::fs::write(&socket, "").unwrap();
    std::env::set_var("WAYLAND_DISPLAY", &socket);
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{path}", dir.path().display()));
    let image = dir.path().join("image");

    fake_wl_paste(dir.path(), "text/plain\\nimage/png\\n");
    std::fs::write(&image, PNG).unwrap();
    let pasted = paste_image().unwrap();
    assert_eq!(pasted.mime, "image/png");
    assert_eq!(pasted.data, "iVBORw0KGgoAAAANSUhEUg==");

    // What says it is an image is checked to be one.
    std::fs::write(&image, "not an image").unwrap();
    let err = paste_image().unwrap_err();
    assert!(err.contains("not a PNG"), "{err}");

    // One too large is read to its end, and refused.
    let mut big = PNG.to_vec();
    big.resize(MAX_IMAGE_BYTES as usize * 2, 0);
    std::fs::write(&image, big).unwrap();
    let err = paste_image().unwrap_err();
    assert!(err.contains("image larger than"), "{err}");

    // Text only.
    fake_wl_paste(dir.path(), "text/plain\\nUTF8_STRING\\n");
    assert_eq!(paste_image().unwrap_err(), "no image in the clipboard");
}
