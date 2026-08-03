#![forbid(unsafe_code)]

use std::env;
use std::fs;
use std::process::ExitCode;

use ard_rs::{ProtocolVersion, parse_security_types};

fn main() -> ExitCode {
    let Some(path) = env::args_os().nth(1) else {
        eprintln!("usage: ard-rs <server-handshake.bin>");
        return ExitCode::from(2);
    };
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    match inspect(&bytes) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn inspect(bytes: &[u8]) -> ard_rs::Result<()> {
    let version = ProtocolVersion::parse(bytes)?;
    println!("protocol: {}.{:03}", version.major, version.minor);
    if bytes.len() > 12 {
        let (types, _) = parse_security_types(&bytes[12..], 64)?;
        println!("security types: {types:?}");
    }
    Ok(())
}
