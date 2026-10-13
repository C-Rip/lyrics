use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::lyr;
use crate::spotify::{cache_dir, Track};

const GET_URL: &str = "https://lrclib.net/api/get";
const SEARCH_URL: &str = "https://lrclib.net/api/search";

pub enum Lyrics {
    Loading,
    Synced { times: Vec<u64>, lines: Vec<String> },
    Plain(Vec<String>),
    Instrumental,
    NotFound,
    Failed(String),
}

#[derive(Deserialize, Default)]
struct Record {
    #[serde(rename = "syncedLyrics", default)]
    synced: Option<String>,
    #[serde(rename = "plainLyrics", default)]
    plain: Option<String>,
    #[serde(default)]
    instrumental: bool,
}

/// Busca en la caché local y si no está, en lrclib.
pub fn fetch(agent: &ureq::Agent, track: &Track) -> Lyrics {
    let base = cache_dir().join(sanitize(&track.id));
    let path = base.with_extension("lyr");
    if let Some(lyrics) = fs::read(&path).ok().and_then(|b| lyr::decode(&b)) {
        return lyrics;
    }

    let legacy = base.with_extension("json");
    let record = match fs::read(&legacy).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
        Some(record) => record,
        None => match lookup(agent, track) {
            Ok(Some(record)) => record,
            Ok(None) => return Lyrics::NotFound,
            Err(e) => return Lyrics::Failed(e),
        },
    };
    let lyrics = from_record(record);
    if let Some(bytes) = lyr::encode(&lyrics)
        && store(&path, &bytes).is_ok()
    {
        let _ = fs::remove_file(&legacy);
    }
    lyrics
}

/// Convierte la caché JSON de versiones anteriores a `.lyr`.
pub fn migrate_cache() {
    let Ok(entries) = fs::read_dir(cache_dir()) else { return };
    for entry in entries.flatten() {
        let legacy = entry.path();
        if legacy.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(record) = fs::read(&legacy).ok().and_then(|b| serde_json::from_slice::<Record>(&b).ok()) else {
            continue;
        };
        let stored = match lyr::encode(&from_record(record)) {
            Some(bytes) => store(&legacy.with_extension("lyr"), &bytes).is_ok(),
            None => true,
        };
        if stored {
            let _ = fs::remove_file(&legacy);
        }
    }
}

fn store(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("lyr.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

fn lookup(agent: &ureq::Agent, track: &Track) -> Result<Option<Record>, String> {
    let duration = (track.duration_ms / 1000).to_string();
    let exact = agent
        .get(GET_URL)
        .query("track_name", &track.title)
        .query("artist_name", &track.main_artist)
        .query("duration", &duration)
        .call();
    match exact {
        Ok(resp) => return resp.into_json().map(Some).map_err(|e| format!("lrclib: {e}")),
        Err(ureq::Error::Status(404, _)) => {}
        Err(e) => return Err(format!("lrclib: {e}")),
    }

    let mut results: Vec<Record> = agent
        .get(SEARCH_URL)
        .query("track_name", &track.title)
        .query("artist_name", &track.main_artist)
        .call()
        .map_err(|e| format!("lrclib: {e}"))?
        .into_json()
        .map_err(|e| format!("lrclib: {e}"))?;
    let best = results
        .iter()
        .position(|r| r.synced.as_deref().is_some_and(|s| !s.is_empty()))
        .unwrap_or(0);
    Ok((best < results.len()).then(|| results.swap_remove(best)))
}

fn from_record(record: Record) -> Lyrics {
    if let Some(synced) = record.synced.as_deref().filter(|s| !s.trim().is_empty()) {
        let (times, lines) = parse_lrc(synced);
        if !lines.is_empty() {
            return Lyrics::Synced { times, lines };
        }
    }
    if let Some(plain) = record.plain.as_deref().filter(|s| !s.trim().is_empty()) {
        return Lyrics::Plain(plain.lines().map(str::to_string).collect());
    }
    if record.instrumental {
        return Lyrics::Instrumental;
    }
    Lyrics::NotFound
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(120)
        .collect()
}

pub fn parse_lrc(source: &str) -> (Vec<u64>, Vec<String>) {
    let mut entries: Vec<(u64, String)> = Vec::new();
    for raw in source.lines() {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while let Some(after) = rest.strip_prefix('[') {
            let Some(end) = after.find(']') else { break };
            let Some(ms) = parse_stamp(&after[..end]) else { break };
            stamps.push(ms);
            rest = &after[end + 1..];
        }
        let text = rest.trim();
        for ms in stamps {
            entries.push((ms, text.to_string()));
        }
    }
    entries.sort_by_key(|(ms, _)| *ms);
    entries.into_iter().unzip()
}

fn parse_stamp(stamp: &str) -> Option<u64> {
    let (minutes, rest) = stamp.split_once(':')?;
    let (seconds, fraction) = rest.split_once(['.', ':']).unwrap_or((rest, ""));
    let minutes: u64 = minutes.parse().ok()?;
    let seconds: u64 = seconds.parse().ok()?;
    let fraction_ms = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<u64>().ok()? * 100,
        2 => fraction.parse::<u64>().ok()? * 10,
        _ => fraction.get(..3)?.parse().ok()?,
    };
    Some(minutes * 60_000 + seconds * 1000 + fraction_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lrc_and_skips_metadata() {
        let (times, lines) = parse_lrc("[ar:Someone]\n[00:03.500] world\n[00:01.50]hello\n[00:05.00]\n");
        assert_eq!(times, vec![1500, 3500, 5000]);
        assert_eq!(lines, vec!["hello", "world", ""]);
    }

    #[test]
    fn repeated_timestamps_on_one_line() {
        let (times, lines) = parse_lrc("[00:01.00][00:10.00]chorus");
        assert_eq!(times, vec![1000, 10_000]);
        assert_eq!(lines, vec!["chorus", "chorus"]);
    }
}
