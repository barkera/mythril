#![deny(missing_docs)]

//! # Abstract time support
//!
//! This module contains types and traits related to time keeping in
//! Mythril. Note that this does not include _date_ information, only
//! abstract system clock, counter, and timer information.

use crate::apic;
use crate::error::Result;
use crate::tsc;
use crate::vcpu;
use crate::{declare_per_core, get_per_core, get_per_core_mut};

use alloc::{collections::BTreeMap, vec};
use core::ops::{Add, AddAssign, Sub, SubAssign};
use core::time::Duration;

const TIMER_VECTOR: u8 = 32;

//TODO: should this just be stored as a VCPU member?
declare_per_core! {
    static mut TIMER_WHEEL: Option<TimerWheel> = None;
}

static mut TIME_SRC: Option<&'static mut dyn TimeSource> = None;
static mut START_TIME: Option<Instant> = None;

/// Determine the best available global system `TimeSource` and calibrate it.
pub unsafe fn init_global_time() -> Result<()> {
    // Currently we only support using the TSC
    TIME_SRC = Some(tsc::calibrate_tsc()?);
    START_TIME = Some(now());
    Ok(())
}

/// Get the current instant from the global system `TimeSource`.
pub fn now() -> Instant {
    unsafe {
        TIME_SRC
            .as_ref()
            .expect("Global time source is not calibrated")
            .now()
    }
}

/// Get the instant the system was started (approximately) in terms
/// of the global system `TimeSource`.
pub fn system_start_time() -> Instant {
    unsafe { START_TIME.expect("Global time source is not started") }
}

/// Returns whether the global system `TimeSource` has be initialized.
pub fn is_global_time_ready() -> bool {
    unsafe { TIME_SRC.is_some() }
}

fn frequency() -> u64 {
    unsafe {
        TIME_SRC
            .as_ref()
            .expect("Global time source is not calibrated")
            .frequency()
    }
}

/// A point in time on the system in terms of the global system `TimeSource`
///
/// An `Instant` can be added/subtracted with a `Duration` to produce an
/// `Instant` in the future or past. However, this requires that the global
/// system time source be initialized, otherwise it will panic.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug)]
pub struct Instant(pub u64);

impl Add<Duration> for Instant {
    type Output = Self;

    fn add(self, other: Duration) -> Self {
        let ticks = (frequency() as u128 * other.as_nanos()) / 1_000_000_000;
        Instant(self.0 + ticks as u64)
    }
}

impl AddAssign<Duration> for Instant {
    fn add_assign(&mut self, other: Duration) {
        *self = *self + other;
    }
}

impl Sub<Duration> for Instant {
    type Output = Self;

    fn sub(self, other: Duration) -> Self {
        let ticks = (frequency() as u128 * other.as_nanos()) / 1_000_000_000;
        Instant(self.0 - ticks as u64)
    }
}

impl SubAssign<Duration> for Instant {
    fn sub_assign(&mut self, other: Duration) {
        *self = *self - other;
    }
}

impl Sub<Instant> for Instant {
    type Output = Duration;

    fn sub(self, other: Self) -> Duration {
        let ticks = (self.0 as i128 - other.0 as i128).abs() as u64;
        let ns = (ticks as u128 * 1_000_000_000) / frequency() as u128;
        Duration::from_nanos(ns as u64)
    }
}

/// A trait representing a counter on the system with a consistent frequency.
pub trait TimeSource {
    /// The current value of the counter.
    fn now(&self) -> Instant;

    /// The frequency this counter increments in ticks per second.
    fn frequency(&self) -> u64;
}

enum TimerMode {
    OneShot,
    Periodic,
}

/// A one-shot or periodic timer that has not not yet been started
pub struct ReadyTimer {
    duration: Duration,
    mode: TimerMode,

    // The interrupt vector to deliver to the guest when this timer
    // expires.
    // TODO: Not all timers represent interrupts to deliver to the guest,
    // so we will need to make this more abstract.
    vector: u8,
}

/// A started one-shot or periodic timer
pub struct RunningTimer {
    duration: Duration,
    mode: TimerMode,
    started: Instant,
    vector: u8,
}

impl ReadyTimer {
    /// Create a new one-shot timer.
    pub fn one_shot(duration: Duration, vector: u8) -> Self {
        Self {
            duration: duration,
            mode: TimerMode::OneShot,
            vector: vector,
        }
    }

    /// Create a new periodic timer.
    pub fn periodic(period: Duration, vector: u8) -> Self {
        Self {
            duration: period,
            mode: TimerMode::Periodic,
            vector: vector,
        }
    }

    /// Start the timer.
    pub fn start(self) -> RunningTimer {
        RunningTimer {
            duration: self.duration,
            mode: self.mode,
            started: now(),
            vector: self.vector,
        }
    }

    /// Returns whether the timer is periodic
    pub fn is_periodic(&self) -> bool {
        match self.mode {
            TimerMode::Periodic => true,
            TimerMode::OneShot => false,
        }
    }
}

impl RunningTimer {
    /// Stop the timer
    pub fn stop(self) -> ReadyTimer {
        ReadyTimer {
            duration: self.duration,
            mode: self.mode,
            vector: self.vector,
        }
    }

    /// Returns whether the timer is periodic
    pub fn is_periodic(&self) -> bool {
        match self.mode {
            TimerMode::Periodic => true,
            TimerMode::OneShot => false,
        }
    }

    /// Returns whether the timer has elapsed
    ///
    /// Note that for a periodic timer, `reset` must still be called
    /// before `elapsed` will return false after having elapsed once.
    pub fn elapsed(&self) -> bool {
        now() - self.started > self.duration
    }

    /// Reset this timer
    ///
    /// Set the effective start time for this timer to 'now' (as determined
    /// by the global system timer) for one-shot timers. For periodic timers,
    /// it sets the start time to the previous elapses_at time.
    pub fn reset(&mut self) {
        self.started = if self.is_periodic() {
            self.elapses_at()
        } else {
            now()
        };
    }

    /// Determine when the timer will next elapse
    pub fn elapses_at(&self) -> Instant {
        self.started + self.duration
    }
}

/// Initialize the timer wheel for the current core
pub unsafe fn init_timer_wheel() -> Result<()> {
    let wheel = get_per_core_mut!(TIMER_WHEEL);
    *wheel = Some(TimerWheel::new());
    Ok(())
}

/// Get a reference to the current core's TimerWheel
pub fn get_timer_wheel() -> &'static TimerWheel {
    get_per_core!(TIMER_WHEEL)
        .as_ref()
        .expect("TimerWheel has not been initialized")
}

/// Get a mutable reference to the current core's TimerWheel
pub unsafe fn get_timer_wheel_mut() -> &'static mut TimerWheel {
    get_per_core_mut!(TIMER_WHEEL)
        .as_mut()
        .expect("TimerWheel has not been initialized")
}

/// Timer identifier that may be used to cancel a running timer
#[derive(Eq, PartialEq, PartialOrd, Ord, Clone, Debug)]
pub struct TimerId(u64);

// TimerId can only be used with the wheel that created the timer. To capture
// this, don't allow the ids to be sent betweeen cores
impl !Send for TimerId {}

/// A container for running timers on a given core
///
/// The TimerWheel allows multiple virtual timers to be serviced by a single
/// physical time source (the global TimeSource).
pub struct TimerWheel {
    counter: u64,
    timers: BTreeMap<TimerId, RunningTimer>,
}

impl TimerWheel {
    fn new() -> Self {
        TimerWheel {
            counter: 0,
            timers: BTreeMap::new(),
        }
    }

    /// Evalute timers and return generated guest interrupts
    ///
    /// This method will remove any one-shot timers that have
    /// expired and will reset any periodic timers.
    pub fn expire_elapsed_timers(
        &mut self,
    ) -> Result<vec::Vec<(u8, vcpu::InjectedInterruptType)>> {
        let mut interrupts = vec![];
        let elapsed_oneshots = self
            .timers
            .iter()
            .filter_map(|(id, timer)| {
                if timer.elapsed() && !timer.is_periodic() {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect::<vec::Vec<_>>();

        for id in elapsed_oneshots {
            interrupts.push((
                self.timers[&id].vector,
                vcpu::InjectedInterruptType::ExternalInterrupt,
            ));
            self.timers.remove(&id);
        }

        for (_, timer) in self
            .timers
            .iter_mut()
            .filter(|(_, timer)| timer.elapsed() && timer.is_periodic())
        {
            interrupts.push((
                timer.vector,
                vcpu::InjectedInterruptType::ExternalInterrupt,
            ));
            timer.reset();
        }

        self.update_interrupt_timer();
        Ok(interrupts)
    }

    fn update_interrupt_timer(&mut self) {
        let soonest = self
            .timers
            .values()
            .map(|timer| (timer.elapses_at(), timer.vector))
            .min();

        // TODO: we should only actually reset this if the new time
        // is sooner than the last time we set
        if let Some((when, _)) = soonest {
            unsafe {
                apic::get_local_apic_mut()
                    .schedule_interrupt(when, TIMER_VECTOR);
            }
        }
    }

    /// Register a tiemr with this TimerWheel
    pub fn register_timer(&mut self, timer: ReadyTimer) -> TimerId {
        let counter = self.counter;
        self.timers.insert(TimerId(counter), timer.start());
        self.counter = self.counter.wrapping_add(1);

        self.update_interrupt_timer();

        TimerId(counter)
    }

    /// Get a timer in this wheel by ID (if one exists)
    pub fn get_timer(&self, id: &TimerId) -> Option<&RunningTimer> {
        self.timers.get(id)
    }

    /// Get a mutable reference to a timer in this wheel by ID (if one exists)
    pub fn get_timer_mut(&mut self, id: &TimerId) -> Option<&mut RunningTimer> {
        self.timers.get_mut(id)
    }

    /// Remove a timer in this wheel by ID
    pub fn remove_timer(&mut self, id: &TimerId) {
        self.timers.remove(id);

        self.update_interrupt_timer();
    }

    /// Returns an iterator over the timers in this wheel
    pub fn iter<'a>(&'a self) -> impl Iterator<Item = &RunningTimer> + 'a {
        self.timers.values()
    }

    /// Returns an iterator that allows modifying each value
    pub fn iter_mut<'a>(
        &'a mut self,
    ) -> impl Iterator<Item = &mut RunningTimer> + 'a {
        self.timers.values_mut()
    }
}

/// Delay current core for the provided `duration`
pub fn busy_wait(duration: core::time::Duration) {
    let start = now();
    while now() < start + duration {
        unsafe {
            // Relax the cpu
            llvm_asm!("rep; nop" ::: "memory");
        }
    }
}

/// Cancel a timer set on this core
pub fn cancel_timer(id: &TimerId) -> Result<()> {
    let wheel = unsafe { get_timer_wheel_mut() };
    wheel.remove_timer(id);
    Ok(())
}

/// Set a one shot timer on this core
pub fn set_oneshot_timer(
    duration: core::time::Duration,
    vector: u8,
) -> TimerId {
    let wheel = unsafe { get_timer_wheel_mut() };
    let timer = ReadyTimer::one_shot(duration, vector);
    wheel.register_timer(timer)
}

/// Set a periodic timer on this core
pub fn set_periodic_timer(
    interval: core::time::Duration,
    vector: u8,
) -> TimerId {
    let wheel = unsafe { get_timer_wheel_mut() };
    let timer = ReadyTimer::periodic(interval, vector);
    wheel.register_timer(timer)
}
