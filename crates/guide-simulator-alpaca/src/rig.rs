//! The state both devices share: the simulation on the wall clock, each
//! device's session, the scripted faults and the records.

use std::time::Instant;

use guide_simulator::{SimTime, Simulator};
use ndarray::{s, Array2, Zip};
use parking_lot::{Mutex, MutexGuard};

use crate::config::{
    CameraDeviceConfig, Device, ExposureRecord, FrameFault, MountDeviceConfig, PulseRecord, Region,
};

/// The simulation, the devices' configuration and their state.
#[derive(Debug)]
pub struct Rig {
    started: Instant,
    pub camera: CameraDeviceConfig,
    pub mount: MountDeviceConfig,
    state: Mutex<State>,
}

/// What changes while the devices run.
#[derive(Debug)]
pub struct State {
    pub sim: Simulator,
    pub camera: CameraSession,
    pub mount: MountSession,
    pub pulses: Vec<PulseRecord>,
    pub exposures: Vec<ExposureRecord>,
    pub aborts: usize,
}

/// The camera's connection, settings, exposure and faults.
#[derive(Debug)]
pub struct CameraSession {
    pub connected: bool,
    pub reachable: bool,
    pub bin: u8,
    pub region: Region,
    pub gain: i32,
    pub offset: i32,
    pub exposure: Option<Exposure>,
    pub faults: Faults,
}

/// An exposure that has started: its frame is rendered already.
#[derive(Debug)]
pub struct Exposure {
    /// When it ends.
    pub end: SimTime,
    /// When `ImageReady` turns true; never when `None`.
    pub ready: Option<SimTime>,
    /// The frame, binned and cut to its region, indexed `[[y, x]]`.
    pub image: Array2<u16>,
}

/// The faults scripted for the coming exposures.
#[derive(Debug, Default)]
pub struct Faults {
    fault: Option<FrameFault>,
    /// How many more exposures fail; all of them while `None`.
    remaining: Option<usize>,
}

impl Faults {
    /// Makes the next `count` exposures fail with `fault`, or every one
    /// while `count` is `None`.
    pub const fn set(&mut self, fault: FrameFault, count: Option<usize>) {
        self.fault = Some(fault);
        self.remaining = count;
    }

    /// Clears every scripted fault.
    pub const fn clear(&mut self) {
        self.fault = None;
        self.remaining = None;
    }

    /// The fault of the exposure starting now, counted off.
    pub fn take(&mut self) -> Option<FrameFault> {
        let fault = self.fault?;
        match self.remaining {
            None => Some(fault),
            Some(0) => {
                self.clear();
                None
            }
            Some(remaining) => {
                let left = remaining.saturating_sub(1);
                if left == 0 {
                    self.clear();
                } else {
                    self.remaining = Some(left);
                }
                Some(fault)
            }
        }
    }
}

/// The mount's connection, what it reports and its scripted faults.
#[derive(Debug)]
pub struct MountSession {
    pub connected: bool,
    pub reachable: bool,
    pub can_pulse_guide: bool,
    pub stuck_pulses: StuckPulses,
}

/// Whether `IsPulseGuiding` ends with its pulse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StuckPulses {
    /// It ends with its pulse.
    Off,
    /// The next pulse keeps it true after its end.
    Armed,
    /// A pulse accepted while armed keeps it true.
    Stuck,
}

impl Rig {
    /// The rig at the start of the simulation, its devices not connected.
    pub fn new(sim: Simulator, camera: CameraDeviceConfig, mount: MountDeviceConfig) -> Self {
        let full = Region {
            x: 0,
            y: 0,
            width: dimension(sim.camera().width),
            height: dimension(sim.camera().height),
        };
        let can_pulse_guide = mount.can_pulse_guide;
        Self {
            started: Instant::now(),
            camera,
            mount,
            state: Mutex::new(State {
                sim,
                camera: CameraSession {
                    connected: false,
                    reachable: true,
                    bin: 1,
                    region: full,
                    gain: 0,
                    offset: 0,
                    exposure: None,
                    faults: Faults::default(),
                },
                mount: MountSession {
                    connected: false,
                    reachable: true,
                    can_pulse_guide,
                    stuck_pulses: StuckPulses::Off,
                },
                pulses: Vec::new(),
                exposures: Vec::new(),
                aborts: 0,
            }),
        }
    }

    /// The wall clock as simulated time: the time since the rig started.
    pub fn now(&self) -> SimTime {
        SimTime::from_start(self.started.elapsed())
    }

    /// Runs `f` on the state, with the simulation run up to now.
    pub fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        let now = self.now();
        f(&mut Self::advanced(self.state.lock(), now))
    }

    fn advanced(mut state: MutexGuard<'_, State>, now: SimTime) -> MutexGuard<'_, State> {
        state.sim.mount_mut().advance_to(now);
        state
    }

    /// Whether `device`'s server answers.
    pub fn reachable(&self, device: Device) -> bool {
        let reachable = |state: &State| match device {
            Device::Camera => state.camera.reachable,
            Device::Mount => state.mount.reachable,
        };
        reachable(&self.state.lock())
    }
}

impl State {
    /// The full frame at `bin`, in binned pixels.
    pub fn full_region(&self, bin: u8) -> Region {
        let bin = u32::from(bin.max(1));
        Region {
            x: 0,
            y: 0,
            width: dimension(self.sim.camera().width)
                .checked_div(bin)
                .unwrap_or(0),
            height: dimension(self.sim.camera().height)
                .checked_div(bin)
                .unwrap_or(0),
        }
    }
}

/// A sensor dimension as Alpaca reports it.
pub fn dimension(pixels: usize) -> u32 {
    u32::try_from(pixels).unwrap_or(u32::MAX)
}

/// Whether `region` lies on a binned sensor `full` wide and high and
/// holds at least one pixel.
pub fn fits(region: Region, full: Region) -> bool {
    let right = region.x.checked_add(region.width);
    let bottom = region.y.checked_add(region.height);
    region.width > 0
        && region.height > 0
        && right.is_some_and(|right| right <= full.width)
        && bottom.is_some_and(|bottom| bottom <= full.height)
}

/// `frame` binned by summing each `bin` × `bin` cell, held to `clip`.
/// The rows and columns past the last whole cell are left out.
pub fn binned(frame: &Array2<u16>, bin: u8, clip: u16) -> Array2<u16> {
    let bin = usize::from(bin.max(1));
    Zip::from(frame.exact_chunks((bin, bin))).map_collect(|cell| {
        let sum: u32 = cell.iter().map(|&value| u32::from(value)).sum();
        u16::try_from(sum.min(u32::from(clip))).unwrap_or(clip)
    })
}

/// `frame` cut to `region`, `narrower` pixels fewer in width. The
/// caller has checked that `region` fits the frame.
pub fn cut(frame: &Array2<u16>, region: Region, narrower: u32) -> Array2<u16> {
    let at = |value: u32| usize::try_from(value).unwrap_or(usize::MAX);
    let x = at(region.x);
    let y = at(region.y);
    let right = at(region
        .x
        .saturating_add(region.width.saturating_sub(narrower)));
    let bottom = at(region.y.saturating_add(region.height));
    frame.slice(s![y..bottom, x..right]).to_owned()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use ndarray::array;

    #[test]
    fn a_count_of_faults_runs_out() {
        let mut faults = Faults::default();
        faults.set(FrameFault::StartFails, Some(2));
        assert_eq!(faults.take(), Some(FrameFault::StartFails));
        assert_eq!(faults.take(), Some(FrameFault::StartFails));
        assert_eq!(faults.take(), None);
    }

    #[test]
    fn faults_without_a_count_last_until_cleared() {
        let mut faults = Faults::default();
        faults.set(FrameFault::NeverReady, None);
        for _ in 0..5 {
            assert_eq!(faults.take(), Some(FrameFault::NeverReady));
        }
        faults.clear();
        assert_eq!(faults.take(), None);
    }

    #[test]
    fn a_count_of_zero_fails_nothing() {
        let mut faults = Faults::default();
        faults.set(FrameFault::WrongSize, Some(0));
        assert_eq!(faults.take(), None);
    }

    #[test]
    fn binning_sums_each_cell_and_leaves_out_a_partial_one() {
        let frame = array![[1_u16, 2, 3, 4, 9], [5, 6, 7, 8, 9], [9, 9, 9, 9, 9]];
        assert_eq!(binned(&frame, 2, u16::MAX), array![[14_u16, 22]]);
    }

    #[test]
    fn a_binned_pixel_is_held_to_the_clip() {
        let frame = array![[40_000_u16, 40_000], [40_000, 40_000]];
        assert_eq!(binned(&frame, 2, 60_000), array![[60_000_u16]]);
    }

    #[test]
    fn binning_by_one_keeps_the_frame() {
        let frame = array![[1_u16, 2], [3, 4]];
        assert_eq!(binned(&frame, 1, u16::MAX), frame);
    }

    #[test]
    fn a_region_must_hold_a_pixel_and_lie_on_the_sensor() {
        let full = Region {
            x: 0,
            y: 0,
            width: 10,
            height: 8,
        };
        let region = |x, y, width, height| Region {
            x,
            y,
            width,
            height,
        };
        assert!(fits(full, full));
        assert!(fits(region(9, 7, 1, 1), full));
        assert!(!fits(region(9, 7, 2, 1), full));
        assert!(!fits(region(0, 0, 0, 1), full));
        assert!(!fits(region(u32::MAX, 0, 2, 1), full));
    }

    #[test]
    fn a_cut_takes_the_region_and_can_be_narrower() {
        let frame = array![[1_u16, 2, 3], [4, 5, 6], [7, 8, 9]];
        let region = Region {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        assert_eq!(cut(&frame, region, 0), array![[5_u16, 6], [8, 9]]);
        assert_eq!(cut(&frame, region, 1), array![[5_u16], [8]]);
    }
}
