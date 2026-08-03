#![forbid(unsafe_code)]

use std::{
    env,
    fs::File,
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::PathBuf,
    thread,
};

fn relay(mut reader: TcpStream, mut writer: TcpStream, mut capture: File) -> io::Result<u64> {
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof
                ) =>
            {
                return Ok(total);
            }
            Err(error) => return Err(error),
        };
        if count == 0 {
            if let Err(error) = writer.shutdown(Shutdown::Write)
                && error.kind() != io::ErrorKind::NotConnected
            {
                return Err(error);
            }
            return Ok(total);
        }
        capture.write_all(&buffer[..count])?;
        capture.flush()?;
        if let Err(error) = writer.write_all(&buffer[..count]) {
            if matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::NotConnected
            ) {
                return Ok(total);
            }
            return Err(error);
        }
        total += count as u64;
    }
}

fn forward_handshake(
    client: &mut TcpStream,
    server: &mut TcpStream,
    client_capture: &mut File,
    server_capture: &mut File,
) -> io::Result<()> {
    let mut banner = [0_u8; 12];
    server.read_exact(&mut banner)?;
    server_capture.write_all(&banner)?;
    client.write_all(&banner)?;

    client.read_exact(&mut banner)?;
    client_capture.write_all(&banner)?;
    server.write_all(&banner)?;

    let mut count = [0_u8; 1];
    server.read_exact(&mut count)?;
    let mut types = vec![0_u8; usize::from(count[0])];
    server.read_exact(&mut types)?;
    let filtered: Vec<u8> = types
        .into_iter()
        .filter(|security_type| *security_type == 30)
        .collect();
    if filtered.is_empty() {
        return Err(io::Error::other(
            "server did not offer Apple security type 30",
        ));
    }
    let filtered_count = u8::try_from(filtered.len())
        .map_err(|_| io::Error::other("too many RFB security types"))?;
    server_capture.write_all(&[filtered_count])?;
    server_capture.write_all(&filtered)?;
    client.write_all(&[filtered_count])?;
    client.write_all(&filtered)?;
    Ok(())
}

fn main() -> io::Result<()> {
    let listen = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:5901".to_owned());
    let target = env::args()
        .nth(2)
        .unwrap_or_else(|| "127.0.0.1:5900".to_owned());
    let directory = env::args_os()
        .nth(3)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/ard-rs-live-capture"));
    std::fs::create_dir_all(&directory)?;

    let listener = TcpListener::bind(&listen)?;
    println!("ARD capture proxy listening on {listen}, forwarding to {target}");
    for connection in 1_u32.. {
        let (client, peer) = listener.accept()?;
        println!("connection {connection}: Screen Sharing connected from {peer}");
        let mut server = TcpStream::connect(&target)?;
        let mut client = client;
        client.set_nodelay(true)?;
        server.set_nodelay(true)?;

        let mut client_to_server =
            File::create(directory.join(format!("connection-{connection:03}-client.bin")))?;
        let mut server_to_client =
            File::create(directory.join(format!("connection-{connection:03}-server.bin")))?;
        forward_handshake(
            &mut client,
            &mut server,
            &mut client_to_server,
            &mut server_to_client,
        )?;

        let client_reader = client.try_clone()?;
        let server_writer = server.try_clone()?;
        let upstream = thread::spawn(move || relay(client_reader, server_writer, client_to_server));

        let downstream = relay(server, client, server_to_client)?;
        let upstream = upstream
            .join()
            .map_err(|_| io::Error::other("client relay thread panicked"))??;

        println!(
            "connection {connection}: client→server={upstream} bytes, \
             server→client={downstream} bytes"
        );
    }
    unreachable!("unbounded connection counter")
}
