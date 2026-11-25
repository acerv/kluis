// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::time::{Duration, Instant};

use common::{inside, output, spawn_inside, system};
use harness_sandbox::listen_in;

/// Any port: nothing else listens in a new network namespace.
const PORT: u16 = 3128;

/// Connect to `PORT` once it listens, and send back what comes.
fn echo_from_listener() -> io::Result<String> {
    let start = Instant::now();
    let mut stream = loop {
        match TcpStream::connect((Ipv4Addr::LOCALHOST, PORT)) {
            Ok(stream) => break stream,
            Err(_) if start.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(err),
        }
    };
    let mut text = String::new();
    stream.read_to_string(&mut text)?;
    Ok(text)
}

#[test]
fn programs_without_network_reach_the_listener() {
    let child = spawn_inside(&system().no_network(), echo_from_listener);
    let listener = listen_in(child.id(), PORT).unwrap();
    let (mut stream, _) = listener.accept().unwrap();
    stream.write_all(b"from the host").unwrap();
    drop(stream);
    assert_eq!(output(child).unwrap(), "from the host");
}

#[test]
fn the_listener_is_only_inside() {
    let child = spawn_inside(&system().no_network(), echo_from_listener);
    let listener = listen_in(child.id(), PORT).unwrap();
    let other = inside(&system().no_network(), move || {
        TcpStream::connect((Ipv4Addr::LOCALHOST, PORT)).map(|_| String::new())
    });
    assert!(other.is_err());
    drop(listener.accept().unwrap());
    assert_eq!(output(child).unwrap(), "");
}

#[test]
fn refuses_namespaces_of_others() {
    // The init of the host, in namespaces this user did not create.
    assert!(listen_in(1, PORT).is_err());
}
