#![allow(unsafe_code)]

use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_LWIN, VK_RWIN};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW, HC_ACTION, HHOOK,
    KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, MSG, PostMessageW, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP,
};

const LEFT_WIN: u8 = 0x01;
const RIGHT_WIN: u8 = 0x02;

struct Interceptor {
    session_hwnd: AtomicUsize,
    input_ready: AtomicBool,
    blocked_keys: AtomicU8,
}

static INTERCEPTOR: OnceLock<Interceptor> = OnceLock::new();

pub fn set_session_window(hwnd: Option<usize>) {
    interceptor()
        .session_hwnd
        .store(hwnd.unwrap_or_default(), Ordering::Release);
}

pub fn set_input_ready(ready: bool) {
    interceptor().input_ready.store(ready, Ordering::Release);
}

fn interceptor() -> &'static Interceptor {
    INTERCEPTOR.get_or_init(|| {
        let (hook_ready, hook_status) = sync_channel(1);
        thread::Builder::new()
            .name("ard-win-key-hook".into())
            .spawn(move || run_hook(hook_ready))
            .expect("Windows key hook thread should start");
        if hook_status.recv() != Ok(true) {
            eprintln!("failed to install Windows key hook");
        }
        Interceptor {
            session_hwnd: AtomicUsize::new(0),
            input_ready: AtomicBool::new(false),
            blocked_keys: AtomicU8::new(0),
        }
    })
}

struct Hook(HHOOK);

impl Drop for Hook {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the live handle returned by SetWindowsHookExW,
        // owned by this guard and unhooked exactly once here.
        unsafe {
            UnhookWindowsHookEx(self.0);
        }
    }
}

fn run_hook(ready: SyncSender<bool>) {
    // SAFETY: FreeRDP uses the same low-level hook plus a dedicated message
    // loop. The callback has the required system ABI and static lifetime, and
    // Hook owns the returned handle for the loop's duration.
    let hook = unsafe {
        let module = GetModuleHandleW(ptr::null());
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), module, 0)
    };
    if hook.is_null() {
        let _ = ready.send(false);
        return;
    }
    let _hook = Hook(hook);
    let _ = ready.send(true);
    let mut message = MSG::default();
    loop {
        // SAFETY: `message` remains valid and exclusively borrowed for each
        // call. Null HWND requests this thread's complete message queue.
        let result = unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) };
        if result <= 0 {
            break;
        }
        // SAFETY: GetMessageW initialized `message` for this iteration.
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: usize, lparam: isize) -> isize {
    if code != HC_ACTION as i32 || lparam == 0 {
        return call_next(code, wparam, lparam);
    }
    // SAFETY: For WH_KEYBOARD_LL with nonnegative code, Windows documents
    // lParam as a valid KBDLLHOOKSTRUCT pointer for the callback duration.
    let event = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
    if event.flags & LLKHF_INJECTED != 0 {
        return call_next(code, wparam, lparam);
    }
    let bit = match event.vkCode {
        key if key == u32::from(VK_LWIN) => LEFT_WIN,
        key if key == u32::from(VK_RWIN) => RIGHT_WIN,
        _ => return call_next(code, wparam, lparam),
    };
    let Some(interceptor) = INTERCEPTOR.get() else {
        return call_next(code, wparam, lparam);
    };
    let pressed = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
    let released = matches!(wparam as u32, WM_KEYUP | WM_SYSKEYUP);

    if released {
        let previous = interceptor.blocked_keys.fetch_and(!bit, Ordering::AcqRel);
        if previous & bit != 0 {
            let _ = post_key_message(interceptor, wparam as u32, event, true);
            return 1;
        }
    } else if pressed && should_intercept(interceptor) {
        let repeated = interceptor.blocked_keys.load(Ordering::Acquire) & bit != 0;
        if post_key_message(interceptor, wparam as u32, event, repeated) {
            interceptor.blocked_keys.fetch_or(bit, Ordering::AcqRel);
            return 1;
        }
        if repeated {
            return 1;
        }
    }

    call_next(code, wparam, lparam)
}

fn should_intercept(interceptor: &Interceptor) -> bool {
    if !interceptor.input_ready.load(Ordering::Acquire) {
        return false;
    }
    let session_hwnd = interceptor.session_hwnd.load(Ordering::Acquire);
    if session_hwnd == 0 {
        return false;
    }
    // SAFETY: GetForegroundWindow has no pointer preconditions. The returned
    // HWND is compared by value only and is never dereferenced.
    unsafe { GetForegroundWindow() as usize == session_hwnd }
}

fn post_key_message(
    interceptor: &Interceptor,
    message: u32,
    event: &KBDLLHOOKSTRUCT,
    previous_state: bool,
) -> bool {
    if !interceptor.input_ready.load(Ordering::Acquire) {
        return false;
    }
    let hwnd = interceptor.session_hwnd.load(Ordering::Acquire) as HWND;
    if hwnd.is_null() {
        return false;
    }
    let mut key_data = 1u32 | ((event.scanCode & 0xff) << 16);
    if event.flags & LLKHF_EXTENDED != 0 {
        key_data |= 1 << 24;
    }
    if previous_state {
        key_data |= 1 << 30;
    }
    if matches!(message, WM_KEYUP | WM_SYSKEYUP) {
        key_data |= (1 << 30) | (1 << 31);
    }

    // Posting back to the session HWND preserves Win+key ordering in winit's
    // normal event queue while the original event remains hidden from Shell.
    // SAFETY: HWND is compared/stored opaquely and PostMessageW copies all
    // scalar arguments before returning.
    unsafe { PostMessageW(hwnd, message, event.vkCode as usize, key_data as isize) != 0 }
}

fn call_next(code: i32, wparam: usize, lparam: isize) -> isize {
    // SAFETY: Forwarding the untouched callback arguments is required by the
    // hook contract whenever this hook does not consume an event.
    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}
