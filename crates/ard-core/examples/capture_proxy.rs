#![forbid(unsafe_code)]

use std::{
    env,
    fs::File,
    io::{self, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket},
    path::{Path, PathBuf},
    thread,
    time::{SystemTime, UNIX_EPOCH},
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
    // Relay the full security-type list unchanged: the native client may use
    // the offered types (e.g. Apple 33/35/36) when deciding whether the
    // connection supports high-performance (pro) mode.
    server_capture.write_all(&count)?;
    server_capture.write_all(&types)?;
    client.write_all(&count)?;
    client.write_all(&types)?;
    Ok(())
}

/// Relays one UDP port between the Screen Sharing client and the real server,
/// recording every packet with a 17-byte header: a direction byte (`0`
/// client-to-server, `1` server-to-client), a big-endian `u64` microsecond
/// timestamp, the source IPv4 address, the source port and the payload
/// length, then the payload itself.
///
/// The media ports are the same on both ends (the client binds the advertised
/// server ports locally), so a plain port-preserving forward is sufficient:
/// client packets go to `target` on the same port, and replies from `target`
/// go back to the client address observed on the forward path. Before any
/// client packet is seen, replies fall back to the TCP peer address, which is
/// where the server pushes its first RTP/RTCP packets.
fn udp_relay(
    port: u16,
    target: SocketAddr,
    fallback_client_ip: std::net::IpAddr,
    mut capture: File,
    socket: UdpSocket,
) -> io::Result<()> {
    eprintln!("udp {port}: forwarding to {target}");
    let mut buffer = [0_u8; 65_536];
    let mut client_addr: Option<SocketAddr> = None;
    let mut first_client = true;
    let mut first_server = true;
    loop {
        let (count, source) = socket.recv_from(&mut buffer)?;
        let from_server = source.ip() == target.ip();
        if from_server && first_server {
            first_server = false;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros();
            eprintln!(
                "udp {port}: first server packet {}:{} len={count} at epoch-us {now}",
                source.ip(),
                source.port()
            );
        } else if !from_server && first_client {
            first_client = false;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros();
            eprintln!(
                "udp {port}: first client packet {}:{} len={count} at epoch-us {now}",
                source.ip(),
                source.port()
            );
        }
        let mut header = [0_u8; 17];
        header[0] = u8::from(from_server);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        header[1..9].copy_from_slice(&timestamp.to_be_bytes());
        if let std::net::IpAddr::V4(ipv4) = source.ip() {
            header[9..13].copy_from_slice(&ipv4.octets());
        }
        header[13..15].copy_from_slice(&source.port().to_be_bytes());
        header[15..17].copy_from_slice(&(count as u16).to_be_bytes());
        capture.write_all(&header)?;
        capture.write_all(&buffer[..count])?;
        capture.flush()?;
        if !from_server {
            client_addr = Some(source);
            socket.send_to(&buffer[..count], target)?;
        } else if let Some(destination) = client_addr {
            socket.send_to(&buffer[..count], destination)?;
        } else {
            socket.send_to(&buffer[..count], SocketAddr::new(fallback_client_ip, port))?;
        }
    }
}

fn start_udp_relays(
    directory: &Path,
    connection: u32,
    target_ip: std::net::IpAddr,
    client_ip: std::net::IpAddr,
    base_port: u16,
    count: u16,
) {
    for offset in 0..count {
        let port = base_port.saturating_add(offset);
        let target = SocketAddr::new(target_ip, port);
        let capture =
            File::create(directory.join(format!("connection-{connection:03}-udp-{port}.bin")))
                .expect("create udp capture file");
        let socket = match UdpSocket::bind(("0.0.0.0", port)) {
            Ok(socket) => socket,
            Err(error) => {
                eprintln!("udp {port}: bind failed: {error}");
                return;
            }
        };
        let _ = thread::spawn(move || {
            if let Err(error) = udp_relay(port, target, client_ip, capture, socket) {
                eprintln!("connection {connection}: udp relay {port} ended: {error}");
            }
        });
    }
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
    let udp_base_port = env::args()
        .nth(4)
        .map(|value| value.parse::<u16>())
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        .unwrap_or(5900);
    let udp_port_count = env::args()
        .nth(5)
        .map(|value| value.parse::<u16>())
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        .unwrap_or(3);
    std::fs::create_dir_all(&directory)?;

    let listener = TcpListener::bind(&listen)?;
    println!("ARD capture proxy listening on {listen}, forwarding to {target}");
    let target_address: SocketAddr = target
        .parse()
        .map_err(|_| io::Error::other("target must be an IP:port address"))?;
    let target_ip = target_address.ip();
    for connection in 1_u32.. {
        let (client, peer) = listener.accept()?;
        println!("connection {connection}: Screen Sharing connected from {peer}");
        start_udp_relays(
            &directory,
            connection,
            target_ip,
            peer.ip(),
            udp_base_port,
            udp_port_count,
        );
        let mut server = loop {
            match TcpStream::connect(target_address) {
                Ok(stream) => break stream,
                Err(error) => {
                    println!("connection {connection}: target connect failed: {error}; retrying");
                    thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        };
        let mut client = client;
        client.set_nodelay(true)?;
        server.set_nodelay(true)?;

        let mut client_to_server =
            File::create(directory.join(format!("connection-{connection:03}-client.bin")))?;
        let mut server_to_client =
            File::create(directory.join(format!("connection-{connection:03}-server.bin")))?;
        if let Err(error) = forward_handshake(
            &mut client,
            &mut server,
            &mut client_to_server,
            &mut server_to_client,
        ) {
            println!("connection {connection}: handshake failed: {error}");
            continue;
        }

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
