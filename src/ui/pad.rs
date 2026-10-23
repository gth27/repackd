//! Gamepad: the D-pad, left stick and face buttons become the keys the window already understands
//! (arrows in the library, Tab between controls in settings), the right stick scrolls. gilrs runs on its
//! own thread and wakes the window per press.
use eframe::egui::{self, Event, Key, Modifiers, MouseWheelSource, MouseWheelUnit, RawInput, TouchPhase};
use gilrs::{Axis, Button, EventType, Gilrs};
use std::sync::{
    atomic::{AtomicBool, AtomicI32, Ordering},
    mpsc::{channel, Receiver},
    Arc,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Pad {
    Up,
    Down,
    Left,
    Right,
    /// A / Cross
    Accept,
    /// B / Circle
    Back,
    /// Start or Y / Triangle: settings
    Menu,
}

/// Presses from the gamepad thread, and the right stick's tilt (-100..100) for scrolling.
pub(super) struct Gamepad {
    rx: Receiver<Pad>,
    scroll: Arc<AtomicI32>,
    /// Whether our window has focus: otherwise the pad belongs to a game, and must not wake or drive us.
    focused: Arc<AtomicBool>,
}

/// Start listening; None when the system has no gamepad support (gilrs could not start).
pub(super) fn start(ctx: egui::Context) -> Option<Gamepad> {
    let (tx, rx) = channel();
    let scroll = Arc::new(AtomicI32::new(0));
    let focused = Arc::new(AtomicBool::new(true));
    let (tilt, ours) = (scroll.clone(), focused.clone());
    std::thread::Builder::new()
        .name("gamepad".into())
        .spawn(move || {
            let Ok(mut gil) = Gilrs::new() else { return };
            let (mut x, mut y) = (0, 0); // which way the left stick is pushed past its dead zone (-1, 0, 1)
            loop {
                let Some(ev) = gil.next_event_blocking(None) else { continue };
                if !ours.load(Ordering::Relaxed) {
                    tilt.store(0, Ordering::Relaxed);
                    continue;
                }
                let press = match ev.event {
                    EventType::ButtonPressed(b, _) => button(b),
                    EventType::AxisChanged(Axis::LeftStickX, v, _) => stick(&mut x, v).map(|d| if d > 0 { Pad::Right } else { Pad::Left }),
                    EventType::AxisChanged(Axis::LeftStickY, v, _) => stick(&mut y, v).map(|d| if d > 0 { Pad::Up } else { Pad::Down }),
                    EventType::AxisChanged(Axis::RightStickY, v, _) => {
                        tilt.store((v * 100.0) as i32, Ordering::Relaxed);
                        ctx.request_repaint();
                        None
                    }
                    _ => None,
                };
                if let Some(p) = press {
                    if tx.send(p).is_err() {
                        return; // the window is gone
                    }
                    ctx.request_repaint();
                }
            }
        })
        .ok()?;
    Some(Gamepad { rx, scroll, focused })
}

fn button(b: Button) -> Option<Pad> {
    Some(match b {
        Button::DPadUp => Pad::Up,
        Button::DPadDown => Pad::Down,
        Button::DPadLeft => Pad::Left,
        Button::DPadRight => Pad::Right,
        Button::South => Pad::Accept,
        Button::East => Pad::Back,
        Button::Start | Button::North => Pad::Menu,
        _ => return None,
    })
}

/// A stick axis as a D-pad: one press when it is pushed past 0.6, none until it comes back under 0.3.
fn stick(held: &mut i32, v: f32) -> Option<i32> {
    let now = if v > 0.6 { 1 } else if v < -0.6 { -1 } else if v.abs() < 0.3 { 0 } else { *held };
    let press = (now != 0 && now != *held).then_some(now);
    *held = now;
    press
}

impl Gamepad {
    /// The presses since the last frame as key events for the view that is open: `form` (a settings page)
    /// moves between controls with Tab, the library and game page use the arrows. Returns the presses
    /// the caller handles itself (Menu).
    pub(super) fn feed(&self, raw: &mut RawInput, form: bool) -> Vec<Pad> {
        let mut own = vec![];
        let before = self.focused.swap(raw.focused, Ordering::Relaxed);
        // presses queued while we were unfocused (even when no frame ran to notice) belonged to the game
        let regained = raw.events.iter().any(|e| matches!(e, Event::WindowFocused(true)));
        if !raw.focused || !before || regained {
            // gilrs reads the pad whatever window is in front: presses meant for a game must not reach us
            while self.rx.try_recv().is_ok() {}
            return own;
        }
        let mut key = |k: Key, shift: bool| {
            for pressed in [true, false] {
                raw.events.push(Event::Key { key: k, physical_key: None, pressed, repeat: false, modifiers: Modifiers { shift, ..Default::default() } });
            }
        };
        while let Ok(p) = self.rx.try_recv() {
            match p {
                Pad::Up if form => key(Key::Tab, true),
                Pad::Down if form => key(Key::Tab, false),
                Pad::Up => key(Key::ArrowUp, false),
                Pad::Down => key(Key::ArrowDown, false),
                Pad::Left => key(Key::ArrowLeft, false),
                Pad::Right => key(Key::ArrowRight, false),
                Pad::Accept => key(Key::Enter, false),
                Pad::Back => key(Key::Escape, false),
                Pad::Menu => own.push(p),
            }
        }
        let tilt = self.scroll.load(Ordering::Relaxed);
        if tilt.abs() > 20 && raw.focused {
            // stick up scrolls up; held, it keeps scrolling while frames come
            raw.events.push(Event::MouseWheel { unit: MouseWheelUnit::Point, delta: egui::vec2(0.0, tilt as f32 * 0.25), phase: TouchPhase::Move, source: MouseWheelSource::Unknown, modifiers: Modifiers::default() });
        }
        own
    }

    pub(super) fn scrolling(&self) -> bool {
        self.focused.load(Ordering::Relaxed) && self.scroll.load(Ordering::Relaxed).abs() > 20
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stick_presses_once_per_push() {
        let mut held = 0;
        let pushes: Vec<Option<i32>> = [0.2, 0.7, 0.9, 0.5, 0.1, -0.8].iter().map(|&v| stick(&mut held, v)).collect();
        assert_eq!(pushes, [None, Some(1), None, None, None, Some(-1)]);
    }
}
