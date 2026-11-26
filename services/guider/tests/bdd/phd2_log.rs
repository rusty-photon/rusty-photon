//! A parser of PHD2's guide log, built from the logs PHD2 wrote on the rigs
//! (`tests/fixtures/guide-logs/`, whose README describes what they hold).
//! It accepts what those files hold and nothing else, so the guider's own
//! log is held to the format by being parsed with it
//! (docs/services/guider.md § Guide log).
//!
//! A file is blocks. A block opens with the version line and holds notes,
//! calibrations and guiding runs, and closes with a blank line, the summary
//! and `Log closed at`. A calibration is its `Calibration Begins at` line,
//! header lines, its column heading, its step rows, leg results and notes,
//! and `Calibration complete`; a guiding run is its `Guiding Begins at`
//! line, header lines, its column heading, its rows and notes, and
//! `Guiding Ends at`. A blank line also ends a section.

/// A parsed log.
#[derive(Debug, Clone, Default)]
pub struct Log {
    pub blocks: Vec<Block>,
}

/// A block, from its version line to its `Log closed at` line.
#[derive(Debug, Clone, Default)]
pub struct Block {
    /// What the version line names after `PHD2 version `.
    pub writer: String,
    pub enabled_at: String,
    /// The notes between the block's sections.
    pub notes: Vec<String>,
    pub sections: Vec<Section>,
    pub summary: Option<Summary>,
    pub closed_at: Option<String>,
}

/// `Log Summary: calcnt:N gcnt:N gdur:N gacnt:N`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub calcnt: u32,
    pub gcnt: u32,
    pub gdur: u64,
    pub gacnt: u32,
}

/// A section of a block.
#[derive(Debug, Clone)]
pub enum Section {
    Calibration(Calibration),
    Guiding(Guiding),
    /// A `Guiding Ends at` line outside a run, as follows a calibration
    /// that was given up.
    StrayEnd(String),
}

/// A calibration section.
#[derive(Debug, Clone, Default)]
pub struct Calibration {
    pub begins_at: String,
    pub header: Vec<String>,
    pub rows: Vec<CalibrationRow>,
    pub legs: Vec<LegComplete>,
    pub notes: Vec<String>,
    /// The mount `Calibration complete, mount = ….` names, when the
    /// calibration ran to its end.
    pub complete: Option<String>,
}

/// `Direction,Step,dx,dy,x,y,Dist`.
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationRow {
    pub direction: String,
    pub step: u32,
    pub dx: f64,
    pub dy: f64,
    pub x: f64,
    pub y: f64,
    pub dist: f64,
}

/// `<West|North> calibration complete. Angle = … deg, Rate = … px/sec,
/// Parity = …`.
#[derive(Debug, Clone, PartialEq)]
pub struct LegComplete {
    pub direction: String,
    pub angle_deg: f64,
    pub rate: f64,
    pub parity: String,
}

/// A guiding section.
#[derive(Debug, Clone, Default)]
pub struct Guiding {
    pub begins_at: String,
    pub header: Vec<String>,
    pub rows: Vec<Row>,
    /// Each note with the number of rows before it.
    pub notes: Vec<(usize, String)>,
    pub ends_at: Option<String>,
}

/// A row of a guiding section.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Mount(MountRow),
    Drop(DropRow),
}

/// A `"Mount"` row.
#[derive(Debug, Clone, PartialEq)]
pub struct MountRow {
    pub frame: u64,
    pub time: f64,
    pub dx: f64,
    pub dy: f64,
    pub ra_raw: f64,
    pub dec_raw: f64,
    pub ra_guide: f64,
    pub dec_guide: f64,
    pub ra_ms: u64,
    pub ra_direction: Option<char>,
    pub dec_ms: u64,
    pub dec_direction: Option<char>,
    pub star_mass: u64,
    pub snr: f64,
}

/// A `"DROP"` row.
#[derive(Debug, Clone, PartialEq)]
pub struct DropRow {
    pub frame: u64,
    pub time: f64,
    pub star_mass: u64,
    pub snr: f64,
    pub code: u32,
    pub reason: String,
}

impl Row {
    #[must_use]
    pub const fn frame(&self) -> u64 {
        match self {
            Self::Mount(row) => row.frame,
            Self::Drop(row) => row.frame,
        }
    }

    #[must_use]
    pub const fn time(&self) -> f64 {
        match self {
            Self::Mount(row) => row.time,
            Self::Drop(row) => row.time,
        }
    }
}

/// The value of `key` in a header line `key = value, …` or
/// `…, key = value, …`, up to the next comma.
#[must_use]
pub fn header_value<'a>(header: &'a [String], key: &str) -> Option<&'a str> {
    let first = format!("{key} = ");
    let later = format!(", {key} = ");
    header.iter().find_map(|line| {
        let line = line.trim_start();
        let value = line
            .strip_prefix(&first)
            .or_else(|| line.split_once(&later).map(|(_, value)| value))?;
        Some(value.split(',').next().unwrap_or(value).trim())
    })
}

const COLUMNS: &str = "Frame,Time,mount,dx,dy,RARawDistance,DECRawDistance,RAGuideDistance,DECGuideDistance,RADuration,RADirection,DECDuration,DECDirection,XStep,YStep,StarMass,SNR,ErrorCode";
const CALIBRATION_COLUMNS: &str = "Direction,Step,dx,dy,x,y,Dist";
const DIRECTIONS: [&str; 5] = ["West", "East", "Backlash", "North", "South"];

/// Where the parser is in a block.
enum State {
    /// Before the first version line.
    Start,
    /// Between sections.
    Between,
    CalibrationHeader,
    Calibrating,
    GuidingHeader,
    Guiding,
    /// After `Log closed at`, before the next version line.
    Closed,
}

/// Parse `text`, the whole of a guide log.
///
/// # Errors
///
/// The first line that the format does not allow, with its number.
pub fn parse(text: &str) -> Result<Log, String> {
    if !text.is_empty() && !text.ends_with('\n') {
        return Err("the log does not end with a line ending".to_owned());
    }
    let mut log = Log::default();
    let mut state = State::Start;
    // The counts the summary of a block carries, which run on from one
    // block of a file to the next.
    let mut completed = 0_u32;
    let mut ended = 0_u32;
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let fail = |why: &str| Err(format!("line {number}: {why}: {line:?}"));
        if let Some(rest) = line.strip_prefix("PHD2 version ") {
            if !matches!(state, State::Start | State::Closed) {
                return fail("a version line inside an open block");
            }
            let Some((writer, enabled)) = rest.split_once(", Log version 2.5. Log enabled at ")
            else {
                return fail("a version line of another shape");
            };
            if !is_time(enabled) {
                return fail("a version line without a time");
            }
            log.blocks.push(Block {
                writer: writer.to_owned(),
                enabled_at: enabled.to_owned(),
                ..Block::default()
            });
            state = State::Between;
            continue;
        }
        let Some(block) = log.blocks.last_mut() else {
            return fail("a line before the first version line");
        };
        match state {
            State::Start => return fail("a line before the first version line"),
            State::Closed => return fail("a line after Log closed"),
            State::Between => {
                if line.is_empty() {
                } else if let Some(note) = line.strip_prefix("INFO: ") {
                    block.notes.push(note.to_owned());
                } else if let Some(at) = line.strip_prefix("Calibration Begins at ") {
                    if !is_time(at) {
                        return fail("a calibration without a time");
                    }
                    block.sections.push(Section::Calibration(Calibration {
                        begins_at: at.to_owned(),
                        ..Calibration::default()
                    }));
                    state = State::CalibrationHeader;
                } else if let Some(at) = line.strip_prefix("Guiding Begins at ") {
                    if !is_time(at) {
                        return fail("a guiding run without a time");
                    }
                    block.sections.push(Section::Guiding(Guiding {
                        begins_at: at.to_owned(),
                        ..Guiding::default()
                    }));
                    state = State::GuidingHeader;
                } else if let Some(at) = line.strip_prefix("Guiding Ends at ") {
                    if !is_time(at) {
                        return fail("an end without a time");
                    }
                    ended += 1;
                    block.sections.push(Section::StrayEnd(at.to_owned()));
                } else if let Some(summary) = line.strip_prefix("Log Summary: ") {
                    let summary = parse_summary(summary).ok_or_else(|| {
                        format!("line {number}: a summary of another shape: {line:?}")
                    })?;
                    if (summary.calcnt, summary.gcnt) != (completed, ended) {
                        return fail(&format!(
                            "a summary that counts {} calibrations and {} runs ended, not {completed} and {ended}",
                            summary.calcnt, summary.gcnt
                        ));
                    }
                    block.summary = Some(summary);
                } else if let Some(at) = line.strip_prefix("Log closed at ") {
                    if block.summary.is_none() || !is_time(at) {
                        return fail("a close without a summary before it, or a time");
                    }
                    block.closed_at = Some(at.to_owned());
                    state = State::Closed;
                } else {
                    return fail("a line no block holds between sections");
                }
            }
            State::CalibrationHeader | State::GuidingHeader => {
                let calibrating = matches!(state, State::CalibrationHeader);
                let heading = if calibrating {
                    CALIBRATION_COLUMNS
                } else {
                    COLUMNS
                };
                if line == heading {
                    state = if calibrating {
                        State::Calibrating
                    } else {
                        State::Guiding
                    };
                } else if is_header_line(line) {
                    match block.sections.last_mut() {
                        Some(Section::Calibration(calibration)) => {
                            calibration.header.push(line.to_owned());
                        }
                        Some(Section::Guiding(guiding)) => guiding.header.push(line.to_owned()),
                        _ => return fail("a header outside its section"),
                    }
                } else {
                    return fail("a header line that is not `key = value`");
                }
            }
            State::Calibrating => {
                let Some(Section::Calibration(calibration)) = block.sections.last_mut() else {
                    return fail("a calibration's line outside a calibration");
                };
                if line.is_empty() {
                    state = State::Between;
                } else if let Some(mount) = line.strip_prefix("Calibration complete, mount = ") {
                    let Some(mount) = mount.strip_suffix('.') else {
                        return fail("a completion line without its full stop");
                    };
                    calibration.complete = Some(mount.to_owned());
                    completed += 1;
                    state = State::Between;
                } else if let Some(at) = line.strip_prefix("Guiding Ends at ") {
                    if !is_time(at) {
                        return fail("an end without a time");
                    }
                    ended += 1;
                    block.sections.push(Section::StrayEnd(at.to_owned()));
                    state = State::Between;
                } else if let Some(note) = line.strip_prefix("INFO: ") {
                    calibration.notes.push(note.to_owned());
                } else if let Some(leg) = parse_leg_complete(line) {
                    calibration.legs.push(leg);
                } else if let Some(row) = parse_calibration_row(line) {
                    calibration.rows.push(row);
                } else {
                    return fail("a line no calibration holds");
                }
            }
            State::Guiding => {
                let Some(Section::Guiding(guiding)) = block.sections.last_mut() else {
                    return fail("a guiding run's line outside a guiding run");
                };
                if line.is_empty() {
                    state = State::Between;
                } else if let Some(at) = line.strip_prefix("Guiding Ends at ") {
                    if !is_time(at) {
                        return fail("an end without a time");
                    }
                    guiding.ends_at = Some(at.to_owned());
                    ended += 1;
                    state = State::Between;
                } else if let Some(note) = line.strip_prefix("INFO: ") {
                    guiding.notes.push((guiding.rows.len(), note.to_owned()));
                } else {
                    let row = parse_row(line).ok_or_else(|| {
                        format!("line {number}: a line no guiding run holds: {line:?}")
                    })?;
                    if let Some(last) = guiding.rows.last() {
                        if row.frame() <= last.frame() || row.time() < last.time() {
                            return fail("a row whose frame or time does not follow the last");
                        }
                    }
                    guiding.rows.push(row);
                }
            }
        }
    }
    match state {
        State::Start => Err("no version line".to_owned()),
        State::Closed => Ok(log),
        _ => Err("the last block does not close".to_owned()),
    }
}

/// `YYYY-MM-DD HH:MM:SS`.
fn is_time(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 19
        && bytes.iter().enumerate().all(|(at, byte)| match at {
            4 | 7 => *byte == b'-',
            10 => *byte == b' ',
            13 | 16 => *byte == b':',
            _ => byte.is_ascii_digit(),
        })
}

/// `key = value`, the key of letters, spaces, `.` and `/` after at most one
/// leading space.
fn is_header_line(line: &str) -> bool {
    let line = line.strip_prefix(' ').unwrap_or(line);
    line.split_once(" = ").is_some_and(|(key, value)| {
        !key.is_empty()
            && !value.is_empty()
            && key.starts_with(|c: char| c.is_ascii_alphabetic())
            && key
                .chars()
                .all(|c| c.is_ascii_alphabetic() || matches!(c, ' ' | '.' | '/'))
    })
}

/// A number with exactly `places` decimals, as `%.<places>f` writes it.
fn fixed(text: &str, places: usize) -> Option<f64> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let (whole, fraction) = if places == 0 {
        (digits, "")
    } else {
        digits.split_once('.')?
    };
    let well_formed = !whole.is_empty()
        && whole.bytes().all(|b| b.is_ascii_digit())
        && fraction.len() == places
        && fraction.bytes().all(|b| b.is_ascii_digit());
    well_formed.then(|| text.parse().ok()).flatten()
}

fn whole<T: std::str::FromStr>(text: &str) -> Option<T> {
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

fn parse_summary(text: &str) -> Option<Summary> {
    let mut fields = text.split(' ');
    let mut field = |name: &str| fields.next()?.strip_prefix(name).map(str::to_owned);
    let summary = Summary {
        calcnt: whole(&field("calcnt:")?)?,
        gcnt: whole(&field("gcnt:")?)?,
        gdur: whole(&field("gdur:")?)?,
        gacnt: whole(&field("gacnt:")?)?,
    };
    fields.next().is_none().then_some(summary)
}

fn parse_leg_complete(line: &str) -> Option<LegComplete> {
    let (direction, rest) = line.split_once(" calibration complete. Angle = ")?;
    if !matches!(direction, "West" | "North") {
        return None;
    }
    let (angle, rest) = rest.split_once(" deg, Rate = ")?;
    let (rate, parity) = rest.split_once(" px/sec, Parity = ")?;
    matches!(parity, "Even" | "Odd" | "N/A").then_some(())?;
    Some(LegComplete {
        direction: direction.to_owned(),
        angle_deg: fixed(angle, 1)?,
        rate: fixed(rate, 3)?,
        parity: parity.to_owned(),
    })
}

fn parse_calibration_row(line: &str) -> Option<CalibrationRow> {
    let fields: Vec<&str> = line.split(',').collect();
    let [direction, step, dx, dy, x, y, dist] = fields.as_slice() else {
        return None;
    };
    DIRECTIONS.contains(direction).then_some(())?;
    Some(CalibrationRow {
        direction: (*direction).to_owned(),
        step: whole(step)?,
        dx: fixed(dx, 3)?,
        dy: fixed(dy, 3)?,
        x: fixed(x, 3)?,
        y: fixed(y, 3)?,
        dist: fixed(dist, 3)?,
    })
}

/// A pulse's duration and direction: the direction empty exactly when the
/// duration is 0.
fn pulse(duration: &str, direction: &str, allowed: [char; 2]) -> Option<(u64, Option<char>)> {
    let ms: u64 = whole(duration)?;
    let direction = match direction {
        "" => None,
        one if one.len() == 1 => one.chars().next().filter(|c| allowed.contains(c)),
        _ => return None,
    };
    ((ms == 0) == direction.is_none()).then_some((ms, direction))
}

fn parse_row(line: &str) -> Option<Row> {
    let fields: Vec<&str> = line.split(',').collect();
    match fields.as_slice() {
        [frame, time, "\"Mount\"", dx, dy, ra_raw, dec_raw, ra_guide, dec_guide, ra_ms, ra_dir, dec_ms, dec_dir, "", "", mass, snr, "0"] =>
        {
            let (ra_ms, ra_direction) = pulse(ra_ms, ra_dir, ['E', 'W'])?;
            let (dec_ms, dec_direction) = pulse(dec_ms, dec_dir, ['N', 'S'])?;
            Some(Row::Mount(MountRow {
                frame: whole(frame)?,
                time: fixed(time, 3)?,
                dx: fixed(dx, 3)?,
                dy: fixed(dy, 3)?,
                ra_raw: fixed(ra_raw, 3)?,
                dec_raw: fixed(dec_raw, 3)?,
                ra_guide: fixed(ra_guide, 3)?,
                dec_guide: fixed(dec_guide, 3)?,
                ra_ms,
                ra_direction,
                dec_ms,
                dec_direction,
                star_mass: whole(mass)?,
                snr: fixed(snr, 2)?,
            }))
        }
        [frame, time, "\"DROP\"", empty @ .., mass, snr, code, reason] if empty.len() == 12 => {
            empty.iter().all(|field| field.is_empty()).then_some(())?;
            let reason = reason.strip_prefix('"')?.strip_suffix('"')?;
            Some(Row::Drop(DropRow {
                frame: whole(frame)?,
                time: fixed(time, 3)?,
                star_mass: whole(mass)?,
                snr: fixed(snr, 2)?,
                code: whole(code)?,
                reason: reason.to_owned(),
            }))
        }
        _ => None,
    }
}
