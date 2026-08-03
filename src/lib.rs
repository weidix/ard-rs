#![forbid(unsafe_code)]

//! A memory-safe, platform-independent Apple Remote Desktop (ARD) wire parser.
//!
//! This crate deliberately does not depend on a VNC client library or any
//! operating-system framework. ARD uses RFB framing, but adds its own protocol
//! version, security methods and framebuffer encodings.

mod auth;
mod decoder;
mod error;
mod framebuffer;
mod mvs;
mod protocol;
mod transport;
mod wire;

pub use auth::{ArdType30ClientExchange, build_ard_type30_client_exchange};
pub use decoder::{DecodeLimits, Decoder};
pub use error::{Error, Result};
pub use framebuffer::Framebuffer;
pub use protocol::{
    ArdAuthChallenge, ArdAuthResponse, ArdClientInit, ArdEncryptionControl, ArdSessionOptions,
    ArdViewerInformation, Encoding, PixelFormat, ProtocolVersion, Rectangle, SecurityType,
    ServerInit, build_framebuffer_update_request, build_set_encodings, build_set_pixel_format,
    parse_ard_auth_challenge, parse_ard_auth_response, parse_ard_client_init,
    parse_ard_encryption_control, parse_ard_session_options, parse_ard_viewer_information,
    parse_framebuffer_update, parse_security_types, parse_server_init,
};
pub use transport::{
    ArdEncryptedRecordFramer, ArdSessionMaterial, ArdSessionRecordDecoder, ArdSessionRecordEncoder,
    ArdVerifiedRecordStream, unwrap_ard_session_material,
};
