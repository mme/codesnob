//! A mouse wheel's notches, animated as Chromium animates them on Linux
//! (`cc/animation/scroll_offset_animation_curve.cc`): ease-in-out over 6 to
//! 12 frames, the shorter the further, and a notch arriving mid-way
//! retargets the curve from where it is, keeping its speed.

use super::fling::{bezier_slope, bezier_y};

/// Chromium's ease-in-out.
const EASE: ((f32, f32), (f32, f32)) = ((0.42, 0.0), (0.58, 1.0));
/// Durations in frames of a sixtieth of a second: 12 for 120 px or less,
/// down to 6 for 480 px or more.
const RAMP_START: f32 = 120.0;
const RAMP_END: f32 = 480.0;
const SHORTEST: f32 = 6.0;
const LONGEST: f32 = 12.0;
const EPSILON: f32 = 0.01;

/// A scroll by the wheel: from its start to its target, in pixels relative
/// to where it began.
pub struct WheelScroll {
    /// The device scale: durations count device pixels.
    scale: f32,
    initial: f32,
    target: f32,
    /// The curve's first control point; the second is ease-in-out's.
    p1: (f32, f32),
    /// Seconds from the start: to the end, and to the last retarget.
    total: f32,
    retarget: f32,
}

impl WheelScroll {
    /// A scroll to `target` pixels away, on a display of `scale`.
    pub fn new(target: f32, scale: f32) -> Self {
        Self {
            scale,
            initial: 0.0,
            target,
            p1: EASE.0,
            total: duration(target, scale),
            retarget: 0.0,
        }
    }

    pub fn target(&self) -> f32 {
        self.target
    }

    /// Where the scroll is `t` seconds in.
    pub fn value(&self, t: f32) -> f32 {
        let span = self.total - self.retarget;
        let t = t - self.retarget;
        if span <= 0.0 || t >= span {
            return self.target;
        }
        if t <= 0.0 {
            return self.initial;
        }
        let p = bezier_y(self.p1, EASE.1, t / span);
        self.initial + (self.target - self.initial) * p
    }

    pub fn done(&self, t: f32) -> bool {
        t >= self.total
    }

    /// Its speed `t` seconds in, in pixels a second.
    fn velocity(&self, t: f32) -> f32 {
        let span = self.total - self.retarget;
        if span <= 0.0 {
            return 0.0;
        }
        let slope = bezier_slope(self.p1, EASE.1, (t - self.retarget) / span);
        slope * (self.target - self.initial) / span
    }

    /// Another notch, `t` seconds in: head for `target` from where the
    /// scroll is, at the speed it has.
    pub fn retarget(&mut self, t: f32, target: f32) {
        let t = t.max(self.retarget);
        if (self.target - target).abs() < EPSILON {
            self.target = target;
            return;
        }
        let now = self.value(t);
        let delta = target - now;
        if delta.abs() < EPSILON {
            self.retarget = t;
            self.total = t;
            self.target = target;
            return;
        }
        if self.total - self.retarget <= 0.0 {
            self.total = duration(delta, self.scale);
            self.target = target;
            return;
        }
        let velocity = self.velocity(t);
        // How long to get there at this speed, with room for easing out;
        // without a bound when standing still or heading the other way.
        let bound = if velocity.abs() < EPSILON {
            f32::INFINITY
        } else {
            let b = delta / velocity * 2.5;
            if b < 0.0 { f32::INFINITY } else { b }
        };
        let span = duration(delta, self.scale).min(bound);
        if span < EPSILON {
            self.target = target;
            self.total = t;
            return;
        }
        let slope = (velocity * span / delta).clamp(-1000.0, 1000.0);
        self.p1 = (EASE.0.0, EASE.0.0 * slope);
        self.initial = now;
        self.target = target;
        self.total = t + span;
        self.retarget = t;
    }
}

/// How long a scroll of `delta` pixels takes, in seconds.
fn duration(delta: f32, scale: f32) -> f32 {
    let slope = (SHORTEST - LONGEST) / (RAMP_END - RAMP_START);
    let offset = LONGEST - RAMP_START * slope;
    (offset + (delta * scale).abs() * slope).clamp(SHORTEST, LONGEST) / 60.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notch_takes_as_long_as_in_chromium() {
        // 120 px a notch: 200 ms at scale 1, 167 ms at scale 2.
        assert!((duration(120.0, 1.0) - 0.2).abs() < 1e-3);
        assert!((duration(120.0, 2.0) - 10.0 / 60.0).abs() < 1e-3);
        assert!((duration(1000.0, 1.0) - 0.1).abs() < 1e-3);
    }

    #[test]
    fn it_eases_in_and_out_to_the_target() {
        let w = WheelScroll::new(120.0, 1.0);
        assert_eq!(w.value(0.0), 0.0);
        assert!(w.value(0.02) < 120.0 * 0.1 / 2.0);
        assert!((w.value(0.1) - 60.0).abs() < 1.0);
        assert_eq!(w.value(0.2), 120.0);
        assert!(w.done(0.2));
    }

    #[test]
    fn a_second_notch_carries_on_without_a_jump() {
        let mut w = WheelScroll::new(120.0, 1.0);
        let t = 0.1;
        let before = w.value(t);
        let speed = w.velocity(t);
        w.retarget(t, 240.0);
        assert!((w.value(t) - before).abs() < 1e-3);
        assert!((w.velocity(t) - speed).abs() < speed * 0.05);
        assert_eq!(w.target(), 240.0);
        assert_eq!(w.value(10.0), 240.0);
    }
}
