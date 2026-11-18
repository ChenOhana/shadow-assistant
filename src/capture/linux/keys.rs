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

/// Compositor key-repeat settings (the desktop backend reads them).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RepeatConfig {
    pub enabled: bool,
    pub delay_ms: Millis,
    pub rate_hz: f64,
}

impl Default for RepeatConfig {
    /// KDE Plasma's defaults (600 ms, 25 Hz).
    fn default() -> Self {
        Self {
            enabled: true,
            delay_ms: 600,
            rate_hz: 25.0,
        }
    }
}

impl RepeatConfig {
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

    /// The contract the input loop must keep: `tick(now)` may only run once
    /// every transition with a timestamp up to `now` has been delivered.
    /// Ticking past a release that is still unread emits repeats the app
    /// never saw (the bug that made held keys produce an extra character).
    #[test]
    fn ticking_past_an_unread_release_adds_repeats() {
        let mut in_order = proc();
        let ok = run(&mut in_order, &[(0, KEY_A, 1, 0), (0, KEY_A, 0, 650)]);
        let mut late = proc();
        let mut bad = run(&mut late, &[(0, KEY_A, 1, 0)]);
        late.tick(700, &mut |k| bad.push(k)); // woke at 700, release (650) not read yet
        late.key(0, KEY_A, 0, 650, &mut |k| bad.push(k));
        assert_eq!(keys(&ok), vec!["a", "a", "a"]); // press, repeats at 600 and 640
        assert_eq!(keys(&bad), vec!["a", "a", "a", "a"]); // + a phantom repeat at 680
    }

    /// Independent model of compositor key repeat: a key repeats `delay`
    /// after its press, then every `1000/rate` ms, until it is released or
    /// another key is pressed; each repeat uses the modifiers held at that
    /// moment. Returns (timestamp, key, repeat) for every key the app gets.
    fn reference(events: &[(u16, bool, Millis)], end: Millis) -> Vec<(Millis, String, bool)> {
        let cfg = RepeatConfig::default();
        let period = 1000.0 / cfg.rate_hz;
        let letter = |code: u16, shift: bool| {
            let c = match code {
                KEY_A => 'a',
                KEY_B => 'b',
                KEY_BACKSPACE => return "Backspace".to_string(),
                _ => unreachable!(),
            };
            if shift { c.to_ascii_uppercase() } else { c }.to_string()
        };
        let mut out = Vec::new();
        let mut shift = false;
        let mut repeating: Option<(u16, f64)> = None;
        let fire_until =
            |t: Millis, shift: bool, rep: &mut Option<(u16, f64)>, out: &mut Vec<_>| {
                while let Some((code, next)) = rep.as_mut() {
                    if *next > t as f64 {
                        break;
                    }
                    out.push((next.floor() as Millis, letter(*code, shift), true));
                    *next += period;
                }
            };
        for &(code, down, t) in events {
            fire_until(t, shift, &mut repeating, &mut out);
            match (code, down) {
                (KEY_LEFTSHIFT, d) => shift = d,
                (c, true) => {
                    out.push((t, letter(c, shift), false));
                    repeating = Some((c, (t + cfg.delay_ms) as f64));
                }
                (c, false) => {
                    if repeating.is_some_and(|(r, _)| r == c) {
                        repeating = None;
                    }
                }
            }
        }
        fire_until(end, shift, &mut repeating, &mut out);
        out
    }

    /// xorshift64*: deterministic randomness without a dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Random typing with long holds, overlapping keys, Shift changes during
    /// holds and holds ending right at a repeat's due time, delivered with
    /// random wakeup timing. Every emitted key and timestamp must match the
    /// reference model, however early or late `tick` runs, as long as key
    /// transitions are delivered in time order.
    #[test]
    fn repeat_stress_matches_reference_model() {
        let mut rng = Rng(0x5EED_1234_ABCD_0001);
        for scenario in 0..300 {
            let mut events: Vec<(u16, bool, Millis)> = Vec::new();
            let mut t: Millis = 1_000;
            let mut shift_down = false;
            for _ in 0..12 {
                let code = [KEY_A, KEY_B, KEY_BACKSPACE][rng.below(3) as usize];
                let hold = match rng.below(5) {
                    0 => rng.below(120) as Millis,      // a tap
                    1 => 560 + rng.below(80) as Millis, // around the delay
                    // exactly on a repeat's due time, or 1 ms either side
                    2 => 600 + 40 * rng.below(20) as Millis + rng.below(3) as Millis - 1,
                    _ => rng.below(3_000) as Millis, // anything up to 3 s
                };
                events.push((code, true, t));
                if rng.below(4) == 0 {
                    // Shift toggled during the hold changes later repeats.
                    let at = t + 1 + rng.below(hold.max(1) as u64) as Millis;
                    shift_down = !shift_down;
                    events.push((KEY_LEFTSHIFT, shift_down, at));
                }
                if rng.below(5) == 0 {
                    // Another key pressed during the hold takes over the repeat;
                    // the first key's release later must not stop it.
                    let other = if code == KEY_A { KEY_B } else { KEY_A };
                    let at = t + 1 + rng.below(hold.max(1) as u64) as Millis;
                    events.push((other, true, at));
                    events.push((other, false, at + hold));
                }
                events.push((code, false, t + hold));
                t += hold + 1 + rng.below(200) as Millis;
            }
            if shift_down {
                events.push((KEY_LEFTSHIFT, false, t));
            }
            events.sort_by_key(|e| e.2); // stable: same-ms events keep their order
            // Overlapping holds can produce a press of a key that is already
            // down, or a release of one that is up: drop those, so the input
            // is something a real keyboard can send.
            let mut down: HashSet<u16> = HashSet::new();
            events.retain(|&(code, d, _)| {
                if d {
                    down.insert(code)
                } else {
                    down.remove(&code)
                }
            });
            let end = events.last().unwrap().2 + 5_000;
            let expected = reference(&events, end);

            let mut p = proc();
            let mut got: Vec<(Millis, String, bool)> = Vec::new();
            let mut emit = |k: KeyOut| got.push((k.ts, k.key, k.repeat));
            let mut last_tick: Millis = 0;
            for &(code, down, ts) in &events {
                // Wakeups at random times before this event: never past it.
                while rng.below(3) == 0 && last_tick < ts {
                    last_tick += 1 + rng.below((ts - last_tick) as u64) as Millis;
                    p.tick(last_tick, &mut emit);
                }
                p.key(0, code, if down { 1 } else { 0 }, ts, &mut emit);
            }
            p.tick(end, &mut emit);
            assert_eq!(got, expected, "scenario {scenario}: {events:?}");
        }
    }
}
