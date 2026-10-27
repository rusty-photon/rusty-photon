//! Mock Advanced 3 transport for testing without real hardware.
//!
//! Provides a [`TransportFactory`] that hands out a [`FrameTransport`]
//! backed by an in-memory hub. Persists state across reconnects so tests can
//! disconnect/reconnect and still observe their prior writes, as the real
//! hub does when an ASCOM client cycles `Connected`.
//!
//! The simulated hub serves the frame captured from the dev box's unit, so
//! a scaling or field-order regression fails `sensor_readings.feature`, and
//! it **streams**: after the first `>G#` it queues a frame every
//! [`STREAM_PERIOD`] of wall-clock time, as the hub does, and when a knob
//! says so it queues two already-streamed frames behind every reply, so the
//! discard before a poll or a write is load-bearing on every request rather
//! than by chance. Its `discard_input` drops the queue. Set commands change
//! the state and draw no reply; `>X`/`>Y` apply to the duty fields at once,
//! until hardware says otherwise.
//!
//! State the driver cannot set is preset from the environment, which is the
//! only channel a BDD harness has into a binary it launches as a subprocess:
//! the firmware and identity replies, which sensors are attached, the dew
//! modes at start, the input voltage, and the three behaviours the fault
//! scenarios need — the stale frames, an ignored first `>H#`, and an output
//! the hub refuses to switch. Each is read once at construction; the
//! `with_…` builders are the in-process equivalents.

// `#[cfg(any(feature = "mock", test))]`-gated test-helper infrastructure
// that never ships in production builds. Excluded from coverage so the
// workspace coverage number reflects only production-shipped code.
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::debug;

use crate::protocol::{DC_OUTPUT_COUNT, DEW_CHANNEL_COUNT, IDENTITY, USB_PORT_COUNT};

/// The `>V#` reply, e.g. `307`. Default `308`, the dev box's unit.
pub const ENV_FIRMWARE: &str = "GEMINI_MOCK_FIRMWARE";
/// The `>H#` reply, for the mismatch scenarios. Default the Advanced 3's.
///
/// A value starting with [`RAW_IDENTITY_PREFIX`] is sent as it is, without
/// the `*H` prefix: a device on the port that is not a Gemini hub at all.
pub const ENV_IDENTITY: &str = "GEMINI_MOCK_IDENTITY";
/// Marks an [`ENV_IDENTITY`] value that goes on the wire verbatim.
pub const RAW_IDENTITY_PREFIX: &str = "raw:";
/// Two flags, AHT20 then DS18B20: `10` is the AHT20 only. Default `11`.
pub const ENV_SENSORS: &str = "GEMINI_MOCK_SENSORS";
/// The two mode digits at start, DEW6 then DEW7: `00` is both in Auto.
/// Default `11`, the hub's power-up state.
pub const ENV_DEW_MODES: &str = "GEMINI_MOCK_DEW_MODES";
/// The input voltage the frame carries, e.g. `24.0`. Default `12.2`.
pub const ENV_INPUT_VOLTAGE: &str = "GEMINI_MOCK_INPUT_VOLTAGE";
/// `1`: once armed, queue two already-streamed frames behind every reply.
pub const ENV_STALE_FRAMES: &str = "GEMINI_MOCK_STALE_FRAMES";
/// `1`: the first `>H#` draws no reply within the timeout.
pub const ENV_IGNORE_FIRST_IDENTITY: &str = "GEMINI_MOCK_IGNORE_FIRST_IDENTITY";
/// A DC output number, 2 to 5, whose state the hub never changes — a
/// tripped output that refuses to come back.
pub const ENV_STUCK_OUTPUT: &str = "GEMINI_MOCK_STUCK_OUTPUT";
/// `1`: the hub is streaming when the port opens, as a unit left armed by
/// an earlier session is — the stream survives a close.
pub const ENV_STREAMING_AT_START: &str = "GEMINI_MOCK_STREAMING_AT_START";
/// `1`: once armed, every reply arrives glued to a periodic frame the hub
/// cut short to answer, in one chunk — what the dev box's unit does.
pub const ENV_CUT_FRAMES: &str = "GEMINI_MOCK_CUT_FRAMES";
/// `1`: every set is answered first by the frame the hub started before
/// acting on it, then by the post-set frame — the periodic frame that
/// starts during the set, which only the second look sees past.
pub const ENV_PRE_WRITE_FRAME: &str = "GEMINI_MOCK_PRE_WRITE_FRAME";

/// How often the armed hub streams a frame: measured 3.000 s.
pub const STREAM_PERIOD: Duration = Duration::from_secs(3);

/// The timeout the mock reports for an ignored request.
const IGNORED_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// How many already-streamed frames the stale-frame knob queues behind each
/// reply. Two, so that a broken discard fails a write on both looks rather
/// than being rescued by the second.
const STALE_FRAMES_PER_REPLY: usize = 2;

/// How much of a periodic frame the hub had sent when it cut it short to
/// answer, on the dev box's unit: through `19.86D12`, 63 of 81 bytes.
const CUT_FRAME_BYTES: usize = 63;

/// One dew channel of the simulated hub.
#[derive(Debug, Clone, Copy)]
struct DewState {
    enabled: bool,
    mode: u8,
    duty: u8,
}

/// The simulated hub's settings and readings.
#[derive(Debug, Clone)]
struct MockDeviceState {
    identity: String,
    firmware: String,
    dc: [bool; DC_OUTPUT_COUNT],
    usb: [bool; USB_PORT_COUNT],
    ambient_sensor_attached: bool,
    lens_probe_attached: bool,
    dew: [DewState; DEW_CHANNEL_COUNT],
    lens_temperature: f64,
    ambient_temperature: f64,
    humidity: f64,
    dew_point: f64,
    input_voltage: f64,
    output_current: f64,
    output_power: f64,
    quirks: Quirks,
    /// How many `>H#` the hub still ignores.
    ignore_identity: u32,
    stuck_output: Option<u8>,
}

/// The behaviours a fault scenario turns on, each the twin of an
/// environment knob.
#[derive(Debug, Clone, Copy, Default)]
struct Quirks {
    /// Two already-streamed frames behind every reply once armed.
    stale_frames_before_reply: bool,
    /// Every reply glued to a periodic frame cut short once armed.
    cut_frames_before_reply: bool,
    /// The stream is already running when the port opens.
    streaming_at_start: bool,
    /// What a set is answered with first.
    readback: Readback,
}

/// What answers a set: the frame the hub produced after acting on it, or
/// first the frame it started before the set reached it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Readback {
    #[default]
    PostSet,
    PreSetFirst,
}

impl Default for MockDeviceState {
    /// The dev box's unit as found on 2026-10-26: everything on, both
    /// sensors attached, both dew channels Manual at 100 %.
    fn default() -> Self {
        let (ambient_sensor_attached, lens_probe_attached) = env_sensors();
        let (dew6_mode, dew7_mode) = env_dew_modes();
        Self {
            identity: std::env::var(ENV_IDENTITY).unwrap_or_else(|_| IDENTITY.to_string()),
            firmware: std::env::var(ENV_FIRMWARE).unwrap_or_else(|_| "308".to_string()),
            dc: [true; DC_OUTPUT_COUNT],
            usb: [true; USB_PORT_COUNT],
            ambient_sensor_attached,
            lens_probe_attached,
            dew: [
                DewState {
                    enabled: true,
                    mode: dew6_mode,
                    duty: 100,
                },
                DewState {
                    enabled: true,
                    mode: dew7_mode,
                    duty: 100,
                },
            ],
            lens_temperature: 23.25,
            ambient_temperature: 24.47,
            humidity: 67.76,
            dew_point: 18.12,
            input_voltage: env_input_voltage(),
            output_current: 0.02,
            output_power: 0.36,
            quirks: Quirks {
                stale_frames_before_reply: env_flag(ENV_STALE_FRAMES),
                cut_frames_before_reply: env_flag(ENV_CUT_FRAMES),
                streaming_at_start: env_flag(ENV_STREAMING_AT_START),
                readback: if env_flag(ENV_PRE_WRITE_FRAME) {
                    Readback::PreSetFirst
                } else {
                    Readback::PostSet
                },
            },
            ignore_identity: u32::from(env_flag(ENV_IGNORE_FIRST_IDENTITY)),
            stuck_output: env_stuck_output(),
        }
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v.trim() == "1")
}

fn env_sensors() -> (bool, bool) {
    std::env::var(ENV_SENSORS)
        .ok()
        .as_deref()
        .and_then(parse_two_flags)
        .unwrap_or((true, true))
}

fn env_dew_modes() -> (u8, u8) {
    std::env::var(ENV_DEW_MODES)
        .ok()
        .as_deref()
        .and_then(parse_two_modes)
        .unwrap_or((1, 1))
}

fn env_input_voltage() -> f64 {
    std::env::var(ENV_INPUT_VOLTAGE)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(12.2)
}

fn env_stuck_output() -> Option<u8> {
    std::env::var(ENV_STUCK_OUTPUT)
        .ok()
        .and_then(|v| v.trim().parse::<u8>().ok())
        .filter(|n| (2..=5).contains(n))
}

/// Two `0`/`1` characters. A typo yields `None`, and the caller falls back
/// to its default rather than guessing.
fn parse_two_flags(raw: &str) -> Option<(bool, bool)> {
    let mut chars = raw.trim().chars();
    let (a, b, rest) = (chars.next()?, chars.next()?, chars.next());
    if rest.is_some() {
        return None;
    }
    let flag = |c: char| match c {
        '0' => Some(false),
        '1' => Some(true),
        _ => None,
    };
    Some((flag(a)?, flag(b)?))
}

/// Two mode digits, each 0 to 2.
fn parse_two_modes(raw: &str) -> Option<(u8, u8)> {
    let mut chars = raw.trim().chars();
    let (a, b, rest) = (chars.next()?, chars.next()?, chars.next());
    if rest.is_some() {
        return None;
    }
    let mode = |c: char| {
        c.to_digit(10)
            .filter(|d| *d <= 2)
            .and_then(|d| u8::try_from(d).ok())
    };
    Some((mode(a)?, mode(b)?))
}

/// Render a run of states as the `0`/`1` characters the frame uses.
fn flags(states: &[bool]) -> String {
    states
        .iter()
        .map(|on| if *on { '1' } else { '0' })
        .collect()
}

impl MockDeviceState {
    /// The `*G` frame in the capture's exact layout and field widths. An
    /// absent sensor's fields read `nan`: what the firmware writes there is
    /// not known, and this is the worst case the parser tolerates.
    fn frame(&self) -> String {
        let reading = |value: f64, attached: bool| {
            if attached {
                format!("{value:6.2}")
            } else {
                "   nan".to_string()
            }
        };
        format!(
            "*G{dc}D{usb}U{a}A{t}T{e6}A{m6}M{e7}B{m7}M{d6}C{d7}C{lens}S{amb}T\
             {hum}H{dp}D{v:.1}V{cur:5.2}C{pow:6.2}P#",
            dc = flags(&self.dc),
            usb = flags(&self.usb),
            a = u8::from(self.ambient_sensor_attached),
            t = u8::from(self.lens_probe_attached),
            e6 = u8::from(self.dew[0].enabled),
            m6 = self.dew[0].mode,
            e7 = u8::from(self.dew[1].enabled),
            m7 = self.dew[1].mode,
            d6 = self.dew[0].duty,
            d7 = self.dew[1].duty,
            lens = reading(self.lens_temperature, self.lens_probe_attached),
            amb = reading(self.ambient_temperature, self.ambient_sensor_attached),
            hum = reading(self.humidity, self.ambient_sensor_attached),
            dp = reading(self.dew_point, self.ambient_sensor_attached),
            v = self.input_voltage,
            cur = self.output_current,
            pow = self.output_power,
        )
    }

    /// `>O<n>#` / `>C<n>#`: outputs 2-5 are DC2-DC5, 6-11 are USB A-F.
    /// Output 1 is DC1, which the hub keeps on; a stuck output keeps its
    /// state whatever is asked.
    fn set_output(&mut self, number: u8, on: bool) {
        if self.stuck_output == Some(number) {
            debug!(number, "mock: output is stuck; ignoring the set");
            return;
        }
        match number {
            2..=5 => {
                if let Some(slot) = usize::from(number)
                    .checked_sub(2)
                    .and_then(|i| self.dc.get_mut(i))
                {
                    *slot = on;
                }
            }
            6..=11 => {
                if let Some(slot) = usize::from(number)
                    .checked_sub(6)
                    .and_then(|i| self.usb.get_mut(i))
                {
                    *slot = on;
                }
            }
            other => debug!(other, "mock: output number outside 2-11; ignored"),
        }
    }

    fn dew_mut(&mut self, channel_digit: char) -> Option<&mut DewState> {
        match channel_digit {
            '1' => self.dew.get_mut(0),
            '2' => self.dew.get_mut(1),
            _ => None,
        }
    }

    /// Apply one `>X…#` command.
    fn apply(&mut self, command: &str) {
        let Some(body) = command.strip_prefix('>').and_then(|c| c.strip_suffix('#')) else {
            debug!(command, "mock: not a Gemini command; ignored");
            return;
        };
        let mut chars = body.chars();
        let letter = chars.next();
        let rest = chars.as_str();
        match letter {
            Some('O' | 'C') => {
                if let Ok(number) = rest.parse::<u8>() {
                    self.set_output(number, letter == Some('O'));
                } else {
                    debug!(command, "mock: bad output number");
                }
            }
            Some('X' | 'Y') => {
                if let Ok(duty) = rest.parse::<u8>() {
                    let index = usize::from(letter == Some('Y'));
                    if let Some(channel) = self.dew.get_mut(index) {
                        channel.duty = duty;
                    }
                } else {
                    debug!(command, "mock: bad duty");
                }
            }
            Some('Z' | 'M') => {
                let mut digits = rest.chars();
                let (Some(channel), Some(value), None) =
                    (digits.next(), digits.next(), digits.next())
                else {
                    debug!(command, "mock: bad channel command");
                    return;
                };
                let Some(state) = self.dew_mut(channel) else {
                    debug!(command, "mock: no such dew channel");
                    return;
                };
                if letter == Some('Z') {
                    state.enabled = value == '1';
                } else if let Some(mode) = value.to_digit(10).filter(|d| *d <= 2) {
                    state.mode = u8::try_from(mode).unwrap_or(1);
                } else {
                    debug!(command, "mock: bad mode digit");
                }
            }
            _ => debug!(command, "mock: unknown command; ignored"),
        }
    }
}

/// The simulated hub plus the bytes it has queued for the host.
#[derive(Debug)]
struct MockState {
    device: MockDeviceState,
    queue: VecDeque<Vec<u8>>,
    /// When the first `>G#` armed the stream, and how many periodic frames
    /// have been emitted since.
    armed_at: Option<Instant>,
    streamed: u64,
    /// The next read reports a timeout: the hub ignored a request.
    answer_with_timeout: bool,
    /// Every command received, newest last, as the driver sent it minus the
    /// newline.
    commands: Vec<String>,
    /// Every `send_frame` buffer received, newest last, trimmed: a set and
    /// its `>G#` arrive as one.
    buffers: Vec<String>,
}

impl MockState {
    fn new(device: MockDeviceState) -> Self {
        let armed_at = device.quirks.streaming_at_start.then(Instant::now);
        Self {
            device,
            queue: VecDeque::new(),
            armed_at,
            streamed: 0,
            answer_with_timeout: false,
            commands: Vec::new(),
            buffers: Vec::new(),
        }
    }

    /// Queue every periodic frame that has fallen due since the stream was
    /// armed, so a request that comes along finds them ahead of its reply —
    /// which is what the discard exists for.
    fn stream_due_frames(&mut self) {
        let Some(armed_at) = self.armed_at else {
            return;
        };
        let due = armed_at
            .elapsed()
            .as_nanos()
            .checked_div(STREAM_PERIOD.as_nanos())
            .and_then(|periods| u64::try_from(periods).ok())
            .unwrap_or(u64::MAX);
        while self.streamed < due {
            self.queue.push_back(self.device.frame().into_bytes());
            self.streamed = self.streamed.saturating_add(1);
        }
    }

    fn reply(&mut self, frame: &str) {
        let mut chunk = Vec::new();
        if self.device.quirks.cut_frames_before_reply && self.armed_at.is_some() {
            // The hub was part-way through a periodic frame when the command
            // arrived: it stops, and the reply follows in the same bytes.
            let cut = self.device.frame().into_bytes();
            chunk.extend_from_slice(cut.get(..CUT_FRAME_BYTES).unwrap_or(&cut));
        }
        chunk.extend_from_slice(frame.as_bytes());
        self.queue.push_back(chunk);
        if self.device.quirks.stale_frames_before_reply && self.armed_at.is_some() {
            for _ in 0..STALE_FRAMES_PER_REPLY {
                self.queue.push_back(self.device.frame().into_bytes());
            }
        }
    }

    fn process(&mut self, command: &str) {
        self.commands.push(command.to_string());
        match command {
            ">H#" => {
                if self.device.ignore_identity > 0 {
                    self.device.ignore_identity = self.device.ignore_identity.saturating_sub(1);
                    self.answer_with_timeout = true;
                    debug!("mock: ignoring >H#");
                } else if let Some(raw) = self.device.identity.strip_prefix(RAW_IDENTITY_PREFIX) {
                    // A device that is not a Gemini hub at all: its frames
                    // never carry the `*H` prefix, and it keeps talking, so
                    // every read inside the skip budget gets one.
                    for _ in 0..=crate::codec::MAX_SKIP {
                        self.queue.push_back(raw.as_bytes().to_vec());
                    }
                } else {
                    self.reply(&format!("*H{}#", self.device.identity));
                }
            }
            ">V#" => self.reply(&format!("*V{}#", self.device.firmware)),
            ">G#" => {
                if self.armed_at.is_none() {
                    self.armed_at = Some(Instant::now());
                    self.streamed = 0;
                }
                self.reply(&self.device.frame());
            }
            set => {
                // A periodic frame the hub started before the set reached
                // it predates the set and is the first whole frame read.
                let before = (self.device.quirks.readback == Readback::PreSetFirst)
                    .then(|| self.device.frame());
                self.device.apply(set);
                if let Some(frame) = before {
                    self.reply(&frame);
                }
            }
        }
    }

    /// One `send_frame`: one or more newline-terminated commands.
    fn process_buffer(&mut self, bytes: &[u8]) {
        let text = std::str::from_utf8(bytes).unwrap_or_default();
        debug!(
            buffer = text.trim(),
            dc = %flags(&self.device.dc),
            usb = %flags(&self.device.usb),
            "mock processing commands"
        );
        self.buffers.push(text.trim().to_string());
        for command in text.split('\n').map(str::trim).filter(|c| !c.is_empty()) {
            self.stream_due_frames();
            self.process(command);
        }
    }
}

/// One open mock transport. Shares state with the factory so persistent
/// device settings survive a reconnect cycle.
struct MockFrameTransport {
    state: Arc<Mutex<MockState>>,
}

#[async_trait]
impl FrameTransport for MockFrameTransport {
    async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        self.state.lock().await.process_buffer(bytes);
        Ok(())
    }

    async fn recv_frame(&mut self, buf: &mut Vec<u8>) -> Result<(), TransportError> {
        let mut state = self.state.lock().await;
        if std::mem::take(&mut state.answer_with_timeout) {
            return Err(TransportError::Timeout(IGNORED_REQUEST_TIMEOUT));
        }
        state.stream_due_frames();
        let frame = match state.queue.pop_front() {
            Some(frame) => frame,
            // An armed hub's next frame is at most one period away; a quiet
            // one has nothing to say.
            None if state.armed_at.is_some() => {
                state.streamed = state.streamed.saturating_add(1);
                state.device.frame().into_bytes()
            }
            None => return Err(TransportError::Eof),
        };
        drop(state);
        buf.clear();
        buf.extend_from_slice(&frame);
        Ok(())
    }

    /// What the real transport does to the OS buffer and its own: whatever
    /// the hub has sent and the host has not read is gone.
    async fn discard_input(&mut self) -> Result<(), TransportError> {
        let dropped = {
            let mut state = self.state.lock().await;
            state.stream_due_frames();
            let dropped = state.queue.len();
            state.queue.clear();
            dropped
        };
        if dropped > 0 {
            debug!(dropped, "mock: input discarded");
        }
        Ok(())
    }
}

/// Mock factory for the hub's transport.
///
/// Maintains persistent device state across multiple open/close cycles so
/// tests can power-cycle the connection without losing the simulated
/// device's settings — matching the behaviour of real hardware.
///
/// [`Default`] reads the presets from the environment; the builders below
/// set them in-process for tests that don't launch a binary.
#[derive(Clone)]
pub struct MockGeminiTransportFactory {
    state: Arc<Mutex<MockState>>,
}

impl Default for MockGeminiTransportFactory {
    fn default() -> Self {
        Self::with_device(MockDeviceState::default())
    }
}

impl MockGeminiTransportFactory {
    fn with_device(device: MockDeviceState) -> Self {
        Self {
            state: Arc::new(Mutex::new(MockState::new(device))),
        }
    }

    /// Change the device before any transport is opened. The builders run
    /// at construction, while this factory holds the only reference, so
    /// the state is reached without a lock; a preset applied after a
    /// transport was handed out would find the reference shared, and is
    /// dropped with a note rather than applied to a hub in use.
    fn map_device(mut self, change: impl FnOnce(&mut MockDeviceState)) -> Self {
        if let Some(state) = Arc::get_mut(&mut self.state) {
            change(&mut state.get_mut().device);
        } else {
            debug!("mock: a preset after the first open is ignored");
        }
        self
    }

    /// The `>V#` reply, e.g. `"307"`: the in-process twin of [`ENV_FIRMWARE`].
    #[must_use]
    pub fn with_firmware(self, firmware: &str) -> Self {
        let firmware = firmware.to_string();
        self.map_device(|d| d.firmware = firmware)
    }

    /// The `>H#` reply: the in-process twin of [`ENV_IDENTITY`].
    #[must_use]
    pub fn with_identity(self, identity: &str) -> Self {
        let identity = identity.to_string();
        self.map_device(|d| d.identity = identity)
    }

    /// Which sensors are attached: the in-process twin of [`ENV_SENSORS`].
    #[must_use]
    pub fn with_sensors(self, ambient: bool, probe: bool) -> Self {
        self.map_device(|d| {
            d.ambient_sensor_attached = ambient;
            d.lens_probe_attached = probe;
        })
    }

    /// Both channels' mode digits at start: the twin of [`ENV_DEW_MODES`].
    #[must_use]
    pub fn with_dew_modes(self, dew6: u8, dew7: u8) -> Self {
        self.map_device(|d| {
            d.dew[0].mode = dew6;
            d.dew[1].mode = dew7;
        })
    }

    /// The input voltage the frame carries: the twin of [`ENV_INPUT_VOLTAGE`].
    #[must_use]
    pub fn with_input_voltage(self, volts: f64) -> Self {
        self.map_device(|d| d.input_voltage = volts)
    }

    /// Queue two already-streamed frames behind every reply once armed: the
    /// twin of [`ENV_STALE_FRAMES`].
    #[must_use]
    pub fn with_stale_frames_before_reply(self) -> Self {
        self.map_device(|d| d.quirks.stale_frames_before_reply = true)
    }

    /// The first `>H#` draws no reply: the twin of
    /// [`ENV_IGNORE_FIRST_IDENTITY`].
    #[must_use]
    pub fn with_ignored_first_identity(self) -> Self {
        self.map_device(|d| d.ignore_identity = 1)
    }

    /// No `>H#` ever draws a reply: a unit that never identifies.
    #[must_use]
    pub fn with_ignored_identity(self) -> Self {
        self.map_device(|d| d.ignore_identity = u32::MAX)
    }

    /// A DC output, 2 to 5, the hub never switches: the twin of
    /// [`ENV_STUCK_OUTPUT`].
    #[must_use]
    pub fn with_stuck_output(self, number: u8) -> Self {
        self.map_device(|d| d.stuck_output = Some(number))
    }

    /// A hub already streaming when the port opens: the twin of
    /// [`ENV_STREAMING_AT_START`]. Arms the stream now, before any command.
    #[must_use]
    pub fn with_stream_running_at_start(mut self) -> Self {
        if let Some(state) = Arc::get_mut(&mut self.state) {
            let state = state.get_mut();
            state.device.quirks.streaming_at_start = true;
            state.armed_at = Some(Instant::now());
            state.streamed = 0;
        } else {
            debug!("mock: a preset after the first open is ignored");
        }
        self
    }

    /// Every reply glued to a periodic frame cut short, once armed: the twin
    /// of [`ENV_CUT_FRAMES`].
    #[must_use]
    pub fn with_cut_frames_before_reply(self) -> Self {
        self.map_device(|d| d.quirks.cut_frames_before_reply = true)
    }

    /// Every set answered first by the frame from before it: the twin of
    /// [`ENV_PRE_WRITE_FRAME`].
    #[must_use]
    pub fn with_a_periodic_frame_started_during_each_set(self) -> Self {
        self.map_device(|d| d.quirks.readback = Readback::PreSetFirst)
    }

    /// Change which sensors are attached on the running hub, as unplugging
    /// one would.
    pub async fn set_sensors(&self, ambient: bool, probe: bool) {
        let mut state = self.state.lock().await;
        state.device.ambient_sensor_attached = ambient;
        state.device.lens_probe_attached = probe;
    }

    /// Change the input voltage on the running hub.
    pub async fn set_input_voltage(&self, volts: f64) {
        self.state.lock().await.device.input_voltage = volts;
    }

    /// Whether DC output `number` (2 to 5) is on — the hub's own state,
    /// independent of the driver's cache.
    pub async fn dc_is_on(&self, number: u8) -> bool {
        let state = self.state.lock().await;
        usize::from(number)
            .checked_sub(2)
            .and_then(|i| state.device.dc.get(i).copied())
            .unwrap_or(false)
    }

    /// DEW6's or DEW7's mode digit on the hub.
    pub async fn dew_mode(&self, index: usize) -> u8 {
        let state = self.state.lock().await;
        state.device.dew.get(index).map_or(9, |d| d.mode)
    }

    /// Every command the hub has received, oldest first, one entry per
    /// command however they were buffered.
    pub async fn sent_commands(&self) -> Vec<String> {
        self.state.lock().await.commands.clone()
    }

    /// Every buffer the host wrote, oldest first: the shape the commands
    /// travelled in, so a set and its `>G#` show as one entry.
    pub async fn sent_buffers(&self) -> Vec<String> {
        self.state.lock().await.buffers.clone()
    }
}

#[async_trait]
impl TransportFactory for MockGeminiTransportFactory {
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        debug!("mock Gemini hub transport opened");
        Ok(Box::new(MockFrameTransport {
            state: Arc::clone(&self.state),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame the dev box's unit answered, byte for byte. The feature
    /// files assert individual fields of it: a change here changes them.
    const CAPTURED_FRAME: &str =
        "*G1111D111111U1A1T1A1M1B1M100C100C 23.25S 24.47T 67.76H 18.12D12.2V 0.02C  0.36P#";

    async fn open(factory: &MockGeminiTransportFactory) -> Box<dyn FrameTransport> {
        factory.open().await.unwrap()
    }

    async fn send(transport: &mut Box<dyn FrameTransport>, command: &str) {
        transport
            .send_frame(format!("{command}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn recv(transport: &mut Box<dyn FrameTransport>) -> String {
        let mut buf = Vec::new();
        transport.recv_frame(&mut buf).await.unwrap();
        String::from_utf8(buf).unwrap()
    }

    async fn exchange(transport: &mut Box<dyn FrameTransport>, command: &str) -> String {
        send(transport, command).await;
        recv(transport).await
    }

    #[tokio::test]
    async fn the_three_queries_answer_as_the_dev_box_did() {
        let factory = MockGeminiTransportFactory::default();
        let mut transport = open(&factory).await;
        assert_eq!(
            exchange(&mut transport, ">H#").await,
            "*HGeminiPowerBoxPlusAdv3#"
        );
        assert_eq!(exchange(&mut transport, ">V#").await, "*V308#");
        assert_eq!(exchange(&mut transport, ">G#").await, CAPTURED_FRAME);
    }

    #[tokio::test]
    async fn a_reply_carries_no_line_ending() {
        let factory = MockGeminiTransportFactory::default();
        let mut transport = open(&factory).await;
        let frame = exchange(&mut transport, ">G#").await;
        assert!(frame.ends_with('#'), "{frame:?}");
        assert_eq!(frame.len(), 81, "the frame is 81 bytes on the wire");
    }

    #[tokio::test]
    async fn a_set_draws_no_reply_and_shows_in_the_next_frame() {
        let factory = MockGeminiTransportFactory::default();
        let mut transport = open(&factory).await;
        exchange(&mut transport, ">G#").await;
        send(&mut transport, ">C3#").await;
        let frame = exchange(&mut transport, ">G#").await;
        assert!(frame.starts_with("*G1011D"), "{frame}");
    }

    #[tokio::test]
    async fn a_set_and_a_status_request_in_one_buffer_answer_with_the_post_set_frame() {
        let factory = MockGeminiTransportFactory::default();
        let mut transport = open(&factory).await;
        transport.send_frame(b">C6#\n>G#\n").await.unwrap();
        let frame = recv(&mut transport).await;
        assert!(frame.starts_with("*G1111D011111U"), "{frame}");
    }

    #[tokio::test]
    async fn every_set_family_changes_its_field() {
        let factory = MockGeminiTransportFactory::default();
        let mut transport = open(&factory).await;
        for command in [">O2#", ">C5#", ">C11#", ">X25#", ">Y0#", ">Z10#", ">M22#"] {
            send(&mut transport, command).await;
        }
        let frame = exchange(&mut transport, ">G#").await;
        assert!(
            frame.starts_with("*G1110D111110U1A1T0A1M1B2M25C0C"),
            "{frame}"
        );
    }

    #[tokio::test]
    async fn output_one_is_never_switched() {
        let factory = MockGeminiTransportFactory::default();
        let mut transport = open(&factory).await;
        send(&mut transport, ">C1#").await;
        let frame = exchange(&mut transport, ">G#").await;
        assert_eq!(frame, CAPTURED_FRAME);
    }

    #[tokio::test]
    async fn a_stuck_output_keeps_its_state() {
        let factory = MockGeminiTransportFactory::default().with_stuck_output(3);
        let mut transport = open(&factory).await;
        send(&mut transport, ">C3#").await;
        send(&mut transport, ">C4#").await;
        let frame = exchange(&mut transport, ">G#").await;
        assert!(frame.starts_with("*G1101D"), "{frame}");
    }

    #[tokio::test]
    async fn the_presets_show_in_the_replies() {
        let factory = MockGeminiTransportFactory::default()
            .with_firmware("309")
            .with_identity("GeminiFlatPanelV3")
            .with_sensors(true, false)
            .with_dew_modes(0, 2)
            .with_input_voltage(24.0);
        let mut transport = open(&factory).await;
        assert_eq!(
            exchange(&mut transport, ">H#").await,
            "*HGeminiFlatPanelV3#"
        );
        assert_eq!(exchange(&mut transport, ">V#").await, "*V309#");
        let frame = exchange(&mut transport, ">G#").await;
        assert!(frame.contains("U1A0T1A0M1B2M"), "{frame}");
        assert!(frame.contains("24.0V"), "{frame}");
    }

    #[tokio::test]
    async fn the_env_knobs_parse_their_documented_forms() {
        assert_eq!(parse_two_flags("10"), Some((true, false)));
        assert_eq!(parse_two_flags(" 01 "), Some((false, true)));
        assert_eq!(parse_two_flags("1"), None);
        assert_eq!(parse_two_flags("12"), None);
        assert_eq!(parse_two_modes("00"), Some((0, 0)));
        assert_eq!(parse_two_modes("21"), Some((2, 1)));
        assert_eq!(parse_two_modes("31"), None);
    }

    #[tokio::test]
    async fn an_unarmed_hub_says_nothing_and_an_armed_one_streams() {
        let factory = MockGeminiTransportFactory::default();
        let mut transport = open(&factory).await;
        let mut buf = Vec::new();
        assert!(matches!(
            transport.recv_frame(&mut buf).await.unwrap_err(),
            TransportError::Eof
        ));
        exchange(&mut transport, ">G#").await;
        // Armed: a read with nothing queued is answered by the next
        // periodic frame.
        let frame = recv(&mut transport).await;
        assert_eq!(frame, CAPTURED_FRAME);
    }

    #[tokio::test(start_paused = true)]
    async fn periodic_frames_fall_due_every_three_seconds_and_a_discard_drops_them() {
        let factory = MockGeminiTransportFactory::default();
        let mut transport = open(&factory).await;
        exchange(&mut transport, ">G#").await;

        tokio::time::advance(STREAM_PERIOD * 2 + Duration::from_millis(100)).await;
        send(&mut transport, ">H#").await;
        // Two streamed frames sit ahead of the identity reply.
        assert!(recv(&mut transport).await.starts_with("*G"));
        assert!(recv(&mut transport).await.starts_with("*G"));
        assert!(recv(&mut transport).await.starts_with("*H"));

        tokio::time::advance(STREAM_PERIOD + Duration::from_millis(100)).await;
        transport.discard_input().await.unwrap();
        assert!(exchange(&mut transport, ">H#").await.starts_with("*H"));
    }

    #[tokio::test]
    async fn the_stale_frame_knob_queues_two_frames_behind_each_reply_once_armed() {
        let factory = MockGeminiTransportFactory::default().with_stale_frames_before_reply();
        let mut transport = open(&factory).await;
        // Not armed yet: the identity reply stands alone.
        assert!(exchange(&mut transport, ">H#").await.starts_with("*H"));
        let mut buf = Vec::new();
        assert!(matches!(
            transport.recv_frame(&mut buf).await.unwrap_err(),
            TransportError::Eof
        ));

        exchange(&mut transport, ">G#").await;
        send(&mut transport, ">C3#\n>G#").await;
        // Without a discard the two stale frames come first.
        assert!(recv(&mut transport).await.starts_with("*G1111D"));
        assert!(recv(&mut transport).await.starts_with("*G1111D"));
        assert!(recv(&mut transport).await.starts_with("*G1011D"));
    }

    #[tokio::test]
    async fn a_raw_identity_answers_every_read_in_the_skip_budget_without_the_prefix() {
        let factory = MockGeminiTransportFactory::default().with_identity("raw:HELLO#");
        let mut transport = open(&factory).await;
        send(&mut transport, ">H#").await;
        for _ in 0..=crate::codec::MAX_SKIP {
            assert_eq!(recv(&mut transport).await, "HELLO#");
        }
        let mut buf = Vec::new();
        assert!(matches!(
            transport.recv_frame(&mut buf).await.unwrap_err(),
            TransportError::Eof
        ));
    }

    #[tokio::test]
    async fn a_hub_streaming_at_start_cuts_a_frame_short_to_answer_the_first_identity_request() {
        let factory = MockGeminiTransportFactory::default()
            .with_stream_running_at_start()
            .with_cut_frames_before_reply();
        let mut transport = open(&factory).await;
        let chunk = exchange(&mut transport, ">H#").await;
        assert!(chunk.starts_with("*G1111D"), "{chunk}");
        assert!(chunk.ends_with("*HGeminiPowerBoxPlusAdv3#"), "{chunk}");
        assert_eq!(
            chunk.matches('#').count(),
            1,
            "one chunk, one terminator: {chunk}"
        );
        assert_eq!(
            chunk.len(),
            CUT_FRAME_BYTES + "*HGeminiPowerBoxPlusAdv3#".len()
        );
    }

    #[tokio::test]
    async fn an_ignored_identity_request_times_out_once_then_answers() {
        let factory = MockGeminiTransportFactory::default().with_ignored_first_identity();
        let mut transport = open(&factory).await;
        send(&mut transport, ">H#").await;
        let mut buf = Vec::new();
        assert!(matches!(
            transport.recv_frame(&mut buf).await.unwrap_err(),
            TransportError::Timeout(_)
        ));
        assert!(exchange(&mut transport, ">H#").await.starts_with("*H"));
    }

    #[tokio::test]
    async fn state_persists_across_reopens_and_commands_are_recorded() {
        let factory = MockGeminiTransportFactory::default();
        {
            let mut transport = open(&factory).await;
            send(&mut transport, ">C2#").await;
        }
        let mut transport = open(&factory).await;
        assert!(exchange(&mut transport, ">G#").await.starts_with("*G0111D"));
        assert_eq!(factory.sent_commands().await, [">C2#", ">G#"]);
        assert_eq!(factory.sent_buffers().await, [">C2#", ">G#"]);
        assert!(!factory.dc_is_on(2).await);
    }
}
