//! Coasting after a touchpad swipe, as Chromium does it on Linux Wayland.
//! Its pieces follow Chromium's sources: the speed from the last scroll
//! frames (`ui/ozone/platform/wayland/host/wayland_event_source.cc`,
//! `ComputeFlingVelocity`), the curve of constant deceleration eased along
//! a cubic Bézier (`ui/events/gestures/physics_based_fling_curve.cc`), how
//! it is stepped frame by frame (`components/input/fling_controller.cc`),
//! and how a quick second flick adds the first one's speed
//! (`ui/events/blink/fling_booster.cc`). Distances are Chrome's: touchpad
//! motion at twelve times the compositor's values.

use std::time::{Duration, Instant};

/// Deceleration in pixels a second squared: 2.7559e-5 in/ms² at 96 ppi.
const DECELERATION: f32 = 2.7559e-5 * 96.0 * 1e6;
/// A fling lasts its speed over this many pixels a second, at most
/// `LONGEST` seconds.
const DURATION_SPEED: f32 = 2300.0;
pub const LONGEST: f32 = 2.0;
/// A fling goes at most this many viewports.
const VIEWPORTS: f32 = 3.0;
/// The curve's second control point.
const P2: (f32, f32) = (0.55, 1.0);
/// Moves smaller than this are left out, though the curve goes on.
const SMALLEST_STEP: f32 = 0.1;
/// Fingers that stopped this long before lifting don't fling.
const RESTED: Duration = Duration::from_millis(200);
/// The scroll frames the speed is measured over.
pub const FRAMES: usize = 3;
/// A started fling's first step comes no later than this after its start;
/// later, the start moves to a frame before it.
const FIRST_STEP_WITHIN: Duration = Duration::from_micros(33_333);
const FRAME: Duration = Duration::from_micros(16_667);

pub struct Fling {
    start: Instant,
    /// Seconds.
    duration: f32,
    /// How far it goes in all, in pixels, signed as the view scrolls.
    distance: f32,
    p1: (f32, f32),
    /// How far it has gone so far, applied or not.
    done: f32,
    /// It has taken its first step.
    stepped: bool,
}

impl Fling {
    /// A fling at `velocity` pixels a second (positive scrolls down) from
    /// `start`, going at most `limit` pixels for at most `longest` seconds;
    /// `None` when it would not move.
    pub fn new(velocity: f32, start: Instant, limit: f32, longest: f32) -> Option<Self> {
        if velocity == 0.0 || !velocity.is_finite() {
            return None;
        }
        let distance = (velocity.signum() * velocity * velocity / (2.0 * DECELERATION))
            .clamp(-limit.abs(), limit.abs());
        let duration = (velocity.abs() / DURATION_SPEED).min(longest);
        // The first control point makes the curve start at the fling's speed.
        let s = velocity.abs() * duration / distance.abs().max(f32::EPSILON);
        let p1 = if 0.2 * s < 1.0 {
            (0.2, 0.2 * s)
        } else {
            (1.0 / s, 1.0)
        };
        Some(Self {
            start,
            duration,
            distance,
            p1,
            done: 0.0,
            stepped: false,
        })
    }

    /// How far a fling goes at most: `VIEWPORTS` of `viewport`, times how
    /// much a boost raised its speed from `unboosted` to `boosted`.
    pub fn limit(viewport: f32, boosted: f32, unboosted: f32) -> f32 {
        let boost = if unboosted.abs() > 0.0 {
            (boosted / unboosted).abs().max(1.0)
        } else {
            1.0
        };
        VIEWPORTS * boost * viewport
    }

    /// How far the fling has gone `t` seconds in.
    pub fn offset(&self, t: f32) -> f32 {
        if t >= self.duration {
            return self.distance;
        }
        self.distance * bezier_y(self.p1, P2, (t / self.duration).max(0.0))
    }

    /// How far to move at `now`, and whether the fling is over. As
    /// Chromium's controller steps it: the first step comes within two
    /// frames of the start, steps too small to see are left out, and the
    /// last one, at the curve's end, is dropped.
    pub fn step(&mut self, now: Instant) -> (f32, bool) {
        if !self.stepped {
            self.stepped = true;
            if now >= self.start + FIRST_STEP_WITHIN {
                self.start = now - FRAME;
            }
        }
        if now <= self.start {
            return (0.0, false);
        }
        let t = (now - self.start).as_secs_f32();
        if t >= self.duration {
            return (0.0, true);
        }
        let to = self.offset(t);
        let by = to - self.done;
        self.done = to;
        if by.abs() > SMALLEST_STEP {
            (by, false)
        } else {
            (0.0, false)
        }
    }
}

/// The y of the curve from (0, 0) to (1, 1) with control points `p1` and
/// `p2` where its x is `x`.
pub fn bezier_y(p1: (f32, f32), p2: (f32, f32), x: f32) -> f32 {
    let u = bezier_u(p1, p2, x);
    bezier_at(p1.1, p2.1, u)
}

/// The curve's slope dy/dx where its x is `x`.
pub fn bezier_slope(p1: (f32, f32), p2: (f32, f32), x: f32) -> f32 {
    let u = bezier_u(p1, p2, x.clamp(0.0, 1.0));
    let dx = bezier_d(p1.0, p2.0, u);
    let dy = bezier_d(p1.1, p2.1, u);
    if dx.abs() < 1e-6 { 0.0 } else { dy / dx }
}

fn bezier_at(a: f32, b: f32, u: f32) -> f32 {
    let v = 1.0 - u;
    3.0 * v * v * u * a + 3.0 * v * u * u * b + u * u * u
}

fn bezier_d(a: f32, b: f32, u: f32) -> f32 {
    let v = 1.0 - u;
    3.0 * v * v * a + 6.0 * v * u * (b - a) + 3.0 * u * u * (1.0 - b)
}

/// The parameter where the curve's x is `x`: x rises with it, so halve
/// the interval down to it.
fn bezier_u(p1: (f32, f32), p2: (f32, f32), x: f32) -> f32 {
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..30 {
        let mid = (lo + hi) / 2.0;
        if bezier_at(p1.0, p2.0, mid) < x {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo + hi) / 2.0
}

/// A swipe's speed when the fingers lift, in pixels a second, as Chromium
/// measures it on Wayland: a least-squares line through the running
/// distance over time, from the lift back across the last scroll frames.
/// `frames` holds each frame's time since the frame before it and its
/// distance, newest first; `stop` is the lift's time since the last frame.
/// Fingers that rested before lifting have none.
pub fn velocity(frames: &[(Duration, f32)], stop: Duration) -> f32 {
    if frames.is_empty() || stop > RESTED {
        return 0.0;
    }
    if let [(dt, d)] = frames {
        // As Chromium has it: a distance times a time, next to nothing.
        return d * (dt.as_secs_f32() + stop.as_secs_f32());
    }
    let n = frames.len() as f32;
    let mut time = stop.as_secs_f32();
    let mut x = 0.0;
    let (mut tt, mut t, mut tx, mut sx) = (0.0, 0.0, 0.0, 0.0);
    for (dt, d) in frames {
        time += dt.as_secs_f32();
        x += d;
        tt += time * time;
        t += time;
        tx += time * x;
        sx += x;
    }
    let det = n * tt - t * t;
    if det == 0.0 {
        return if t != 0.0 { x / t } else { 0.0 };
    }
    (n * tx - t * sx) / det
}

/// Whether a fling may add the last one's starting speed, as Chromium's
/// booster decides: the new swipe began within 50 ms of catching the last
/// fling, kept going its way at 150 px/s or more, and lifted within 50 ms
/// of its last move; both flings at 350 px/s or more.
#[derive(Default)]
pub struct Booster {
    /// The last fling's starting speed; zero when there is none to add.
    start_velocity: f32,
    /// Boosting needs the next event before this.
    cutoff: Option<Instant>,
    /// The last move of the swipe that may boost.
    last_move: Option<Instant>,
}

const BOOST_WITHIN: Duration = Duration::from_millis(50);
const BOOST_FLING: f32 = 350.0;
const BOOST_SWIPE: f32 = 150.0;

impl Booster {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Whether the booster has a fling to add and its window is still open
    /// at `at`; resets when it has run out.
    fn live(&mut self, at: Instant) -> bool {
        if self.start_velocity == 0.0 {
            return false;
        }
        if self.cutoff.is_some_and(|c| at > c) {
            self.reset();
            return false;
        }
        true
    }

    /// A fling was caught: by fingers touching the pad, or a new swipe.
    pub fn caught(&mut self, at: Instant) {
        if self.live(at) {
            self.last_move = None;
            self.cutoff = Some(at + BOOST_WITHIN);
        }
    }

    /// A swipe began.
    pub fn began(&mut self, at: Instant) {
        if self.live(at) {
            self.cutoff = Some(at + BOOST_WITHIN);
        }
    }

    /// A swipe moved `by` pixels.
    pub fn moved(&mut self, at: Instant, by: f32) {
        if !self.live(at) || self.cutoff.is_none() {
            return;
        }
        if self.start_velocity * by <= 0.0 {
            self.reset();
            return;
        }
        if let Some(last) = self.last_move {
            let secs = (at - last).as_secs_f32();
            if secs >= 0.001 && (by / secs).abs() < BOOST_SWIPE {
                self.reset();
                return;
            }
        }
        self.last_move = Some(at);
        self.cutoff = Some(at + BOOST_WITHIN);
    }

    /// A fling begins at `velocity`: the speed it gets, the last fling's
    /// added when it may boost.
    pub fn fling(&mut self, at: Instant, velocity: f32) -> f32 {
        let boost = self.start_velocity != 0.0
            && self.cutoff.is_some_and(|c| at <= c)
            && self.start_velocity * velocity > 0.0
            && self.start_velocity.abs() >= BOOST_FLING
            && velocity.abs() >= BOOST_FLING;
        let velocity = if boost {
            velocity + self.start_velocity
        } else {
            velocity
        };
        self.reset();
        self.start_velocity = velocity;
        velocity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn distance_grows_with_the_square_of_the_speed() {
        let now = Instant::now();
        let slow = Fling::new(1000.0, now, 1e6, LONGEST).unwrap();
        let fast = Fling::new(2000.0, now, 1e6, LONGEST).unwrap();
        // Chromium's numbers: 1000 px/s goes 189 px in 0.43 s.
        assert!((slow.offset(10.0) - 189.0).abs() < 1.0);
        assert!((slow.duration - 1000.0 / 2300.0).abs() < 1e-3);
        assert!((fast.offset(10.0) / slow.offset(10.0) - 4.0).abs() < 0.01);
    }

    #[test]
    fn it_starts_at_the_swipes_speed_and_eases_to_a_stop() {
        let f = Fling::new(1600.0, Instant::now(), 1e6, LONGEST).unwrap();
        let speed = |t: f32| (f.offset(t + 0.001) - f.offset(t)) / 0.001;
        assert!((speed(0.0) - 1600.0).abs() < 1600.0 * 0.05);
        assert!(speed(f.duration * 0.95) < 1600.0 * 0.05);
        let mut last = 0.0;
        for i in 0..=100 {
            let o = f.offset(f.duration * i as f32 / 100.0);
            assert!(o >= last - 1e-3);
            last = o;
        }
    }

    #[test]
    fn up_is_negative_and_far_flings_stop_at_the_limit() {
        let f = Fling::new(-8000.0, Instant::now(), 500.0, LONGEST).unwrap();
        assert_eq!(f.offset(10.0), -500.0);
        assert!(Fling::new(0.0, Instant::now(), 500.0, LONGEST).is_none());
        assert_eq!(Fling::limit(100.0, 2000.0, 1000.0), 600.0);
    }

    #[test]
    fn stepping_drops_small_steps_and_the_last() {
        let t0 = Instant::now();
        let mut f = Fling::new(1000.0, t0, 1e6, LONGEST).unwrap();
        // At its start nothing moves; a frame later it does.
        assert_eq!(f.step(t0), (0.0, false));
        let (by, over) = f.step(t0 + ms(16));
        assert!(by > 10.0 && !over);
        let (_, over) = f.step(t0 + ms(1000));
        assert!(over);
        // A first step late moves the start to a frame before it.
        let mut late = Fling::new(1000.0, t0, 1e6, LONGEST).unwrap();
        let (by, _) = late.step(t0 + ms(100));
        assert!(by > 10.0 && by < 20.0);
    }

    #[test]
    fn speed_as_chromium_measures_it() {
        // 40 px every 10 ms: 4000 px/s, lifted 10 ms after the last move.
        let frames = [(ms(10), 40.0), (ms(10), 40.0), (ms(10), 40.0)];
        assert!((velocity(&frames, ms(10)) - 4000.0).abs() < 1.0);
        // Rested before lifting: no fling.
        assert_eq!(velocity(&frames, ms(250)), 0.0);
        assert_eq!(velocity(&[], ms(10)), 0.0);
        // One frame alone: next to nothing.
        assert!(velocity(&[(ms(10), 40.0)], ms(10)).abs() < 1.0);
    }

    #[test]
    fn a_quick_second_flick_adds_the_first() {
        let t = Instant::now();
        let mut b = Booster::default();
        assert_eq!(b.fling(t, 1000.0), 1000.0);
        b.caught(t + ms(100));
        b.began(t + ms(110));
        b.moved(t + ms(120), 30.0);
        b.moved(t + ms(130), 30.0);
        assert_eq!(b.fling(t + ms(140), 900.0), 1900.0);
        // Too slow a swipe, or a pause, and it doesn't.
        let mut b = Booster::default();
        b.fling(t, 1000.0);
        b.caught(t + ms(100));
        b.moved(t + ms(110), 30.0);
        b.moved(t + ms(410), 1.0);
        assert_eq!(b.fling(t + ms(420), 900.0), 900.0);
        let mut b = Booster::default();
        b.fling(t, 1000.0);
        b.caught(t + ms(100));
        assert_eq!(b.fling(t + ms(200), 900.0), 900.0);
        // The other way: no boost.
        let mut b = Booster::default();
        b.fling(t, 1000.0);
        b.caught(t + ms(100));
        b.moved(t + ms(110), -30.0);
        assert_eq!(b.fling(t + ms(120), -900.0), -900.0);
    }
}
