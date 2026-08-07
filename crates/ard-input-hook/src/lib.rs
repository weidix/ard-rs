//! Platform-native global keyboard capture for remote input forwarding.
//!
//! This crate intentionally has no dependency on iced, winit, or the remote
//! protocol layer.  Each platform implements the same [`KeyboardHook`] API
//! behind `cfg(target_os = "...")`:
//!
//! - Windows: a `WH_KEYBOARD_LL` hook installed with `SetWindowsHookExW`.
//! - macOS: a `CGEventTap` driven by a Core Foundation run loop.
//! - Linux/X11: an exclusive `XGrabKeyboard` grab.
//! - Linux/Wayland: a `zwp_keyboard_shortcuts_inhibit_v1` inhibitor.
//!
//! Hook work runs on a dedicated thread.  The native callback only parses
//! key events, maintains modifier state, decides whether the event must be
//! suppressed, and delivers a [`RawKeyEvent`] through a non-blocking channel.
//! No UI, network, or expensive work happens inside a callback.

mod common;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(all(target_os = "linux", feature = "x11"))]
mod x11;
#[cfg(all(target_os = "linux", feature = "wayland"))]
mod wayland;

pub use common::*;

#[allow(unused_imports)]
use std::sync::mpsc::Receiver;

/// A running keyboard hook.
///
/// The handle owns the dedicated hook thread.  Dropping it stops the thread
/// and releases the native hook, event tap, keyboard grab, or Wayland
/// inhibitor.
pub struct KeyboardHook {
    inner: PlatformHook,
}

enum PlatformHook {
    #[cfg(target_os = "windows")]
    Windows {
        hook: windows::WindowsHook,
        events: Receiver<HookEvent>,
    },
    #[cfg(target_os = "macos")]
    MacOs {
        hook: macos::MacOsHook,
        events: Receiver<HookEvent>,
    },
    #[cfg(all(target_os = "linux", feature = "x11"))]
    X11 {
        hook: x11::X11Hook,
        events: Receiver<HookEvent>,
    },
    #[cfg(all(target_os = "linux", feature = "wayland"))]
    Wayland {
        hook: wayland::WaylandHook,
        events: Receiver<HookEvent>,
    },
}

impl KeyboardHook {
    /// Starts the platform hook on a dedicated thread.
    ///
    /// On Linux the backend is selected from the environment: Wayland is used
    /// when `WAYLAND_DISPLAY` is set, otherwise X11 (when `DISPLAY` is set).
    pub fn start(config: HookConfig) -> Result<Self, HookError> {
        #[cfg(target_os = "windows")]
        let inner = windows_start(config)?;
        #[cfg(target_os = "macos")]
        let inner = macos_start(config)?;
        #[cfg(all(target_os = "linux", feature = "wayland"))]
        let inner = {
            if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                wayland_start(config)?
            } else {
                linux_fallback(config)?
            }
        };
        #[cfg(all(target_os = "linux", not(feature = "wayland")))]
        let inner = linux_fallback(config)?;
        Ok(Self { inner })
    }

    /// Enables or disables capture/suppression.
    ///
    /// While disabled the native hook stays installed but passes every event
    /// through untouched (on X11 the grab is released, on Wayland the
    /// inhibitor is removed).
    pub fn set_enabled(&self, enabled: bool) -> Result<(), HookError> {
        #[allow(unreachable_patterns)]
        match &self.inner {
            #[cfg(target_os = "windows")]
            PlatformHook::Windows { hook, .. } => hook.set_enabled(enabled),
            #[cfg(target_os = "macos")]
            PlatformHook::MacOs { hook, .. } => hook.set_enabled(enabled),
            #[cfg(all(target_os = "linux", feature = "x11"))]
            PlatformHook::X11 { hook, .. } => hook.set_enabled(enabled),
            #[cfg(all(target_os = "linux", feature = "wayland"))]
            PlatformHook::Wayland { hook, .. } => hook.set_enabled(enabled),
            _ => Err(HookError::Unsupported("no keyboard hook backend enabled".into())),
        }
    }

    /// Returns the next queued hook event without blocking.
    pub fn try_recv(&self) -> Option<HookEvent> {
        #[allow(unreachable_patterns)]
        match &self.inner {
            #[cfg(target_os = "windows")]
            PlatformHook::Windows { events, .. } => events.try_recv().ok(),
            #[cfg(target_os = "macos")]
            PlatformHook::MacOs { events, .. } => events.try_recv().ok(),
            #[cfg(all(target_os = "linux", feature = "x11"))]
            PlatformHook::X11 { events, .. } => events.try_recv().ok(),
            #[cfg(all(target_os = "linux", feature = "wayland"))]
            PlatformHook::Wayland { events, .. } => events.try_recv().ok(),
            _ => None,
        }
    }
}

#[cfg(target_os = "windows")]
fn windows_start(config: HookConfig) -> Result<PlatformHook, HookError> {
    let (hook, events) = windows::WindowsHook::start(config)?;
    Ok(PlatformHook::Windows { hook, events })
}

#[cfg(target_os = "macos")]
fn macos_start(config: HookConfig) -> Result<PlatformHook, HookError> {
    let (hook, events) = macos::MacOsHook::start(config)?;
    Ok(PlatformHook::MacOs { hook, events })
}

#[cfg(all(target_os = "linux", feature = "x11"))]
fn linux_fallback(config: HookConfig) -> Result<PlatformHook, HookError> {
    let (hook, events) = x11::X11Hook::start(config)?;
    Ok(PlatformHook::X11 { hook, events })
}

#[cfg(all(target_os = "linux", feature = "wayland"))]
fn wayland_start(config: HookConfig) -> Result<PlatformHook, HookError> {
    let (hook, events) = wayland::WaylandHook::start(config)?;
    Ok(PlatformHook::Wayland { hook, events })
}

#[cfg(all(target_os = "linux", not(feature = "x11")))]
fn linux_fallback(_config: HookConfig) -> Result<PlatformHook, HookError> {
    Err(HookError::Unsupported(
        "no Linux input backend feature (x11/wayland) is enabled".into(),
    ))
}

impl std::fmt::Debug for KeyboardHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyboardHook").finish_non_exhaustive()
    }
}

impl Drop for KeyboardHook {
    fn drop(&mut self) {
        // Each platform handle stops its thread and releases native
        // resources in its own Drop implementation.
    }
}
