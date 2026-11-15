//! Turns raw evdev key transitions into the key events apps actually received.
//!
//! This is pure logic with no I/O, so it is driven directly by unit tests:
//! * characters and modifiers are resolved with xkbcommon ([`super::xkb`]),
//! * auto-repeat is synthesized on the compositor's schedule ([`Repeater`]).
//!   The kernel's own repeat events (value 2) run on a different timer than
//!   the one Wayland clients use, so they are ignored,
//! * per-device held-key tracking releases keys when a device disappears, so
//!   a modifier can never get stuck "down".

use super::xkb::{Keyboard, key_name};
use crate::event::Mod;
use crate::timefmt::Millis;
use std::collections::{HashMap, HashSet};

/// Compositor key-repeat settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RepeatConfig {
    pub enabled: bool,
    pub delay_ms: Millis,
    pub rate_hz: f64,
}

impl Default for RepeatConfig {
    /// KDE Plasma's defaults.
    fn default() -> Self {
        Self {
            enabled: true,
            delay_ms: 600,
            rate_hz: 25.0,
        }
    }
}

impl RepeatConfig {
    /// Reads `[Keyboard]` from KDE's `kcminputrc` text, falling back to defaults.
    pub fn from_kcminputrc(text: &str) -> Self {
        let mut cfg = Self::default();
        let mut in_keyboard = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                in_keyboard = line == "[Keyboard]";
                continue;
            }
            if !in_keyboard {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k.trim() {
                "KeyRepeat" => cfg.enabled = v.trim() == "repeat" || v.trim() == "true",
                "RepeatDelay" => {
                    if let Ok(d) = v.trim().parse::<f64>()
                        && d >= 0.0
                    {
                        cfg.delay_ms = d as Millis;
                    }
                }
                "RepeatRate" => {
                    if let Ok(r) = v.trim().parse::<f64>() {
                        if r > 0.0 {
                            cfg.rate_hz = r;
                        } else {
                            cfg.enabled = false;
                        }
                    }
                }
                _ => {}
            }
        }
        cfg
    }

    fn interval_ms(&self) -> f64 {
        1000.0 / self.rate_hz
    }
}

/// Synthesizes auto-repeat for the most recently pressed repeating key, the
/// same way Wayland clients do: after `delay_ms`, then every `1/rate_hz`.
#[derive(Debug)]
pub struct Repeater {
    cfg: RepeatConfig,
    held: Option<Held>,
}

#[derive(Debug, Clone, Copy)]
struct Held {
    code: u16,
    /// Time of the next repeat, fractional ms so rates like 30 Hz don't drift.
    next_at: f64,
}

impl Repeater {
    pub fn new(cfg: RepeatConfig) -> Self {
        Self { cfg, held: None }
    }

    pub fn on_press(&mut self, code: u16, ts: Millis) {
        if self.cfg.enabled {
            self.held = Some(Held {
                code,
                next_at: (ts + self.cfg.delay_ms) as f64,
            });
        }
    }

    pub fn on_release(&mut self, code: u16) {
        if self.held.is_some_and(|h| h.code == code) {
            self.held = None;
        }
    }

    pub fn cancel(&mut self) {
        self.held = None;
    }

    /// When the next repeat fires, if a key is held.
    pub fn deadline(&self) -> Option<Millis> {
        self.held.map(|h| h.next_at.ceil() as Millis)
    }

    /// Pops the next repeat due at or before `now`, as (keycode, timestamp).
    pub fn pop_due(&mut self, now: Millis) -> Option<(u16, Millis)> {
        let h = self.held.as_mut()?;
        if h.next_at > now as f64 {
            return None;
        }
        let ts = h.next_at.floor() as Millis;
        h.next_at += self.cfg.interval_ms();
        Some((h.code, ts))
    }
}

/// A resolved key event, ready to be recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyOut {
    pub ts: Millis,
    pub key: String,
    pub mods: Vec<Mod>,
    pub repeat: bool,
}

/// Device-independent key processing: one keymap state shared by all
/// keyboards, as in the compositor.
pub struct KeyProcessor {
    kb: Keyboard,
    repeater: Repeater,
    held: HashMap<u32, HashSet<u16>>, // device id -> held keycodes
}

impl KeyProcessor {
    pub fn new(kb: Keyboard, repeat: RepeatConfig) -> Self {
        Self {
            kb,
            repeater: Repeater::new(repeat),
            held: HashMap::new(),
        }
    }

    /// Replaces the keymap (layout list changed). Held keys are released first.
    pub fn replace_keyboard(&mut self, kb: Keyboard) {
        self.release_all();
        self.kb = kb;
    }

    pub fn set_layout_index(&mut self, index: u32) {
        self.kb.set_layout_index(index);
    }

    pub fn deadline(&self) -> Option<Millis> {
        self.repeater.deadline()
    }

    /// Emits repeats due at or before `now`.
    pub fn tick(&mut self, now: Millis, out: &mut impl FnMut(KeyOut)) {
        while let Some((code, ts)) = self.repeater.pop_due(now) {
            // Clients resolve each repeat against the *current* state, so
            // pressing Shift while holding `a` turns the repeats into `A`.
            let mods = self.kb.active_mods();
            if let Some(key) = key_name(self.kb.shortcut_keysym(code, &mods)) {
                out(KeyOut {
                    ts,
                    key,
                    mods,
                    repeat: true,
                });
            }
        }
    }

    /// Handles a key transition from device `dev`: `value` 1 = press, 0 = release.
    /// Kernel auto-repeat (`value` 2) is ignored; see the module docs.
    pub fn key(
        &mut self,
        dev: u32,
        code: u16,
        value: i32,
        ts: Millis,
        out: &mut impl FnMut(KeyOut),
    ) {
        // Repeats that fired before this transition come first.
        self.tick(ts, out);
        match value {
            1 => {
                let held = self.held.entry(dev).or_default();
                if !held.insert(code) {
                    return; // duplicate press without release
                }
                let mods = self.kb.active_mods();
                let sym = self.kb.shortcut_keysym(code, &mods);
                self.kb.update_key(code, true);
                if let Some(key) = key_name(sym) {
                    out(KeyOut {
                        ts,
                        key,
                        mods,
                        repeat: false,
                    });
                    if self.kb.key_repeats(code) {
                        self.repeater.on_press(code, ts);
                    }
                }
            }
            0 => {
                // A release for a key we never saw go down (held before the
                // device was opened) must not touch the modifier state.
                if self.held.get_mut(&dev).is_some_and(|h| h.remove(&code)) {
                    self.kb.update_key(code, false);
                    self.repeater.on_release(code);
                }
            }
            _ => {}
        }
    }

    /// Reconciles held keys for `dev` with the kernel's view (after SYN_DROPPED).
    pub fn resync(&mut self, dev: u32, pressed_now: &HashSet<u16>) {
        let held = self.held.entry(dev).or_default();
        let released: Vec<u16> = held.difference(pressed_now).copied().collect();
        let pressed: Vec<u16> = pressed_now.difference(held).copied().collect();
        for code in released {
            held.remove(&code);
            self.kb.update_key(code, false);
            self.repeater.on_release(code);
        }
        for code in pressed {
            held.insert(code);
            self.kb.update_key(code, true); // state only: we missed the press itself
        }
    }

    /// Releases every key held on `dev` (the device went away).
    pub fn device_gone(&mut self, dev: u32) {
        if let Some(held) = self.held.remove(&dev) {
            for code in held {
                self.kb.update_key(code, false);
                self.repeater.on_release(code);
            }
        }
    }

    /// Releases all keys on all devices (e.g. before re-opening after resume).
    pub fn release_all(&mut self) {
        let devs: Vec<u32> = self.held.keys().copied().collect();
        for d in devs {
            self.device_gone(d);
        }
        self.repeater.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::super::xkb::LayoutSpec;
    use super::*;

    const KEY_A: u16 = 30;
    const KEY_B: u16 = 48;
    const KEY_BACKSPACE: u16 = 14;
    const KEY_LEFTSHIFT: u16 = 42;
    const KEY_LEFTALT: u16 = 56;
    const KEY_LEFTCTRL: u16 = 29;
    const KEY_V: u16 = 47;

    fn proc() -> KeyProcessor {
        let kb = Keyboard::new(&LayoutSpec {
            layout: "us,il".into(),
            variant: ",".into(),
            options: "grp:alt_shift_toggle".into(),
            ..Default::default()
        })
        .unwrap();
        KeyProcessor::new(kb, RepeatConfig::default())
    }

    fn run(p: &mut KeyProcessor, steps: &[(u32, u16, i32, Millis)]) -> Vec<KeyOut> {
        let mut out = Vec::new();
        for &(dev, code, value, ts) in steps {
            p.key(dev, code, value, ts, &mut |k| out.push(k));
        }
        out
    }

    fn keys(v: &[KeyOut]) -> Vec<&str> {
        v.iter().map(|k| k.key.as_str()).collect()
    }

    #[test]
    fn quick_taps_do_not_repeat() {
        let mut p = proc();
        let out = run(
            &mut p,
            &[
                (0, KEY_A, 1, 0),
                (0, KEY_A, 0, 80),
                (0, KEY_B, 1, 100),
                (0, KEY_B, 0, 180),
            ],
        );
        assert_eq!(keys(&out), vec!["a", "b"]);
        assert!(out.iter().all(|k| !k.repeat));
        assert_eq!(p.deadline(), None);
    }

    #[test]
    fn held_key_repeats_on_compositor_schedule() {
        let mut p = proc();
        // Hold Backspace for 1000 ms: first repeat at 600 ms, then every 40 ms
        // (25 Hz): 600, 640, ..., 1000. Repeats due at or before the release
        // timestamp fire first, so the one at exactly 1000 is included (11 total).
        let out = run(
            &mut p,
            &[(0, KEY_BACKSPACE, 1, 0), (0, KEY_BACKSPACE, 0, 1000)],
        );
        assert_eq!(out.len(), 1 + 11);
        assert!(!out[0].repeat);
        assert!(out[1..].iter().all(|k| k.repeat && k.key == "Backspace"));
        assert_eq!(out[1].ts, 600);
        assert_eq!(out[2].ts, 640);
        assert_eq!(out.last().unwrap().ts, 1000);
    }

    #[test]
    fn kernel_repeats_are_ignored() {
        let mut p = proc();
        let out = run(
            &mut p,
            &[
                (0, KEY_A, 1, 0),
                (0, KEY_A, 2, 250),
                (0, KEY_A, 2, 283),
                (0, KEY_A, 0, 300),
            ],
        );
        assert_eq!(keys(&out), vec!["a"]);
    }

    #[test]
    fn tick_emits_repeats_while_idle() {
        let mut p = proc();
        run(&mut p, &[(0, KEY_A, 1, 0)]);
        assert_eq!(p.deadline(), Some(600));
        let mut out = Vec::new();
        p.tick(599, &mut |k| out.push(k));
        assert!(out.is_empty());
        p.tick(700, &mut |k| out.push(k));
        assert_eq!(
            out.iter().map(|k| k.ts).collect::<Vec<_>>(),
            vec![600, 640, 680]
        );
        assert_eq!(p.deadline(), Some(720));
    }

    #[test]
    fn new_press_takes_over_repeat() {
        let mut p = proc();
        let out = run(
            &mut p,
            &[
                (0, KEY_A, 1, 0),
                (0, KEY_B, 1, 300),
                (0, KEY_A, 0, 400),
                (0, KEY_B, 0, 950),
            ],
        );
        // b repeats from 900; releasing a does not stop b's repeat.
        assert_eq!(keys(&out), vec!["a", "b", "b", "b"]);
        assert_eq!(out[2].ts, 900);
    }

    #[test]
    fn modifiers_shared_across_devices_and_released_on_unplug() {
        let mut p = proc();
        let out = run(
            &mut p,
            &[
                (1, KEY_LEFTSHIFT, 1, 0),
                (2, KEY_A, 1, 10),
                (2, KEY_A, 0, 20),
            ],
        );
        assert_eq!(out[0].key, "A");
        assert_eq!(out[0].mods, vec![Mod::Shift]);
        p.device_gone(1);
        let out = run(&mut p, &[(2, KEY_A, 1, 30)]);
        assert_eq!(out[0].key, "a");
        assert!(out[0].mods.is_empty());
    }

    #[test]
    fn stray_release_does_not_corrupt_state() {
        let mut p = proc();
        let out = run(&mut p, &[(0, KEY_LEFTSHIFT, 0, 0), (0, KEY_A, 1, 10)]);
        assert_eq!(out[0].key, "a");
    }

    #[test]
    fn alt_shift_toggles_layout_like_the_compositor() {
        let mut p = proc();
        let out = run(
            &mut p,
            &[
                (0, KEY_LEFTALT, 1, 0),
                (0, KEY_LEFTSHIFT, 1, 10),
                (0, KEY_LEFTSHIFT, 0, 20),
                (0, KEY_LEFTALT, 0, 30),
                (0, KEY_A, 1, 40),
            ],
        );
        assert_eq!(keys(&out), vec!["ש"]);
    }

    #[test]
    fn shortcuts_resolve_on_the_latin_layout() {
        // On the Hebrew layout, Ctrl+A is Ctrl+A (not Ctrl+ש) and Ctrl+V is
        // Ctrl+V, the way applications see them; plain typing stays Hebrew.
        let mut p = proc();
        p.set_layout_index(1);
        let out = run(
            &mut p,
            &[
                (0, KEY_A, 1, 0),
                (0, KEY_A, 0, 5),
                (0, KEY_LEFTCTRL, 1, 10),
                (0, KEY_A, 1, 20),
                (0, KEY_A, 0, 25),
                (0, KEY_V, 1, 30),
                (0, KEY_V, 0, 35),
                (0, KEY_LEFTCTRL, 0, 40),
            ],
        );
        assert_eq!(keys(&out), vec!["ש", "a", "v"]);
        assert_eq!(out[1].mods, vec![Mod::Ctrl]);
    }

    #[test]
    fn resync_after_dropped_events() {
        let mut p = proc();
        run(&mut p, &[(0, KEY_LEFTSHIFT, 1, 0)]);
        p.resync(0, &HashSet::new()); // kernel says nothing is held
        let out = run(&mut p, &[(0, KEY_A, 1, 10)]);
        assert_eq!(out[0].key, "a");
    }

    #[test]
    fn parses_kcminputrc() {
        let cfg = RepeatConfig::from_kcminputrc(
            "[Mouse]\nRepeatDelay=1\n[Keyboard]\nKeyRepeat=repeat\nRepeatDelay=250\nRepeatRate=30.5\n",
        );
        assert_eq!(
            cfg,
            RepeatConfig {
                enabled: true,
                delay_ms: 250,
                rate_hz: 30.5
            }
        );
        let off = RepeatConfig::from_kcminputrc("[Keyboard]\nKeyRepeat=nothing\n");
        assert!(!off.enabled);
        assert_eq!(RepeatConfig::from_kcminputrc(""), RepeatConfig::default());
    }
}
