use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Mutex, OnceLock};

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, MSG, SetWindowsHookExW, TranslateMessage, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP,
};

pub type OnPress = Box<dyn Fn(u32) + Send + Sync>;

type Watched = Vec<(u16, Mods, u32)>;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Mods {
    shift: bool,
    ctrl: bool,
    alt: bool,
    win: bool,
}

struct Shared {
    watched: Mutex<Watched>,
    presses: Mutex<Sender<u32>>,
}

static HOOK: OnceLock<Shared> = OnceLock::new();
static RUNNING: AtomicBool = AtomicBool::new(false);
static HELD: AtomicU32 = AtomicU32::new(0);

const MASK_KEY: VIRTUAL_KEY = VIRTUAL_KEY(0xE8);

pub fn start(on_press: OnPress) -> bool {
    if RUNNING.load(Ordering::Relaxed) {
        return true;
    }
    let (tx, rx) = channel::<u32>();
    if HOOK.set(Shared { watched: Mutex::new(Vec::new()), presses: Mutex::new(tx) }).is_err() {
        return false;
    }
    let worker = std::thread::Builder::new().name("key hook presses".into()).spawn(move || {
        for id in rx {
            on_press(id);
        }
    });
    if worker.is_err() {
        return false;
    }
    let (made_tx, made_rx) = channel();
    let spawned = std::thread::Builder::new().name("key hook".into()).spawn(move || {
        let hook = unsafe {
            let module = GetModuleHandleW(None).ok().map(|m| HINSTANCE(m.0));
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(callback), module, 0)
        };
        if hook.is_err() {
            let _ = made_tx.send(false);
            return;
        }
        RUNNING.store(true, Ordering::Relaxed);
        let _ = made_tx.send(true);
        let mut msg = MSG::default();
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        RUNNING.store(false, Ordering::Relaxed);
    });
    spawned.is_ok() && made_rx.recv().unwrap_or(false)
}

pub fn watch(hotkeys: &[HotKey]) {
    let Some(shared) = HOOK.get() else { return };
    *shared.watched.lock().unwrap() = hotkeys.iter().filter_map(|hk| Some((vk(hk.key)?.0, mods(hk.mods), hk.id()))).collect();
}

fn down(key: VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(key.0 as i32) as u16 & 0x8000 != 0 }
}

fn held_mods() -> Mods {
    Mods { shift: down(VK_SHIFT), ctrl: down(VK_CONTROL), alt: down(VK_MENU), win: down(VK_LWIN) || down(VK_RWIN) }
}

fn press_mask_key() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: MASK_KEY, dwFlags: flags, ..Default::default() } },
    };
    unsafe { SendInput(&[key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)], std::mem::size_of::<INPUT>() as i32) };
}

unsafe extern "system" fn callback(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        if let Some(shared) = HOOK.get() {
            let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let key = info.vkCode;
            match wparam.0 as u32 {
                WM_KEYDOWN | WM_SYSKEYDOWN => {
                    let mods = held_mods();
                    let hit = shared.watched.lock().unwrap().iter().find(|(k, m, _)| u32::from(*k) == key && *m == mods).map(|(_, _, id)| *id);
                    if let Some(id) = hit {
                        if HELD.swap(key, Ordering::Relaxed) != key {
                            let _ = shared.presses.lock().unwrap().send(id);
                        }
                        if mods.alt || mods.win {
                            press_mask_key();
                        }
                        return LRESULT(1);
                    }
                    HELD.store(0, Ordering::Relaxed);
                }
                WM_KEYUP | WM_SYSKEYUP => {
                    let _ = HELD.compare_exchange(key, 0, Ordering::Relaxed, Ordering::Relaxed);
                }
                _ => {}
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn mods(m: Modifiers) -> Mods {
    Mods {
        shift: m.contains(Modifiers::SHIFT),
        ctrl: m.contains(Modifiers::CONTROL),
        alt: m.contains(Modifiers::ALT),
        win: m.intersects(Modifiers::SUPER | Modifiers::META),
    }
}

fn vk(code: Code) -> Option<VIRTUAL_KEY> {
    Some(match code {
        Code::KeyA => VK_A,
        Code::KeyB => VK_B,
        Code::KeyC => VK_C,
        Code::KeyD => VK_D,
        Code::KeyE => VK_E,
        Code::KeyF => VK_F,
        Code::KeyG => VK_G,
        Code::KeyH => VK_H,
        Code::KeyI => VK_I,
        Code::KeyJ => VK_J,
        Code::KeyK => VK_K,
        Code::KeyL => VK_L,
        Code::KeyM => VK_M,
        Code::KeyN => VK_N,
        Code::KeyO => VK_O,
        Code::KeyP => VK_P,
        Code::KeyQ => VK_Q,
        Code::KeyR => VK_R,
        Code::KeyS => VK_S,
        Code::KeyT => VK_T,
        Code::KeyU => VK_U,
        Code::KeyV => VK_V,
        Code::KeyW => VK_W,
        Code::KeyX => VK_X,
        Code::KeyY => VK_Y,
        Code::KeyZ => VK_Z,
        Code::Digit0 => VK_0,
        Code::Digit1 => VK_1,
        Code::Digit2 => VK_2,
        Code::Digit3 => VK_3,
        Code::Digit4 => VK_4,
        Code::Digit5 => VK_5,
        Code::Digit6 => VK_6,
        Code::Digit7 => VK_7,
        Code::Digit8 => VK_8,
        Code::Digit9 => VK_9,
        Code::Equal => VK_OEM_PLUS,
        Code::Comma => VK_OEM_COMMA,
        Code::Minus => VK_OEM_MINUS,
        Code::Period => VK_OEM_PERIOD,
        Code::Semicolon => VK_OEM_1,
        Code::Slash => VK_OEM_2,
        Code::Backquote => VK_OEM_3,
        Code::BracketLeft => VK_OEM_4,
        Code::Backslash => VK_OEM_5,
        Code::BracketRight => VK_OEM_6,
        Code::Quote => VK_OEM_7,
        Code::Backspace => VK_BACK,
        Code::Tab => VK_TAB,
        Code::Space => VK_SPACE,
        Code::Enter | Code::NumpadEnter => VK_RETURN,
        Code::Escape => VK_ESCAPE,
        Code::PageUp => VK_PRIOR,
        Code::PageDown => VK_NEXT,
        Code::End => VK_END,
        Code::Home => VK_HOME,
        Code::ArrowLeft => VK_LEFT,
        Code::ArrowUp => VK_UP,
        Code::ArrowRight => VK_RIGHT,
        Code::ArrowDown => VK_DOWN,
        Code::PrintScreen => VK_SNAPSHOT,
        Code::Insert => VK_INSERT,
        Code::Delete => VK_DELETE,
        Code::F1 => VK_F1,
        Code::F2 => VK_F2,
        Code::F3 => VK_F3,
        Code::F4 => VK_F4,
        Code::F5 => VK_F5,
        Code::F6 => VK_F6,
        Code::F7 => VK_F7,
        Code::F8 => VK_F8,
        Code::F9 => VK_F9,
        Code::F10 => VK_F10,
        Code::F11 => VK_F11,
        Code::F12 => VK_F12,
        Code::F13 => VK_F13,
        Code::F14 => VK_F14,
        Code::F15 => VK_F15,
        Code::F16 => VK_F16,
        Code::F17 => VK_F17,
        Code::F18 => VK_F18,
        Code::F19 => VK_F19,
        Code::F20 => VK_F20,
        Code::F21 => VK_F21,
        Code::F22 => VK_F22,
        Code::F23 => VK_F23,
        Code::F24 => VK_F24,
        Code::Numpad0 => VK_NUMPAD0,
        Code::Numpad1 => VK_NUMPAD1,
        Code::Numpad2 => VK_NUMPAD2,
        Code::Numpad3 => VK_NUMPAD3,
        Code::Numpad4 => VK_NUMPAD4,
        Code::Numpad5 => VK_NUMPAD5,
        Code::Numpad6 => VK_NUMPAD6,
        Code::Numpad7 => VK_NUMPAD7,
        Code::Numpad8 => VK_NUMPAD8,
        Code::Numpad9 => VK_NUMPAD9,
        Code::NumpadAdd => VK_ADD,
        Code::NumpadDecimal => VK_DECIMAL,
        Code::NumpadDivide => VK_DIVIDE,
        Code::NumpadMultiply => VK_MULTIPLY,
        Code::NumpadSubtract => VK_SUBTRACT,
        Code::Pause => VK_PAUSE,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn keys_match_the_regular_shortcuts() {
        let keys = [
            "KeyA", "KeyH", "KeyZ", "Digit0", "Digit5", "F1", "F5", "F10", "F12", "F20", "Space", "Enter", "Tab", "Backspace", "Delete", "Insert", "Home", "End", "PageUp", "PageDown",
            "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Minus", "Equal", "Comma", "Period", "Slash", "Backslash", "Semicolon", "Quote", "Backquote", "BracketLeft", "BracketRight",
        ];
        for k in keys {
            let hk = HotKey::from_str(&format!("alt+{k}")).unwrap();
            assert!(vk(hk.key).is_some(), "{k}");
        }
        assert_eq!(vk(Code::F10), Some(VK_F10));
        assert_eq!(mods(HotKey::from_str("alt+F10").unwrap().mods), Mods { alt: true, ..Default::default() });
        assert_eq!(mods(HotKey::from_str("shift+super+KeyS").unwrap().mods), Mods { shift: true, win: true, ..Default::default() });
    }
}
