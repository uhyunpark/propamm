//! Replay of recorded feeds in receive-time order.
//!
//! Each feed is a chain of hourly `.jsonl.zst` files, read lazily. A k-way merge over the feeds
//! yields raw messages sorted by `recv_ns`.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::Value;
use tracing::warn;

use crate::feeds::FEEDS;
use crate::parse::split_line;

type Lines = std::io::Lines<BufReader<zstd::stream::read::Decoder<'static, BufReader<File>>>>;

/// One feed's lines, oldest file first.
struct FeedReader {
    files: VecDeque<PathBuf>,
    current: Option<Lines>,
    skipped: u64,
}

impl FeedReader {
    /// Files of `<dir>/<date>/<HH>.jsonl.zst`, sorted by name (UTC date, then hour).
    fn open(dir: &Path) -> anyhow::Result<Self> {
        let mut files = Vec::new();
        for day in std::fs::read_dir(dir)? {
            let day = day?.path();
            if day.is_dir() {
                for f in std::fs::read_dir(&day)? {
                    let f = f?.path();
                    if f.extension().is_some_and(|e| e == "zst") {
                        files.push(f);
                    }
                }
            }
        }
        files.sort();
        Ok(Self {
            files: files.into(),
            current: None,
            skipped: 0,
        })
    }

    /// Next `(recv_ns, message)`; unreadable lines and truncated files are skipped with a warning.
    fn next_msg(&mut self) -> Option<(u64, Value)> {
        loop {
            if self.current.is_none() {
                let path = self.files.pop_front()?;
                match File::open(&path).and_then(zstd::stream::read::Decoder::new) {
                    Ok(dec) => self.current = Some(BufReader::new(dec).lines()),
                    Err(e) => warn!(file = %path.display(), "cannot open: {e}"),
                }
                continue;
            }
            let lines = self.current.as_mut()?;
            match lines.next() {
                Some(Ok(line)) => match split_line(&line) {
                    Ok(rec) => return Some(rec),
                    Err(_) => self.skipped += 1,
                },
                Some(Err(e)) => {
                    // The recorder may still be writing the last frame.
                    warn!("read error, dropping rest of file: {e}");
                    self.current = None;
                }
                None => self.current = None,
            }
        }
    }
}

/// A raw recorded message.
#[derive(Debug)]
pub struct Record {
    /// Feed name as in `feeds::FEEDS`.
    pub feed: &'static str,
    pub recv_ns: u64,
    pub msg: Value,
}

/// Time-ordered merge of all feeds present under `data_dir`.
pub struct Replay {
    feeds: Vec<(&'static str, FeedReader)>,
    heads: Vec<Option<(u64, Value)>>,
    heap: BinaryHeap<Reverse<(u64, usize)>>,
}

impl Replay {
    pub fn open(data_dir: &Path) -> anyhow::Result<Self> {
        let mut feeds = Vec::new();
        for spec in FEEDS {
            let dir = data_dir.join(spec.name);
            if dir.is_dir() {
                let reader =
                    FeedReader::open(&dir).with_context(|| format!("reading {}", dir.display()))?;
                feeds.push((spec.name, reader));
            }
        }
        anyhow::ensure!(
            !feeds.is_empty(),
            "no feed directories in {}",
            data_dir.display()
        );
        let mut heads = Vec::with_capacity(feeds.len());
        let mut heap = BinaryHeap::new();
        for (i, (_, reader)) in feeds.iter_mut().enumerate() {
            let first = reader.next_msg();
            if let Some((t, _)) = &first {
                heap.push(Reverse((*t, i)));
            }
            heads.push(first);
        }
        Ok(Self { feeds, heads, heap })
    }

    /// Lines dropped as malformed so far.
    pub fn skipped_lines(&self) -> u64 {
        self.feeds.iter().map(|(_, r)| r.skipped).sum()
    }
}

impl Iterator for Replay {
    type Item = Record;

    fn next(&mut self) -> Option<Record> {
        let Reverse((recv_ns, i)) = self.heap.pop()?;
        let msg = self.heads[i].take().map(|(_, m)| m)?;
        self.heads[i] = self.feeds[i].1.next_msg();
        if let Some((t, _)) = &self.heads[i] {
            self.heap.push(Reverse((*t, i)));
        }
        Some(Record {
            feed: self.feeds[i].0,
            recv_ns,
            msg,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_zst(path: &Path, times: &[u64], feed: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut enc = zstd::stream::write::Encoder::new(File::create(path).unwrap(), 0).unwrap();
        for t in times {
            writeln!(
                enc,
                r#"{{"recv_ns":{t},"feed":"{feed}","msg":{{"n":{t}}}}}"#
            )
            .unwrap();
        }
        enc.finish().unwrap();
    }

    #[test]
    fn merges_feeds_and_hour_files_by_recv_time() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("binance_spot_sol_bbo/2026-10-05");
        let b = dir.path().join("binance_perp_sol_bbo/2026-10-05");
        write_zst(&a.join("00.jsonl.zst"), &[10, 40], "binance_spot_sol_bbo");
        write_zst(&a.join("01.jsonl.zst"), &[60, 90], "binance_spot_sol_bbo");
        write_zst(
            &b.join("00.jsonl.zst"),
            &[20, 50, 70, 100],
            "binance_perp_sol_bbo",
        );
        let got: Vec<(u64, &str)> = Replay::open(dir.path())
            .unwrap()
            .map(|r| (r.recv_ns, r.feed))
            .collect();
        let spot = "binance_spot_sol_bbo";
        let perp = "binance_perp_sol_bbo";
        assert_eq!(
            got,
            vec![
                (10, spot),
                (20, perp),
                (40, spot),
                (50, perp),
                (60, spot),
                (70, perp),
                (90, spot),
                (100, perp)
            ]
        );
    }

    #[test]
    fn empty_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Replay::open(dir.path()).is_err());
    }
}
