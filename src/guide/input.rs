//! Seeing the user's clicks and key presses anywhere on screen while a guide
//! records. macOS: a Core Graphics event tap, which needs the Accessibility
//! permission (the same one scrolling capture uses). Windows: low-level
//! mouse and keyboard hooks.
//!
//! Events are only passed on, never changed or held up: each callback does
//! nothing slower than a channel send.

use std::sync::mpsc::Sender;

use anyhow::Result;

use super::Button;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Mods {
    /// ⌘ on macOS, the Windows key on Windows.
    pub cmd: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Mods {
    /// A shortcut rather than typing: ⌘ or Ctrl (or ⌥/Alt without text) held.
    pub fn is_shortcut(self) -> bool {
        self.cmd || self.ctrl
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Key {
    /// A key that types: what it typed (Shift and the keyboard layout applied).
    Text(String),
    Enter,
    Tab,
    Escape,
    Backspace,
    Delete,
    /// Arrows, Home, End, Page Up/Down.
    Nav,
    /// Function keys, F1 = 1.
    F(u8),
    Other,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Input {
    /// A mouse button went down at a global position, in the units the
    /// platform reports the cursor in (`capture::to_logical` makes points).
    Down { x: f64, y: f64, button: Button },
    Key {
        key: Key,
        mods: Mods,
        /// How the key is labelled, for naming a shortcut (`C` in ⌘C).
        name: String,
        /// Held down and repeating.
        repeat: bool,
    },
}

/// Shortcut display, e.g. `⌃⌥⇧⌘K` on macOS or `Ctrl+Alt+K` on Windows.
pub fn combo(mods: Mods, name: &str) -> String {
    let mut s = String::new();
    if cfg!(target_os = "macos") {
        for (on, sym) in [(mods.ctrl, "⌃"), (mods.alt, "⌥"), (mods.shift, "⇧"), (mods.cmd, "⌘")] {
            if on {
                s.push_str(sym);
            }
        }
    } else {
        for (on, sym) in [(mods.ctrl, "Ctrl+"), (mods.cmd, "Win+"), (mods.alt, "Alt+"), (mods.shift, "Shift+")] {
            if on {
                s.push_str(sym);
            }
        }
    }
    s.push_str(name);
    s
}

/// Listens until dropped.
pub struct Listener {
    #[cfg(target_os = "macos")]
    inner: mac::Tap,
    #[cfg(windows)]
    inner: win::Hooks,
}

/// Start passing every click and key press to `tx`.
pub fn listen(tx: Sender<Input>) -> Result<Listener> {
    #[cfg(target_os = "macos")]
    return Ok(Listener { inner: mac::Tap::start(tx)? });
    #[cfg(windows)]
    return Ok(Listener { inner: win::Hooks::start(tx)? });
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = tx;
        anyhow::bail!("recording guides isn't supported on this system")
    }
}

impl Listener {
    pub fn stop(self) {
        drop(self.inner);
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
    use std::sync::mpsc::{self, Sender};
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};

    use anyhow::{Result, anyhow};

    use super::{Button, Input, Key, Mods};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    type Callback = unsafe extern "C" fn(proxy: *mut c_void, ty: u32, event: *mut c_void, user: *mut c_void) -> *mut c_void;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventTapCreate(tap: u32, place: u32, options: u32, events: u64, callback: Callback, user: *mut c_void) -> *mut c_void;
        fn CGEventTapEnable(tap: *mut c_void, enable: bool);
        fn CGEventGetLocation(event: *const c_void) -> CGPoint;
        fn CGEventGetFlags(event: *const c_void) -> u64;
        fn CGEventGetIntegerValueField(event: *const c_void, field: u32) -> i64;
        fn CGEventKeyboardGetUnicodeString(event: *const c_void, max: usize, actual: *mut usize, chars: *mut u16);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: *const c_void;
        fn CFMachPortCreateRunLoopSource(alloc: *const c_void, port: *mut c_void, order: isize) -> *mut c_void;
        fn CFMachPortInvalidate(port: *mut c_void);
        fn CFRunLoopGetCurrent() -> *mut c_void;
        fn CFRunLoopAddSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
        fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source_handled: bool) -> i32;
        fn CFRelease(cf: *const c_void);
    }

    const SESSION_TAP: u32 = 1;
    const HEAD_INSERT: u32 = 0;
    /// An active tap (it returns every event untouched): a listen-only tap
    /// would need the separate Input Monitoring permission for key presses.
    const TAP_DEFAULT: u32 = 0;
    const LEFT_DOWN: u32 = 1;
    const RIGHT_DOWN: u32 = 3;
    const KEY_DOWN: u32 = 10;
    const OTHER_DOWN: u32 = 25;
    const DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
    const DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
    const FIELD_BUTTON: u32 = 3;
    const FIELD_AUTOREPEAT: u32 = 8;
    const FIELD_KEYCODE: u32 = 9;

    struct Context {
        tx: Sender<Input>,
        tap: AtomicPtr<c_void>,
    }

    pub struct Tap {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Tap {
        pub fn start(tx: Sender<Input>) -> Result<Tap> {
            let stop = Arc::new(AtomicBool::new(false));
            let (ready_tx, ready_rx) = mpsc::channel::<bool>();
            let stop_c = stop.clone();
            let thread = thread::spawn(move || {
                let mask = (1u64 << LEFT_DOWN) | (1 << RIGHT_DOWN) | (1 << OTHER_DOWN) | (1 << KEY_DOWN);
                let context = Box::into_raw(Box::new(Context { tx, tap: AtomicPtr::new(std::ptr::null_mut()) }));
                // SAFETY: the tap, its run-loop source and the context live on
                // this thread and are released here, after the tap is
                // invalidated, so the callback never sees a freed context.
                unsafe {
                    let tap = CGEventTapCreate(SESSION_TAP, HEAD_INSERT, TAP_DEFAULT, mask, callback, context.cast());
                    if tap.is_null() {
                        drop(Box::from_raw(context));
                        let _ = ready_tx.send(false);
                        return;
                    }
                    (*context).tap.store(tap, Ordering::Release);
                    let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
                    CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
                    CGEventTapEnable(tap, true);
                    let _ = ready_tx.send(true);
                    while !stop_c.load(Ordering::Relaxed) {
                        CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.1, false);
                    }
                    CGEventTapEnable(tap, false);
                    CFMachPortInvalidate(tap);
                    CFRelease(source);
                    CFRelease(tap);
                    drop(Box::from_raw(context));
                }
            });
            if ready_rx.recv().unwrap_or(false) {
                Ok(Tap { stop, thread: Some(thread) })
            } else {
                let _ = thread.join();
                Err(anyhow!("Rega can't see clicks and typing — allow it under Accessibility in System Settings"))
            }
        }
    }

    impl Drop for Tap {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    unsafe extern "C" fn callback(_proxy: *mut c_void, ty: u32, event: *mut c_void, user: *mut c_void) -> *mut c_void {
        // An unwinding panic must not cross into Core Graphics.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: `user` is the context, alive while the tap is; `event`
            // is valid for the length of this call.
            unsafe {
                let context = &*(user as *const Context);
                if let Some(input) = read(ty, event, context) {
                    let _ = context.tx.send(input);
                }
            }
        }));
        event
    }

    /// SAFETY: `event` must be the event a tap callback was handed.
    unsafe fn read(ty: u32, event: *mut c_void, context: &Context) -> Option<Input> {
        unsafe {
            match ty {
                DISABLED_BY_TIMEOUT | DISABLED_BY_USER_INPUT => {
                    // The system switches a tap off when it's slow to answer;
                    // switch it straight back on.
                    let tap = context.tap.load(Ordering::Acquire);
                    if !tap.is_null() {
                        CGEventTapEnable(tap, true);
                    }
                    None
                }
                LEFT_DOWN | RIGHT_DOWN | OTHER_DOWN => {
                    let p = CGEventGetLocation(event);
                    let button = match ty {
                        LEFT_DOWN => Button::Left,
                        RIGHT_DOWN => Button::Right,
                        _ if CGEventGetIntegerValueField(event, FIELD_BUTTON) == 2 => Button::Middle,
                        _ => return None,
                    };
                    Some(Input::Down { x: p.x, y: p.y, button })
                }
                KEY_DOWN => {
                    let code = CGEventGetIntegerValueField(event, FIELD_KEYCODE) as u16;
                    let flags = CGEventGetFlags(event);
                    let mods = Mods {
                        shift: flags & 0x0002_0000 != 0,
                        ctrl: flags & 0x0004_0000 != 0,
                        alt: flags & 0x0008_0000 != 0,
                        cmd: flags & 0x0010_0000 != 0,
                    };
                    let repeat = CGEventGetIntegerValueField(event, FIELD_AUTOREPEAT) != 0;
                    let mut buf = [0u16; 8];
                    let mut len = 0usize;
                    CGEventKeyboardGetUnicodeString(event, buf.len(), &mut len, buf.as_mut_ptr());
                    let text = String::from_utf16_lossy(&buf[..len.min(buf.len())]);
                    let (key, name) = classify(code, text, mods);
                    Some(Input::Key { key, mods, name, repeat })
                }
                _ => None,
            }
        }
    }

    /// The key behind a macOS virtual key code, and its label.
    fn classify(code: u16, text: String, mods: Mods) -> (Key, String) {
        let named = |key: Key, name: &str| (key, name.to_owned());
        match code {
            36 | 76 => named(Key::Enter, "Return"),
            48 => named(Key::Tab, "Tab"),
            53 => named(Key::Escape, "Esc"),
            51 => named(Key::Backspace, "Delete"),
            117 => named(Key::Delete, "Forward Delete"),
            123 => named(Key::Nav, "←"),
            124 => named(Key::Nav, "→"),
            125 => named(Key::Nav, "↓"),
            126 => named(Key::Nav, "↑"),
            115 => named(Key::Nav, "Home"),
            119 => named(Key::Nav, "End"),
            116 => named(Key::Nav, "Page Up"),
            121 => named(Key::Nav, "Page Down"),
            _ => {
                if let Some(n) = F_KEYS.iter().position(|&c| c == code) {
                    return (Key::F(n as u8 + 1), format!("F{}", n + 1));
                }
                let printable = !text.is_empty() && !text.chars().any(char::is_control);
                // In a shortcut, the key as labelled: ⌥ and ⌃ change what a
                // key types (⌥A types å) but not what the shortcut is called.
                let name = match (mods.is_shortcut() || mods.alt, us_label(code)) {
                    (true, Some(label)) => label.to_owned(),
                    _ if text == " " => "Space".to_owned(),
                    _ if printable => text.to_uppercase(),
                    _ => us_label(code).unwrap_or("Key").to_owned(),
                };
                if printable { (Key::Text(text), name) } else { (Key::Other, name) }
            }
        }
    }

    /// F1…F12 key codes.
    const F_KEYS: [u16; 12] = [122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111];

    /// What the key at a code is labelled on a US keyboard.
    fn us_label(code: u16) -> Option<&'static str> {
        const LABELS: [&str; 51] = [
            "A", "S", "D", "F", "H", "G", "Z", "X", "C", "V", "§", "B", "Q", "W", "E", "R", "Y", "T", "1", "2", "3", "4", "6", "5", "=", "9", "7",
            "-", "8", "0", "]", "O", "U", "[", "I", "P", "", "L", "J", "'", "K", ";", "\\", ",", "/", "N", "M", ".", "", "Space", "`",
        ];
        LABELS.get(code as usize).copied().filter(|l| !l.is_empty())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn keys_are_named_as_labelled() {
            let none = Mods::default();
            let cmd = Mods { cmd: true, ..none };
            let alt = Mods { alt: true, ..none };
            assert_eq!(classify(8, "c".into(), cmd), (Key::Text("c".into()), "C".into()));
            assert_eq!(classify(0, "å".into(), alt), (Key::Text("å".into()), "A".into()), "⌥A types å, named A");
            assert_eq!(classify(8, "\u{3}".into(), Mods { ctrl: true, ..none }), (Key::Other, "C".into()));
            assert_eq!(classify(49, " ".into(), none), (Key::Text(" ".into()), "Space".into()));
            assert_eq!(classify(36, "\r".into(), none).0, Key::Enter);
            assert_eq!(classify(96, String::new(), none), (Key::F(5), "F5".into()));
            // A Hebrew layout types ש on the A key.
            assert_eq!(classify(0, "ש".into(), none), (Key::Text("ש".into()), "ש".into()));
        }
    }
}

#[cfg(windows)]
mod win {
    use std::sync::Mutex;
    use std::sync::mpsc::{self, Sender};
    use std::thread::{self, JoinHandle};

    use anyhow::{Result, anyhow};
    use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, GetKeyState, GetKeyboardLayout, ToUnicodeEx};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE,
        PeekMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_LBUTTONDOWN,
        WM_MBUTTONDOWN, WM_QUIT, WM_RBUTTONDOWN, WM_SYSKEYDOWN,
    };
    use windows::core::PCWSTR;

    use super::{Button, Input, Key, Mods};

    /// Hook procedures get no context pointer; one guide records at a time.
    static SINK: Mutex<Option<Sender<Input>>> = Mutex::new(None);
    /// The last key down, to tell a held key's repeats from new presses.
    static LAST_KEY: Mutex<u32> = Mutex::new(0);

    pub struct Hooks {
        thread_id: u32,
        thread: Option<JoinHandle<()>>,
    }

    impl Hooks {
        pub fn start(tx: Sender<Input>) -> Result<Hooks> {
            *SINK.lock().unwrap() = Some(tx);
            let (ready_tx, ready_rx) = mpsc::channel::<Option<u32>>();
            let thread = thread::spawn(move || {
                // SAFETY: plain Win32 calls; the hooks are removed on this
                // thread before it ends.
                unsafe {
                    let mut msg = MSG::default();
                    // Gives the thread its message queue, so stopping can post to it.
                    let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                    let module = GetModuleHandleW(PCWSTR::null()).ok().map(|m| HINSTANCE(m.0));
                    let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0);
                    let keys = SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_proc), module, 0);
                    let (mouse, keys) = match (mouse, keys) {
                        (Ok(mouse), Ok(keys)) => (mouse, keys),
                        (mouse, keys) => {
                            for hook in [mouse, keys].into_iter().flatten() {
                                let _ = UnhookWindowsHookEx(hook);
                            }
                            let _ = ready_tx.send(None);
                            return;
                        }
                    };
                    let _ = ready_tx.send(Some(GetCurrentThreadId()));
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
                    let _ = UnhookWindowsHookEx(mouse);
                    let _ = UnhookWindowsHookEx(keys);
                }
            });
            match ready_rx.recv().ok().flatten() {
                Some(thread_id) => Ok(Hooks { thread_id, thread: Some(thread) }),
                None => {
                    let _ = thread.join();
                    *SINK.lock().unwrap() = None;
                    Err(anyhow!("couldn't watch the mouse and keyboard"))
                }
            }
        }
    }

    impl Drop for Hooks {
        fn drop(&mut self) {
            // SAFETY: posts WM_QUIT to the hook thread's queue.
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
            *SINK.lock().unwrap() = None;
        }
    }

    fn send(input: Input) {
        if let Ok(sink) = SINK.lock()
            && let Some(tx) = sink.as_ref()
        {
            let _ = tx.send(input);
        }
    }

    unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 {
            let button = match wparam.0 as u32 {
                WM_LBUTTONDOWN => Some(Button::Left),
                WM_RBUTTONDOWN => Some(Button::Right),
                WM_MBUTTONDOWN => Some(Button::Middle),
                _ => None,
            };
            if let Some(button) = button {
                // SAFETY: for mouse messages, lparam points at an MSLLHOOKSTRUCT.
                let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
                send(Input::Down { x: info.pt.x as f64, y: info.pt.y as f64, button });
            }
        }
        // SAFETY: passes the event on unchanged.
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    unsafe extern "system" fn key_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let message = wparam.0 as u32;
        if code >= 0 && (message == WM_KEYDOWN || message == WM_SYSKEYDOWN) {
            // SAFETY: for key messages, lparam points at a KBDLLHOOKSTRUCT.
            let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let _ = std::panic::catch_unwind(|| {
                if let Some(input) = read_key(info.vkCode, info.scanCode) {
                    send(input);
                }
            });
        }
        // SAFETY: passes the event on unchanged.
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    fn down(vk: i32) -> bool {
        // SAFETY: plain query.
        unsafe { GetAsyncKeyState(vk) < 0 }
    }

    fn read_key(vk: u32, scan: u32) -> Option<Input> {
        // Modifier keys on their own (Shift, Ctrl, Alt, Windows, Caps Lock).
        if matches!(vk, 0x10..=0x12 | 0x14 | 0x5B | 0x5C | 0xA0..=0xA5) {
            return None;
        }
        let repeat = {
            let mut last = LAST_KEY.lock().ok()?;
            let repeat = *last == vk;
            *last = vk;
            repeat
        };
        let mut mods = Mods { ctrl: down(0x11), alt: down(0x12), shift: down(0x10), cmd: down(0x5B) || down(0x5C) };
        let named = |key: Key, name: &str| Some((key, name.to_owned()));
        let special = match vk {
            0x0D => named(Key::Enter, "Enter"),
            0x09 => named(Key::Tab, "Tab"),
            0x1B => named(Key::Escape, "Esc"),
            0x08 => named(Key::Backspace, "Backspace"),
            0x2E => named(Key::Delete, "Delete"),
            0x25 => named(Key::Nav, "←"),
            0x26 => named(Key::Nav, "↑"),
            0x27 => named(Key::Nav, "→"),
            0x28 => named(Key::Nav, "↓"),
            0x24 => named(Key::Nav, "Home"),
            0x23 => named(Key::Nav, "End"),
            0x21 => named(Key::Nav, "Page Up"),
            0x22 => named(Key::Nav, "Page Down"),
            0x70..=0x87 => Some((Key::F((vk - 0x6F) as u8), format!("F{}", vk - 0x6F))),
            _ => None,
        };
        let (key, name) = match special {
            Some(found) => found,
            None => {
                let text = typed_text(vk, scan, mods);
                let printable = !text.is_empty() && !text.chars().any(char::is_control);
                // AltGr arrives as Ctrl+Alt: with it, keys type (@, €, …).
                if printable && mods.ctrl && mods.alt {
                    mods.ctrl = false;
                    mods.alt = false;
                }
                let label = match vk {
                    0x30..=0x39 | 0x41..=0x5A => Some(char::from(vk as u8).to_string()),
                    0x20 => Some("Space".to_owned()),
                    _ => None,
                };
                let name = match label {
                    Some(l) if mods.is_shortcut() || mods.alt => l,
                    _ if printable => text.to_uppercase(),
                    Some(l) => l,
                    None => "Key".to_owned(),
                };
                if printable { (Key::Text(text), name) } else { (Key::Other, name) }
            }
        };
        Some(Input::Key { key, mods, name, repeat })
    }

    /// What the key types in the foreground window's keyboard layout, with
    /// Shift, Caps Lock and AltGr applied but not Ctrl alone.
    fn typed_text(vk: u32, scan: u32, mods: Mods) -> String {
        let mut state = [0u8; 256];
        if mods.shift {
            state[0x10] = 0x80;
        }
        // SAFETY: plain queries and a ToUnicodeEx that leaves the keyboard state alone (flag 4).
        unsafe {
            if GetKeyState(0x14) & 1 != 0 {
                state[0x14] = 1;
            }
            if mods.ctrl && mods.alt {
                state[0x11] = 0x80;
                state[0x12] = 0x80;
            }
            let layout = GetKeyboardLayout(GetWindowThreadProcessId(GetForegroundWindow(), None));
            let mut buf = [0u16; 8];
            let n = ToUnicodeEx(vk, scan, &state, &mut buf, 4, Some(layout));
            if n > 0 { String::from_utf16_lossy(&buf[..n as usize]) } else { String::new() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_are_spelled_per_platform() {
        let mods = Mods { cmd: true, shift: true, ..Default::default() };
        if cfg!(target_os = "macos") {
            assert_eq!(combo(mods, "S"), "⇧⌘S");
            assert_eq!(combo(Mods { ctrl: true, alt: true, ..Default::default() }, "G"), "⌃⌥G");
        } else {
            assert_eq!(combo(mods, "S"), "Win+Shift+S");
        }
    }
}
