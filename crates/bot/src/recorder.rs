//! Writes raw messages to hourly-rotated, zstd-compressed JSONL files.
//!
//! Layout: `<out>/<feed>/<YYYY-MM-DD>/<HH>.jsonl.zst` (UTC, by local receive time).
//! Line: `{"recv_ns":<u64>,"feed":"<name>","msg":<raw message verbatim>}`.
//!
//! Skipped: pure keepalive replies (OKX literal `pong`, Hyperliquid `{"channel":"pong"}`,
//! Bybit `{"op":"pong"}`) and any non-JSON text. A restart inside an hour appends a new
//! zstd frame to the existing file; decoders read concatenated frames transparently.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use serde_json::value::RawValue;
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, error};

use crate::connector::{RawMessage, now_unix_ns};

const FLUSH_EVERY_NS: u64 = 5_000_000_000;
const NS_PER_HOUR: u64 = 3_600_000_000_000;
const ZSTD_LEVEL: i32 = 3;
const TICK_EVERY: Duration = Duration::from_secs(1);

type Encoder = zstd::Encoder<'static, File>;

struct Open {
    hour: u64,
    enc: Encoder,
}

pub struct Recorder {
    out: PathBuf,
    open: HashMap<&'static str, Open>,
    last_flush_ns: u64,
}

/// What `write` did with a message.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Written,
    Skipped,
}

impl Recorder {
    pub fn new(out: impl Into<PathBuf>) -> Self {
        Self {
            out: out.into(),
            open: HashMap::new(),
            last_flush_ns: 0,
        }
    }

    pub fn write(&mut self, msg: &RawMessage) -> io::Result<Outcome> {
        let Some(line) = encode_line(msg) else {
            return Ok(Outcome::Skipped);
        };
        let hour = msg.recv_unix_ns / NS_PER_HOUR;
        if self.open.get(msg.feed).is_some_and(|o| o.hour != hour) {
            self.close(msg.feed)?;
        }
        let o = match self.open.entry(msg.feed) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(Open {
                hour,
                enc: open_encoder(&path_for(&self.out, msg.feed, hour))?,
            }),
        };
        if let Err(e) = o.enc.write_all(line.as_bytes()) {
            // Drop the broken encoder so the next message reopens the file.
            self.open.remove(msg.feed);
            return Err(e);
        }
        Ok(Outcome::Written)
    }

    /// Periodic housekeeping: flush every 5 s, finish files whose hour has passed.
    pub fn tick(&mut self, now_ns: u64) -> io::Result<()> {
        let now_hour = now_ns / NS_PER_HOUR;
        let mut result = self.close_where(|o| o.hour < now_hour);
        if now_ns.saturating_sub(self.last_flush_ns) >= FLUSH_EVERY_NS {
            self.last_flush_ns = now_ns;
            for o in self.open.values_mut() {
                if let Err(e) = o.enc.flush() {
                    result = Err(e);
                }
            }
        }
        result
    }

    /// Finish every open file so all of them are valid zstd streams.
    pub fn finish(&mut self) -> io::Result<()> {
        self.close_where(|_| true)
    }

    fn close(&mut self, feed: &str) -> io::Result<()> {
        match self.open.remove(feed) {
            Some(o) => o.enc.finish()?.sync_all(),
            None => Ok(()),
        }
    }

    /// Close every matching file, even after a failure; returns the last error.
    fn close_where(&mut self, pred: impl Fn(&Open) -> bool) -> io::Result<()> {
        let feeds: Vec<_> = self
            .open
            .iter()
            .filter(|(_, o)| pred(o))
            .map(|(f, _)| *f)
            .collect();
        let mut result = Ok(());
        for feed in feeds {
            if let Err(e) = self.close(feed) {
                result = Err(e);
            }
        }
        result
    }
}

fn path_for(out: &Path, feed: &str, hour: u64) -> PathBuf {
    let (y, m, d) = civil_from_days(hour / 24);
    out.join(feed)
        .join(format!("{y:04}-{m:02}-{d:02}"))
        .join(format!("{:02}.jsonl.zst", hour % 24))
}

fn open_encoder(path: &Path) -> io::Result<Encoder> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    zstd::Encoder::new(file, ZSTD_LEVEL)
}

/// Build the JSONL line, or `None` for keepalives and non-JSON text.
fn encode_line(msg: &RawMessage) -> Option<String> {
    let text = msg.text.trim();
    // Cheap pre-check so ordinary messages skip the keepalive parse.
    if text.contains("pong") && is_keepalive(text) {
        return None;
    }
    let raw: &RawValue = serde_json::from_str(text).ok()?;
    // A raw newline would break JSONL; compact such messages (rare).
    let compact;
    let body = if text.contains(['\n', '\r']) {
        compact = serde_json::from_str::<Value>(text).ok()?.to_string();
        compact.as_str()
    } else {
        raw.get()
    };
    Some(format!(
        "{{\"recv_ns\":{},\"feed\":\"{}\",\"msg\":{}}}\n",
        msg.recv_unix_ns, msg.feed, body
    ))
}

fn is_keepalive(text: &str) -> bool {
    if text == "pong" {
        return true;
    }
    if text.len() > 256 {
        return false;
    }
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    v.get("channel").and_then(Value::as_str) == Some("pong")
        || v.get("op").and_then(Value::as_str) == Some("pong")
        || v.get("ret_msg").and_then(Value::as_str) == Some("pong")
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (Hinnant's algorithm).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    (y, m, d)
}

/// Consume messages until all senders are dropped, then finish all files.
pub async fn run(mut rx: mpsc::Receiver<RawMessage>, mut rec: Recorder) {
    let mut timer = interval(TICK_EVERY);
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(m) => match rec.write(&m) {
                    Ok(Outcome::Written) => {}
                    Ok(Outcome::Skipped) => debug!(feed = m.feed, "skipped keepalive/non-JSON"),
                    Err(e) => error!(feed = m.feed, error = %e, "write failed"),
                },
                None => break,
            },
            _ = timer.tick() => {
                if let Err(e) = rec.tick(now_unix_ns()) {
                    error!(error = %e, "housekeeping failed");
                }
            }
        }
    }
    if let Err(e) = rec.finish() {
        error!(error = %e, "finishing files failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    // 2026-10-04 12:00:00 UTC
    const T0: u64 = 1_791_115_200 * 1_000_000_000;
    const HOUR: u64 = NS_PER_HOUR;

    fn msg(feed: &'static str, ns: u64, text: &str) -> RawMessage {
        RawMessage {
            feed,
            recv_unix_ns: ns,
            text: text.to_owned(),
        }
    }

    fn fixture() -> (tempfile::TempDir, Recorder) {
        let dir = tempfile::tempdir().unwrap();
        let rec = Recorder::new(dir.path());
        (dir, rec)
    }

    /// Decode `<dir>/<rel>` into its JSONL lines.
    fn read_lines(dir: &tempfile::TempDir, rel: &str) -> Vec<Value> {
        let mut s = String::new();
        zstd::Decoder::new(File::open(dir.path().join(rel)).unwrap())
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        s.lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(T0 / HOUR / 24), (2026, 10, 4));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn writes_expected_paths_and_roundtrips() {
        let (dir, mut rec) = fixture();
        let a = r#"{"b":"121.4","a":"121.5"}"#;
        let b = r#"{"type":"heartbeat","n":[1,2]}"#;
        assert_eq!(rec.write(&msg("feed_a", T0, a)).unwrap(), Outcome::Written);
        assert_eq!(
            rec.write(&msg("feed_b", T0 + 5, b)).unwrap(),
            Outcome::Written
        );
        rec.finish().unwrap();

        let lines = read_lines(&dir, "feed_a/2026-10-04/12.jsonl.zst");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["recv_ns"], T0);
        assert_eq!(lines[0]["feed"], "feed_a");
        assert_eq!(lines[0]["msg"], serde_json::from_str::<Value>(a).unwrap());
        let lines = read_lines(&dir, "feed_b/2026-10-04/12.jsonl.zst");
        assert_eq!(lines[0]["msg"], serde_json::from_str::<Value>(b).unwrap());
    }

    #[test]
    fn rotates_hourly_with_injected_time() {
        let (dir, mut rec) = fixture();
        rec.write(&msg("f", T0, r#"{"i":1}"#)).unwrap();
        rec.write(&msg("f", T0 + HOUR, r#"{"i":2}"#)).unwrap();
        // Housekeeping at hour 14 finishes the hour-13 file without a new message.
        rec.tick(T0 + 2 * HOUR).unwrap();
        assert!(rec.open.is_empty());

        let first = read_lines(&dir, "f/2026-10-04/12.jsonl.zst");
        let second = read_lines(&dir, "f/2026-10-04/13.jsonl.zst");
        assert_eq!(first[0]["msg"]["i"], 1);
        assert_eq!(second[0]["msg"]["i"], 2);
    }

    #[test]
    fn rotates_across_midnight() {
        let (dir, mut rec) = fixture();
        rec.write(&msg("f", T0 + 11 * HOUR, r#"{"i":1}"#)).unwrap();
        rec.write(&msg("f", T0 + 12 * HOUR, r#"{"i":2}"#)).unwrap();
        rec.finish().unwrap();
        assert!(dir.path().join("f/2026-10-04/23.jsonl.zst").exists());
        assert!(dir.path().join("f/2026-10-05/00.jsonl.zst").exists());
    }

    #[test]
    fn skips_keepalives_and_non_json() {
        let (dir, mut rec) = fixture();
        for text in [
            "pong",
            r#"{"channel":"pong"}"#,
            r#"{"op":"pong","success":true}"#,
            r#"{"success":true,"ret_msg":"pong","op":"ping"}"#,
            "not json",
        ] {
            assert_eq!(
                rec.write(&msg("f", T0, text)).unwrap(),
                Outcome::Skipped,
                "{text}"
            );
        }
        assert_eq!(
            rec.write(&msg("f", T0, r#"{"channel":"bbo"}"#)).unwrap(),
            Outcome::Written
        );
        rec.finish().unwrap();
        assert_eq!(read_lines(&dir, "f/2026-10-04/12.jsonl.zst").len(), 1);
    }

    #[test]
    fn multiline_json_stays_one_line() {
        let (dir, mut rec) = fixture();
        rec.write(&msg("f", T0, "{\n \"a\": 1\n}")).unwrap();
        rec.finish().unwrap();
        let lines = read_lines(&dir, "f/2026-10-04/12.jsonl.zst");
        assert_eq!(lines[0]["msg"]["a"], 1);
    }

    #[test]
    fn reopen_in_same_hour_appends_valid_frame() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..2 {
            let mut rec = Recorder::new(dir.path());
            rec.write(&msg("f", T0 + i, r#"{"x":1}"#)).unwrap();
            rec.finish().unwrap();
        }
        assert_eq!(read_lines(&dir, "f/2026-10-04/12.jsonl.zst").len(), 2);
    }
}
