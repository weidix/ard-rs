//! Platform-independent keyboard symbols used by RFB/ARD key events.
//!
//! RFB carries X11 keysyms rather than an operating-system scan code.  The
//! viewer therefore converts a platform event into one of these stable
//! symbols before it reaches the wire.  Printable characters use the X11
//! Unicode keysym form (`0x0100_0000 | codepoint`) when they are not already
//! representable by the Latin-1 keysym range.

/// A named key with a stable RFB/X11 keysym.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArdNamedKey {
    Backspace,
    Tab,
    Enter,
    Space,
    Escape,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowLeft,
    ArrowUp,
    ArrowRight,
    ArrowDown,
    ShiftLeft,
    ShiftRight,
    ControlLeft,
    ControlRight,
    AltLeft,
    AltRight,
    MetaLeft,
    MetaRight,
    SuperLeft,
    SuperRight,
    CapsLock,
    NumLock,
    ScrollLock,
    PrintScreen,
    Pause,
    ContextMenu,
    Function(u8),
    Numpad(u8),
    NumpadAdd,
    NumpadSubtract,
    NumpadMultiply,
    NumpadDivide,
    NumpadDecimal,
    NumpadEnter,
    NumpadEqual,
}

/// A platform-neutral key representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArdKey {
    Character(char),
    Named(ArdNamedKey),
}

pub const XK_BACKSPACE: u32 = 0xff08;
pub const XK_TAB: u32 = 0xff09;
pub const XK_RETURN: u32 = 0xff0d;
pub const XK_ESCAPE: u32 = 0xff1b;
pub const XK_HOME: u32 = 0xff50;
pub const XK_LEFT: u32 = 0xff51;
pub const XK_UP: u32 = 0xff52;
pub const XK_RIGHT: u32 = 0xff53;
pub const XK_DOWN: u32 = 0xff54;
pub const XK_PAGE_UP: u32 = 0xff55;
pub const XK_PAGE_DOWN: u32 = 0xff56;
pub const XK_END: u32 = 0xff57;
pub const XK_INSERT: u32 = 0xff63;
pub const XK_CONTEXT_MENU: u32 = 0xff67;
pub const XK_DELETE: u32 = 0xffff;
pub const XK_SPACE: u32 = 0x20;
pub const XK_PAUSE: u32 = 0xff13;
pub const XK_SCROLL_LOCK: u32 = 0xff14;
pub const XK_PRINT_SCREEN: u32 = 0xff61;
pub const XK_NUM_LOCK: u32 = 0xff7f;
pub const XK_SHIFT_LEFT: u32 = 0xffe1;
pub const XK_SHIFT_RIGHT: u32 = 0xffe2;
pub const XK_CONTROL_LEFT: u32 = 0xffe3;
pub const XK_CONTROL_RIGHT: u32 = 0xffe4;
pub const XK_CAPS_LOCK: u32 = 0xffe5;
pub const XK_ALT_LEFT: u32 = 0xffe9;
pub const XK_ALT_RIGHT: u32 = 0xffea;
pub const XK_META_LEFT: u32 = 0xffe7;
pub const XK_META_RIGHT: u32 = 0xffe8;
pub const XK_SUPER_LEFT: u32 = 0xffeb;
pub const XK_SUPER_RIGHT: u32 = 0xffec;
pub const XK_F1: u32 = 0xffbe;
pub const XK_KP_0: u32 = 0xffb0;
pub const XK_KP_1: u32 = 0xffb1;
pub const XK_KP_2: u32 = 0xffb2;
pub const XK_KP_3: u32 = 0xffb3;
pub const XK_KP_4: u32 = 0xffb4;
pub const XK_KP_5: u32 = 0xffb5;
pub const XK_KP_6: u32 = 0xffb6;
pub const XK_KP_7: u32 = 0xffb7;
pub const XK_KP_8: u32 = 0xffb8;
pub const XK_KP_9: u32 = 0xffb9;
pub const XK_KP_MULTIPLY: u32 = 0xffaa;
pub const XK_KP_ADD: u32 = 0xffab;
pub const XK_KP_SEPARATOR: u32 = 0xffac;
pub const XK_KP_SUBTRACT: u32 = 0xffad;
pub const XK_KP_DECIMAL: u32 = 0xffae;
pub const XK_KP_DIVIDE: u32 = 0xffaf;
pub const XK_KP_ENTER: u32 = 0xff8d;
pub const XK_KP_EQUAL: u32 = 0xffbd;
pub const XK_ARROW_LEFT: u32 = XK_LEFT;
pub const XK_ARROW_UP: u32 = XK_UP;
pub const XK_ARROW_RIGHT: u32 = XK_RIGHT;
pub const XK_ARROW_DOWN: u32 = XK_DOWN;

/// Converts a Unicode scalar value to the X11/RFB keysym representation.
pub fn unicode_keysym(character: char) -> Option<u32> {
    let codepoint = u32::from(character);
    if codepoint == 0 || (0xd800..=0xdfff).contains(&codepoint) {
        return None;
    }
    if codepoint <= 0xff {
        Some(codepoint)
    } else {
        Some(0x0100_0000 | codepoint)
    }
}

/// Converts a platform-neutral key to the corresponding RFB/X11 keysym.
pub fn keysym_for_key(key: ArdKey) -> Option<u32> {
    match key {
        ArdKey::Character(character) => match character {
            '\u{8}' => Some(XK_BACKSPACE),
            '\t' => Some(XK_TAB),
            '\r' | '\n' => Some(XK_RETURN),
            character => unicode_keysym(character),
        },
        ArdKey::Named(key) => keysym_for_named_key(key),
    }
}

/// Converts a named key to a stable RFB/X11 keysym.
pub fn keysym_for_named_key(key: ArdNamedKey) -> Option<u32> {
    Some(match key {
        ArdNamedKey::Backspace => XK_BACKSPACE,
        ArdNamedKey::Tab => XK_TAB,
        ArdNamedKey::Enter => XK_RETURN,
        ArdNamedKey::Space => XK_SPACE,
        ArdNamedKey::Escape => XK_ESCAPE,
        ArdNamedKey::Delete => 0xffff,
        ArdNamedKey::Insert => XK_INSERT,
        ArdNamedKey::Home => XK_HOME,
        ArdNamedKey::End => XK_END,
        ArdNamedKey::PageUp => XK_PAGE_UP,
        ArdNamedKey::PageDown => XK_PAGE_DOWN,
        ArdNamedKey::ArrowLeft => XK_LEFT,
        ArdNamedKey::ArrowUp => XK_UP,
        ArdNamedKey::ArrowRight => XK_RIGHT,
        ArdNamedKey::ArrowDown => XK_DOWN,
        ArdNamedKey::ShiftLeft => XK_SHIFT_LEFT,
        ArdNamedKey::ShiftRight => XK_SHIFT_RIGHT,
        ArdNamedKey::ControlLeft => XK_CONTROL_LEFT,
        ArdNamedKey::ControlRight => XK_CONTROL_RIGHT,
        ArdNamedKey::AltLeft => XK_ALT_LEFT,
        ArdNamedKey::AltRight => XK_ALT_RIGHT,
        ArdNamedKey::MetaLeft => XK_META_LEFT,
        ArdNamedKey::MetaRight => XK_META_RIGHT,
        ArdNamedKey::SuperLeft => XK_SUPER_LEFT,
        ArdNamedKey::SuperRight => XK_SUPER_RIGHT,
        ArdNamedKey::CapsLock => XK_CAPS_LOCK,
        ArdNamedKey::NumLock => XK_NUM_LOCK,
        ArdNamedKey::ScrollLock => XK_SCROLL_LOCK,
        ArdNamedKey::PrintScreen => XK_PRINT_SCREEN,
        ArdNamedKey::Pause => XK_PAUSE,
        ArdNamedKey::ContextMenu => XK_CONTEXT_MENU,
        ArdNamedKey::Function(number) if (1..=35).contains(&number) => {
            XK_F1 + u32::from(number - 1)
        }
        ArdNamedKey::Function(_) => return None,
        ArdNamedKey::Numpad(number) if number <= 9 => XK_KP_0 + u32::from(number),
        ArdNamedKey::Numpad(_) => return None,
        ArdNamedKey::NumpadAdd => XK_KP_ADD,
        ArdNamedKey::NumpadSubtract => XK_KP_SUBTRACT,
        ArdNamedKey::NumpadMultiply => XK_KP_MULTIPLY,
        ArdNamedKey::NumpadDivide => XK_KP_DIVIDE,
        ArdNamedKey::NumpadDecimal => XK_KP_DECIMAL,
        ArdNamedKey::NumpadEnter => XK_KP_ENTER,
        ArdNamedKey::NumpadEqual => XK_KP_EQUAL,
    })
}

#[cfg(test)]
mod tests {
    use super::{ArdKey, ArdNamedKey, XK_F1, XK_KP_9, keysym_for_key, unicode_keysym};

    #[test]
    fn encodes_latin_and_unicode_keysyms() {
        assert_eq!(unicode_keysym('A'), Some(0x41));
        assert_eq!(unicode_keysym('é'), Some(0xe9));
        assert_eq!(unicode_keysym('中'), Some(0x0100_4e2d));
        assert_eq!(unicode_keysym('\0'), None);
    }

    #[test]
    fn maps_named_and_keypad_keys() {
        assert_eq!(
            keysym_for_key(ArdKey::Named(ArdNamedKey::Function(1))),
            Some(XK_F1)
        );
        assert_eq!(
            keysym_for_key(ArdKey::Named(ArdNamedKey::Numpad(9))),
            Some(XK_KP_9)
        );
    }
}
