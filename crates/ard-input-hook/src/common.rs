//! Platform-independent types shared by every hook backend.

use std::error::Error;
use std::fmt;

/// Startup configuration for a [`KeyboardHook`](crate::KeyboardHook).
#[derive(Debug, Clone, Copy)]
pub struct HookConfig {
    /// Whether capture/suppression starts armed.
    pub capture_enabled: bool,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            capture_enabled: true,
        }
    }
}

/// Errors reported while starting or driving a hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookError {
    /// The OS denied the hook (for example missing Accessibility/Input
    /// Monitoring permission on macOS, or a taken grab on X11).
    PermissionDenied(String),
    /// The platform/compositor does not provide the required API.
    Unsupported(String),
    /// A native call failed.
    Io(String),
    /// Another failure (the payload is already user-readable).
    Other(String),
}

impl fmt::Display for HookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PermissionDenied(message)
            | Self::Unsupported(message)
            | Self::Io(message)
            | Self::Other(message) => f.write_str(message),
        }
    }
}

impl Error for HookError {}

/// Events produced by the hook thread.
#[derive(Debug, Clone)]
pub enum HookEvent {
    /// A key event that must be forwarded to the remote input layer.
    Key(RawKeyEvent),
    /// A runtime error (grab failure, tap being disabled, ...).
    Error(HookError),
    /// The compositor granted the shortcut inhibitor (Wayland).
    InhibitActive,
    /// The compositor revoked the shortcut inhibitor (Wayland).
    InhibitInactive,
}

/// Semantic classification of a captured key.
///
/// `Other` carries the platform key code in [`RawKeyEvent::key_code`]; on X11
/// the key is additionally resolved to an X11 keysym.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyKind {
    WinLeft,
    WinRight,
    AltLeft,
    AltRight,
    CtrlLeft,
    CtrlRight,
    ShiftLeft,
    ShiftRight,
    PrintScreen,
    Tab,
    Escape,
    Other,
}

/// Normalized modifier state tracked from the raw event stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Windows key / Command / Super depending on platform.
    pub meta: bool,
    pub caps_lock: bool,
    pub num_lock: bool,
}

impl Modifiers {
    /// Compact bitset useful for logging and protocol layers.
    pub fn bits(&self) -> u16 {
        let mut bits = 0u16;
        if self.ctrl {
            bits |= 1 << 0;
        }
        if self.alt {
            bits |= 1 << 1;
        }
        if self.shift {
            bits |= 1 << 2;
        }
        if self.meta {
            bits |= 1 << 3;
        }
        if self.caps_lock {
            bits |= 1 << 4;
        }
        if self.num_lock {
            bits |= 1 << 5;
        }
        bits
    }
}

#[cfg(test)]
mod tests {
    use super::Modifiers;

    #[test]
    fn modifier_bitset_marks_every_pressed_modifier() {
        let modifiers = Modifiers {
            ctrl: true,
            alt: false,
            shift: true,
            meta: true,
            caps_lock: false,
            num_lock: true,
        };
        assert_eq!(modifiers.bits(), (1 << 0) | (1 << 2) | (1 << 3) | (1 << 5));
        assert_eq!(Modifiers::default().bits(), 0);
    }
}

/// A single key event as seen by the platform hook.
///
/// All numeric fields preserve the raw values reported by the native API so
/// the remote input layer can reconstruct platform-specific information:
///
/// - Windows: `key_code` = virtual-key code, `scan_code` = hardware scan
///   code, `flags` = `KBDLLHOOKSTRUCT.flags`, [`PlatformKeyInfo::Windows`]
///   keeps `dwExtraInfo` and the injected flag.
/// - macOS: `key_code` = virtual keycode, `scan_code` = hardware keycode,
///   `flags` = `CGEventFlags`, [`PlatformKeyInfo::MacOs`] keeps the raw
///   unicode characters carried by the event.
/// - X11: `key_code` = keycode, `scan_code` = keycode, `flags` = X11 state
///   mask, [`PlatformKeyInfo::X11`] keeps the raw keysym and state.
#[derive(Debug, Clone)]
pub struct RawKeyEvent {
    pub pressed: bool,
    pub repeat: bool,
    pub kind: KeyKind,
    /// Platform key code (VK code / virtual keycode / X11 keycode).
    pub key_code: u32,
    /// Platform scan code (hardware scan code / hardware keycode / X11 keycode).
    pub scan_code: u32,
    /// Platform flags (LL hook flags / CGEventFlags / X11 state mask).
    pub flags: u32,
    /// X11 keysym when the backend can resolve one for this key.
    pub keysym: Option<u32>,
    pub modifiers: Modifiers,
    /// Milliseconds since an arbitrary epoch (platform timestamp).
    pub timestamp: u64,
    pub platform: PlatformKeyInfo,
}

/// Extra platform-specific keyboard information preserved for the remote
/// input layer.
#[derive(Debug, Clone)]
pub enum PlatformKeyInfo {
    #[cfg(target_os = "windows")]
    Windows {
        /// `KBDLLHOOKSTRUCT.dwExtraInfo`
        extra_info: usize,
        /// Whether the event was injected.
        injected: bool,
    },
    #[cfg(target_os = "macos")]
    MacOs {
        /// Raw `CGEventFlags` as a u64.
        flags64: u64,
        /// Unicode string carried by the event (key down only).
        unicode: Option<String>,
        /// Whether the event is a system-defined repeat.
        autorepeat: bool,
    },
    #[cfg(all(target_os = "linux", feature = "x11"))]
    X11 {
        /// Raw X11 state mask.
        state: u16,
        /// The event time (X11 `Time`).
        time: u32,
        /// Raw X11 keysym resolved from the keymap.
        keysym: u32,
    },
    #[cfg(all(target_os = "linux", feature = "wayland"))]
    Wayland {
        /// Raw Wayland key code.
        code: u32,
        /// Raw `mods_depressed` modifier mask.
        mods_depressed: u32,
    },
}

/// X11 keysyms used by the hook as the neutral remote key encoding.
pub mod keysyms {
    pub const XK_TAB: u32 = 0xff09;
    pub const XK_ESCAPE: u32 = 0xff1b;
    pub const XK_PRINT: u32 = 0xff61;
    pub const XK_SHIFT_LEFT: u32 = 0xffe1;
    pub const XK_SHIFT_RIGHT: u32 = 0xffe2;
    pub const XK_CONTROL_LEFT: u32 = 0xffe3;
    pub const XK_CONTROL_RIGHT: u32 = 0xffe4;
    pub const XK_META_LEFT: u32 = 0xffe7;
    pub const XK_META_RIGHT: u32 = 0xffe8;
    pub const XK_ALT_LEFT: u32 = 0xffe9;
    pub const XK_ALT_RIGHT: u32 = 0xffea;
    pub const XK_SUPER_LEFT: u32 = 0xffeb;
    pub const XK_SUPER_RIGHT: u32 = 0xffec;
}
