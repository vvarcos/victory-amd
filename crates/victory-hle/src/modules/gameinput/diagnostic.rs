//! Diagnostic command channel (feature `diagnostics`).
//!
//! `VICTORY_INPUT_CMD=<file>` makes the host read short, explicitly armed
//! input commands from `file` and OR them into pad 0 and into the
//! keyboard/stand-in pad. It is off unless the variable names a file. The
//! path is read once, when the input host starts.
//!
//! File protocol: UTF-8, one command per LF line, tab-separated; blank
//! lines and lines whose first non-space byte is `#` are ignored. Only
//! appended bytes are read (append-only file). On a new process the bytes
//! already in the file are skipped on the first look: append commands after
//! the input host starts, so a stale `DISARM` or an old command is never
//! replayed. (A file that did not exist on that first look is read from its
//! start when it appears.)
//!
//! ```text
//! <id> \t <deadline_ms> \t <buttons> \t <keys>
//! <id> \t DISARM
//! ```
//!
//! - `id`: decimal u64, strictly increasing. An id equal to or lower than
//!   the last well-formed one is rejected as a replay and never applies.
//! - `deadline_ms`: Unix epoch milliseconds (UTC). At or after it the
//!   command is dead and contributes no input.
//! - `buttons`: `-` or empty for none, `0x<mask>` of console pad bits, or a
//!   `,`/`+` list of `A B X Y MENU VIEW UP DOWN LEFT RIGHT LB RB LS RS`.
//! - `keys`: `-` or empty for none, or a `,` list of virtual keys: decimal,
//!   `0xNN`, `Enter Esc Tab Back Up Down Left Right`, or one letter/digit.
//!
//! A command is held for at most 200 ms (`HOLD`); only one is active at a
//! time. Commands read while one is active are rejected (`busy`) and do
//! not wait. The file is checked at most every 50 ms, not on every 8 ms
//! source poll, and a check reads at most 16 KiB, 32 lines and lines of at
//! most 4 KiB. Anything malformed, expired, replayed or unreadable is
//! fail-closed: it contributes no input.
//!
//! Acknowledgements are log lines only. `ACCEPTED` means the command was
//! parsed, is armed and still inside its TTL; `DELIVERED_HLE` means its
//! bits/keys were included in an HLE poll's readings. There is no
//! consumed-by-title acknowledgement: delivery does not prove the title
//! sampled or acted on it. `RELEASED`, `DISARMED` and `REJECTED` complete
//! the vocabulary. All of them go through `tracing` at info/warn level.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{
    A, B, DOWN, LEFT, LEFT_SHOULDER, LEFT_STICK, MENU, PadState, RIGHT, RIGHT_SHOULDER,
    RIGHT_STICK, UP, VIEW, X, Y,
};

/// The command file is checked at most this often; the source polls every 8 ms.
const CHECK: Duration = Duration::from_millis(50);
/// A command presses its buttons/keys for at most this long.
const HOLD: Duration = Duration::from_millis(200);
/// Bytes read from the command file per check.
const READ_MAX: usize = 16 * 1024;
/// Longest command line accepted.
const LINE_MAX: usize = 4 * 1024;
/// Virtual keys one command may press.
const KEYS_MAX: usize = 16;
/// Command lines handled per check (the rest waits for the next one).
const LINES_MAX: usize = 32;

const ALL_BUTTONS: u32 = MENU
    | VIEW
    | A
    | B
    | X
    | Y
    | UP
    | DOWN
    | LEFT
    | RIGHT
    | LEFT_SHOULDER
    | RIGHT_SHOULDER
    | LEFT_STICK
    | RIGHT_STICK;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reject {
    Malformed,
    Id,
    Buttons,
    Keys,
    Empty,
    Replay,
    Expired,
    Busy,
    Disarmed,
}

impl Reject {
    fn as_str(self) -> &'static str {
        match self {
            Reject::Malformed => "malformed",
            Reject::Id => "id",
            Reject::Buttons => "buttons",
            Reject::Keys => "keys",
            Reject::Empty => "empty",
            Reject::Replay => "replay",
            Reject::Expired => "expired",
            Reject::Busy => "busy",
            Reject::Disarmed => "disarmed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Command {
    id: u64,
    deadline_ms: u64,
    buttons: u32,
    keys: Vec<u8>,
    disarm: bool,
}

struct Active {
    id: u64,
    buttons: u32,
    keys: Vec<u8>,
    deadline_ms: u64,
    started: Instant,
    delivered: bool,
}

/// The diagnostic channel: off when `path` is `None`.
pub(super) struct Channel {
    path: Option<PathBuf>,
    /// Bytes of the append-only file already consumed.
    offset: u64,
    /// A line without its newline yet.
    pending: Vec<u8>,
    /// An overlong line is being dropped up to its newline.
    skipping: bool,
    last_check: Option<Instant>,
    /// Last well-formed id, replayed ids are refused forever.
    last_id: Option<u64>,
    /// A DISARM was accepted: nothing is accepted until a new process.
    disarmed: bool,
    active: Option<Active>,
    /// The first look at the file happened (its old bytes are not replayed).
    started: bool,
    /// A file error was logged and no success has happened since.
    reported: bool,
}

impl Default for Channel {
    fn default() -> Self {
        Self::from_env()
    }
}

impl Channel {
    fn new(path: Option<PathBuf>) -> Self {
        Channel {
            path,
            offset: 0,
            pending: Vec::new(),
            skipping: false,
            last_check: None,
            last_id: None,
            disarmed: false,
            active: None,
            started: false,
            reported: false,
        }
    }

    fn from_env() -> Self {
        let path = std::env::var_os("VICTORY_INPUT_CMD")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        Self::new(path)
    }

    /// Advances once per source poll, before any reading is taken.
    pub(super) fn tick(&mut self, utc_ms: Option<u64>, now: Instant) {
        let Some(utc) = utc_ms else {
            if self.active.take().is_some() {
                tracing::warn!(
                    "input command RELEASED reason=clock (no UTC clock; diagnostic input stops)"
                );
            }
            return;
        };
        self.release_if_due(utc, now);
        if self.disarmed {
            return;
        }
        if self
            .last_check
            .is_none_or(|last| now.saturating_duration_since(last) >= CHECK)
        {
            self.last_check = Some(now);
            self.read(utc, now);
        }
    }

    /// Records that the active command reached the HLE source for this poll.
    pub(super) fn mark_delivered(&mut self) {
        if let Some(active) = &mut self.active
            && !active.delivered
        {
            active.delivered = true;
            tracing::info!(
                id = active.id,
                buttons = format_args!("{:#x}", active.buttons),
                keys = active.keys.len(),
                "input command DELIVERED_HLE (in HLE readings; no consumed-by-title ack)"
            );
        }
    }

    /// Buttons the active command presses on pad 0 / the stand-in pad.
    pub(super) fn buttons(&self) -> u32 {
        self.active.as_ref().map_or(0, |active| active.buttons)
    }

    /// Virtual keys the active command holds.
    pub(super) fn keys(&self) -> &[u8] {
        self.active
            .as_ref()
            .map_or(&[] as &[u8], |active| active.keys.as_slice())
    }

    fn release_if_due(&mut self, utc: u64, now: Instant) {
        let Some((id, ttl, hold)) = self.active.as_ref().map(|active| {
            (
                active.id,
                utc >= active.deadline_ms,
                now.saturating_duration_since(active.started) >= HOLD,
            )
        }) else {
            return;
        };
        if !ttl && !hold {
            return;
        }
        self.active = None;
        tracing::info!(
            id,
            reason = if ttl { "ttl" } else { "hold" },
            "input command RELEASED"
        );
    }

    fn read(&mut self, utc: u64, now: Instant) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let len = match std::fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                self.started = true;
                self.warn_once(error);
                return;
            }
        };
        if !self.started {
            self.started = true;
            if len > 0 {
                self.offset = len;
                tracing::info!(
                    bytes = len,
                    "input command file: pre-existing bytes ignored at start"
                );
                return;
            }
        }
        if len < self.offset {
            self.offset = 0;
            self.pending.clear();
            self.skipping = false;
        }
        if len > self.offset {
            let mut file = match File::open(&path) {
                Ok(file) => file,
                Err(error) => {
                    self.warn_once(error);
                    return;
                }
            };
            if file.seek(SeekFrom::Start(self.offset)).is_err() {
                self.offset = 0;
                self.pending.clear();
                self.skipping = false;
                return;
            }
            let mut buffer = [0u8; READ_MAX];
            match file.read(&mut buffer) {
                Ok(0) => {}
                Ok(n) => {
                    self.offset += n as u64;
                    self.pending.extend_from_slice(&buffer[..n]);
                    self.reported = false;
                }
                Err(error) => {
                    self.warn_once(error);
                    return;
                }
            }
        }
        self.process(utc, now);
    }

    fn warn_once(&mut self, error: std::io::Error) {
        if !self.reported {
            self.reported = true;
            tracing::warn!(error = %error, "input command file not readable; no diagnostic input");
        }
    }

    fn process(&mut self, utc: u64, now: Instant) {
        let mut start = 0;
        let mut handled = 0;
        while let Some(found) = self.pending[start..].iter().position(|&byte| byte == b'\n') {
            let end = start + found;
            let line = self.pending[start..end].to_vec();
            start = end + 1;
            if self.skipping {
                self.skipping = false;
                continue;
            }
            if line.len() > LINE_MAX {
                tracing::warn!("input command REJECTED reason=line-too-long");
                continue;
            }
            self.line(&line, utc, now);
            handled += 1;
            if handled >= LINES_MAX {
                break;
            }
        }
        if start > 0 {
            self.pending.drain(..start);
        }
        if self.skipping {
            self.pending.clear();
        } else if self.pending.len() > LINE_MAX {
            self.pending.clear();
            self.skipping = true;
            tracing::warn!("input command REJECTED reason=line-too-long");
        }
    }

    fn line(&mut self, line: &[u8], utc: u64, now: Instant) {
        let Ok(line) = std::str::from_utf8(line) else {
            tracing::warn!("input command REJECTED reason=encoding");
            return;
        };
        let line = line
            .trim_start_matches('\u{feff}')
            .trim_end_matches('\r')
            .trim();
        if line.is_empty() || line.starts_with('#') {
            return;
        }
        match parse(line) {
            Ok(command) => self.submit(command, utc, now),
            Err(reason) => {
                if let Some(id) = line
                    .split('\t')
                    .next()
                    .and_then(|id| id.trim().parse::<u64>().ok())
                {
                    self.consume(id);
                }
                tracing::warn!(
                    reason = reason.as_str(),
                    "input command REJECTED (malformed)"
                );
            }
        }
    }

    /// Consumes a monotonic id even when its command is rejected.
    fn consume(&mut self, id: u64) {
        if self.last_id.is_none_or(|last| id > last) {
            self.last_id = Some(id);
        }
    }

    fn submit(&mut self, command: Command, utc: u64, now: Instant) {
        if self.disarmed {
            tracing::warn!(
                id = command.id,
                reason = Reject::Disarmed.as_str(),
                "input command REJECTED"
            );
            return;
        }
        if self.last_id.is_some_and(|last| command.id <= last) {
            tracing::warn!(
                id = command.id,
                reason = Reject::Replay.as_str(),
                "input command REJECTED"
            );
            return;
        }
        self.last_id = Some(command.id);
        if command.disarm {
            let cleared = self.active.take().is_some();
            self.disarmed = true;
            tracing::info!(
                id = command.id,
                active_cleared = cleared,
                "input command DISARMED (nothing accepted until a new process)"
            );
            return;
        }
        if utc >= command.deadline_ms {
            tracing::warn!(
                id = command.id,
                reason = Reject::Expired.as_str(),
                "input command REJECTED"
            );
            return;
        }
        if self.active.is_some() {
            tracing::warn!(
                id = command.id,
                reason = Reject::Busy.as_str(),
                "input command REJECTED"
            );
            return;
        }
        if command.buttons == 0 && command.keys.is_empty() {
            tracing::warn!(
                id = command.id,
                reason = Reject::Empty.as_str(),
                "input command REJECTED"
            );
            return;
        }
        tracing::info!(
            id = command.id,
            deadline_ms = command.deadline_ms,
            buttons = format_args!("{:#x}", command.buttons),
            keys = command.keys.len(),
            hold_ms = HOLD.as_millis() as u64,
            "input command ACCEPTED (armed; not delivered or consumed yet)"
        );
        self.active = Some(Active {
            id: command.id,
            buttons: command.buttons,
            keys: command.keys,
            deadline_ms: command.deadline_ms,
            started: now,
            delivered: false,
        });
    }
}

/// Unix epoch milliseconds (UTC), or `None` if the clock is unusable.
pub(super) fn utc_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_millis()).ok())
}

/// ORs a command's buttons into a pad, keeping everything already there.
pub(super) fn merge_pad(pad: &mut PadState, buttons: u32) {
    pad.buttons |= buttons;
}

/// Appends virtual keys not already held, keeping physical keys first.
pub(super) fn merge_keys(held: &mut Vec<u8>, extra: &[u8]) {
    for &key in extra {
        if !held.contains(&key) {
            held.push(key);
        }
    }
}

fn parse(line: &str) -> Result<Command, Reject> {
    if line.len() > LINE_MAX {
        return Err(Reject::Malformed);
    }
    let mut fields = line.split('\t');
    let id = fields.next().ok_or(Reject::Malformed)?.trim();
    let id = id.parse::<u64>().map_err(|_| Reject::Id)?;
    let second = fields.next().ok_or(Reject::Malformed)?.trim();
    if second.eq_ignore_ascii_case("DISARM") {
        return match fields.next() {
            None => Ok(Command {
                id,
                deadline_ms: 0,
                buttons: 0,
                keys: Vec::new(),
                disarm: true,
            }),
            Some(_) => Err(Reject::Malformed),
        };
    }
    let deadline_ms = second.parse::<u64>().map_err(|_| Reject::Malformed)?;
    let buttons = fields.next().ok_or(Reject::Malformed)?.trim();
    let keys = fields.next().ok_or(Reject::Malformed)?.trim();
    if fields.next().is_some() {
        return Err(Reject::Malformed);
    }
    let buttons = parse_buttons(buttons)?;
    let keys = parse_keys(keys)?;
    if buttons == 0 && keys.is_empty() {
        return Err(Reject::Empty);
    }
    Ok(Command {
        id,
        deadline_ms,
        buttons,
        keys,
        disarm: false,
    })
}

fn parse_buttons(field: &str) -> Result<u32, Reject> {
    if field.is_empty() || field == "-" {
        return Ok(0);
    }
    if let Some(hex) = field
        .strip_prefix("0x")
        .or_else(|| field.strip_prefix("0X"))
    {
        let bits = u32::from_str_radix(hex, 16).map_err(|_| Reject::Buttons)?;
        if bits & !ALL_BUTTONS != 0 {
            return Err(Reject::Buttons);
        }
        return Ok(bits);
    }
    let mut bits = 0;
    for name in field.split(|c| c == ',' || c == '+') {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        bits |= match name.to_ascii_uppercase().as_str() {
            "A" => A,
            "B" => B,
            "X" => X,
            "Y" => Y,
            "MENU" | "START" => MENU,
            "VIEW" | "BACK" => VIEW,
            "UP" => UP,
            "DOWN" => DOWN,
            "LEFT" => LEFT,
            "RIGHT" => RIGHT,
            "LB" | "L1" | "LEFT_SHOULDER" => LEFT_SHOULDER,
            "RB" | "R1" | "RIGHT_SHOULDER" => RIGHT_SHOULDER,
            "LS" | "L3" | "LEFT_STICK" => LEFT_STICK,
            "RS" | "R3" | "RIGHT_STICK" => RIGHT_STICK,
            _ => return Err(Reject::Buttons),
        };
    }
    Ok(bits)
}

fn parse_keys(field: &str) -> Result<Vec<u8>, Reject> {
    if field.is_empty() || field == "-" {
        return Ok(Vec::new());
    }
    let mut keys = Vec::new();
    for token in field.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let key = parse_key(token).ok_or(Reject::Keys)?;
        if !keys.contains(&key) {
            keys.push(key);
        }
        if keys.len() > KEYS_MAX {
            return Err(Reject::Keys);
        }
    }
    Ok(keys)
}

fn parse_key(token: &str) -> Option<u8> {
    let value = if let Some(hex) = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
    {
        u8::from_str_radix(hex, 16).ok()?
    } else if token.bytes().all(|byte| byte.is_ascii_digit()) {
        token.parse::<u8>().ok()?
    } else {
        match token.to_ascii_lowercase().as_str() {
            "enter" => 0x0D,
            "esc" => 0x1B,
            "tab" => 0x09,
            "back" => 0x08,
            "up" => 0x26,
            "down" => 0x28,
            "left" => 0x25,
            "right" => 0x27,
            single if single.len() == 1 && single.as_bytes()[0].is_ascii_alphanumeric() => {
                single.as_bytes()[0].to_ascii_uppercase()
            }
            _ => return None,
        }
    };
    (value != 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::super::{A, B, MENU, PadState};
    use super::{Channel, Command, HOLD, LINE_MAX, Reject, merge_keys, merge_pad, parse};

    const NOW: u64 = 1_000_000;

    fn press(id: u64, deadline: u64, buttons: &str, keys: &str) -> Command {
        parse(&format!("{id}\t{deadline}\t{buttons}\t{keys}")).unwrap()
    }

    #[test]
    fn parses_the_documented_protocol() {
        let command = press(7, NOW + 500, "A,B", "Enter,0x1B");
        assert_eq!(command.id, 7);
        assert_eq!(command.deadline_ms, NOW + 500);
        assert_eq!(command.buttons, A | B);
        assert_eq!(command.keys, vec![0x0Du8, 0x1B]);
        assert!(!command.disarm);

        let command = press(8, NOW + 500, "0x5", "-");
        assert_eq!(command.buttons, MENU | A);
        assert!(command.keys.is_empty());

        let command = parse("9\tDISARM").expect("disarm");
        assert_eq!(command.id, 9);
        assert!(command.disarm);
    }

    #[test]
    fn rejects_malformed_lines() {
        assert_eq!(parse("x\t1\tA\t-"), Err(Reject::Id));
        assert_eq!(parse("1\t1\tNOPE\t-"), Err(Reject::Buttons));
        assert_eq!(parse("1\t1\tA\t0xZZ"), Err(Reject::Keys));
        assert_eq!(parse("1\t1\t-\t-"), Err(Reject::Empty));
        assert_eq!(parse("1\t1\tA"), Err(Reject::Malformed));
        assert_eq!(parse("1\t1\tA\t-\textra"), Err(Reject::Malformed));
        assert_eq!(parse("1\tDISARM\textra"), Err(Reject::Malformed));
        assert_eq!(parse(&"1".repeat(LINE_MAX + 1)), Err(Reject::Malformed));
    }

    #[test]
    fn ttl_expires_before_any_input() {
        let mut channel = Channel::new(None);
        let start = Instant::now();
        let late = press(1, NOW + 10, "A", "-");
        channel.submit(late, NOW + 10, start);
        channel.tick(Some(NOW + 10), start);
        assert_eq!(channel.buttons(), 0);

        channel.submit(press(2, NOW + 1000, "A", "-"), NOW, start);
        channel.tick(Some(NOW), start);
        assert_eq!(channel.buttons(), A);
        channel.tick(Some(NOW + 1000), start + Duration::from_millis(1));
        assert_eq!(channel.buttons(), 0);
    }

    #[test]
    fn hold_stops_after_two_hundred_milliseconds() {
        let mut channel = Channel::new(None);
        let start = Instant::now();
        channel.submit(press(1, u64::MAX, "B", "-"), NOW, start);
        channel.tick(Some(NOW), start);
        assert_eq!(channel.buttons(), B);
        channel.tick(Some(NOW), start + Duration::from_millis(199));
        assert_eq!(channel.buttons(), B);
        channel.tick(Some(NOW), start + HOLD);
        assert_eq!(channel.buttons(), 0);
    }

    #[test]
    fn ids_are_monotonic_and_never_replay() {
        let mut channel = Channel::new(None);
        let start = Instant::now();
        channel.submit(press(2, u64::MAX, "A", "-"), NOW, start);
        channel.tick(Some(NOW), start);
        assert_eq!(channel.buttons(), A);

        // Same id with other content: refused, the first command stays.
        channel.submit(press(2, u64::MAX, "B", "-"), NOW, start);
        assert_eq!(channel.buttons(), A);

        channel.tick(Some(NOW), start + HOLD);
        assert_eq!(channel.buttons(), 0);

        // The released command never comes back; older ids are refused too.
        channel.submit(press(2, u64::MAX, "B", "-"), NOW, start + HOLD);
        channel.tick(Some(NOW), start + HOLD);
        assert_eq!(channel.buttons(), 0);
        channel.submit(press(1, u64::MAX, "B", "-"), NOW, start + HOLD);
        channel.tick(Some(NOW), start + HOLD);
        assert_eq!(channel.buttons(), 0);
    }

    #[test]
    fn one_command_is_active_at_a_time() {
        let mut channel = Channel::new(None);
        let start = Instant::now();
        channel.submit(press(1, u64::MAX, "A", "-"), NOW, start);
        channel.tick(Some(NOW), start);
        channel.submit(press(2, u64::MAX, "B", "-"), NOW, start);
        assert_eq!(channel.buttons(), A, "busy command must not replace");

        channel.tick(Some(NOW), start + HOLD);
        assert_eq!(channel.buttons(), 0);
        // The busy command was consumed when rejected, not queued.
        channel.submit(press(2, u64::MAX, "B", "-"), NOW, start + HOLD);
        channel.tick(Some(NOW), start + HOLD);
        assert_eq!(channel.buttons(), 0);
    }

    #[test]
    fn disarm_is_permanent_for_the_process() {
        let mut channel = Channel::new(None);
        let start = Instant::now();
        channel.submit(press(1, u64::MAX, "A", "-"), NOW, start);
        channel.tick(Some(NOW), start);
        assert_eq!(channel.buttons(), A);

        channel.submit(parse("2\tDISARM").expect("disarm"), NOW, start);
        assert!(channel.disarmed);
        assert_eq!(channel.buttons(), 0, "disarm releases the active command");

        channel.submit(press(3, u64::MAX, "B", "-"), NOW, start);
        channel.tick(Some(NOW), start);
        assert_eq!(channel.buttons(), 0);
        assert!(!Channel::new(None).disarmed, "a new process starts armed");
    }

    #[test]
    fn pending_is_shared_by_pad_and_keys() {
        let mut channel = Channel::new(None);
        let start = Instant::now();
        channel.submit(press(1, u64::MAX, "A", "Enter"), NOW, start);
        channel.tick(Some(NOW), start);
        assert_eq!(channel.buttons(), A);
        assert_eq!(channel.keys(), &[0x0Du8][..]);
        // Reading for the pad did not consume it for the keyboard.
        assert_eq!(channel.buttons(), A);
        assert_eq!(channel.keys(), &[0x0Du8][..]);
    }

    #[test]
    fn delivery_is_recorded_once_per_command() {
        let mut channel = Channel::new(None);
        let start = Instant::now();
        channel.submit(press(1, u64::MAX, "A", "Enter"), NOW, start);
        channel.tick(Some(NOW), start);
        assert!(!channel.active.as_ref().expect("active").delivered);
        channel.mark_delivered();
        assert!(channel.active.as_ref().expect("active").delivered);
        channel.mark_delivered();
        assert!(channel.active.as_ref().expect("active").delivered);
    }

    #[test]
    fn malformed_line_contributes_no_input() {
        let mut channel = Channel::new(None);
        let start = Instant::now();
        // The id is consumed even though the command is rejected.
        channel.line(b"5\t100\tNOPE\t-", NOW, start);
        channel.line(b"1\t100\tA", NOW, start);
        channel.submit(press(4, u64::MAX, "A", "-"), NOW, start);
        channel.tick(Some(NOW), start);
        assert_eq!(channel.buttons(), 0);
        assert!(channel.keys().is_empty());
    }

    #[test]
    fn merge_preserves_physical_input() {
        let mut pad = PadState {
            buttons: A,
            left_x: 0.5,
            ..Default::default()
        };
        merge_pad(&mut pad, B | MENU);
        assert_eq!(pad.buttons, A | B | MENU);
        assert_eq!(pad.left_x, 0.5);

        let mut held = vec![b'W', 0x0D];
        merge_keys(&mut held, &[0x0D, b'L']);
        assert_eq!(held, vec![b'W', 0x0D, b'L']);
    }
}
