#![forbid(unsafe_code)]

//! A memory-safe, platform-independent Apple Remote Desktop (ARD) wire parser.
//!
//! This crate deliberately does not depend on a VNC client library or any
//! operating-system framework. ARD uses RFB framing, but adds its own protocol
//! version, security methods and framebuffer encodings.

mod auth;
mod client;
mod decoder;
mod dispatcher;
mod error;
mod framebuffer;
mod input;
mod mvs;
mod oracle;
mod protocol;
mod transport;
mod wire;

pub use auth::{ArdType30ClientExchange, build_ard_type30_client_exchange};
pub use client::{
    ArdClient, ArdClientConfig, ArdClientError, ArdClientEvent, ArdClientInput, ArdFrameInfo,
    ArdFrameOutput, ArdReconnectPolicy, ArdVideoQuality,
};
pub use decoder::{DecodeLimits, Decoder};
pub use dispatcher::{ArdMessageDispatcher, ArdServerMessage};
pub use error::{Error, Result};
pub use framebuffer::{Framebuffer, FramebufferFormat};
pub use input::{
    ArdKey, ArdNamedKey, XK_ALT_LEFT, XK_ALT_RIGHT, XK_ARROW_DOWN, XK_ARROW_LEFT, XK_ARROW_RIGHT,
    XK_ARROW_UP, XK_BACKSPACE, XK_CAPS_LOCK, XK_CONTEXT_MENU, XK_CONTROL_LEFT, XK_CONTROL_RIGHT,
    XK_DELETE, XK_DOWN, XK_END, XK_ESCAPE, XK_F1, XK_HOME, XK_INSERT, XK_KP_0, XK_KP_1, XK_KP_2,
    XK_KP_3, XK_KP_4, XK_KP_5, XK_KP_6, XK_KP_7, XK_KP_8, XK_KP_9, XK_KP_ADD, XK_KP_DECIMAL,
    XK_KP_DIVIDE, XK_KP_ENTER, XK_KP_EQUAL, XK_KP_MULTIPLY, XK_KP_SEPARATOR, XK_KP_SUBTRACT,
    XK_LEFT, XK_META_LEFT, XK_META_RIGHT, XK_NUM_LOCK, XK_PAGE_DOWN, XK_PAGE_UP, XK_PAUSE,
    XK_PRINT_SCREEN, XK_RETURN, XK_RIGHT, XK_SCROLL_LOCK, XK_SHIFT_LEFT, XK_SHIFT_RIGHT, XK_SPACE,
    XK_SUPER_LEFT, XK_SUPER_RIGHT, XK_TAB, XK_UP, keysym_for_key, keysym_for_named_key,
    unicode_keysym,
};
pub use mvs::{MvsGpuFrame, MvsGpuTile, MvsGpuTileUpdate};
pub use oracle::{EncryptedTransportOracle, OracleReport};
pub use protocol::{
    ArdAuthChallenge, ArdAuthResponse, ArdClientInit, ArdEncryptionControl, ArdServerInitExtension,
    ArdSessionOptions, ArdSetEncryptionLevel, ArdViewerInformation, Encoding, PixelFormat,
    ProtocolVersion, Rectangle, SecurityType, ServerInit, build_ard_auto_frame_update,
    build_ard_encryption_activation, build_ard_server_init, build_ard_set_encryption_level,
    build_client_cut_text, build_clipboard_text, build_framebuffer_update_request, build_key_event,
    build_pointer_event, build_set_encodings, build_set_pixel_format, parse_ard_auth_challenge,
    parse_ard_auth_response, parse_ard_client_init, parse_ard_encryption_control,
    parse_ard_session_options, parse_ard_set_encryption_level, parse_ard_viewer_information,
    parse_framebuffer_update, parse_security_types, parse_server_init,
};
pub use transport::{
    ArdEncryptedRecordFramer, ArdSessionMaterial, ArdSessionRecordDecoder, ArdSessionRecordEncoder,
    ArdVerifiedRecordStream, unwrap_ard_session_material,
};
