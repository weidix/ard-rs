use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use ard_rs::ArdClientInput;
use hookmap_core::button::{Button, ButtonAction};
use hookmap_core::event::Event;

const COMMAND_LEFT_KEYSYM: u32 = 0xffe9;
const COMMAND_RIGHT_KEYSYM: u32 = 0xffea;

#[derive(Default)]
struct State {
    input: Option<ArdClientInput>,
    left_pressed: bool,
    right_pressed: bool,
}

struct Interceptor {
    focused: AtomicBool,
    state: Mutex<State>,
}

static INTERCEPTOR: OnceLock<Arc<Interceptor>> = OnceLock::new();

fn interceptor() -> &'static Arc<Interceptor> {
    INTERCEPTOR.get_or_init(|| {
        let interceptor = Arc::new(Interceptor {
            focused: AtomicBool::new(false),
            state: Mutex::new(State::default()),
        });
        let worker = Arc::clone(&interceptor);
        thread::Builder::new()
            .name("ard-windows-key-hook".into())
            .spawn(move || run_hook(worker))
            .expect("Windows input hook thread should start");
        interceptor
    })
}

pub fn set_input(input: Option<ArdClientInput>) {
    let interceptor = interceptor();
    if input.is_none() {
        release_remote_command(interceptor);
    }
    if let Ok(mut state) = interceptor.state.lock() {
        state.input = input;
    }
}

pub fn set_session_focused(focused: bool) {
    let interceptor = interceptor();
    interceptor.focused.store(focused, Ordering::Release);
    if !focused {
        release_remote_command(interceptor);
    }
}

fn run_hook(interceptor: Arc<Interceptor>) {
    let events = hookmap_core::install_hook();
    while let Ok((event, native)) = events.recv() {
        let Event::Button(event) = event else {
            native.dispatch();
            continue;
        };
        let (right, keysym) = match event.target {
            Button::LSuper => (false, COMMAND_LEFT_KEYSYM),
            Button::RSuper => (true, COMMAND_RIGHT_KEYSYM),
            _ => {
                native.dispatch();
                continue;
            }
        };
        if event.injected || !interceptor.focused.load(Ordering::Acquire) {
            native.dispatch();
            continue;
        }

        let pressed = event.action == ButtonAction::Press;
        if let Ok(mut state) = interceptor.state.lock() {
            let was_pressed = if right {
                &mut state.right_pressed
            } else {
                &mut state.left_pressed
            };
            if *was_pressed != pressed {
                *was_pressed = pressed;
                if let Some(input) = state.input.clone() {
                    let _ = input.send_key_event(pressed, keysym);
                }
            }
        }
        native.block();
    }
}

fn release_remote_command(interceptor: &Interceptor) {
    if let Ok(mut state) = interceptor.state.lock() {
        let input = state.input.clone();
        if state.left_pressed {
            if let Some(input) = &input {
                let _ = input.send_key_event(false, COMMAND_LEFT_KEYSYM);
            }
            state.left_pressed = false;
        }
        if state.right_pressed {
            if let Some(input) = &input {
                let _ = input.send_key_event(false, COMMAND_RIGHT_KEYSYM);
            }
            state.right_pressed = false;
        }
    }
}
