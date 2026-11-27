//! Runs the real `fonsviva org` commands on scratch dirs. Nothing here
//! touches the user's state.

#![allow(dead_code)]

use std::path::Path;
use std::process::{Command, Output, Stdio};

pub fn fonsviva(scratch: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_fonsviva"));
    c.env("HOME", scratch.join("home"))
        .env("USERPROFILE", scratch.join("home"))
        .env("FONSVIVA_HOME", scratch.join("fv"))
        .env("FONSVIVA_PORT", "0")
        .env("XDG_CACHE_HOME", scratch.join("cache"))
        .env("XDG_CONFIG_HOME", scratch.join("config"))
        .env("XDG_DATA_HOME", scratch.join("share"))
        .env("LOCALAPPDATA", scratch.join("local"))
        .env("APPDATA", scratch.join("roaming"))
        .stdin(Stdio::null());
    c
}

pub fn init(scratch: &Path, data: &Path, url: &str) -> Output {
    fonsviva(scratch)
        .args(["org", "init", "--url", url, "--data"])
        .arg(data)
        .output()
        .unwrap()
}

/// The token at the end of a join link.
pub fn token_of(link: &str) -> String {
    link.trim().rsplit('/').next().unwrap().to_string()
}
