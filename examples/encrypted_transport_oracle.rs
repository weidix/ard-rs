#![forbid(unsafe_code)]

//! One-shot ARD server that exercises the complete modern encrypted
//! transport: type-30 authentication, extended ServerInit, `0x21`/`0x12`
//! parsing, the 1103 control rectangle, activation, and AES-CBC records
//! carrying MVS frames.
//!
//! Usage:
//!
//! ```sh
//! cargo run --release --example encrypted_transport_oracle \
//!   -- 5999 0.0.0.0 192.168.65.1
//! ```
//!
//! Connect Screen Sharing to `vnc://127.0.0.1:5999`. If the server reports a
//! rejected peer, restart it with the exact peer IP from the message.

use std::env;
use std::io;
use std::net::{IpAddr, TcpListener};

use ard_rs::EncryptedTransportOracle;

fn main() -> std::io::Result<()> {
    let port = env::args().nth(1).unwrap_or_else(|| "5999".to_owned());
    let host = env::args().nth(2).unwrap_or_else(|| "127.0.0.1".to_owned());
    let allowed_peer: Option<IpAddr> = env::args()
        .nth(3)
        .map(|value| {
            value
                .parse()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
        })
        .transpose()?;
    let oracle = EncryptedTransportOracle {
        allowed_peer,
        ..EncryptedTransportOracle::default()
    };
    println!("listening on vnc://{host}:{port}");
    let listener = TcpListener::bind(format!("{host}:{port}"))?;
    let (stream, peer) = loop {
        let (candidate, peer) = listener.accept()?;
        if oracle
            .allowed_peer
            .is_none_or(|allowed| allowed == peer.ip())
        {
            break (candidate, peer);
        }
        println!("rejected non-local peer {peer}");
    };
    println!("client connected from {peer}");
    let report = oracle.run(stream, peer)?;
    println!("{report:#?}");
    Ok(())
}
