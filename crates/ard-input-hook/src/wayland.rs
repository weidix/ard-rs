//! Linux/Wayland backend implementing the
//! `zwp_keyboard_shortcuts_inhibit_v1` protocol through `wayland-client` and
//! `wayland-protocols`.
//!
//! Wayland has no global keyboard grab: key routing is owned by the
//! compositor, and the compositor-only mechanism an application has is the
//! shortcut inhibitor.  This backend connects to the compositor on a
//! dedicated thread, binds the `wl_seat` and
//! `zwp_keyboard_shortcuts_inhibit_manager_v1` globals, creates a surface,
//! and asks the compositor to block its own shortcuts for that surface + seat
//! (`inhibit_shortcuts`).  While enabled the inhibitor exists, when disabled
//! or on drop it is destroyed (the protocol offers no activate/deactivate
//! requests; the compositor decides activation through its `active`/`inactive`
//! events).
//!
//! Note: iced/winit do not expose the session window's `wl_surface`, so this
//! backend requests inhibition for its own surface.  Hosts that can supply
//! their focused `wl_surface` would pass it in place of the internally
//! created one; the protocol interaction (surface + seat request, active /
//! inactive events) is identical.  Key events themselves
//! continue to arrive through the application's own Wayland surface, which is
//! the only way Wayland delivers keys.

use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use wayland_client::globals::{BindError, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_compositor, wl_registry, wl_seat, wl_surface};
use wayland_client::backend::WaylandError;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1 as manager,
    zwp_keyboard_shortcuts_inhibitor_v1 as inhibitor,
};

use crate::common::{HookConfig, HookError, HookEvent};

struct WaylandState {
    enabled: AtomicBool,
    shutdown: AtomicBool,
}

/// Handle that owns the Wayland hook thread.
pub struct WaylandHook {
    state: Arc<WaylandState>,
    thread: Option<JoinHandle<()>>,
}

impl WaylandHook {
    pub fn start(config: HookConfig) -> Result<(Self, Receiver<HookEvent>), HookError> {
        let (sender, receiver) = sync_channel(256);
        let state = Arc::new(WaylandState {
            enabled: AtomicBool::new(config.capture_enabled),
            shutdown: AtomicBool::new(false),
        });
        let thread_state = Arc::clone(&state);
        let thread = thread::Builder::new()
            .name("ard-input-hook-wayland".into())
            .spawn(move || wayland_thread_main(thread_state, sender))
            .map_err(|error| HookError::Io(format!("cannot start Wayland hook thread: {error}")))?;
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

impl Drop for WaylandHook {
    fn drop(&mut self) {
        self.state.shutdown.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// State passed to the event queue dispatcher.
#[derive(Default)]
struct QueueState {
    /// Whether the compositor currently reports the inhibitor as active.
    inhibit_active: bool,
}

/// User data attached to the inhibitor proxy.
struct InhibitorData {
    sender: SyncSender<HookEvent>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for QueueState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for QueueState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_compositor::WlCompositor,
        _event: wl_compositor::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for QueueState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_surface::WlSurface,
        _event: wl_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for QueueState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<manager::ZwpKeyboardShortcutsInhibitManagerV1, ()> for QueueState {
    fn event(
        _state: &mut Self,
        _proxy: &manager::ZwpKeyboardShortcutsInhibitManagerV1,
        _event: manager::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<inhibitor::ZwpKeyboardShortcutsInhibitorV1, InhibitorData> for QueueState {
    fn event(
        state: &mut Self,
        _proxy: &inhibitor::ZwpKeyboardShortcutsInhibitorV1,
        event: inhibitor::Event,
        data: &InhibitorData,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            inhibitor::Event::Active => {
                state.inhibit_active = true;
                let _ = data.sender.try_send(HookEvent::InhibitActive);
            }
            inhibitor::Event::Inactive => {
                state.inhibit_active = false;
                let _ = data.sender.try_send(HookEvent::InhibitInactive);
            }
            _ => {}
        }
    }
}

fn wayland_thread_main(state: Arc<WaylandState>, sender: SyncSender<HookEvent>) {
    let connection = match Connection::connect_to_env() {
        Ok(connection) => connection,
        Err(error) => {
            let _ = sender.try_send(HookEvent::Error(HookError::Io(format!(
                "cannot connect to the Wayland compositor: {error}"
            ))));
            return;
        }
    };

    let (globals, mut event_queue) = match registry_queue_init::<QueueState>(&connection) {
        Ok(initialized) => initialized,
        Err(error) => {
            let _ = sender.try_send(HookEvent::Error(HookError::Io(format!(
                "cannot initialize the Wayland registry: {error}"
            ))));
            return;
        }
    };

    let compositor: wl_compositor::WlCompositor =
        match globals.bind(&event_queue.handle(), 1..=1, ()) {
        Ok(compositor) => compositor,
        Err(error) => {
            let _ = sender.try_send(HookEvent::Error(HookError::Unsupported(format!(
                "wl_compositor is not available: {error}"
            ))));
            return;
        }
    };
    let seat: wl_seat::WlSeat = match globals.bind(&event_queue.handle(), 1..=1, ()) {
        Ok(seat) => seat,
        Err(error) => {
            let _ = sender.try_send(HookEvent::Error(HookError::Unsupported(format!(
                "wl_seat is not available: {error}"
            ))));
            return;
        }
    };
    let manager: manager::ZwpKeyboardShortcutsInhibitManagerV1 =
        match globals.bind(&event_queue.handle(), 1..=1, ()) {
        Ok(manager) => manager,
        Err(BindError::NotPresent) => {
            let _ = sender.try_send(HookEvent::Error(HookError::Unsupported(
                "the compositor does not advertise zwp_keyboard_shortcuts_inhibit_manager_v1"
                    .into(),
            )));
            return;
        }
        Err(error) => {
            let _ = sender.try_send(HookEvent::Error(HookError::Io(format!(
                "cannot bind zwp_keyboard_shortcuts_inhibit_manager_v1: {error}"
            ))));
            return;
        }
    };

    let surface = compositor.create_surface(&event_queue.handle(), ());

    let mut queue_state = QueueState::default();
    let mut inhibitor = None;
    let mut last_enabled = state.enabled.load(Ordering::SeqCst);
    let _ = event_queue.flush();

    loop {
        if state.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let enabled = state.enabled.load(Ordering::SeqCst);
        if enabled != last_enabled {
            match (&inhibitor, enabled) {
                (None, true) => {
                    inhibitor = Some(manager.inhibit_shortcuts(
                        &surface,
                        &seat,
                        &event_queue.handle(),
                        InhibitorData {
                            sender: sender.clone(),
                        },
                    ));
                    let _ = event_queue.flush();
                }
                (Some(_), false) => {
                    if let Some(inhibitor) = inhibitor.take() {
                        inhibitor.destroy();
                        let _ = event_queue.flush();
                    }
                }
                _ => {}
            }
            last_enabled = enabled;
        }

        if let Err(error) = event_queue.dispatch_pending(&mut queue_state) {
            let _ = sender.try_send(HookEvent::Error(HookError::Io(format!(
                "Wayland dispatch failed: {error}"
            ))));
            break;
        }
        let _ = event_queue.flush();

        if let Some(guard) = event_queue.prepare_read() {
            match guard.read() {
                Ok(_) => {
                    let _ = event_queue.dispatch_pending(&mut queue_state);
                }
                Err(WaylandError::Io(error)) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) => {
                    let _ = sender.try_send(HookEvent::Error(HookError::Io(format!(
                        "Wayland connection error: {error}"
                    ))));
                    break;
                }
            }
        }
        thread::sleep(Duration::from_millis(1));
    }

    if let Some(inhibitor) = inhibitor {
        inhibitor.destroy();
    }
    surface.destroy();
    let _ = event_queue.flush();
}
