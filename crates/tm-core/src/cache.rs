//! Per-device token-count cache (plan Phase 2.8, N-36): o200k counts keyed
//! by an XXH3-128 hash of the exact text, so a re-merge — watch mode, a
//! merge after "Scan & curate", the next CLI run — only tokenizes files
//! that changed. Counts are a pure function of the text, so a hit can never
//! change an output.
//!
//! Off unless an application turns it on (`enable`): library callers and
//! tests stay hermetic. File: `<cache dir>/turbomerger/token-counts-v1.bin`
//! (`TURBOMERGER_CACHE_DIR` moves it, `TURBOMERGER_NO_CACHE` disables it).
//! Layout: magic, the counter's id (a new tokenizer or version starts a
//! fresh cache), then (u128 hash, u32 count) records, then an XXH3-64
//! checksum. Anything unexpected — truncated, foreign, corrupt — is ignored
//! and rebuilt.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use twox_hash::{XxHash3_128, XxHash3_64};

const MAGIC: &[u8; 8] = b"TMTC\x00\x01\x00\x00";
/// Identifies what the counts mean; bump when the counter changes.
const COUNTER_ID: &str = "o200k_base/tiktoken-rs-0.12/ordinary";
const FILE_NAME: &str = "token-counts-v1.bin";
/// Texts shorter than this are cheaper to count than to look up.
const MIN_LEN: usize = 256;
/// At most this many entries are kept on disk (~24 MB).
const MAX_ENTRIES: usize = 1_000_000;
const SHARDS: usize = 16;

struct Entry {
    count: u32,
    /// Looked up or added by this process (kept first when trimming).
    used: bool,
}

struct Cache {
    path: PathBuf,
    shards: Vec<Mutex<HashMap<u128, Entry>>>,
    dirty: std::sync::atomic::AtomicBool,
}

static CACHE: OnceLock<Option<Cache>> = OnceLock::new();

/// Turn the cache on for this process: `dir` or, by default, the user's
/// cache directory. Honours `TURBOMERGER_CACHE_DIR` / `TURBOMERGER_NO_CACHE`.
/// The first call wins.
pub fn enable(dir: Option<PathBuf>) {
    let _ = CACHE.get_or_init(|| {
        if std::env::var_os("TURBOMERGER_NO_CACHE").is_some_and(|v| !v.is_empty()) {
            return None;
        }
        let dir = std::env::var_os("TURBOMERGER_CACHE_DIR")
            .map(PathBuf::from)
            .or(dir)
            .or_else(|| dirs::cache_dir().map(|d| d.join("turbomerger")))?;
        let path = dir.join(FILE_NAME);
        let shards: Vec<Mutex<HashMap<u128, Entry>>> =
            (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect();
        if let Ok(records) = load(&path) {
            for (hash, count) in records {
                shards[shard(hash)]
                    .lock()
                    .expect("cache shard")
                    .insert(hash, Entry { count, used: false });
            }
        }
        Some(Cache {
            path,
            shards,
            dirty: std::sync::atomic::AtomicBool::new(false),
        })
    });
}

/// `tokens::count`, through the cache when it is on.
pub fn count(text: &str) -> usize {
    let Some(cache) = CACHE.get().and_then(Option::as_ref) else {
        return crate::tokens::count(text);
    };
    if text.len() < MIN_LEN {
        return crate::tokens::count(text);
    }
    let hash = XxHash3_128::oneshot(text.as_bytes());
    if let Some(e) = cache.shards[shard(hash)]
        .lock()
        .expect("cache shard")
        .get_mut(&hash)
    {
        e.used = true;
        return e.count as usize;
    }
    let n = crate::tokens::count(text);
    if let Ok(count) = u32::try_from(n) {
        cache.shards[shard(hash)]
            .lock()
            .expect("cache shard")
            .insert(hash, Entry { count, used: true });
        cache
            .dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    n
}

/// Write the cache back if anything was added. Best effort: a cache that
/// cannot be saved only costs time next run.
pub fn save() -> io::Result<()> {
    let Some(cache) = CACHE.get().and_then(Option::as_ref) else {
        return Ok(());
    };
    if !cache
        .dirty
        .swap(false, std::sync::atomic::Ordering::Relaxed)
    {
        return Ok(());
    }
    let mut records: Vec<(u128, u32, bool)> = Vec::new();
    for s in &cache.shards {
        let map = s.lock().expect("cache shard");
        records.extend(map.iter().map(|(h, e)| (*h, e.count, e.used)));
    }
    if records.len() > MAX_ENTRIES {
        // Keep what this run used, then the rest, up to the cap.
        records.sort_by_key(|r| !r.2);
        records.truncate(MAX_ENTRIES);
    }
    records.sort_by_key(|r| r.0);
    let mut payload = Vec::with_capacity(records.len() * 20 + 64);
    payload.extend_from_slice(MAGIC);
    payload.extend_from_slice(&(COUNTER_ID.len() as u32).to_le_bytes());
    payload.extend_from_slice(COUNTER_ID.as_bytes());
    payload.extend_from_slice(&(records.len() as u64).to_le_bytes());
    for (h, c, _) in &records {
        payload.extend_from_slice(&h.to_le_bytes());
        payload.extend_from_slice(&c.to_le_bytes());
    }
    let sum = XxHash3_64::oneshot(&payload);
    payload.extend_from_slice(&sum.to_le_bytes());
    write_atomically(&cache.path, &payload)
}

fn shard(hash: u128) -> usize {
    (hash as usize) % SHARDS
}

fn load(path: &Path) -> io::Result<Vec<(u128, u32)>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut bytes)?;
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "token cache: unexpected format");
    if bytes.len() < MAGIC.len() + 4 + 8 + 8 || &bytes[..MAGIC.len()] != MAGIC {
        return Err(bad());
    }
    let (body, sum) = bytes.split_at(bytes.len() - 8);
    if XxHash3_64::oneshot(body).to_le_bytes() != sum {
        return Err(bad());
    }
    let mut at = MAGIC.len();
    let take = |at: &mut usize, n: usize| -> io::Result<&[u8]> {
        let s = body.get(*at..*at + n).ok_or_else(bad)?;
        *at += n;
        Ok(s)
    };
    let id_len = u32::from_le_bytes(take(&mut at, 4)?.try_into().expect("4 bytes")) as usize;
    if take(&mut at, id_len)? != COUNTER_ID.as_bytes() {
        return Err(bad()); // another counter: start over
    }
    let n = u64::from_le_bytes(take(&mut at, 8)?.try_into().expect("8 bytes")) as usize;
    if n > MAX_ENTRIES || body.len() - at != n * 20 {
        return Err(bad());
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let h = u128::from_le_bytes(take(&mut at, 16)?.try_into().expect("16 bytes"));
        let c = u32::from_le_bytes(take(&mut at, 4)?.try_into().expect("4 bytes"));
        out.push((h, c));
    }
    Ok(out)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    // No fsync: a cache needs no durability (a torn or empty file fails the
    // checksum and is rebuilt), and syncing can stall behind other writes.
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_damage_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        // Build a file the way `save` does, for two records.
        let mut payload = Vec::new();
        payload.extend_from_slice(MAGIC);
        payload.extend_from_slice(&(COUNTER_ID.len() as u32).to_le_bytes());
        payload.extend_from_slice(COUNTER_ID.as_bytes());
        payload.extend_from_slice(&2u64.to_le_bytes());
        for (h, c) in [(7u128, 11u32), (u128::MAX, 3)] {
            payload.extend_from_slice(&h.to_le_bytes());
            payload.extend_from_slice(&c.to_le_bytes());
        }
        let sum = XxHash3_64::oneshot(&payload);
        let mut good = payload.clone();
        good.extend_from_slice(&sum.to_le_bytes());
        std::fs::write(&path, &good).unwrap();
        assert_eq!(load(&path).unwrap(), vec![(7, 11), (u128::MAX, 3)]);

        // One flipped byte, a truncation, another counter: all ignored.
        let mut flipped = good.clone();
        flipped[MAGIC.len() + 10] ^= 1;
        std::fs::write(&path, &flipped).unwrap();
        assert!(load(&path).is_err());
        std::fs::write(&path, &good[..good.len() - 3]).unwrap();
        assert!(load(&path).is_err());
        let other = payload
            .windows(COUNTER_ID.len())
            .position(|w| w == COUNTER_ID.as_bytes())
            .unwrap();
        let mut foreign = payload.clone();
        foreign[other] ^= 0x20;
        let s = XxHash3_64::oneshot(&foreign);
        foreign.extend_from_slice(&s.to_le_bytes());
        std::fs::write(&path, &foreign).unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn disabled_cache_counts_directly() {
        // Nothing enabled the cache in this test binary: plain counts.
        let text = "fn main() {}\n".repeat(100);
        assert_eq!(count(&text), crate::tokens::count(&text));
        assert!(save().is_ok());
    }
}
