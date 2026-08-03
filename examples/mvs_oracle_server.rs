#![forbid(unsafe_code)]

//! Minimal one-shot ARD server for comparing ard-rs output with Apple's
//! Screen Sharing decoder. It accepts any response to a local-only ARD
//! authentication exchange and sends one MVS partial-update rectangle.

use std::env;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};

const DH_GROUP2_PRIME: [u8; 128] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xc9, 0x0f, 0xda, 0xa2, 0x21, 0x68, 0xc2, 0x34,
    0xc4, 0xc6, 0x62, 0x8b, 0x80, 0xdc, 0x1c, 0xd1, 0x29, 0x02, 0x4e, 0x08, 0x8a, 0x67, 0xcc, 0x74,
    0x02, 0x0b, 0xbe, 0xa6, 0x3b, 0x13, 0x9b, 0x22, 0x51, 0x4a, 0x08, 0x79, 0x8e, 0x34, 0x04, 0xdd,
    0xef, 0x95, 0x19, 0xb3, 0xcd, 0x3a, 0x43, 0x1b, 0x30, 0x2b, 0x0a, 0x6d, 0xf2, 0x5f, 0x14, 0x37,
    0x4f, 0xe1, 0x35, 0x6d, 0x6d, 0x51, 0xc2, 0x45, 0xe4, 0x85, 0xb5, 0x76, 0x62, 0x5e, 0x7e, 0xc6,
    0xf4, 0x4c, 0x42, 0xe9, 0xa6, 0x37, 0xed, 0x6b, 0x0b, 0xff, 0x5c, 0xb6, 0xf4, 0x06, 0xb7, 0xed,
    0xee, 0x38, 0x6b, 0xfb, 0x5a, 0x89, 0x9f, 0xa5, 0xae, 0x9f, 0x24, 0x11, 0x7c, 0x4b, 0x1f, 0xe6,
    0x49, 0x28, 0x66, 0x51, 0xec, 0xe6, 0x53, 0x81, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
];

fn ard_authenticate(stream: &mut (impl Read + Write)) -> std::io::Result<()> {
    stream.write_all(&[0, 2])?; // two-byte big-endian generator
    stream.write_all(&(DH_GROUP2_PRIME.len() as u16).to_be_bytes())?;
    stream.write_all(&DH_GROUP2_PRIME)?;
    let mut public_key = [0_u8; 128];
    public_key[127] = 2; // g^1 mod p
    stream.write_all(&public_key)?;
    stream.flush()?;
    println!("sent ARD Diffie-Hellman parameters");

    let mut encrypted_credentials = [0_u8; 128];
    stream.read_exact(&mut encrypted_credentials)?;
    let mut client_public_key = [0_u8; 128];
    stream.read_exact(&mut client_public_key)?;
    println!("received ARD encrypted credentials and public key");
    Ok(())
}

fn vnc_authenticate(stream: &mut (impl Read + Write)) -> std::io::Result<()> {
    stream.write_all(&[0x5a; 16])?;
    stream.flush()?;
    println!("sent VNC challenge");
    let mut response = [0_u8; 16];
    stream.read_exact(&mut response)?;
    println!("received {}-byte Apple VNC response", response.len());
    Ok(())
}

fn wait_for_update_request(stream: &mut TcpStream) -> std::io::Result<()> {
    loop {
        let mut kind = [0_u8; 1];
        stream.read_exact(&mut kind)?;
        match kind[0] {
            0 => {
                let mut set_pixel_format = [0_u8; 19];
                stream.read_exact(&mut set_pixel_format)?;
            }
            2 => {
                let mut header = [0_u8; 3];
                stream.read_exact(&mut header)?;
                let count = usize::from(u16::from_be_bytes([header[1], header[2]]));
                let mut encodings = vec![0_u8; count.saturating_mul(4)];
                stream.read_exact(&mut encodings)?;
                let advertises_mvs = encodings
                    .chunks_exact(4)
                    .any(|bytes| i32::from_be_bytes(bytes.try_into().unwrap()) == 1011);
                println!("client advertised {count} encodings; MVS={advertises_mvs}");
            }
            3 => {
                let mut request = [0_u8; 9];
                stream.read_exact(&mut request)?;
                println!("received framebuffer update request");
                return Ok(());
            }
            4 => {
                let mut key_event = [0_u8; 7];
                stream.read_exact(&mut key_event)?;
            }
            5 => {
                let mut pointer_event = [0_u8; 5];
                stream.read_exact(&mut pointer_event)?;
            }
            6 => {
                let mut header = [0_u8; 7];
                stream.read_exact(&mut header)?;
                let length = u32::from_be_bytes(header[3..7].try_into().unwrap()) as usize;
                let mut text = vec![0_u8; length];
                stream.read_exact(&mut text)?;
            }
            10 => {
                let mut apple_session_options = [0_u8; 3];
                stream.read_exact(&mut apple_session_options)?;
                println!("received Apple session options {apple_session_options:02x?}");
            }
            other => {
                return Err(std::io::Error::other(format!(
                    "unsupported client message {other}"
                )));
            }
        }
    }
}

fn push_bits(output: &mut Vec<bool>, value: u32, width: u8) {
    for shift in (0..width).rev() {
        output.push(value & (1 << shift) != 0);
    }
}

fn pack_bits(bits: &[bool]) -> Vec<u8> {
    let mut output = vec![0_u8; bits.len().div_ceil(8)];
    for (position, value) in bits.iter().copied().enumerate() {
        if value {
            output[position / 8] |= 0x80 >> (position % 8);
        }
    }
    output
}

fn mvs_white(width: u16, height: u16) -> Vec<u8> {
    let tiles = usize::from(width).div_ceil(8) * usize::from(height).div_ceil(8);
    let repeat = tiles - 1;
    let mut bits = Vec::new();
    push_bits(&mut bits, 0, 1); // initial state
    push_bits(&mut bits, 0, 3); // white tile update
    if repeat == 0 {
        push_bits(&mut bits, 0, 1);
    } else if repeat <= 15 {
        push_bits(&mut bits, 1, 1);
        push_bits(&mut bits, (repeat - 1) as u32, 4);
    } else {
        push_bits(&mut bits, 1, 1);
        push_bits(&mut bits, 15, 4);
        let mut value = repeat - 16;
        for group_index in 0..3 {
            let has_more = value >= 0x80 && group_index != 2;
            let group = (value & 0x7f) | (usize::from(has_more) * 0x80);
            push_bits(&mut bits, group as u32, 8);
            value >>= 7;
            if !has_more {
                break;
            }
        }
    }
    push_bits(&mut bits, 0x6d, 8); // primary marker

    let primary = pack_bits(&bits);
    let secondary_offset = 6 + primary.len();
    let mut update = vec![
        0,
        0,
        0,
        ((secondary_offset >> 16) & 0xff) as u8,
        ((secondary_offset >> 8) & 0xff) as u8,
        (secondary_offset & 0xff) as u8,
    ];
    update.extend_from_slice(&primary);
    update.push(0x6d); // secondary marker

    let mut framed = (update.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&update);
    framed
}

fn mvs_solid_ycbcr(y: u8, cb: u8, cr: u8) -> Vec<u8> {
    let mut primary_bits = Vec::new();
    push_bits(&mut primary_bits, 0, 1); // initial state
    push_bits(&mut primary_bits, 4, 3); // solid/two-colour update
    push_bits(&mut primary_bits, 0, 1); // no repeat
    push_bits(&mut primary_bits, 0, 3); // white tile update
    push_bits(&mut primary_bits, 1, 1); // extended repeat
    push_bits(&mut primary_bits, 15, 4);
    push_bits(&mut primary_bits, 46, 8); // repeat count 62, covering 63 tiles
    push_bits(&mut primary_bits, 0x6d, 8); // primary marker
    let primary = pack_bits(&primary_bits);

    let mut secondary_bits = Vec::new();
    push_bits(&mut secondary_bits, 0, 1); // solid rather than two-colour
    push_bits(&mut secondary_bits, 0, 1); // transmit a new colour
    push_bits(&mut secondary_bits, u32::from(y), 8);
    push_bits(&mut secondary_bits, u32::from(cb >> 2), 6);
    push_bits(&mut secondary_bits, u32::from(cr >> 2), 6);
    push_bits(&mut secondary_bits, 0x6d, 8); // secondary marker
    let secondary = pack_bits(&secondary_bits);

    let secondary_offset = 6 + primary.len();
    let mut update = vec![
        0,
        0,
        0,
        ((secondary_offset >> 16) & 0xff) as u8,
        ((secondary_offset >> 8) & 0xff) as u8,
        (secondary_offset & 0xff) as u8,
    ];
    update.extend_from_slice(&primary);
    update.extend_from_slice(&secondary);

    let mut framed = (update.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&update);
    framed
}

fn mvs_dct(with_ac: bool) -> Vec<u8> {
    let mut primary_bits = Vec::new();
    push_bits(&mut primary_bits, 0, 1); // initial state
    push_bits(&mut primary_bits, 5, 3); // Rice/DCT tile
    push_bits(&mut primary_bits, 0, 1); // no repeat
    push_bits(&mut primary_bits, 0, 3); // white tile update
    push_bits(&mut primary_bits, 1, 1); // extended repeat
    push_bits(&mut primary_bits, 15, 4);
    push_bits(&mut primary_bits, 46, 8); // repeat count 62, covering 63 tiles
    push_bits(&mut primary_bits, 0x6d, 8); // primary marker
    let primary = pack_bits(&primary_bits);

    let mut secondary_bits = Vec::new();
    push_bits(&mut secondary_bits, 0, 1); // decode rather than reuse prior block
    push_bits(&mut secondary_bits, 3, 2); // retain both zero chroma predictors
    push_bits(&mut secondary_bits, 0, 1); // zero DC unary prefix
    push_bits(&mut secondary_bits, 0, 1); // zero DC magnitude
    if with_ac {
        push_bits(&mut secondary_bits, 0, 1); // AC short-control form
        push_bits(&mut secondary_bits, 2, 2); // positive base coefficient
    }
    push_bits(&mut secondary_bits, 0, 1); // AC short-control form
    push_bits(&mut secondary_bits, 1, 2); // AC end-of-block selector
    push_bits(&mut secondary_bits, 0, 1); // end block
    push_bits(&mut secondary_bits, 0x6d, 8); // secondary marker
    let secondary = pack_bits(&secondary_bits);

    let secondary_offset = 6 + primary.len();
    let mut update = vec![
        0,
        0,
        0,
        ((secondary_offset >> 16) & 0xff) as u8,
        ((secondary_offset >> 8) & 0xff) as u8,
        (secondary_offset & 0xff) as u8,
    ];
    update.extend_from_slice(&primary);
    update.extend_from_slice(&secondary);

    let mut framed = (update.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&update);
    framed
}

fn mvs_full_dct(with_ac: bool) -> Vec<u8> {
    let mut primary_bits = Vec::new();
    push_bits(&mut primary_bits, 0, 1); // initial state
    push_bits(&mut primary_bits, 5, 3); // Rice/DCT tile
    push_bits(&mut primary_bits, 1, 1); // extended repeat
    push_bits(&mut primary_bits, 15, 4);
    push_bits(&mut primary_bits, 47, 8); // repeat count 63
    push_bits(&mut primary_bits, 0x6d, 8); // primary marker
    let primary = pack_bits(&primary_bits);

    let mut secondary_bits = Vec::new();
    push_bits(&mut secondary_bits, 0, 1); // new coefficient block
    push_bits(&mut secondary_bits, 3, 2); // retain zero chroma predictors
    push_bits(&mut secondary_bits, 0, 1);
    push_bits(&mut secondary_bits, 0, 1); // zero DC
    if with_ac {
        push_bits(&mut secondary_bits, 0, 1); // AC short-control form
        push_bits(&mut secondary_bits, 2, 2); // positive base coefficient
    }
    push_bits(&mut secondary_bits, 0, 1);
    push_bits(&mut secondary_bits, 1, 2);
    push_bits(&mut secondary_bits, 0, 1); // AC end-of-block
    for _ in 1..64 {
        push_bits(&mut secondary_bits, 1, 1); // reuse prior coefficient block
    }
    push_bits(&mut secondary_bits, 0x6d, 8); // secondary marker
    let secondary = pack_bits(&secondary_bits);

    let secondary_offset = 6 + primary.len();
    let mut update = vec![
        0,
        0,
        0,
        ((secondary_offset >> 16) & 0xff) as u8,
        ((secondary_offset >> 8) & 0xff) as u8,
        (secondary_offset & 0xff) as u8,
    ];
    update.extend_from_slice(&primary);
    update.extend_from_slice(&secondary);

    let mut framed = (update.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&update);
    framed
}

fn mvs_full_differential() -> Vec<u8> {
    let mut bits = Vec::new();
    push_bits(&mut bits, 0, 2); // leave the edge-clipped first tile unchanged
    push_bits(&mut bits, 1, 2); // differential DCT selector
    push_bits(&mut bits, 1, 6); // two stored luma coefficients
    push_bits(&mut bits, 1, 3); // increase the signed AC coefficient by one
    push_bits(&mut bits, 0, 1); // unchanged Cr DC
    push_bits(&mut bits, 1, 2); // JPEG symbol 0x01 (one AC coefficient)
    push_bits(&mut bits, 1, 1); // positive coefficient value
    push_bits(&mut bits, 0, 2); // JPEG chrominance AC end-of-block
    push_bits(&mut bits, 0, 1); // unchanged Cb DC
    push_bits(&mut bits, 0, 2); // JPEG chrominance AC end-of-block
    for _ in 2..64 {
        push_bits(&mut bits, 0, 2); // unchanged remaining tiles
    }
    push_bits(&mut bits, 0x6d, 8);
    push_bits(&mut bits, 0x76, 8);
    push_bits(&mut bits, 0x73, 8);
    let encoded = pack_bits(&bits);
    let mut update = vec![1, 64, 64];
    update.extend_from_slice(&encoded);
    let mut framed = (update.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&update);
    framed
}

fn mvs_partial_cache(index: u16) -> Vec<u8> {
    let mut primary_bits = Vec::new();
    push_bits(&mut primary_bits, 0, 1);
    push_bits(&mut primary_bits, 6, 3);
    push_bits(&mut primary_bits, 0, 1);
    push_bits(&mut primary_bits, 0x6d, 8);
    let primary = pack_bits(&primary_bits);
    let mut secondary_bits = Vec::new();
    push_bits(&mut secondary_bits, u32::from(index >> 8), 8);
    push_bits(&mut secondary_bits, u32::from(index & 0xff), 8);
    push_bits(&mut secondary_bits, 0x6d, 8);
    let secondary = pack_bits(&secondary_bits);
    let secondary_offset = 6 + primary.len();
    let mut update = vec![
        0,
        0,
        0,
        ((secondary_offset >> 16) & 0xff) as u8,
        ((secondary_offset >> 8) & 0xff) as u8,
        (secondary_offset & 0xff) as u8,
    ];
    update.extend_from_slice(&primary);
    update.extend_from_slice(&secondary);
    let mut framed = (update.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&update);
    framed
}

fn mvs_full_cache(index: u16) -> Vec<u8> {
    let mut bits = Vec::new();
    push_bits(&mut bits, 3, 2);
    push_bits(&mut bits, 0, 1);
    push_bits(&mut bits, u32::from(index >> 8), 8);
    push_bits(&mut bits, u32::from(index & 0xff), 8);
    push_bits(&mut bits, 0x6d, 8);
    push_bits(&mut bits, 0x76, 8);
    push_bits(&mut bits, 0x73, 8);
    let mut update = vec![1, 0, 0];
    update.extend_from_slice(&pack_bits(&bits));
    let mut framed = (update.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&update);
    framed
}

fn send_mvs_rectangle(
    stream: &mut TcpStream,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    payload: &[u8],
) -> std::io::Result<()> {
    let mut update = vec![0, 0, 0, 1]; // FramebufferUpdate, one rectangle
    update.extend_from_slice(&x.to_be_bytes());
    update.extend_from_slice(&y.to_be_bytes());
    update.extend_from_slice(&width.to_be_bytes());
    update.extend_from_slice(&height.to_be_bytes());
    update.extend_from_slice(&1011_i32.to_be_bytes());
    update.extend_from_slice(payload);
    stream.write_all(&update)?;
    stream.flush()
}

fn main() -> std::io::Result<()> {
    let port = env::args().nth(1).unwrap_or_else(|| "5999".to_owned());
    let host = env::args().nth(2).unwrap_or_else(|| "127.0.0.1".to_owned());
    let host_ip: IpAddr = host
        .parse()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let allowed_peer: IpAddr = env::args()
        .nth(3)
        .map_or(Ok(host_ip), |value| value.parse())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let authentication = env::args().nth(4).unwrap_or_else(|| "ard".to_owned());
    let frame_kind = env::args().nth(5).unwrap_or_else(|| "white".to_owned());
    let listener = TcpListener::bind(format!("{host}:{port}"))?;
    println!("listening on vnc://{host}:{port}");
    let (mut stream, peer) = loop {
        let (candidate, peer) = listener.accept()?;
        if peer.ip() == allowed_peer {
            break (candidate, peer);
        }
        println!("rejected non-local peer {peer}");
    };
    println!("client connected from {peer}");

    // Use Apple's protocol banner so Screen Sharing keeps its ARD negotiation
    // path. Credentials are accepted only from this host and are not decoded.
    stream.write_all(b"RFB 003.889\n")?;
    let mut banner = [0_u8; 12];
    stream.read_exact(&mut banner)?;
    println!("client banner: {}", String::from_utf8_lossy(&banner));

    if &banner == b"RFB 003.003\n" {
        return Err(std::io::Error::other(
            "Apple ARD authentication requires protocol 3.889",
        ));
    }
    let security_type = if authentication == "vnc" { 2 } else { 30 };
    stream.write_all(&[1, security_type])?;
    stream.flush()?;
    if security_type == 2 {
        vnc_authenticate(&mut stream)?;
    } else {
        ard_authenticate(&mut stream)?;
    }
    stream.write_all(&0_u32.to_be_bytes())?; // accept any loopback response
    stream.flush()?;
    println!("accepted authentication");

    let mut shared = [0_u8; 1];
    stream.read_exact(&mut shared)?;
    println!("shared-session flag: {}", shared[0]);

    let (width, height) = (64_u16, 64_u16);
    let (update_width, update_height) = (width, height);
    let name = b"ard-rs MVS oracle";
    let mut init = Vec::new();
    init.extend_from_slice(&width.to_be_bytes());
    init.extend_from_slice(&height.to_be_bytes());
    init.extend_from_slice(&[
        32, 24, 0, 1, // bpp, depth, little-endian, true-colour
        0, 255, 0, 255, 0, 255, // channel maxima
        16, 8, 0, // channel shifts
        0, 0, 0, // padding
    ]);
    init.extend_from_slice(&(name.len() as u32).to_be_bytes());
    init.extend_from_slice(name);
    stream.write_all(&init)?;
    stream.flush()?;
    println!("sent server initialization");
    wait_for_update_request(&mut stream)?;

    let mut payload = match frame_kind.as_str() {
        "solid" => mvs_solid_ycbcr(200, 128, 128),
        "dct" => mvs_dct(false),
        "dct-ac" => mvs_dct(true),
        "dct-full" => mvs_full_dct(false),
        "dct-ac-full" => mvs_full_dct(true),
        "full-diff" => mvs_full_dct(true),
        _ => mvs_white(update_width, update_height),
    };
    if frame_kind == "full-diff" {
        // Keep two compact luma coefficients in Screen Sharing's per-tile
        // differential baseline.
        payload[5] = 2;
        payload[6] = 2;
    }
    send_mvs_rectangle(&mut stream, 0, 0, update_width, update_height, &payload)?;
    println!("sent {}-byte MVS rectangle", payload.len());
    if frame_kind == "full-diff" {
        let differential = mvs_full_differential();
        send_mvs_rectangle(
            &mut stream,
            0,
            0,
            update_width,
            update_height,
            &differential,
        )?;
        println!(
            "sent {}-byte full-update differential rectangle",
            differential.len()
        );
        let partial_cache = mvs_partial_cache(1);
        send_mvs_rectangle(&mut stream, 16, 0, 8, 8, &partial_cache)?;
        println!("sent explicit partial cache selector");
        let full_cache = mvs_full_cache(1);
        send_mvs_rectangle(&mut stream, 24, 0, 8, 8, &full_cache)?;
        println!("sent explicit full cache selector");
    }

    let mut sink = [0_u8; 4096];
    while stream.read(&mut sink)? != 0 {}
    Ok(())
}
