//! macOS backend based on a `CGEventTap`.
//!
//! A dedicated thread owns a Core Foundation run loop that feeds the event
//! tap.  The tap callback runs on that thread and is kept extremely small:
//! it parses the key event, tracks modifier state, decides whether the event
//! must be suppressed, and pushes a [`RawKeyEvent`] into a non-blocking
//! channel.
//!
//! While enabled the tap suppresses:
//!
//! - the Command key itself (both sides), which covers every `Command+...`
//!   system shortcut (Spotlight, Mission Control, app switcher, ...);
//! - the print-screen key (F13);
//! - `Command+Tab`, `Command+Esc` and `Control+Esc` (the macOS equivalents of
//!   the Windows `Alt+Tab` / `Alt+Esc` / `Ctrl+Esc` combos).
//!
//! All other keys are passed through untouched and reach the application
//! through its normal window input path.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use objc2_core_foundation::{CFMachPort, CFRunLoop, CFString, kCFRunLoopDefaultMode};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType,
};

use crate::common::{
    HookConfig, HookError, HookEvent, KeyKind, Modifiers, PlatformKeyInfo, RawKeyEvent,
};
use crate::keysyms;

/// Hardware keycodes for the keys the tap cares about.
mod keycodes {
    pub const COMMAND_RIGHT: u32 = 54;
    pub const COMMAND_LEFT: u32 = 55;
    pub const SHIFT: u32 = 56;
    pub const CONTROL: u32 = 59;
    pub const OPTION: u32 = 58;
    pub const RIGHT_SHIFT: u32 = 60;
    pub const RIGHT_OPTION: u32 = 61;
    pub const RIGHT_CONTROL: u32 = 62;
    pub const TAB: u32 = 48;
    pub const ESCAPE: u32 = 53;
    /// The print-screen key found on full-size Apple keyboards.
    pub const F13: u32 = 105;
}

const MASK_KEY_DOWN: u64 = 1 << 10;
const MASK_KEY_UP: u64 = 1 << 11;
const MASK_FLAGS_CHANGED: u64 = 1 << 12;

/// Shared state visible to the native callback through its `user_info`
/// pointer and to the handle through an `Arc`.
struct TapState {
    enabled: AtomicBool,
    shutdown: AtomicBool,
    /// Raw `CFMachPortRef` of the active tap, used to re-enable a tap that
    /// macOS disabled (timeout or user input).
    tap: AtomicUsize,
    sender: Mutex<Option<SyncSender<HookEvent>>>,
}

/// Handle that owns the tap thread.
pub struct MacOsHook {
    state: Arc<TapState>,
    thread: Option<JoinHandle<()>>,
}

impl MacOsHook {
    pub fn start(config: HookConfig) -> Result<(Self, Receiver<HookEvent>), HookError> {
        let (sender, receiver) = sync_channel(256);
        let state = Arc::new(TapState {
            enabled: AtomicBool::new(config.capture_enabled),
            shutdown: AtomicBool::new(false),
            tap: AtomicUsize::new(0),
            sender: Mutex::new(Some(sender)),
        });
        let thread_state = Arc::clone(&state);
        let thread = thread::Builder::new()
            .name("ard-input-hook-macos".into())
            .spawn(move || run_tap_thread(thread_state))
            .map_err(|error| HookError::Io(format!("cannot start macOS hook thread: {error}")))?;
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

impl Drop for MacOsHook {
    fn drop(&mut self) {
        self.state.shutdown.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_tap_thread(state: Arc<TapState>) {
    // The callback receives this raw pointer as user_info.  It is valid for
    // the whole thread lifetime; we re-materialize the Arc when we exit.
    let user_info = Arc::into_raw(Arc::clone(&state)) as *mut c_void;

    let tap = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::HIDEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            MASK_KEY_DOWN | MASK_KEY_UP | MASK_FLAGS_CHANGED,
            Some(tap_callback),
            user_info,
        )
    };

    let Some(tap) = tap else {
        state.sender.lock().ok().and_then(|mut sender| {
            sender
                .take()
                .and_then(|sender| sender.try_send(HookEvent::Error(HookError::PermissionDenied(
                    "CGEventTap could not be created; grant the app Accessibility or Input Monitoring permission".into(),
                ))).ok())
        });
        unsafe { drop(Arc::from_raw(user_info.cast::<TapState>())) };
        return;
    };

    state.tap.store(&*tap as *const CFMachPort as usize, Ordering::SeqCst);

        let run_loop = CFRunLoop::current().expect("macOS thread has a CFRunLoop");
    let source = CFMachPort::new_run_loop_source(None, Some(&tap), 0)
        .expect("CFMachPortCreateRunLoopSource failed");
    // SAFETY: reading the framework's static mode string is safe for the
    // process lifetime.
    let mode: Option<&CFString> = unsafe { kCFRunLoopDefaultMode };
    run_loop.add_source(Some(&source), mode);

    // Run the loop in short slices so the shutdown flag is observed quickly
    // even when no keyboard event wakes the loop.
    while !state.shutdown.load(Ordering::SeqCst) {
        // `run_in_mode` always operates on the current thread's run loop.
        CFRunLoop::run_in_mode(mode, 0.05, false);
    }

    CGEvent::tap_enable(&tap, false);
    run_loop.remove_source(Some(&source), mode);
    tap.invalidate();
    if let Ok(mut sender) = state.sender.lock() {
        *sender = None;
    }

    // SAFETY: `user_info` was produced by `Arc::into_raw` above and is not
    // used again after this point.
    unsafe { drop(Arc::from_raw(user_info.cast::<TapState>())) };
}

/// Native tap callback.  Returns the event to let it pass through, or NULL to
/// suppress it.
unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: std::ptr::NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    // SAFETY: the pointer comes from `Arc::into_raw` in `run_tap_thread` and
    // stays valid while the tap is installed.
    let state = unsafe { &*user_info.cast::<TapState>() };

    if event_type == CGEventType::TapDisabledByTimeout
        || event_type == CGEventType::TapDisabledByUserInput
    {
        // macOS disabled the tap; put it back on line.
        let tap = state.tap.load(Ordering::SeqCst) as *const CFMachPort;
        if !tap.is_null() {
            // SAFETY: the pointer is the `&tap` stored in `run_tap_thread`,
            // which outlives this callback.
            unsafe { CGEvent::tap_enable(&*tap, true) };
        }
        return std::ptr::null_mut();
    }

    if !matches!(
        event_type,
        CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged
    ) {
        return event.as_ptr();
    }

    let enabled = state.enabled.load(Ordering::SeqCst);
    // SAFETY: the tap delivers a valid non-null CGEvent for keyboard events.
    let event_ref = unsafe { event.as_ref() };
    let keycode =
        CGEvent::integer_value_field(Some(event_ref), CGEventField::KeyboardEventKeycode) as u32;
    let flags = CGEvent::flags(Some(event_ref));
    let autorepeat =
        CGEvent::integer_value_field(Some(event_ref), CGEventField::KeyboardEventAutorepeat) != 0;
    let timestamp = CGEvent::timestamp(Some(event_ref));

    let (kind, is_modifier) = classify_keycode(keycode);
    let pressed = match event_type {
        CGEventType::FlagsChanged => modifier_is_pressed(keycode, flags),
        CGEventType::KeyDown => true,
        _ => false,
    };

    let suppress = enabled
        && match kind {
            // Block the Command key itself: this also covers every Command
            // combination without having to know the other key in advance.
            KeyKind::WinLeft | KeyKind::WinRight => is_modifier,
            KeyKind::PrintScreen => true,
            KeyKind::Tab => flags.contains(CGEventFlags::MaskCommand),
            KeyKind::Escape => {
                flags.contains(CGEventFlags::MaskCommand)
                    || flags.contains(CGEventFlags::MaskControl)
            }
            _ => false,
        };

    if suppress && !matches!(kind, KeyKind::Other) {
        let modifiers = modifiers_from_flags(flags);
        let keysym = keysym_for_kind(kind);
        let raw = RawKeyEvent {
            pressed,
            repeat: autorepeat,
            kind,
            key_code: keycode,
            scan_code: keycode,
            flags: flags.bits() as u32,
            keysym,
            modifiers,
            timestamp,
            platform: PlatformKeyInfo::MacOs {
                flags64: flags.bits(),
                unicode: None,
                autorepeat,
            },
        };
        if let Ok(sender) = state.sender.lock()
            && let Some(sender) = sender.as_ref()
        {
            let _ = sender.try_send(HookEvent::Key(raw));
        }
    }

    if suppress {
        std::ptr::null_mut()
    } else {
        event.as_ptr()
    }
}

fn classify_keycode(keycode: u32) -> (KeyKind, bool) {
    let (kind, is_modifier) = match keycode {
        keycodes::COMMAND_LEFT => (KeyKind::WinLeft, true),
        keycodes::COMMAND_RIGHT => (KeyKind::WinRight, true),
        keycodes::SHIFT => (KeyKind::ShiftLeft, true),
        keycodes::RIGHT_SHIFT => (KeyKind::ShiftRight, true),
        keycodes::CONTROL => (KeyKind::CtrlLeft, true),
        keycodes::RIGHT_CONTROL => (KeyKind::CtrlRight, true),
        keycodes::OPTION => (KeyKind::AltLeft, true),
        keycodes::RIGHT_OPTION => (KeyKind::AltRight, true),
        keycodes::TAB => (KeyKind::Tab, false),
        keycodes::ESCAPE => (KeyKind::Escape, false),
        keycodes::F13 => (KeyKind::PrintScreen, false),
        _ => (KeyKind::Other, false),
    };
    (kind, is_modifier)
}

fn modifier_is_pressed(keycode: u32, flags: CGEventFlags) -> bool {
    match keycode {
        keycodes::COMMAND_LEFT | keycodes::COMMAND_RIGHT => {
            flags.contains(CGEventFlags::MaskCommand)
        }
        keycodes::SHIFT | keycodes::RIGHT_SHIFT => flags.contains(CGEventFlags::MaskShift),
        keycodes::CONTROL | keycodes::RIGHT_CONTROL => {
            flags.contains(CGEventFlags::MaskControl)
        }
        keycodes::OPTION | keycodes::RIGHT_OPTION => {
            flags.contains(CGEventFlags::MaskAlternate)
        }
        _ => false,
    }
}

fn modifiers_from_flags(flags: CGEventFlags) -> Modifiers {
    Modifiers {
        ctrl: flags.contains(CGEventFlags::MaskControl),
        alt: flags.contains(CGEventFlags::MaskAlternate),
        shift: flags.contains(CGEventFlags::MaskShift),
        meta: flags.contains(CGEventFlags::MaskCommand),
        caps_lock: flags.contains(CGEventFlags::MaskAlphaShift),
        num_lock: false,
    }
}

/// Keysyms follow the convention used by the remote input layer: Command maps
/// to the Alt keysyms, Option to the Meta keysyms (matching ARD's RFB
/// behavior), Super keysyms are not used.
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
    fn classifies_commands_tabs_escapes_and_print_screen() {
        assert_eq!(classify_keycode(keycodes::COMMAND_LEFT), (KeyKind::WinLeft, true));
        assert_eq!(
            classify_keycode(keycodes::COMMAND_RIGHT),
            (KeyKind::WinRight, true)
        );
        assert_eq!(classify_keycode(keycodes::TAB), (KeyKind::Tab, false));
        assert_eq!(classify_keycode(keycodes::ESCAPE), (KeyKind::Escape, false));
        assert_eq!(
            classify_keycode(keycodes::F13),
            (KeyKind::PrintScreen, false)
        );
        assert_eq!(classify_keycode(0x2a), (KeyKind::Other, false));
    }

    #[test]
    fn command_flags_change_tracks_press_and_release() {
        let down = CGEventFlags::MaskCommand | CGEventFlags::MaskShift;
        assert!(modifier_is_pressed(keycodes::COMMAND_LEFT, down));
        assert!(!modifier_is_pressed(keycodes::COMMAND_RIGHT, CGEventFlags::MaskShift));
        assert!(modifier_is_pressed(keycodes::SHIFT, down));
    }

    #[test]
    fn flags_map_to_normalized_modifiers() {
        let flags = CGEventFlags::MaskControl
            | CGEventFlags::MaskAlternate
            | CGEventFlags::MaskCommand
            | CGEventFlags::MaskAlphaShift;
        let modifiers = modifiers_from_flags(flags);
        assert!(modifiers.ctrl && modifiers.alt && modifiers.meta && modifiers.caps_lock);
        assert!(!modifiers.shift && !modifiers.num_lock);
    }

    #[test]
    fn intercepted_keys_always_resolve_to_a_keysym() {
        for kind in [
            KeyKind::WinLeft,
            KeyKind::WinRight,
            KeyKind::AltLeft,
            KeyKind::AltRight,
            KeyKind::CtrlLeft,
            KeyKind::CtrlRight,
            KeyKind::ShiftLeft,
            KeyKind::ShiftRight,
            KeyKind::PrintScreen,
            KeyKind::Tab,
            KeyKind::Escape,
        ] {
            assert!(keysym_for_kind(kind).is_some());
        }
        assert_eq!(keysym_for_kind(KeyKind::Other), None);
    }
}
