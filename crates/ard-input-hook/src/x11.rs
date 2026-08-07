//! Linux/X11 backend based on an exclusive keyboard grab
//! (`XGrabKeyboard` through `x11rb`).
//!
//! A dedicated thread owns the X11 connection.  While enabled it actively
//! grabs the keyboard on the root window; every `KeyPress`/`KeyRelease` is
//! reported to this client and no other client (including the application's
//! own iced window) receives the event, so suppression is inherent.  Each
//! event is resolved to an X11 keysym and pushed into a non-blocking channel.
//! When disabled the grab is released and the root event mask is cleared, so
//! the application regains its normal input path.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt, EventMask, GetKeyboardMappingReply,
    GetModifierMappingReply, GrabMode, GrabStatus, Keycode, Keysym,
};
use x11rb::{CURRENT_TIME, connect};

use crate::common::{
    HookConfig, HookError, HookEvent, KeyKind, Modifiers, PlatformKeyInfo, RawKeyEvent,
};
use crate::keysyms;

const KEYMAP_COLUMN_UNSHIFTED: usize = 0;
const KEYMAP_COLUMN_SHIFTED: usize = 1;

struct X11State {
    enabled: AtomicBool,
    shutdown: AtomicBool,
}

/// Handle that owns the X11 hook thread.
pub struct X11Hook {
    state: Arc<X11State>,
    thread: Option<JoinHandle<()>>,
}

impl X11Hook {
    pub fn start(config: HookConfig) -> Result<(Self, Receiver<HookEvent>), HookError> {
        let (sender, receiver) = sync_channel(256);
        let state = Arc::new(X11State {
            enabled: AtomicBool::new(config.capture_enabled),
            shutdown: AtomicBool::new(false),
        });
        let thread_state = Arc::clone(&state);
        let thread = thread::Builder::new()
            .name("ard-input-hook-x11".into())
            .spawn(move || x11_thread_main(thread_state, sender))
            .map_err(|error| HookError::Io(format!("cannot start X11 hook thread: {error}")))?;
        Ok((
            Self {
                state,
                thread: Some(thread),
            },
            receiver,
        ))
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<(), HookError> {
        self.state.enabled.store(enabled, Ordering::SeqCst);
        Ok(())
    }
}

impl Drop for X11Hook {
    fn drop(&mut self) {
        self.state.shutdown.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn x11_thread_main(state: Arc<X11State>, sender: SyncSender<HookEvent>) {
    let (connection, screen) = match connect(None) {
        Ok(connected) => connected,
        Err(error) => {
            let _ = sender.try_send(HookEvent::Error(HookError::Io(format!(
                "cannot connect to the X11 display: {error}"
            ))));
            return;
        }
    };
    let setup = connection.setup();
    let root = setup.roots[screen].root;
    let first_keycode = setup.min_keycode;
    let keycode_count = setup.max_keycode - setup.min_keycode + 1;

    let keymap = match connection.get_keyboard_mapping(first_keycode, keycode_count) {
        Ok(cookie) => match cookie.reply() {
            Ok(reply) => Some(reply),
            Err(error) => {
                let _ = sender.try_send(HookEvent::Error(HookError::Io(format!(
                    "cannot read the X11 keymap: {error}"
                ))));
                None
            }
        },
        Err(error) => {
            let _ = sender.try_send(HookEvent::Error(HookError::Io(format!(
                "cannot read the X11 keymap: {error}"
            ))));
            None
        }
    };
    let modifier_masks = match connection.get_modifier_mapping() {
        Ok(cookie) => match cookie.reply() {
            Ok(reply) => resolve_modifier_masks(&reply, first_keycode, keymap.as_ref()),
            Err(error) => {
                let _ = sender.try_send(HookEvent::Error(HookError::Io(format!(
                    "cannot read the X11 modifier mapping: {error}"
                ))));
                Default::default()
            }
        },
        Err(error) => {
            let _ = sender.try_send(HookEvent::Error(HookError::Io(format!(
                "cannot read the X11 modifier mapping: {error}"
            ))));
            Default::default()
        }
    };

    let mut grabbed = false;
    let mut last_enabled = state.enabled.load(Ordering::SeqCst);
    loop {
        if state.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let enabled = state.enabled.load(Ordering::SeqCst);
        if enabled != last_enabled {
            if enabled {
                grabbed = grab_keyboard(&connection, root, &sender);
            } else {
                release_keyboard(&connection, root);
                grabbed = false;
            }
            last_enabled = enabled;
        }

        let mut received = false;
        while let Ok(Some(event)) = connection.poll_for_event() {
            received = true;
            match event {
                Event::KeyPress(event) => {
                    if grabbed {
                        let state = u16::from(event.state);
                        handle_key_event(
                            keymap.as_ref(),
                            first_keycode,
                            state,
                            event.time,
                            event.detail,
                            true,
                            &modifier_masks,
                            &sender,
                        );
                    }
                }
                Event::KeyRelease(event) => {
                    if grabbed {
                        let state = u16::from(event.state);
                        handle_key_event(
                            keymap.as_ref(),
                            first_keycode,
                            state,
                            event.time,
                            event.detail,
                            false,
                            &modifier_masks,
                            &sender,
                        );
                    }
                }
                _ => {}
            }
        }
        if !received {
            thread::sleep(Duration::from_millis(1));
        }
    }

    if grabbed {
        release_keyboard(&connection, root);
    }
}

fn grab_keyboard<C: Connection>(connection: &C, root: u32, sender: &SyncSender<HookEvent>) -> bool {
    let grab =
        match connection.grab_keyboard(false, root, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)
        {
            Ok(cookie) => cookie.reply(),
            Err(error) => Err(error.into()),
        };
    match grab {
        Ok(reply) if reply.status == GrabStatus::SUCCESS => {
            let aux = ChangeWindowAttributesAux::new()
                .event_mask(EventMask::KEY_PRESS | EventMask::KEY_RELEASE);
            let _ = connection.change_window_attributes(root, &aux);
            let _ = connection.flush();
            true
        }
        Ok(reply) => {
            let _ = sender.try_send(HookEvent::Error(HookError::PermissionDenied(format!(
                "XGrabKeyboard was refused by the server (status {:?})",
                reply.status
            ))));
            false
        }
        Err(error) => {
            let _ = sender.try_send(HookEvent::Error(HookError::Io(format!(
                "XGrabKeyboard failed: {error}"
            ))));
            false
        }
    }
}

fn release_keyboard<C: Connection>(connection: &C, root: u32) {
    let _ = connection.ungrab_keyboard(CURRENT_TIME);
    let aux = ChangeWindowAttributesAux::new().event_mask(None);
    let _ = connection.change_window_attributes(root, &aux);
    let _ = connection.flush();
}

fn handle_key_event(
    keymap: Option<&GetKeyboardMappingReply>,
    first_keycode: Keycode,
    state: u16,
    time: u32,
    keycode: Keycode,
    pressed: bool,
    masks: &ModifierMasks,
    sender: &SyncSender<HookEvent>,
) {
    let raw_keysym = keysym_for_keycode(keymap, first_keycode, state, keycode);
    let kind = classify_keysym(raw_keysym);
    // Follow the remote input layer's neutral encoding: Win/Super maps to the
    // Alt keysyms, Alt to the Meta keysyms (same convention the iced input
    // path uses), so both sources produce identical RFB keysyms.
    let keysym = keysym_for_kind(kind).unwrap_or(raw_keysym);
    let modifiers = modifiers_from_state(state, masks);
    let raw = RawKeyEvent {
        pressed,
        repeat: false,
        kind,
        key_code: u32::from(keycode),
        scan_code: u32::from(keycode),
        flags: u32::from(state),
        keysym: Some(keysym),
        modifiers,
        timestamp: u64::from(time),
        platform: PlatformKeyInfo::X11 {
            state,
            time,
            keysym: raw_keysym,
        },
    };
    let _ = sender.try_send(HookEvent::Key(raw));
}

fn keysym_for_keycode(
    keymap: Option<&GetKeyboardMappingReply>,
    first_keycode: Keycode,
    state: u16,
    keycode: Keycode,
) -> Keysym {
    let Some(keymap) = keymap else {
        return 0;
    };
    let index = usize::from(keycode.saturating_sub(first_keycode));
    let keysyms_per_keycode = usize::from(keymap.keysyms_per_keycode.max(1));
    let Some(keysyms) = keymap.keysyms.get(index * keysyms_per_keycode..) else {
        return 0;
    };
    let column = if state & (1 << 0) != 0 {
        KEYMAP_COLUMN_SHIFTED
    } else {
        KEYMAP_COLUMN_UNSHIFTED
    };
    keysyms
        .get(column.min(keysyms_per_keycode - 1))
        .copied()
        .unwrap_or(0)
}

fn classify_keysym(keysym: Keysym) -> KeyKind {
    match keysym {
        keysyms::XK_SHIFT_LEFT => KeyKind::ShiftLeft,
        keysyms::XK_SHIFT_RIGHT => KeyKind::ShiftRight,
        keysyms::XK_CONTROL_LEFT => KeyKind::CtrlLeft,
        keysyms::XK_CONTROL_RIGHT => KeyKind::CtrlRight,
        keysyms::XK_ALT_LEFT => KeyKind::AltLeft,
        keysyms::XK_ALT_RIGHT => KeyKind::AltRight,
        keysyms::XK_META_LEFT => KeyKind::AltLeft,
        keysyms::XK_META_RIGHT => KeyKind::AltRight,
        keysyms::XK_SUPER_LEFT => KeyKind::WinLeft,
        keysyms::XK_SUPER_RIGHT => KeyKind::WinRight,
        keysyms::XK_PRINT => KeyKind::PrintScreen,
        keysyms::XK_TAB => KeyKind::Tab,
        keysyms::XK_ESCAPE => KeyKind::Escape,
        _ => KeyKind::Other,
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct ModifierMasks {
    shift: u16,
    lock: u16,
    ctrl: u16,
    alt: u16,
    meta: u16,
    super_: u16,
}

fn resolve_modifier_masks(
    reply: &GetModifierMappingReply,
    first_keycode: Keycode,
    keymap: Option<&GetKeyboardMappingReply>,
) -> ModifierMasks {
    let mut masks = ModifierMasks::default();
    let per_modifier = usize::from(reply.keycodes_per_modifier());
    if per_modifier == 0 {
        return masks;
    }
    for (modifier_index, keycodes) in reply.keycodes.chunks(per_modifier).enumerate() {
        let bit = 1 << modifier_index;
        for keycode in keycodes {
            let keysym = keysym_for_keycode(keymap, first_keycode, 0, *keycode);
            match keysym {
                keysyms::XK_SHIFT_LEFT | keysyms::XK_SHIFT_RIGHT => masks.shift |= bit,
                keysyms::XK_CONTROL_LEFT | keysyms::XK_CONTROL_RIGHT => masks.ctrl |= bit,
                keysyms::XK_ALT_LEFT | keysyms::XK_ALT_RIGHT => masks.alt |= bit,
                keysyms::XK_META_LEFT | keysyms::XK_META_RIGHT => masks.meta |= bit,
                keysyms::XK_SUPER_LEFT | keysyms::XK_SUPER_RIGHT => masks.super_ |= bit,
                0xffe5 => masks.lock |= bit,
                _ => {}
            }
        }
    }
    masks
}

fn modifiers_from_state(state: u16, masks: &ModifierMasks) -> Modifiers {
    Modifiers {
        ctrl: state & masks.ctrl != 0,
        alt: state & masks.alt != 0,
        shift: state & masks.shift != 0,
        meta: state & (masks.meta | masks.super_) != 0,
        caps_lock: state & masks.lock != 0,
        num_lock: false,
    }
}

/// Neutral keysym for the semantic key kind (see `keysym_for_kind` in the
/// other backends).  `Other` returns `None` so the raw keysym is kept.
fn keysym_for_kind(kind: KeyKind) -> Option<u32> {
    Some(match kind {
        KeyKind::WinLeft => keysyms::XK_ALT_LEFT,
        KeyKind::WinRight => keysyms::XK_ALT_RIGHT,
        KeyKind::AltLeft => keysyms::XK_META_LEFT,
        KeyKind::AltRight => keysyms::XK_META_RIGHT,
        KeyKind::CtrlLeft => keysyms::XK_CONTROL_LEFT,
        KeyKind::CtrlRight => keysyms::XK_CONTROL_RIGHT,
        KeyKind::ShiftLeft => keysyms::XK_SHIFT_LEFT,
        KeyKind::ShiftRight => keysyms::XK_SHIFT_RIGHT,
        KeyKind::PrintScreen => keysyms::XK_PRINT,
        KeyKind::Tab => keysyms::XK_TAB,
        KeyKind::Escape => keysyms::XK_ESCAPE,
        KeyKind::Other => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysym_lookup_uses_the_shift_column() {
        let keymap = GetKeyboardMappingReply {
            keysyms_per_keycode: 2,
            sequence: 0,
            keysyms: vec![
                // Keycode 8: '1' / '!'
                0x31, 0x21, // Keycode 9: Super_L / Super_L
                0xffeb, 0xffeb,
            ],
        };
        assert_eq!(keysym_for_keycode(Some(&keymap), 8, 0, 8), 0x31);
        assert_eq!(keysym_for_keycode(Some(&keymap), 8, 1, 8), 0x21);
        assert_eq!(keysym_for_keycode(Some(&keymap), 8, 0, 9), 0xffeb);
        assert_eq!(keysym_for_keycode(Some(&keymap), 8, 0, 99), 0);
        assert_eq!(keysym_for_keycode(None, 8, 0, 8), 0);
    }

    #[test]
    fn keysyms_classify_modifiers_and_captured_keys() {
        assert_eq!(classify_keysym(keysyms::XK_SUPER_LEFT), KeyKind::WinLeft);
        assert_eq!(classify_keysym(keysyms::XK_ALT_LEFT), KeyKind::AltLeft);
        assert_eq!(
            classify_keysym(keysyms::XK_CONTROL_RIGHT),
            KeyKind::CtrlRight
        );
        assert_eq!(classify_keysym(keysyms::XK_TAB), KeyKind::Tab);
        assert_eq!(classify_keysym(keysyms::XK_ESCAPE), KeyKind::Escape);
        assert_eq!(classify_keysym(keysyms::XK_PRINT), KeyKind::PrintScreen);
        assert_eq!(classify_keysym(0x61), KeyKind::Other);
    }

    #[test]
    fn neutral_keysym_follows_the_remote_input_convention() {
        assert_eq!(
            keysym_for_kind(KeyKind::WinLeft),
            Some(keysyms::XK_ALT_LEFT)
        );
        assert_eq!(
            keysym_for_kind(KeyKind::AltLeft),
            Some(keysyms::XK_META_LEFT)
        );
        assert_eq!(keysym_for_kind(KeyKind::Tab), Some(keysyms::XK_TAB));
        assert_eq!(keysym_for_kind(KeyKind::Other), None);
    }

    #[test]
    fn modifier_masks_are_resolved_from_the_keymap() {
        let keymap = GetKeyboardMappingReply {
            keysyms_per_keycode: 1,
            sequence: 0,
            keysyms: vec![
                0x0000,                   // keycode 8 unused
                keysyms::XK_SHIFT_LEFT,   // 9
                0x0000,                   // 10
                keysyms::XK_CONTROL_LEFT, // 11
                0x0000,                   // 12
                keysyms::XK_SUPER_LEFT,   // 13
            ],
        };
        // 8 modifiers x 1 keycode; Mod2/Mod4 rows carry shift/ctrl/super.
        let mut keycodes = vec![0u8; 8];
        keycodes[1] = 9;
        keycodes[2] = 11;
        keycodes[4] = 13;
        let reply = GetModifierMappingReply {
            sequence: 0,
            length: 0,
            keycodes,
        };
        let masks = resolve_modifier_masks(&reply, 8, Some(&keymap));
        assert_eq!(masks.shift, 1 << 1);
        assert_eq!(masks.ctrl, 1 << 2);
        assert_eq!(masks.super_, 1 << 4);
        assert_eq!(masks.alt, 0);
    }
}
