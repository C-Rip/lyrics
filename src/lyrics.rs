use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::lyr;
use crate::spotify::{Track, cache_dir};

const GET_URL: &str = "https://lrclib.net/api/get";
const SEARCH_URL: &str = "https://lrclib.net/api/search";

pub enum Lyrics {
    Loading,
    Synced {
        times: Vec<u64>,
        lines: Vec<String>,
    },
    Plain(Vec<String>),
    Instrumental,
    NotFound,
    /// lrclib saturado, timeout o red. No se guarda en caché.
    Retrying(String),
    Failed(String),
}

enum LookupError {
    Transient(String),
    Fatal(String),
}

#[derive(Deserialize, Default)]
struct Record {
    #[serde(rename = "syncedLyrics", default)]
    synced: Option<String>,
    #[serde(rename = "plainLyrics", default)]
    plain: Option<String>,
    #[serde(default)]
    instrumental: bool,
    /// LRCLIB manda segundos con decimales. `u64` directo fallaría al deserializar.
    #[serde(default)]
    duration: f64,
}

/// Busca en la caché local y si no está, en lrclib.
pub fn fetch(agent: &ureq::Agent, track: &Track) -> Lyrics {
    let base = cache_dir().join(sanitize(&track.id));
    let path = base.with_extension("lyr");
    if let Some(lyrics) = fs::read(&path).ok().and_then(|b| lyr::decode(&b)) {
        return lyrics;
    }

    let legacy = base.with_extension("json");
    let record = match fs::read(&legacy)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
    {
        Some(record) => record,
        None => match lookup(agent, track) {
            Ok(Some(record)) => record,
            Ok(None) => return Lyrics::NotFound,
            Err(LookupError::Transient(e)) => return Lyrics::Retrying(e),
            Err(LookupError::Fatal(e)) => return Lyrics::Failed(e),
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
    let Ok(entries) = fs::read_dir(cache_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let legacy = entry.path();
        if legacy.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(record) = fs::read(&legacy)
            .ok()
            .and_then(|b| serde_json::from_slice::<Record>(&b).ok())
        else {
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

fn lookup(agent: &ureq::Agent, track: &Track) -> Result<Option<Record>, LookupError> {
    let video_secs = track.duration_ms / 1000;
    let duration = video_secs.to_string();
    let exact = agent
        .get(GET_URL)
        .query("track_name", &track.title)
        .query("artist_name", &track.main_artist)
        .query("duration", &duration)
        .call();
    match exact {
        Ok(resp) => {
            return resp
                .into_json()
                .map(Some)
                .map_err(|e| LookupError::Fatal(format!("lrclib: {e}")));
        }
        Err(ureq::Error::Status(404, _)) => {}
        Err(e) => return Err(classify(e)),
    }

    let results = search(agent, Some((&track.title, &track.main_artist)), None)?;
    if !results.is_empty() {
        return Ok(best_match(results, video_secs));
    }
    let q = format!("{} {}", track.main_artist, track.title);
    let q = q.trim();
    if q.is_empty() {
        return Ok(None);
    }
    let results = search(agent, None, Some(q))?;
    Ok(best_match(results, video_secs))
}

fn search(
    agent: &ureq::Agent,
    fields: Option<(&str, &str)>,
    q: Option<&str>,
) -> Result<Vec<Record>, LookupError> {
    let mut request = agent.get(SEARCH_URL);
    if let Some((title, artist)) = fields {
        request = request
            .query("track_name", title)
            .query("artist_name", artist);
    }
    if let Some(q) = q {
        request = request.query("q", q);
    }
    request
        .call()
        .map_err(classify)?
        .into_json()
        .map_err(|e| LookupError::Fatal(format!("lrclib: {e}")))
}

/// Margen en el que una letra sincronizada sigue valiendo para el video.
const SYNC_MARGIN_SECS: u64 = 8;

/// Elige la letra sincronizada más cercana a la duración del video.
/// Si todas quedan más lejos de [`SYNC_MARGIN_SECS`], devuelve texto plano.
fn best_match(mut records: Vec<Record>, video_secs: u64) -> Option<Record> {
    if records.is_empty() {
        return None;
    }
    if video_secs == 0 {
        if let Some(index) = records.iter().position(has_synced) {
            return Some(records.swap_remove(index));
        }
        if let Some(index) = records.iter().position(has_plain) {
            return Some(records.swap_remove(index));
        }
        return records.into_iter().find(|record| record.instrumental);
    }

    if let Some(index) = closest(
        records
            .iter()
            .enumerate()
            .filter(|(_, record)| has_synced(record)),
        video_secs,
    ) && duration_diff(&records[index], video_secs) <= SYNC_MARGIN_SECS
    {
        return Some(records.swap_remove(index));
    }

    let nearest = closest(records.iter().enumerate(), video_secs).unwrap();
    let nearest_diff = duration_diff(&records[nearest], video_secs);
    let lyric_closer = records
        .iter()
        .any(|record| has_lyrics(record) && duration_diff(record, video_secs) < nearest_diff);
    if records[nearest].instrumental && !has_lyrics(&records[nearest]) && !lyric_closer {
        return Some(records.swap_remove(nearest));
    }

    let lyrics: Vec<usize> = records
        .iter()
        .enumerate()
        .filter(|(_, record)| has_lyrics(record))
        .map(|(index, _)| index)
        .collect();
    if lyrics.is_empty() {
        return None;
    }
    let within: Vec<usize> = lyrics
        .iter()
        .copied()
        .filter(|&index| duration_diff(&records[index], video_secs) <= SYNC_MARGIN_SECS)
        .collect();
    if !within.is_empty() {
        let index = closest(
            within.iter().map(|&index| (index, &records[index])),
            video_secs,
        )
        .unwrap();
        return Some(records.swap_remove(index));
    }
    let index = closest(
        lyrics.iter().map(|&index| (index, &records[index])),
        video_secs,
    )
    .unwrap();
    Some(force_plain(records.swap_remove(index)))
}

fn closest<'a, I>(records: I, video_secs: u64) -> Option<usize>
where
    I: Iterator<Item = (usize, &'a Record)>,
{
    records
        .min_by_key(|(index, record)| (duration_diff(record, video_secs), *index))
        .map(|(index, _)| index)
}

fn duration_diff(record: &Record, video_secs: u64) -> u64 {
    let secs = record.duration.round() as u64;
    secs.abs_diff(video_secs)
}

fn has_synced(record: &Record) -> bool {
    record
        .synced
        .as_deref()
        .is_some_and(|text| !text.trim().is_empty())
}

fn has_plain(record: &Record) -> bool {
    record
        .plain
        .as_deref()
        .is_some_and(|text| !text.trim().is_empty())
}

fn has_lyrics(record: &Record) -> bool {
    has_synced(record) || has_plain(record)
}

/// `from_record` prefiere la sincronía. Aquí se apaga para no mostrar una letra desfasada.
fn force_plain(mut record: Record) -> Record {
    let plain_empty = record
        .plain
        .as_deref()
        .is_none_or(|text| text.trim().is_empty());
    if plain_empty && let Some(synced) = record.synced.as_deref() {
        let text = strip_timestamps(synced);
        if !text.trim().is_empty() {
            record.plain = Some(text);
        }
    }
    record.synced = None;
    record
}

fn strip_timestamps(synced: &str) -> String {
    let (_times, lines) = parse_lrc(synced);
    lines.join("\n")
}

fn classify(err: ureq::Error) -> LookupError {
    let message = lrclib_message(&err);
    if is_transient(&err) {
        LookupError::Transient(message)
    } else {
        LookupError::Fatal(message)
    }
}

fn is_transient(err: &ureq::Error) -> bool {
    match err {
        ureq::Error::Status(code, _) => transient_status(*code),
        ureq::Error::Transport(_) => true,
    }
}

/// 429 y 5xx se reintentan. El resto de códigos HTTP se muestran y no se repiten.
fn transient_status(code: u16) -> bool {
    code == 429 || (500..=599).contains(&code)
}

/// Código o motivo, sin la URL que ureq incluye en el error.
fn lrclib_message(err: &ureq::Error) -> String {
    match err {
        ureq::Error::Status(code, _) => format!("lrclib: {code}"),
        ureq::Error::Transport(transport) => match transport.message() {
            Some(message) => format!("lrclib: {message}"),
            None => format!("lrclib: {}", transport.kind()),
        },
    }
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
            let Some(ms) = parse_stamp(&after[..end]) else {
                break;
            };
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
        let (times, lines) =
            parse_lrc("[ar:Someone]\n[00:03.500] world\n[00:01.50]hello\n[00:05.00]\n");
        assert_eq!(times, vec![1500, 3500, 5000]);
        assert_eq!(lines, vec!["hello", "world", ""]);
    }

    #[test]
    fn repeated_timestamps_on_one_line() {
        let (times, lines) = parse_lrc("[00:01.00][00:10.00]chorus");
        assert_eq!(times, vec![1000, 10_000]);
        assert_eq!(lines, vec!["chorus", "chorus"]);
    }

    #[test]
    fn saturated_statuses_are_transient() {
        for code in [429, 500, 503] {
            assert!(transient_status(code), "{code}");
        }
        for code in [400, 401] {
            assert!(!transient_status(code), "{code}");
        }
    }

    fn sample(
        duration: f64,
        synced: Option<&str>,
        plain: Option<&str>,
        instrumental: bool,
    ) -> Record {
        Record {
            duration,
            synced: synced.map(str::to_string),
            plain: plain.map(str::to_string),
            instrumental,
        }
    }

    #[test]
    fn duration_accepts_decimals() {
        let record: Record = serde_json::from_str(r#"{"duration": 259.4}"#).unwrap();
        assert_eq!(record.duration.round() as u64, 259);
    }

    #[test]
    fn synced_within_eight_seconds_wins() {
        let records = vec![
            sample(400.0, Some("[00:01.00]lejos"), None, false),
            sample(259.4, Some("[00:01.00]cerca"), None, false),
        ];
        let best = best_match(records, 259).unwrap();
        assert_eq!(best.synced.as_deref(), Some("[00:01.00]cerca"));
    }

    #[test]
    fn beyond_eight_seconds_becomes_plain_text() {
        let records = vec![sample(
            100.0,
            Some("[00:01.00]hola\n[00:02.00]mundo"),
            None,
            false,
        )];
        let best = best_match(records, 120).unwrap();
        assert!(best.synced.is_none());
        let Lyrics::Plain(lines) = from_record(best) else {
            panic!("se esperaba texto plano");
        };
        assert_eq!(lines, ["hola", "mundo"]);
    }

    #[test]
    fn far_sync_keeps_an_existing_plain_lyric() {
        let records = vec![sample(100.0, Some("[00:01.00]sync"), Some("plana"), false)];
        let best = best_match(records, 120).unwrap();
        assert!(best.synced.is_none());
        let Lyrics::Plain(lines) = from_record(best) else {
            panic!("se esperaba texto plano");
        };
        assert_eq!(lines, ["plana"]);
    }

    #[test]
    fn eight_seconds_keeps_the_sync() {
        let records = vec![sample(108.0, Some("[00:01.00]hola"), Some("ya"), false)];
        let best = best_match(records, 100).unwrap();
        assert!(best.synced.is_some());
    }

    #[test]
    fn closer_instrumental_beats_a_far_lyric() {
        let records = vec![
            sample(200.0, Some("[00:01.00]lejos"), None, false),
            sample(101.0, None, None, true),
        ];
        let best = best_match(records, 100).unwrap();
        assert!(matches!(from_record(best), Lyrics::Instrumental));
    }

    #[test]
    fn unknown_duration_takes_the_first_synced_line() {
        let records = vec![
            sample(10.0, None, Some("plana"), false),
            sample(20.0, Some("[00:01.00]sync"), None, false),
        ];
        let best = best_match(records, 0).unwrap();
        assert_eq!(best.synced.as_deref(), Some("[00:01.00]sync"));
    }
}
