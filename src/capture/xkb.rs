//! Minimal safe wrapper over libxkbcommon: keymap, state, key → character.
//!
//! The compositor turns evdev keycodes into characters with xkbcommon, using
//! the user's layout. Running the same library over the same keycodes, with
//! one modifier state shared by all keyboards (as the compositor keeps it),
//! gives the characters the apps actually received.
//!
//! We link the runtime library `libxkbcommon.so.0` directly, by its verbatim
//! file name. Every Wayland desktop ships it, so building `brain` needs no
//! `-dev` package.

use crate::event::Mod;
use anyhow::{Result, bail};
use std::ffi::{CString, c_char, c_int, c_void};
use std::ptr::NonNull;

#[allow(non_camel_case_types)]
type xkb_keysym_t = u32;

#[repr(C)]
struct XkbRuleNames {
    rules: *const c_char,
    model: *const c_char,
    layout: *const c_char,
    variant: *const c_char,
    options: *const c_char,
}

const XKB_KEY_UP: c_int = 0;
const XKB_KEY_DOWN: c_int = 1;
const XKB_STATE_MODS_DEPRESSED: c_int = 1 << 0;
const XKB_STATE_MODS_LATCHED: c_int = 1 << 1;
const XKB_STATE_MODS_LOCKED: c_int = 1 << 2;
/// evdev keycodes are offset by 8 in the XKB keycode space.
const EVDEV_OFFSET: u32 = 8;

#[link(name = "libxkbcommon.so.0", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn xkb_context_new(flags: c_int) -> *mut c_void;
    fn xkb_context_unref(ctx: *mut c_void);
    fn xkb_keymap_new_from_names(
        ctx: *mut c_void,
        names: *const XkbRuleNames,
        flags: c_int,
    ) -> *mut c_void;
    fn xkb_keymap_unref(keymap: *mut c_void);
    fn xkb_keymap_key_repeats(keymap: *mut c_void, key: u32) -> c_int;
    fn xkb_keymap_num_layouts(keymap: *mut c_void) -> u32;
    fn xkb_state_new(keymap: *mut c_void) -> *mut c_void;
    fn xkb_state_unref(state: *mut c_void);
    fn xkb_state_update_key(state: *mut c_void, key: u32, direction: c_int) -> c_int;
    fn xkb_state_update_mask(
        state: *mut c_void,
        depressed_mods: u32,
        latched_mods: u32,
        locked_mods: u32,
        depressed_layout: u32,
        latched_layout: u32,
        locked_layout: u32,
    ) -> c_int;
    fn xkb_state_serialize_mods(state: *mut c_void, components: c_int) -> u32;
    fn xkb_state_key_get_one_sym(state: *mut c_void, key: u32) -> xkb_keysym_t;
    fn xkb_state_mod_name_is_active(state: *mut c_void, name: *const c_char, kind: c_int) -> c_int;
    fn xkb_keysym_to_utf32(keysym: xkb_keysym_t) -> u32;
    fn xkb_keysym_get_name(keysym: xkb_keysym_t, buffer: *mut c_char, size: usize) -> c_int;
}

/// Which keyboard layout(s) to compile, in XKB "rule names" terms.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LayoutSpec {
    /// Comma-separated, for example `"us,il"`. Empty means the XKB default (`us`).
    pub layout: String,
    /// Comma-separated, positionally matching `layout`.
    pub variant: String,
    pub model: String,
    pub options: String,
}

/// A compiled keymap plus its live state. Not thread-safe (xkbcommon objects
/// are not), so it is owned by the capture thread alone.
pub struct Keyboard {
    ctx: NonNull<c_void>,
    keymap: NonNull<c_void>,
    state: NonNull<c_void>,
    mod_names: [(Mod, CString); 5],
}

// SAFETY: the xkb objects are uniquely owned by this struct and only ever used
// through `&mut self` or `&self` on one thread at a time; moving them between
// threads is fine (xkbcommon has no thread-local state).
unsafe impl Send for Keyboard {}

impl Keyboard {
    pub fn new(spec: &LayoutSpec) -> Result<Self> {
        // SAFETY: plain FFI constructor; null is checked.
        let ctx = NonNull::new(unsafe { xkb_context_new(0) })
            .ok_or_else(|| anyhow::anyhow!("xkb_context_new failed"))?;
        let to_c = |s: &str| CString::new(s).map_err(|_| anyhow::anyhow!("NUL in layout spec"));
        let (rules, model, layout, variant, options) = (
            to_c("evdev")?,
            to_c(&spec.model)?,
            to_c(&spec.layout)?,
            to_c(&spec.variant)?,
            to_c(&spec.options)?,
        );
        let nullable = |c: &CString| {
            if c.as_bytes().is_empty() {
                std::ptr::null()
            } else {
                c.as_ptr()
            }
        };
        let names = XkbRuleNames {
            rules: rules.as_ptr(),
            model: nullable(&model),
            layout: nullable(&layout),
            variant: nullable(&variant),
            options: nullable(&options),
        };
        // SAFETY: `names` and the CStrings it points to outlive the call.
        let keymap = unsafe { xkb_keymap_new_from_names(ctx.as_ptr(), &names, 0) };
        let Some(keymap) = NonNull::new(keymap) else {
            // SAFETY: ctx is valid and owned.
            unsafe { xkb_context_unref(ctx.as_ptr()) };
            bail!("could not compile XKB keymap for {spec:?}");
        };
        // SAFETY: keymap is valid.
        let Some(state) = NonNull::new(unsafe { xkb_state_new(keymap.as_ptr()) }) else {
            unsafe {
                xkb_keymap_unref(keymap.as_ptr());
                xkb_context_unref(ctx.as_ptr());
            }
            bail!("xkb_state_new failed");
        };
        let mod_names = [
            (Mod::Shift, CString::new("Shift").unwrap()),
            (Mod::Ctrl, CString::new("Control").unwrap()),
            (Mod::Alt, CString::new("Mod1").unwrap()),
            (Mod::Super, CString::new("Mod4").unwrap()),
            (Mod::Altgr, CString::new("Mod5").unwrap()),
        ];
        Ok(Self {
            ctx,
            keymap,
            state,
            mod_names,
        })
    }

    /// Feeds a key press (`down = true`) or release into the state machine.
    pub fn update_key(&mut self, evdev_code: u16, down: bool) {
        let dir = if down { XKB_KEY_DOWN } else { XKB_KEY_UP };
        // SAFETY: state is valid; any keycode is accepted.
        unsafe { xkb_state_update_key(self.state.as_ptr(), evdev_code as u32 + EVDEV_OFFSET, dir) };
    }

    /// The keysym the key produces in the current state (before this key's own
    /// press is applied, which matters only for modifier keys).
    pub fn keysym(&self, evdev_code: u16) -> u32 {
        // SAFETY: state is valid.
        unsafe { xkb_state_key_get_one_sym(self.state.as_ptr(), evdev_code as u32 + EVDEV_OFFSET) }
    }

    pub fn key_repeats(&self, evdev_code: u16) -> bool {
        // SAFETY: keymap is valid.
        unsafe {
            xkb_keymap_key_repeats(self.keymap.as_ptr(), evdev_code as u32 + EVDEV_OFFSET) != 0
        }
    }

    /// Held or latched modifiers (not locked ones: Caps Lock is already
    /// reflected in the resolved character).
    pub fn active_mods(&self) -> Vec<Mod> {
        let mut out = Vec::new();
        for (m, name) in &self.mod_names {
            // SAFETY: state and the C string are valid.
            let active = unsafe {
                xkb_state_mod_name_is_active(
                    self.state.as_ptr(),
                    name.as_ptr(),
                    XKB_STATE_MODS_DEPRESSED | XKB_STATE_MODS_LATCHED,
                )
            };
            if active > 0 {
                out.push(*m);
            }
        }
        out
    }

    pub fn num_layouts(&self) -> u32 {
        // SAFETY: keymap is valid.
        unsafe { xkb_keymap_num_layouts(self.keymap.as_ptr()) }
    }

    /// Locks the active layout group (for example when KDE's layout switcher
    /// changes it). Modifier state is preserved.
    pub fn set_layout_index(&mut self, index: u32) {
        if index >= self.num_layouts() {
            return;
        }
        let s = self.state.as_ptr();
        // SAFETY: state is valid.
        unsafe {
            let dep = xkb_state_serialize_mods(s, XKB_STATE_MODS_DEPRESSED);
            let lat = xkb_state_serialize_mods(s, XKB_STATE_MODS_LATCHED);
            let lock = xkb_state_serialize_mods(s, XKB_STATE_MODS_LOCKED);
            xkb_state_update_mask(s, dep, lat, lock, 0, 0, index);
        }
    }
}

impl Drop for Keyboard {
    fn drop(&mut self) {
        // SAFETY: each pointer is owned and released exactly once.
        unsafe {
            xkb_state_unref(self.state.as_ptr());
            xkb_keymap_unref(self.keymap.as_ptr());
            xkb_context_unref(self.ctx.as_ptr());
        }
    }
}

/// Keysyms of keys that only modify others. They produce no event of their own.
pub fn is_modifier_keysym(sym: u32) -> bool {
    // Shift_L..Hyper_R (0xffe1..=0xffee) covers Shift, Control, Caps/Shift Lock,
    // Meta, Alt, Super, Hyper. 0xfe01..=0xfe13 is the ISO Lock group
    // (ISO_Level3_Shift = AltGr, ISO_Level5_Shift, group latches/locks...).
    // Num_Lock (0xff7f) and Mode_switch (0xff7e) are also pure state keys.
    matches!(sym, 0xffe1..=0xffee | 0xfe01..=0xfe13 | 0xff7e | 0xff7f)
}

/// The `key` string for a keysym: the character for printable keys, otherwise
/// a stable name. Returns `None` for keys that should not be recorded (pure
/// modifiers and NoSymbol).
pub fn key_name(sym: u32) -> Option<String> {
    if sym == 0 || is_modifier_keysym(sym) {
        return None;
    }
    // SAFETY: pure function.
    let cp = unsafe { xkb_keysym_to_utf32(sym) };
    if cp >= 0x20
        && cp != 0x7f
        && !(0x80..0xa0).contains(&cp)
        && let Some(c) = char::from_u32(cp)
    {
        return Some(c.to_string());
    }
    let raw = raw_keysym_name(sym)?;
    Some(
        normalize_keysym_name(&raw)
            .map(str::to_string)
            .unwrap_or(raw),
    )
}

fn raw_keysym_name(sym: u32) -> Option<String> {
    let mut buf = [0 as c_char; 64];
    // SAFETY: the buffer is valid for `buf.len()` bytes; xkb NUL-terminates.
    let n = unsafe { xkb_keysym_get_name(sym, buf.as_mut_ptr(), buf.len()) };
    if n <= 0 {
        return None;
    }
    // SAFETY: NUL-terminated by xkb_keysym_get_name on success.
    let s = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    Some(s.to_string_lossy().into_owned())
}

/// Maps XKB's names for editing keys to the short names used in the log.
/// Keypad variants (numlock off) collapse onto the main keys.
fn normalize_keysym_name(raw: &str) -> Option<&'static str> {
    Some(match raw {
        "Return" | "KP_Enter" | "ISO_Enter" => "Enter",
        "BackSpace" => "Backspace",
        "Delete" | "KP_Delete" => "Delete",
        "Tab" | "ISO_Left_Tab" | "KP_Tab" => "Tab",
        "Escape" => "Escape",
        "Left" | "KP_Left" => "Left",
        "Right" | "KP_Right" => "Right",
        "Up" | "KP_Up" => "Up",
        "Down" | "KP_Down" => "Down",
        "Home" | "KP_Home" => "Home",
        "End" | "KP_End" => "End",
        "Prior" | "Page_Up" | "KP_Prior" | "KP_Page_Up" => "PageUp",
        "Next" | "Page_Down" | "KP_Next" | "KP_Page_Down" => "PageDown",
        "Insert" | "KP_Insert" => "Insert",
        "KP_Begin" => "Begin",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // evdev codes (linux/input-event-codes.h)
    const KEY_H: u16 = 35;
    const KEY_A: u16 = 30;
    const KEY_1: u16 = 2;
    const KEY_ENTER: u16 = 28;
    const KEY_BACKSPACE: u16 = 14;
    const KEY_LEFTSHIFT: u16 = 42;
    const KEY_LEFTCTRL: u16 = 29;
    const KEY_CAPSLOCK: u16 = 58;
    const KEY_SPACE: u16 = 57;
    const KEY_TAB: u16 = 15;

    fn us() -> Keyboard {
        Keyboard::new(&LayoutSpec {
            layout: "us".into(),
            ..Default::default()
        })
        .unwrap()
    }

    fn press(kb: &mut Keyboard, code: u16) -> (Option<String>, Vec<Mod>) {
        let name = key_name(kb.keysym(code));
        let mods = kb.active_mods();
        kb.update_key(code, true);
        (name, mods)
    }

    #[test]
    fn plain_and_shifted_letters() {
        let mut kb = us();
        assert_eq!(press(&mut kb, KEY_H), (Some("h".into()), vec![]));
        kb.update_key(KEY_H, false);
        kb.update_key(KEY_LEFTSHIFT, true);
        assert_eq!(press(&mut kb, KEY_H), (Some("H".into()), vec![Mod::Shift]));
        kb.update_key(KEY_H, false);
        assert_eq!(press(&mut kb, KEY_1), (Some("!".into()), vec![Mod::Shift]));
    }

    #[test]
    fn caps_lock_is_reflected_in_char_not_mods() {
        let mut kb = us();
        kb.update_key(KEY_CAPSLOCK, true);
        kb.update_key(KEY_CAPSLOCK, false);
        assert_eq!(press(&mut kb, KEY_A), (Some("A".into()), vec![]));
    }

    #[test]
    fn ctrl_combo_keeps_the_letter() {
        let mut kb = us();
        kb.update_key(KEY_LEFTCTRL, true);
        assert_eq!(press(&mut kb, KEY_A), (Some("a".into()), vec![Mod::Ctrl]));
    }

    #[test]
    fn named_keys() {
        let mut kb = us();
        assert_eq!(press(&mut kb, KEY_ENTER).0.as_deref(), Some("Enter"));
        assert_eq!(
            press(&mut kb, KEY_BACKSPACE).0.as_deref(),
            Some("Backspace")
        );
        assert_eq!(press(&mut kb, KEY_SPACE).0.as_deref(), Some(" "));
        assert_eq!(press(&mut kb, KEY_TAB).0.as_deref(), Some("Tab"));
        kb.update_key(KEY_LEFTSHIFT, true);
        assert_eq!(
            press(&mut kb, KEY_TAB).0.as_deref(),
            Some("Tab"),
            "ISO_Left_Tab"
        );
    }

    #[test]
    fn modifiers_are_not_keys() {
        let kb = us();
        assert_eq!(key_name(kb.keysym(KEY_LEFTSHIFT)), None);
        assert_eq!(key_name(kb.keysym(KEY_LEFTCTRL)), None);
        assert_eq!(key_name(kb.keysym(KEY_CAPSLOCK)), None);
        assert!(!kb.key_repeats(KEY_LEFTSHIFT));
        assert!(kb.key_repeats(KEY_A));
    }

    #[test]
    fn layout_switching() {
        // Hebrew layout as second group: KEY_A produces shin (U+05E9).
        let mut kb = Keyboard::new(&LayoutSpec {
            layout: "us,il".into(),
            variant: ",".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(kb.num_layouts(), 2);
        assert_eq!(key_name(kb.keysym(KEY_A)).as_deref(), Some("a"));
        kb.set_layout_index(1);
        assert_eq!(key_name(kb.keysym(KEY_A)).as_deref(), Some("ש"));
        kb.set_layout_index(7); // out of range: ignored
        assert_eq!(key_name(kb.keysym(KEY_A)).as_deref(), Some("ש"));
    }
}
