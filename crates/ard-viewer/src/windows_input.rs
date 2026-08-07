#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Mutex, OnceLock};
use std::thread;

use ard_rs::{ArdKey, ArdNamedKey, XK_KP_SEPARATOR, keysym_for_key, unicode_keysym};
use windows_sys::Win32::Foundation::{GetLastError, HWND};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayout, GetKeyboardState, ToUnicodeEx, VK_ADD, VK_APPS, VK_BACK, VK_CANCEL,
    VK_CAPITAL, VK_CLEAR, VK_CONTROL, VK_DECIMAL, VK_DELETE, VK_DIVIDE, VK_DOWN, VK_END, VK_ESCAPE,
    VK_F1, VK_F24, VK_HOME, VK_INSERT, VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU,
    VK_MULTIPLY, VK_NEXT, VK_NUMLOCK, VK_NUMPAD0, VK_NUMPAD9, VK_PACKET, VK_PAUSE, VK_PRIOR,
    VK_PROCESSKEY, VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SCROLL,
    VK_SEPARATOR, VK_SHIFT, VK_SNAPSHOT, VK_SPACE, VK_SUBTRACT, VK_TAB, VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GA_ROOT, GetAncestor, GetForegroundWindow, GetMessageW,
    GetWindowThreadProcessId, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED,
    MSG, PM_NOREMOVE, PM_NOYIELD, PeekMessageW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::session_runtime::InputCommand;

const LEFT_WIN: u8 = 0x01;
const RIGHT_WIN: u8 = 0x02;
const TO_UNICODE_DO_NOT_CHANGE_STATE: u32 = 0x04;

struct KeyboardState {
    keys: [u8; 256],
    initialized: bool,
    remote_pressed: HashMap<u32, u32>,
}

impl Default for KeyboardState {
    fn default() -> Self {
        Self {
            keys: [0; 256],
            initialized: false,
            remote_pressed: HashMap::new(),
        }
    }
}

struct Interceptor {
    session_hwnd: AtomicUsize,
    input_ready: AtomicBool,
    ime_active: AtomicBool,
    capture_system_shortcuts: AtomicBool,
    hook_started: AtomicBool,
    hook_installed: AtomicBool,
    blocked_win_keys: AtomicU8,
    focus_active: AtomicBool,
    dispatch_error_reported: AtomicBool,
    commands: Mutex<Option<SyncSender<InputCommand>>>,
    keyboard: Mutex<KeyboardState>,
    last_error: Mutex<Option<String>>,
}

static INTERCEPTOR: OnceLock<Interceptor> = OnceLock::new();

pub fn install(commands: SyncSender<InputCommand>) {
    let interceptor = interceptor();
    interceptor.input_ready.store(false, Ordering::Release);
    release_remote_keys(interceptor);
    if let Ok(mut current) = interceptor.commands.lock() {
        *current = Some(commands);
    }
    interceptor
        .dispatch_error_reported
        .store(false, Ordering::Release);
}

pub fn set_session_window(hwnd: Option<usize>) {
    let interceptor = interceptor();
    interceptor.session_hwnd.store(0, Ordering::Release);
    interceptor.focus_active.store(false, Ordering::Release);
    interceptor.ime_active.store(false, Ordering::Release);
    release_remote_keys(interceptor);
    interceptor
        .session_hwnd
        .store(hwnd.unwrap_or_default(), Ordering::Release);
    if let Some(hwnd) = hwnd {
        ensure_hook(interceptor);
        eprintln!("Windows remote session window registered: HWND 0x{hwnd:x}");
    }
}

pub fn set_input_ready(ready: bool) {
    let interceptor = interceptor();
    if ready {
        interceptor.input_ready.store(true, Ordering::Release);
    } else {
        interceptor.input_ready.store(false, Ordering::Release);
        interceptor.focus_active.store(false, Ordering::Release);
        interceptor.ime_active.store(false, Ordering::Release);
        release_remote_keys(interceptor);
    }
}

pub fn set_capture_system_shortcuts(capture: bool) {
    let Some(interceptor) = INTERCEPTOR.get() else {
        return;
    };
    let changed = interceptor
        .capture_system_shortcuts
        .swap(capture, Ordering::AcqRel)
        != capture;
    if changed {
        release_remote_keys_preserving_win(interceptor);
    }
}

pub fn set_session_focused(focused: bool) {
    if !focused {
        let Some(interceptor) = INTERCEPTOR.get() else {
            return;
        };
        interceptor.focus_active.store(false, Ordering::Release);
        interceptor.ime_active.store(false, Ordering::Release);
        release_remote_keys(interceptor);
    }
}

pub fn set_ime_active(active: bool) {
    let interceptor = interceptor();
    if active && !interceptor.ime_active.swap(true, Ordering::AcqRel) {
        release_remote_keys_preserving_win(interceptor);
    } else if !active {
        interceptor.ime_active.store(false, Ordering::Release);
    }
}

pub fn take_error() -> Option<String> {
    INTERCEPTOR
        .get()
        .and_then(|interceptor| interceptor.last_error.lock().ok())
        .and_then(|mut error| error.take())
}

fn interceptor() -> &'static Interceptor {
    INTERCEPTOR.get_or_init(|| Interceptor {
        session_hwnd: AtomicUsize::new(0),
        input_ready: AtomicBool::new(false),
        ime_active: AtomicBool::new(false),
        capture_system_shortcuts: AtomicBool::new(false),
        hook_started: AtomicBool::new(false),
        hook_installed: AtomicBool::new(false),
        blocked_win_keys: AtomicU8::new(0),
        focus_active: AtomicBool::new(false),
        dispatch_error_reported: AtomicBool::new(false),
        commands: Mutex::new(None),
        keyboard: Mutex::new(KeyboardState::default()),
        last_error: Mutex::new(None),
    })
}

fn ensure_hook(interceptor: &'static Interceptor) {
    if interceptor
        .hook_started
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    let (ready_tx, ready_rx) = sync_channel(1);
    if let Err(error) = thread::Builder::new()
        .name("ard-win-key-hook".into())
        .spawn(move || run_hook(ready_tx))
    {
        interceptor.hook_started.store(false, Ordering::Release);
        record_error(
            interceptor,
            format!("failed to start Windows key hook thread: {error}"),
        );
        return;
    }

    match ready_rx.recv() {
        Ok(Ok(())) => {
            interceptor.hook_installed.store(true, Ordering::Release);
            eprintln!("Windows low-level keyboard hook installed");
        }
        Ok(Err(error)) => {
            interceptor.hook_started.store(false, Ordering::Release);
            record_error(interceptor, error);
        }
        Err(_) => {
            interceptor.hook_started.store(false, Ordering::Release);
            record_error(
                interceptor,
                "Windows key hook thread stopped during initialization".to_owned(),
            );
        }
    }
}

struct Hook(HHOOK);

impl Drop for Hook {
    fn drop(&mut self) {
        // SAFETY: The handle is returned by SetWindowsHookExW and owned by
        // this guard until the hook message loop exits.
        let result = unsafe { UnhookWindowsHookEx(self.0) };
        if result == 0 {
            // SAFETY: GetLastError has no preconditions.
            let error = unsafe { GetLastError() };
            eprintln!("failed to uninstall Windows key hook: error {error}");
        }
    }
}

fn run_hook(ready: SyncSender<Result<(), String>>) {
    let mut message = MSG::default();
    // A low-level hook is delivered through the installing thread's message
    // queue. Creating it explicitly also matches TigerVNC's Win32 client.
    // SAFETY: `message` is a valid writable MSG and the null HWND requests
    // this thread's queue.
    unsafe {
        PeekMessageW(
            &mut message,
            ptr::null_mut(),
            0,
            0,
            PM_NOREMOVE | PM_NOYIELD,
        );
    }

    // SAFETY: The callback has the required system ABI and static lifetime;
    // the module handle belongs to this process.
    let hook = unsafe {
        let module = GetModuleHandleW(ptr::null());
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), module, 0)
    };
    if hook.is_null() {
        // SAFETY: GetLastError is read immediately after the failed call.
        let error = unsafe { GetLastError() };
        let _ = ready.send(Err(format!(
            "failed to install Windows key hook: error {error}"
        )));
        return;
    }
    let _hook = Hook(hook);
    let _ = ready.send(Ok(()));

    loop {
        // SAFETY: GetMessageW initializes `message` before it is dispatched.
        let result = unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) };
        if result == -1 {
            // SAFETY: GetLastError is read immediately after GetMessageW.
            let error = unsafe { GetLastError() };
            if let Some(interceptor) = INTERCEPTOR.get() {
                interceptor.hook_installed.store(false, Ordering::Release);
                record_error(
                    interceptor,
                    format!("Windows key hook message loop failed: error {error}"),
                );
            }
            break;
        }
        if result == 0 {
            break;
        }
        // SAFETY: `message` was initialized by GetMessageW for this loop.
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    if let Some(interceptor) = INTERCEPTOR.get() {
        interceptor.hook_installed.store(false, Ordering::Release);
        interceptor.hook_started.store(false, Ordering::Release);
        interceptor.focus_active.store(false, Ordering::Release);
        release_remote_keys(interceptor);
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: usize, lparam: isize) -> isize {
    if code != HC_ACTION as i32 || lparam == 0 {
        return call_next(code, wparam, lparam);
    }
    // SAFETY: WH_KEYBOARD_LL documents lParam as a KBDLLHOOKSTRUCT for
    // HC_ACTION callbacks, and the pointer is valid for this callback.
    let event = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
    let pressed = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
    let released = matches!(wparam as u32, WM_KEYUP | WM_SYSKEYUP);
    if !pressed && !released {
        return call_next(code, wparam, lparam);
    }
    let Some(interceptor) = INTERCEPTOR.get() else {
        return call_next(code, wparam, lparam);
    };
    let injected = event.flags & LLKHF_INJECTED != 0;
    let win_bit = win_key_bit(event.vkCode);

    // A Win key was already consumed while the session had focus. If focus
    // changed before its release, consume that release as well so the shell
    // cannot open the Start menu after the remote window loses focus.
    if released && !injected && win_bit != 0 {
        let blocked = interceptor
            .blocked_win_keys
            .fetch_and(!win_bit, Ordering::AcqRel);
        if blocked & win_bit != 0 && !session_is_foreground(interceptor) {
            interceptor.focus_active.store(false, Ordering::Release);
            release_remote_keys(interceptor);
            return 1;
        }
    }

    if !session_is_foreground(interceptor) {
        if interceptor.focus_active.swap(false, Ordering::AcqRel) {
            release_remote_keys(interceptor);
        }
        return call_next(code, wparam, lparam);
    }
    interceptor.focus_active.store(true, Ordering::Release);

    if pressed && !injected && win_bit != 0 {
        interceptor
            .blocked_win_keys
            .fetch_or(win_bit, Ordering::AcqRel);
    }

    let capture = interceptor.capture_system_shortcuts.load(Ordering::Acquire);
    let ime_active = interceptor.ime_active.load(Ordering::Acquire);
    let consumed = if injected {
        false
    } else {
        process_key_event(interceptor, event, pressed, capture, ime_active)
    };
    if consumed {
        1
    } else {
        call_next(code, wparam, lparam)
    }
}

fn process_key_event(
    interceptor: &Interceptor,
    event: &KBDLLHOOKSTRUCT,
    pressed: bool,
    capture: bool,
    ime_active: bool,
) -> bool {
    let Ok(mut keyboard) = interceptor.keyboard.lock() else {
        record_error(
            interceptor,
            "Windows keyboard state lock was poisoned".to_owned(),
        );
        return false;
    };
    initialize_keyboard_state(interceptor, &mut keyboard);
    let key_id = key_id(event);
    let normalized_vk = normalized_vk(event);
    update_keyboard_state(&mut keyboard.keys, normalized_vk, pressed);

    let win_chord =
        win_key_bit(event.vkCode) != 0 || interceptor.blocked_win_keys.load(Ordering::Acquire) != 0;
    let tracked = keyboard.remote_pressed.contains_key(&key_id);
    let direct = should_route_direct(tracked, win_chord, capture, ime_active, event.vkCode);
    if !direct {
        return false;
    }

    if pressed {
        let keysym = keyboard
            .remote_pressed
            .get(&key_id)
            .copied()
            .or_else(|| native_keysym(event, &keyboard.keys));
        if let Some(keysym) = keysym
            && queue_key(interceptor, true, keysym)
        {
            keyboard.remote_pressed.insert(key_id, keysym);
        }
    } else if let Some(&keysym) = keyboard.remote_pressed.get(&key_id)
        && queue_key(interceptor, false, keysym)
    {
        keyboard.remote_pressed.remove(&key_id);
    }
    true
}

fn initialize_keyboard_state(interceptor: &Interceptor, keyboard: &mut KeyboardState) {
    if keyboard.initialized {
        return;
    }
    // SAFETY: The array is exactly the 256-byte buffer required by Win32.
    if unsafe { GetKeyboardState(keyboard.keys.as_mut_ptr()) } == 0 {
        // SAFETY: GetLastError has no preconditions.
        let error = unsafe { GetLastError() };
        record_error(
            interceptor,
            format!("failed to read Windows keyboard state: error {error}"),
        );
        keyboard.keys.fill(0);
    }
    keyboard.initialized = true;
}

fn release_remote_keys(interceptor: &Interceptor) {
    release_remote_keys_inner(interceptor, false);
}

fn release_remote_keys_preserving_win(interceptor: &Interceptor) {
    release_remote_keys_inner(interceptor, true);
}

fn release_remote_keys_inner(interceptor: &Interceptor, preserve_win: bool) {
    let Ok(mut keyboard) = interceptor.keyboard.lock() else {
        record_error(
            interceptor,
            "Windows keyboard state lock was poisoned".to_owned(),
        );
        return;
    };
    let mut pressed = Vec::new();
    let mut preserved = HashMap::new();
    for (key_id, keysym) in std::mem::take(&mut keyboard.remote_pressed) {
        if preserve_win && win_key_bit(key_id >> 16) != 0 {
            preserved.insert(key_id, keysym);
        } else {
            pressed.push((key_id, keysym));
        }
    }
    // Release ordinary keys before modifiers, matching the order used by the
    // local InputState cleanup path.
    pressed.sort_by_key(|(key_id, _)| is_modifier_vk(*key_id >> 16));
    let mut failed = Vec::new();
    for (key_id, keysym) in pressed {
        if !queue_key(interceptor, false, keysym) {
            failed.push((key_id, keysym));
        }
    }
    preserved.extend(failed);
    keyboard.remote_pressed = preserved;
    keyboard.keys.fill(0);
    keyboard.initialized = false;
}

fn queue_key(interceptor: &Interceptor, pressed: bool, keysym: u32) -> bool {
    let sender = interceptor
        .commands
        .lock()
        .ok()
        .and_then(|commands| commands.clone());
    let Some(sender) = sender else {
        report_dispatch_error(interceptor, "remote input dispatcher is unavailable");
        return false;
    };
    match sender.try_send(InputCommand::Key { pressed, keysym }) {
        Ok(()) => {
            interceptor
                .dispatch_error_reported
                .store(false, Ordering::Release);
            true
        }
        Err(TrySendError::Full(_)) => {
            report_dispatch_error(interceptor, "remote input queue is full");
            false
        }
        Err(TrySendError::Disconnected(_)) => {
            report_dispatch_error(interceptor, "remote input dispatcher has stopped");
            false
        }
    }
}

fn report_dispatch_error(interceptor: &Interceptor, message: &str) {
    if !interceptor
        .dispatch_error_reported
        .swap(true, Ordering::AcqRel)
    {
        record_error(
            interceptor,
            format!("Windows key capture failed: {message}"),
        );
    }
}

fn record_error(interceptor: &Interceptor, message: String) {
    if let Ok(mut current) = interceptor.last_error.lock() {
        *current = Some(message);
    }
}

fn session_is_foreground(interceptor: &Interceptor) -> bool {
    if !interceptor.input_ready.load(Ordering::Acquire) {
        return false;
    }
    let session = interceptor.session_hwnd.load(Ordering::Acquire) as HWND;
    if session.is_null() {
        return false;
    }
    // SAFETY: HWND values are compared opaquely; neither API dereferences
    // caller-owned memory.
    unsafe {
        let foreground = GetForegroundWindow();
        !foreground.is_null() && root_window(session) == root_window(foreground)
    }
}

unsafe fn root_window(hwnd: HWND) -> HWND {
    // SAFETY: GetAncestor accepts an HWND and returns another opaque handle.
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    if root.is_null() { hwnd } else { root }
}

fn native_keysym(event: &KBDLLHOOKSTRUCT, keyboard: &[u8; 256]) -> Option<u32> {
    named_keysym(event).or_else(|| translated_keysym(event, keyboard))
}

fn named_keysym(event: &KBDLLHOOKSTRUCT) -> Option<u32> {
    let vk = event.vkCode;
    if (u32::from(VK_F1)..=u32::from(VK_F24)).contains(&vk) {
        return keysym_for_key(ArdKey::Named(ArdNamedKey::Function(
            (vk - u32::from(VK_F1) + 1) as u8,
        )));
    }
    if (u32::from(VK_NUMPAD0)..=u32::from(VK_NUMPAD9)).contains(&vk) {
        return keysym_for_key(ArdKey::Named(ArdNamedKey::Numpad(
            (vk - u32::from(VK_NUMPAD0)) as u8,
        )));
    }
    if vk == u32::from(VK_SEPARATOR) {
        return Some(XK_KP_SEPARATOR);
    }
    let extended = event.flags & LLKHF_EXTENDED != 0;
    let named = if vk == u32::from(VK_BACK) {
        ArdNamedKey::Backspace
    } else if vk == u32::from(VK_TAB) {
        ArdNamedKey::Tab
    } else if vk == u32::from(VK_RETURN) {
        if extended {
            ArdNamedKey::NumpadEnter
        } else {
            ArdNamedKey::Enter
        }
    } else if vk == u32::from(VK_CLEAR) {
        ArdNamedKey::Numpad(5)
    } else if vk == u32::from(VK_CANCEL) || vk == u32::from(VK_PAUSE) {
        ArdNamedKey::Pause
    } else if vk == u32::from(VK_CAPITAL) {
        ArdNamedKey::CapsLock
    } else if vk == u32::from(VK_ESCAPE) {
        ArdNamedKey::Escape
    } else if vk == u32::from(VK_SPACE) {
        ArdNamedKey::Space
    } else if vk == u32::from(VK_PRIOR) {
        ArdNamedKey::PageUp
    } else if vk == u32::from(VK_NEXT) {
        ArdNamedKey::PageDown
    } else if vk == u32::from(VK_END) {
        ArdNamedKey::End
    } else if vk == u32::from(VK_HOME) {
        ArdNamedKey::Home
    } else if vk == u32::from(VK_LEFT) {
        ArdNamedKey::ArrowLeft
    } else if vk == u32::from(VK_UP) {
        ArdNamedKey::ArrowUp
    } else if vk == u32::from(VK_RIGHT) {
        ArdNamedKey::ArrowRight
    } else if vk == u32::from(VK_DOWN) {
        ArdNamedKey::ArrowDown
    } else if vk == u32::from(VK_SNAPSHOT) {
        ArdNamedKey::PrintScreen
    } else if vk == u32::from(VK_INSERT) {
        ArdNamedKey::Insert
    } else if vk == u32::from(VK_DELETE) {
        ArdNamedKey::Delete
    } else if vk == u32::from(VK_LSHIFT) || (vk == u32::from(VK_SHIFT) && event.scanCode != 0x36) {
        ArdNamedKey::ShiftLeft
    } else if vk == u32::from(VK_RSHIFT) || (vk == u32::from(VK_SHIFT) && event.scanCode == 0x36) {
        ArdNamedKey::ShiftRight
    } else if vk == u32::from(VK_LCONTROL) || (vk == u32::from(VK_CONTROL) && !extended) {
        ArdNamedKey::ControlLeft
    } else if vk == u32::from(VK_RCONTROL) || (vk == u32::from(VK_CONTROL) && extended) {
        ArdNamedKey::ControlRight
    } else if vk == u32::from(VK_LMENU) || (vk == u32::from(VK_MENU) && !extended) {
        ArdNamedKey::MetaLeft
    } else if vk == u32::from(VK_RMENU) || (vk == u32::from(VK_MENU) && extended) {
        ArdNamedKey::MetaRight
    } else if vk == u32::from(VK_LWIN) {
        ArdNamedKey::AltLeft
    } else if vk == u32::from(VK_RWIN) {
        ArdNamedKey::AltRight
    } else if vk == u32::from(VK_NUMLOCK) {
        ArdNamedKey::NumLock
    } else if vk == u32::from(VK_SCROLL) {
        ArdNamedKey::ScrollLock
    } else if vk == u32::from(VK_APPS) {
        ArdNamedKey::ContextMenu
    } else if vk == u32::from(VK_MULTIPLY) {
        ArdNamedKey::NumpadMultiply
    } else if vk == u32::from(VK_ADD) {
        ArdNamedKey::NumpadAdd
    } else if vk == u32::from(VK_SUBTRACT) {
        ArdNamedKey::NumpadSubtract
    } else if vk == u32::from(VK_DECIMAL) {
        ArdNamedKey::NumpadDecimal
    } else if vk == u32::from(VK_DIVIDE) {
        ArdNamedKey::NumpadDivide
    } else {
        return None;
    };
    keysym_for_key(ArdKey::Named(named))
}

fn translated_keysym(event: &KBDLLHOOKSTRUCT, keyboard: &[u8; 256]) -> Option<u32> {
    if event.vkCode == u32::from(VK_PROCESSKEY) {
        return None;
    }
    if event.vkCode == u32::from(VK_PACKET) {
        return char::from_u32(event.scanCode & 0xffff).and_then(unicode_keysym);
    }

    let mut state = *keyboard;
    if !key_down(&state, u32::from(VK_RMENU)) {
        for vk in [
            VK_CONTROL,
            VK_LCONTROL,
            VK_RCONTROL,
            VK_MENU,
            VK_LMENU,
            VK_RMENU,
        ] {
            state[usize::from(vk)] &= 0x7f;
        }
    }
    let mut buffer = [0u16; 8];
    // SAFETY: The buffers are fixed-size and valid for ToUnicodeEx. The
    // foreground thread's layout is used because layout is thread-local.
    let translated = unsafe {
        let foreground = GetForegroundWindow();
        let thread_id = if foreground.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, ptr::null_mut())
        };
        ToUnicodeEx(
            event.vkCode,
            event.scanCode,
            state.as_ptr(),
            buffer.as_mut_ptr(),
            buffer.len() as i32,
            TO_UNICODE_DO_NOT_CHANGE_STATE,
            GetKeyboardLayout(thread_id),
        )
    };
    if translated != 0 {
        let length = usize::try_from(translated.unsigned_abs())
            .ok()?
            .min(buffer.len());
        if let Some(character) =
            char::decode_utf16(buffer[..length].iter().copied()).find_map(Result::ok)
        {
            return unicode_keysym(character);
        }
    }

    if (u32::from(b'A')..=u32::from(b'Z')).contains(&event.vkCode) {
        let shifted = key_down(keyboard, u32::from(VK_SHIFT))
            || key_down(keyboard, u32::from(VK_LSHIFT))
            || key_down(keyboard, u32::from(VK_RSHIFT));
        let caps_lock = keyboard[usize::from(VK_CAPITAL)] & 0x01 != 0;
        let mut character = char::from_u32(event.vkCode)?;
        if !(shifted ^ caps_lock) {
            character = character.to_ascii_lowercase();
        }
        return unicode_keysym(character);
    }
    if (u32::from(b'0')..=u32::from(b'9')).contains(&event.vkCode) {
        return char::from_u32(event.vkCode).and_then(unicode_keysym);
    }
    None
}

fn normalized_vk(event: &KBDLLHOOKSTRUCT) -> u32 {
    let extended = event.flags & LLKHF_EXTENDED != 0;
    if event.vkCode == u32::from(VK_SHIFT) {
        if event.scanCode == 0x36 {
            u32::from(VK_RSHIFT)
        } else {
            u32::from(VK_LSHIFT)
        }
    } else if event.vkCode == u32::from(VK_CONTROL) {
        if extended {
            u32::from(VK_RCONTROL)
        } else {
            u32::from(VK_LCONTROL)
        }
    } else if event.vkCode == u32::from(VK_MENU) {
        if extended {
            u32::from(VK_RMENU)
        } else {
            u32::from(VK_LMENU)
        }
    } else {
        event.vkCode
    }
}

fn update_keyboard_state(keyboard: &mut [u8; 256], vk: u32, pressed: bool) {
    set_key_state(keyboard, vk, pressed);
    if vk == u32::from(VK_LSHIFT) || vk == u32::from(VK_RSHIFT) {
        sync_generic_modifier(keyboard, VK_SHIFT, VK_LSHIFT, VK_RSHIFT);
    } else if vk == u32::from(VK_LCONTROL) || vk == u32::from(VK_RCONTROL) {
        sync_generic_modifier(keyboard, VK_CONTROL, VK_LCONTROL, VK_RCONTROL);
    } else if vk == u32::from(VK_LMENU) || vk == u32::from(VK_RMENU) {
        sync_generic_modifier(keyboard, VK_MENU, VK_LMENU, VK_RMENU);
    }
}

fn sync_generic_modifier(keyboard: &mut [u8; 256], generic: u16, left: u16, right: u16) {
    let pressed = key_down(keyboard, u32::from(left)) || key_down(keyboard, u32::from(right));
    set_key_state(keyboard, u32::from(generic), pressed);
}

fn set_key_state(keyboard: &mut [u8; 256], vk: u32, pressed: bool) {
    let Ok(index) = usize::try_from(vk) else {
        return;
    };
    let Some(state) = keyboard.get_mut(index) else {
        return;
    };
    let was_pressed = *state & 0x80 != 0;
    if pressed {
        *state |= 0x80;
        if !was_pressed && is_lock_key(vk) {
            *state ^= 0x01;
        }
    } else {
        *state &= 0x7f;
    }
}

fn key_down(keyboard: &[u8; 256], vk: u32) -> bool {
    usize::try_from(vk)
        .ok()
        .and_then(|index| keyboard.get(index))
        .is_some_and(|state| state & 0x80 != 0)
}

fn is_modifier_vk(vk: u32) -> bool {
    [
        VK_SHIFT,
        VK_LSHIFT,
        VK_RSHIFT,
        VK_CONTROL,
        VK_LCONTROL,
        VK_RCONTROL,
        VK_MENU,
        VK_LMENU,
        VK_RMENU,
        VK_LWIN,
        VK_RWIN,
    ]
    .into_iter()
    .any(|modifier| vk == u32::from(modifier))
}

fn is_lock_key(vk: u32) -> bool {
    vk == u32::from(VK_CAPITAL) || vk == u32::from(VK_NUMLOCK) || vk == u32::from(VK_SCROLL)
}

fn should_route_direct(
    tracked: bool,
    win_chord: bool,
    capture: bool,
    ime_active: bool,
    vk: u32,
) -> bool {
    tracked || win_chord || (capture && !ime_active && !is_lock_key(vk))
}

fn win_key_bit(vk: u32) -> u8 {
    if vk == u32::from(VK_LWIN) {
        LEFT_WIN
    } else if vk == u32::from(VK_RWIN) {
        RIGHT_WIN
    } else {
        0
    }
}

fn key_id(event: &KBDLLHOOKSTRUCT) -> u32 {
    (normalized_vk(event) << 16)
        | ((event.flags & LLKHF_EXTENDED != 0) as u32) << 8
        | (event.scanCode & 0xff)
}

fn call_next(code: i32, wparam: usize, lparam: isize) -> isize {
    // SAFETY: Forwarding untouched arguments is required when the hook does
    // not consume an event.
    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(vk_code: u16, scan_code: u32, extended: bool) -> KBDLLHOOKSTRUCT {
        KBDLLHOOKSTRUCT {
            vkCode: u32::from(vk_code),
            scanCode: scan_code,
            flags: if extended { LLKHF_EXTENDED } else { 0 },
            time: 0,
            dwExtraInfo: 0,
        }
    }

    #[test]
    fn win_keys_map_to_apple_command_keysyms() {
        assert_eq!(named_keysym(&event(VK_LWIN, 0x5b, true)), Some(0xffe9));
        assert_eq!(named_keysym(&event(VK_RWIN, 0x5c, true)), Some(0xffea));
    }

    #[test]
    fn windows_alt_maps_to_apple_option_keysyms() {
        assert_eq!(named_keysym(&event(VK_LMENU, 0x38, false)), Some(0xffe7));
        assert_eq!(named_keysym(&event(VK_RMENU, 0x38, true)), Some(0xffe8));
    }

    #[test]
    fn generic_modifier_stays_pressed_until_both_sides_are_released() {
        let mut keyboard = [0; 256];
        update_keyboard_state(&mut keyboard, u32::from(VK_LSHIFT), true);
        update_keyboard_state(&mut keyboard, u32::from(VK_RSHIFT), true);
        update_keyboard_state(&mut keyboard, u32::from(VK_LSHIFT), false);
        assert!(key_down(&keyboard, u32::from(VK_SHIFT)));
        update_keyboard_state(&mut keyboard, u32::from(VK_RSHIFT), false);
        assert!(!key_down(&keyboard, u32::from(VK_SHIFT)));
    }

    #[test]
    fn lock_keys_are_left_to_the_local_keyboard_state() {
        assert!(is_lock_key(u32::from(VK_CAPITAL)));
        assert!(!is_lock_key(u32::from(VK_SPACE)));
    }

    #[test]
    fn win_chords_use_the_direct_remote_path_even_during_ime() {
        assert!(should_route_direct(
            false,
            true,
            false,
            true,
            u32::from(b'V')
        ));
        assert!(should_route_direct(
            false,
            false,
            true,
            false,
            u32::from(b'V')
        ));
        assert!(!should_route_direct(
            false,
            false,
            true,
            true,
            u32::from(b'V')
        ));
        assert!(!should_route_direct(
            false,
            false,
            true,
            false,
            u32::from(VK_CAPITAL)
        ));
    }
}
