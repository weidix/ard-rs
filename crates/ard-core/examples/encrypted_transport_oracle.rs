#![forbid(unsafe_code)]

//! Thin executable wrapper around the fixture-backed oracle in `src`.
//!
//! Usage:
//!
//! ```sh
//! cargo run -p ard-core --release --example encrypted_transport_oracle \
//!   -- 5999 0.0.0.0 192.168.65.1
//! ```

use std::env;
use std::io;
use std::net::{IpAddr, TcpListener};

use ard_rs::Oracle;

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
    let oracle = Oracle {
        allowed_peer,
        ..Oracle::default()
    };
    println!("listening on vnc://{host}:{port}");
    let listener = TcpListener::bind(format!("{host}:{port}"))?;
    loop {
        let (stream, peer) = listener.accept()?;
        if oracle
            .allowed_peer
            .is_some_and(|allowed| allowed != peer.ip())
        {
            println!("rejected peer {peer}");
            continue;
        }
        println!("client connected from {peer}");
        match oracle.run(stream, peer) {
            Ok(report) => println!("{report:#?}"),
            Err(error) => eprintln!("client {peer} disconnected: {error}"),
        }
    }
}
