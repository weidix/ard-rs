//! Windows backend based on a low-level keyboard hook
//! (`WH_KEYBOARD_LL` / `SetWindowsHookExW`).
//!
//! The hook is installed on a dedicated thread that runs a message pump; the
//! hook procedure is invoked on that thread.  The procedure only parses the
//! key event, updates modifier state, decides whether the event must be
//! suppressed, and pushes a [`RawKeyEvent`] into a non-blocking channel.
//!
//! While enabled the hook suppresses:
//!
//! - the Win key itself (left and right), which prevents the Start menu and
//!   every `Win+...` combination from acting locally;
//! - `PrintScreen`;
//! - `Alt+Tab`, `Alt+Esc` and `Ctrl+Esc`.
//!
//! All other keys are passed on to the system and reach the application
//! through its normal window input path.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VIRTUAL_KEY, VK_CAPITAL, VK_CONTROL, VK_ESCAPE, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN,
    VK_MENU, VK_NUMLOCK, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SCROLL, VK_SHIFT,
    VK_SNAPSHOT, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED,
    LLKHF_INJECTED, MSG, PostThreadMessageW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::common::{
    HookConfig, HookError, HookEvent, KeyKind, Modifiers, PlatformKeyInfo, RawKeyEvent,
};
use crate::keysyms;

const MOD_CTRL_LEFT: u16 = 1 << 0;
const MOD_CTRL_RIGHT: u16 = 1 << 1;
const MOD_ALT_LEFT: u16 = 1 << 2;
const MOD_ALT_RIGHT: u16 = 1 << 3;
const MOD_SHIFT_LEFT: u16 = 1 << 4;
const MOD_SHIFT_RIGHT: u16 = 1 << 5;
const MOD_WIN_LEFT: u16 = 1 << 6;
const MOD_WIN_RIGHT: u16 = 1 << 7;
const MOD_CAPS_LOCK: u16 = 1 << 8;
const MOD_NUM_LOCK: u16 = 1 << 9;
const MOD_SCROLL_LOCK: u16 = 1 << 10;

// Only one process-wide hook is supported (the viewer keeps a single
// KeyboardHook), so module-level statics are the natural home for the state
// the `extern "system"` callback needs.
static SENDER: Mutex<Option<SyncSender<HookEvent>>> = Mutex::new(None);
static ENABLED: AtomicBool = AtomicBool::new(false);
static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static MODIFIER_STATE: AtomicU16 = AtomicU16::new(0);
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// Handle that owns the hook thread.
pub struct WindowsHook {
    thread: Option<JoinHandle<()>>,
}

impl WindowsHook {
    pub fn start(config: HookConfig) -> Result<(Self, Receiver<HookEvent>), HookError> {
        let (sender, receiver) = sync_channel(256);
        *SENDER
            .lock()
            .map_err(|_| HookError::Other("Windows hook state poisoned".into()))? = Some(sender);
        ENABLED.store(config.capture_enabled, Ordering::SeqCst);
        SHUTDOWN.store(false, Ordering::SeqCst);
        MODIFIER_STATE.store(0, Ordering::SeqCst);
        HOOK_THREAD_ID.store(0, Ordering::SeqCst);

        let thread = thread::Builder::new()
            .name("ard-input-hook-windows".into())
            .spawn(hook_thread_main)
            .map_err(|error| HookError::Io(format!("cannot start Windows hook thread: {error}")))?;

        // Wait until the message pump is live so Drop can reliably post
        // WM_QUIT to it.
        let deadline = Instant::now() + Duration::from_secs(2);
        while HOOK_THREAD_ID.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        if HOOK_THREAD_ID.load(Ordering::SeqCst) == 0 {
            SHUTDOWN.store(true, Ordering::SeqCst);
            let _ = thread.join();
            return Err(HookError::Io(
                "Windows hook thread did not start its message pump".into(),
            ));
        }
        Ok((Self { thread: Some(thread) }, receiver))
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<(), HookError> {
        ENABLED.store(enabled, Ordering::SeqCst);
        Ok(())
    }
}

impl Drop for WindowsHook {
    fn drop(&mut self) {
        SHUTDOWN.store(true, Ordering::SeqCst);
        ENABLED.store(false, Ordering::SeqCst);
        let thread_id = HOOK_THREAD_ID.load(Ordering::SeqCst);
        if thread_id != 0 {
            // SAFETY: PostThreadMessageW only needs a valid thread id.
            unsafe {
                let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn hook_thread_main() {
    // SAFETY: native Windows calls; the hook procedure and message pump live
    // entirely inside this thread.
    unsafe {
        let module = GetModuleHandleW(None).ok().map(|handle| HINSTANCE(handle.0));
        match SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), module, 0) {
            Ok(hook) => {
                HOOK_THREAD_ID.store(GetCurrentThreadId(), Ordering::SeqCst);
                let mut message = MSG::default();
                while !SHUTDOWN.load(Ordering::SeqCst)
                    && GetMessageW(&mut message, None, 0, 0).as_bool()
                {
                    let _ = TranslateMessage(&message);
                    let _ = DispatchMessageW(&message);
                }
                let _ = UnhookWindowsHookEx(hook);
            }
            Err(error) => {
                if let Ok(sender) = SENDER.lock()
                    && let Some(sender) = sender.as_ref()
                {
                    let _ = sender.try_send(HookEvent::Error(HookError::Io(format!(
                        "SetWindowsHookExW failed: {error}"
                    ))));
                }
            }
        }
        HOOK_THREAD_ID.store(0, Ordering::SeqCst);
        if let Ok(mut sender) = SENDER.lock() {
            *sender = None;
        }
    }
}

unsafe extern "system" fn low_level_keyboard_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if n_code != HC_ACTION as i32 {
        // SAFETY: chaining the hook is the required behavior for nCode < 0 or
        // non-action codes.
        return unsafe { CallNextHookEx(None, n_code, w_param, l_param) };
    }
    // SAFETY: for HC_ACTION with WH_KEYBOARD_LL, lParam points to a
    // KBDLLHOOKSTRUCT for the lifetime of the call.
    let info = unsafe { &*(l_param.0 as *const KBDLLHOOKSTRUCT) };
    let message = w_param.0 as u32;
    let pressed = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
    let released = matches!(message, WM_KEYUP | WM_SYSKEYUP);
    let mut suppress = false;
    if pressed || released {
    let flags = info.flags.0;
    let kind = classify_key(info.vkCode, flags);
        update_modifier_state(kind, info.vkCode, pressed, released);
        if ENABLED.load(Ordering::SeqCst) {
            suppress = should_suppress(kind);
            if suppress {
                let raw = RawKeyEvent {
                    pressed,
                    repeat: false,
                    kind,
                    key_code: info.vkCode,
                    scan_code: info.scanCode,
                    flags,
                    keysym: keysym_for_kind(kind),
                    modifiers: current_modifiers(),
                    timestamp: u64::from(info.time),
                    platform: PlatformKeyInfo::Windows {
                        extra_info: info.dwExtraInfo,
                        injected: flags & LLKHF_INJECTED.0 != 0,
                    },
                };
                if let Ok(sender) = SENDER.lock()
                    && let Some(sender) = sender.as_ref()
                {
                    let _ = sender.try_send(HookEvent::Key(raw));
                }
            }
        }
    }
    if suppress {
        // Returning a non-zero value blocks the event from reaching the
        // system, so the Win key and the captured combos never act locally.
        LRESULT(1)
    } else {
        // SAFETY: standard hook chaining call.
        unsafe { CallNextHookEx(None, n_code, w_param, l_param) }
    }
}

fn classify_key(vk_code: u32, flags: u32) -> KeyKind {
    let vk = VIRTUAL_KEY(vk_code as u16);
    let extended = flags & LLKHF_EXTENDED.0 != 0;
    match vk {
        VK_LWIN => KeyKind::WinLeft,
        VK_RWIN => KeyKind::WinRight,
        VK_SNAPSHOT => KeyKind::PrintScreen,
        VK_TAB => KeyKind::Tab,
        VK_ESCAPE => KeyKind::Escape,
        VK_LCONTROL => KeyKind::CtrlLeft,
        VK_RCONTROL => KeyKind::CtrlRight,
        VK_CONTROL if extended => KeyKind::CtrlRight,
        VK_CONTROL => KeyKind::CtrlLeft,
        VK_LSHIFT => KeyKind::ShiftLeft,
        VK_RSHIFT => KeyKind::ShiftRight,
        VK_SHIFT if extended => KeyKind::ShiftRight,
        VK_SHIFT => KeyKind::ShiftLeft,
        VK_LMENU => KeyKind::AltLeft,
        VK_RMENU => KeyKind::AltRight,
        VK_MENU if extended => KeyKind::AltRight,
        VK_MENU => KeyKind::AltLeft,
        _ => KeyKind::Other,
    }
}

fn update_modifier_state(kind: KeyKind, vk_code: u32, pressed: bool, released: bool) {
    let mut bits = MODIFIER_STATE.load(Ordering::SeqCst);
    let bit = match kind {
        KeyKind::CtrlLeft => MOD_CTRL_LEFT,
        KeyKind::CtrlRight => MOD_CTRL_RIGHT,
        KeyKind::AltLeft => MOD_ALT_LEFT,
        KeyKind::AltRight => MOD_ALT_RIGHT,
        KeyKind::ShiftLeft => MOD_SHIFT_LEFT,
        KeyKind::ShiftRight => MOD_SHIFT_RIGHT,
        KeyKind::WinLeft => MOD_WIN_LEFT,
        KeyKind::WinRight => MOD_WIN_RIGHT,
        KeyKind::Other => 0,
        _ => 0,
    };
    if bit != 0 {
        if pressed {
            bits |= bit;
        } else if released {
            bits &= !bit;
        }
    } else if pressed {
        // Lock keys toggle on transition.
        if vk_code == VK_CAPITAL.0 as u32 {
            bits ^= MOD_CAPS_LOCK;
        } else if vk_code == VK_NUMLOCK.0 as u32 {
            bits ^= MOD_NUM_LOCK;
        } else if vk_code == VK_SCROLL.0 as u32 {
            bits ^= MOD_SCROLL_LOCK;
        }
    }
    MODIFIER_STATE.store(bits, Ordering::SeqCst);
}

fn current_modifiers() -> Modifiers {
    let bits = MODIFIER_STATE.load(Ordering::SeqCst);
    Modifiers {
        ctrl: bits & (MOD_CTRL_LEFT | MOD_CTRL_RIGHT) != 0,
        alt: bits & (MOD_ALT_LEFT | MOD_ALT_RIGHT) != 0,
        shift: bits & (MOD_SHIFT_LEFT | MOD_SHIFT_RIGHT) != 0,
        meta: bits & (MOD_WIN_LEFT | MOD_WIN_RIGHT) != 0,
        caps_lock: bits & MOD_CAPS_LOCK != 0,
        num_lock: bits & MOD_NUM_LOCK != 0,
    }
}

fn should_suppress(kind: KeyKind) -> bool {
    let bits = MODIFIER_STATE.load(Ordering::SeqCst);
    let alt = bits & (MOD_ALT_LEFT | MOD_ALT_RIGHT) != 0;
    let ctrl = bits & (MOD_CTRL_LEFT | MOD_CTRL_RIGHT) != 0;
    match kind {
        KeyKind::WinLeft | KeyKind::WinRight | KeyKind::PrintScreen => true,
        KeyKind::Tab => alt,
        KeyKind::Escape => alt || ctrl,
        _ => false,
    }
}

/// Keysyms follow the convention used by the remote input layer: the Win key
/// maps to the Alt keysyms (matching ARD's RFB behavior for Command), Alt to
/// the Meta keysyms.
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
    fn classifies_windows_keys_and_extended_modifiers() {
        let plain = 0;
        let extended = LLKHF_EXTENDED.0;
        assert_eq!(classify_key(VK_LWIN.0 as u32, plain), KeyKind::WinLeft);
        assert_eq!(classify_key(VK_RWIN.0 as u32, plain), KeyKind::WinRight);
        assert_eq!(classify_key(VK_SNAPSHOT.0 as u32, plain), KeyKind::PrintScreen);
        assert_eq!(classify_key(VK_TAB.0 as u32, plain), KeyKind::Tab);
        assert_eq!(classify_key(VK_ESCAPE.0 as u32, plain), KeyKind::Escape);
        assert_eq!(classify_key(VK_CONTROL.0 as u32, plain), KeyKind::CtrlLeft);
        assert_eq!(classify_key(VK_CONTROL.0 as u32, extended), KeyKind::CtrlRight);
        assert_eq!(classify_key(VK_MENU.0 as u32, plain), KeyKind::AltLeft);
        assert_eq!(classify_key(VK_MENU.0 as u32, extended), KeyKind::AltRight);
    }

    #[test]
    fn suppression_rules_cover_win_printscreen_and_alt_ctrl_combos() {
        MODIFIER_STATE.store(0, Ordering::SeqCst);
        assert!(should_suppress(KeyKind::WinLeft));
        assert!(should_suppress(KeyKind::WinRight));
        assert!(should_suppress(KeyKind::PrintScreen));
        assert!(!should_suppress(KeyKind::Tab));
        assert!(!should_suppress(KeyKind::Escape));

        MODIFIER_STATE.store(MOD_ALT_LEFT, Ordering::SeqCst);
        assert!(should_suppress(KeyKind::Tab));
        assert!(should_suppress(KeyKind::Escape));

        MODIFIER_STATE.store(MOD_CTRL_RIGHT, Ordering::SeqCst);
        assert!(!should_suppress(KeyKind::Tab));
        assert!(should_suppress(KeyKind::Escape));
        MODIFIER_STATE.store(0, Ordering::SeqCst);
    }

    #[test]
    fn intercepted_keys_always_resolve_to_a_keysym() {
        assert_eq!(keysym_for_kind(KeyKind::WinLeft), Some(keysyms::XK_ALT_LEFT));
        assert_eq!(
            keysym_for_kind(KeyKind::PrintScreen),
            Some(keysyms::XK_PRINT)
        );
        assert_eq!(keysym_for_kind(KeyKind::Other), None);
    }
}
