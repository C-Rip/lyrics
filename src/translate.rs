//! Traducción de letras con DeepL.
//!
//! La letra original no se modifica en disco. La traducción se guarda aparte,
//! con los mismos tiempos, y solo se vuelve a pedir si cambia el texto.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::lyr;
use crate::lyrics::Lyrics;
use crate::player::State;
use crate::spotify::{self, cache_dir};

const FREE_URL: &str = "https://api-free.deepl.com/v2/translate";
const PRO_URL: &str = "https://api.deepl.com/v2/translate";
const BATCH_COUNT: usize = 50;
const BATCH_BYTES: usize = 100 * 1024;
const RETRY_MIN: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(20);

#[derive(Clone, Copy)]
pub struct Lang {
    code: &'static str,
}

pub fn parse_lang(name: &str) -> Option<Lang> {
    let code = match name.trim().to_lowercase().as_str() {
        "spanish" | "español" | "es" => "ES",
        "es-latam" => "ES-419",
        "english" | "en" | "en-us" => "EN-US",
        "en-gb" => "EN-GB",
        "portuguese" | "portugués" | "portugues" | "pt" | "pt-br" => "PT-BR",
        "pt-pt" => "PT-PT",
        "french" | "francés" | "frances" | "fr" => "FR",
        "fr-ca" => "FR-CA",
        "german" | "alemán" | "aleman" | "de" => "DE",
        "de-ch" => "DE-CH",
        "italian" | "italiano" | "it" => "IT",
        "japanese" | "japonés" | "japones" | "ja" => "JA",
        "korean" | "coreano" | "ko" => "KO",
        "chinese" | "chino" | "zh" | "zh-hans" => "ZH",
        "zh-hant" => "ZH-HANT",
        "russian" | "ruso" | "ru" => "RU",
        _ => return None,
    };
    Some(Lang { code })
}

pub(crate) struct Job {
    pub track_id: String,
    pub lyrics: Arc<Lyrics>,
}

enum Failure {
    Transient(String),
    Fatal(String),
}

impl Failure {
    fn message(self) -> String {
        match self {
            Failure::Transient(text) | Failure::Fatal(text) => text,
        }
    }
}

#[derive(Serialize)]
struct Payload<'a> {
    text: &'a [String],
    target_lang: &'a str,
    preserve_formatting: bool,
}

#[derive(Deserialize)]
struct Reply {
    translations: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    detected_source_language: String,
    text: String,
}

/// Clave de `DEEPL_API_KEY` o, si no está, del archivo de estado.
pub fn api_key() -> Result<String, String> {
    if let Ok(key) = std::env::var("DEEPL_API_KEY") {
        let key = key.trim();
        if !key.is_empty() {
            return Ok(key.to_string());
        }
    }
    let path = key_path();
    let key = fs::read_to_string(&path).map_err(|_| missing_key())?;
    let key = key.trim();
    if key.is_empty() {
        return Err(missing_key());
    }
    Ok(key.to_string())
}

pub fn save_key(key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("la clave está vacía".into());
    }
    let path = key_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| {
            use std::io::Write;
            file.write_all(key.as_bytes())
        })
        .and_then(|_| fs::rename(&tmp, &path))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    println!("Clave guardada en {}", path.display());
    Ok(())
}

fn key_path() -> PathBuf {
    spotify::state_dir().join("deepl-key")
}

fn missing_key() -> String {
    "Sin clave de DeepL. Ejecuta: lyrics deepl <clave>".into()
}

fn endpoint(key: &str) -> &'static str {
    if key.ends_with(":fx") {
        FREE_URL
    } else {
        PRO_URL
    }
}

/// Traduce antes de abrir la pantalla. La usa `--demo`.
pub fn blocking(track_id: &str, lyrics: &Lyrics, lang: &Lang) -> Result<Lyrics, String> {
    if let Some(hit) = cached(track_id, lyrics, lang) {
        return Ok(hit);
    }
    let key = api_key()?;
    let done = translate(&spotify::http_agent(), &key, lang, lyrics).map_err(Failure::message)?;
    store(track_id, lyrics, lang, &done);
    Ok(done)
}

pub(crate) fn cached(track_id: &str, lyrics: &Lyrics, lang: &Lang) -> Option<Lyrics> {
    let path = cache_path(track_id, lyrics, lang)?;
    fs::read(&path).ok().and_then(|bytes| lyr::decode(&bytes))
}

pub(crate) fn spawn(lang: Lang, state: Arc<Mutex<State>>) -> Sender<Job> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("translate".into())
        .spawn(move || worker(lang, state, rx))
        .expect("no se pudo crear el hilo de traducción");
    tx
}

fn worker(lang: Lang, state: Arc<Mutex<State>>, rx: Receiver<Job>) {
    let Ok(key) = api_key() else {
        return;
    };
    let agent = spotify::http_agent();
    while let Ok(job) = rx.recv() {
        let mut job = latest(&rx, job);
        let mut delay = RETRY_MIN;
        loop {
            match translate(&agent, &key, &lang, &job.lyrics) {
                Ok(done) => {
                    store(&job.track_id, &job.lyrics, &lang, &done);
                    match rx.try_recv() {
                        Ok(newer) => {
                            job = latest(&rx, newer);
                            delay = RETRY_MIN;
                        }
                        Err(_) => {
                            publish(&state, &job.track_id, done);
                            break;
                        }
                    }
                }
                Err(Failure::Fatal(text)) => match rx.try_recv() {
                    Ok(newer) => {
                        job = latest(&rx, newer);
                        delay = RETRY_MIN;
                    }
                    Err(_) => {
                        set_notice(&state, &job.track_id, text);
                        break;
                    }
                },
                Err(Failure::Transient(_)) => match rx.recv_timeout(delay) {
                    Ok(newer) => {
                        job = latest(&rx, newer);
                        delay = RETRY_MIN;
                    }
                    Err(RecvTimeoutError::Timeout) => delay = next_delay(delay),
                    Err(RecvTimeoutError::Disconnected) => return,
                },
            }
        }
    }
}

fn latest(rx: &Receiver<Job>, mut job: Job) -> Job {
    while let Ok(newer) = rx.try_recv() {
        job = newer;
    }
    job
}

fn next_delay(current: Duration) -> Duration {
    (current * 2).min(RETRY_MAX)
}

fn publish(state: &Mutex<State>, track_id: &str, lyrics: Lyrics) {
    let mut state = state.lock().unwrap();
    if state
        .now
        .as_ref()
        .is_some_and(|now| now.track.id == track_id)
    {
        state.translated = Some(Arc::new(lyrics));
        state.notice = None;
    }
}

fn set_notice(state: &Mutex<State>, track_id: &str, notice: String) {
    let mut state = state.lock().unwrap();
    if state
        .now
        .as_ref()
        .is_some_and(|now| now.track.id == track_id)
    {
        state.notice = Some(notice);
    }
}

fn translate(
    agent: &ureq::Agent,
    key: &str,
    lang: &Lang,
    lyrics: &Lyrics,
) -> Result<Lyrics, Failure> {
    let Some(lines) = lines_of(lyrics) else {
        return Err(Failure::Fatal("no hay letra que traducir".into()));
    };
    let (phrases, ids) = unique_ids(lines);
    if phrases.is_empty() {
        return with_lines(lyrics, lines.to_vec())
            .ok_or_else(|| Failure::Fatal("no hay letra que traducir".into()));
    }
    let mut translated = Vec::with_capacity(phrases.len());
    for batch in chunks(&phrases) {
        let items = post(agent, key, lang, batch)?;
        if items.len() != batch.len() {
            return Err(Failure::Fatal(
                "DeepL devolvió una respuesta incompleta".into(),
            ));
        }
        for (original, item) in batch.iter().zip(items) {
            translated.push(choose(&item, original, lang.code));
        }
    }
    let lines = substitute(ids, &translated);
    with_lines(lyrics, lines).ok_or_else(|| Failure::Fatal("no hay letra que traducir".into()))
}

fn post(
    agent: &ureq::Agent,
    key: &str,
    lang: &Lang,
    batch: &[String],
) -> Result<Vec<Item>, Failure> {
    let response = agent
        .post(endpoint(key))
        .set("Authorization", &format!("DeepL-Auth-Key {key}"))
        .send_json(Payload {
            text: batch,
            target_lang: lang.code,
            preserve_formatting: true,
        })
        .map_err(classify)?;
    let reply: Reply = response
        .into_json()
        .map_err(|_| Failure::Fatal("DeepL: respuesta inválida".into()))?;
    Ok(reply.translations)
}

fn classify(err: ureq::Error) -> Failure {
    match err {
        ureq::Error::Status(403, _) => Failure::Fatal("DeepL rechazó la clave".into()),
        ureq::Error::Status(456, _) => Failure::Fatal("Cuota de DeepL agotada".into()),
        ureq::Error::Status(code, _) if code == 429 || (500..600).contains(&code) => {
            Failure::Transient(format!("DeepL: {code}"))
        }
        ureq::Error::Status(code, _) => Failure::Fatal(format!("DeepL: {code}")),
        ureq::Error::Transport(transport) => {
            let detail = transport
                .message()
                .map(str::to_string)
                .unwrap_or_else(|| transport.kind().to_string());
            Failure::Transient(format!("DeepL: {detail}"))
        }
    }
}

fn choose(item: &Item, original: &str, target: &str) -> String {
    if same_lang(&item.detected_source_language, target) {
        original.to_string()
    } else {
        item.text.clone()
    }
}

fn same_lang(detected: &str, target: &str) -> bool {
    let detected = detected.split('-').next().unwrap_or(detected);
    let target = target.split('-').next().unwrap_or(target);
    detected.eq_ignore_ascii_case(target)
}

fn lines_of(lyrics: &Lyrics) -> Option<&[String]> {
    match lyrics {
        Lyrics::Synced { lines, .. } | Lyrics::Plain(lines) => Some(lines),
        _ => None,
    }
}

fn with_lines(lyrics: &Lyrics, lines: Vec<String>) -> Option<Lyrics> {
    match lyrics {
        Lyrics::Synced { times, .. } => Some(Lyrics::Synced {
            times: times.clone(),
            lines,
        }),
        Lyrics::Plain(_) => Some(Lyrics::Plain(lines)),
        _ => None,
    }
}

/// Frases distintas, sin las líneas vacías. El índice `None` es una línea vacía.
fn unique_ids(lines: &[String]) -> (Vec<String>, Vec<Option<usize>>) {
    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut phrases = Vec::new();
    let ids = lines
        .iter()
        .map(|line| {
            if line.is_empty() {
                return None;
            }
            Some(*index.entry(line.as_str()).or_insert_with(|| {
                phrases.push(line.clone());
                phrases.len() - 1
            }))
        })
        .collect();
    (phrases, ids)
}

fn substitute(ids: Vec<Option<usize>>, phrases: &[String]) -> Vec<String> {
    ids.into_iter()
        .map(|id| match id {
            None => String::new(),
            Some(id) => phrases[id].clone(),
        })
        .collect()
}

fn chunks(phrases: &[String]) -> Vec<&[String]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut bytes = 0usize;
    let mut count = 0usize;
    for (i, phrase) in phrases.iter().enumerate() {
        let size = phrase.len();
        if count > 0 && (count == BATCH_COUNT || bytes + size > BATCH_BYTES) {
            out.push(&phrases[start..i]);
            start = i;
            bytes = 0;
            count = 0;
        }
        bytes += size;
        count += 1;
    }
    if start < phrases.len() {
        out.push(&phrases[start..]);
    }
    out
}

fn cache_path(track_id: &str, lyrics: &Lyrics, lang: &Lang) -> Option<PathBuf> {
    let lines = lines_of(lyrics)?;
    let name = format!(
        "{}.{}.{hash:016x}.lyr",
        sanitize(track_id),
        lang.code,
        hash = fingerprint(lines),
    );
    Some(cache_dir().join("translations").join(name))
}

fn store(track_id: &str, original: &Lyrics, lang: &Lang, translated: &Lyrics) {
    let Some(path) = cache_path(track_id, original, lang) else {
        return;
    };
    let Some(bytes) = lyr::encode(translated) else {
        return;
    };
    if let Some(dir) = path.parent()
        && fs::create_dir_all(dir).is_err()
    {
        return;
    }
    let tmp = path.with_extension("lyr.tmp");
    if fs::write(&tmp, bytes).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(120)
        .collect()
}

/// FNV-1a de 64 bits. No cambia entre versiones de Rust, a diferencia de `DefaultHasher`.
fn fingerprint(lines: &[String]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for line in lines {
        for byte in line.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_aliases() {
        assert_eq!(parse_lang("spanish").unwrap().code, "ES");
        assert_eq!(parse_lang("Español").unwrap().code, "ES");
        assert_eq!(parse_lang("es").unwrap().code, "ES");
        assert_eq!(parse_lang("es-latam").unwrap().code, "ES-419");
        assert_eq!(parse_lang("ES-LATAM").unwrap().code, "ES-419");
        assert_eq!(parse_lang("english").unwrap().code, "EN-US");
        assert_eq!(parse_lang("en-gb").unwrap().code, "EN-GB");
        assert_eq!(parse_lang("pt").unwrap().code, "PT-BR");
        assert_eq!(parse_lang("fr-ca").unwrap().code, "FR-CA");
        assert_eq!(parse_lang("de-ch").unwrap().code, "DE-CH");
        assert_eq!(parse_lang("ja").unwrap().code, "JA");
        assert_eq!(parse_lang("ZH-HANT").unwrap().code, "ZH-HANT");
        assert!(parse_lang("klingon").is_none());
    }

    #[test]
    fn fingerprint_is_stable_and_order_sensitive() {
        let lines = [String::from("ab"), String::from("c")];
        assert_eq!(fingerprint(&lines), fingerprint(&lines));
        assert_ne!(
            fingerprint(&lines),
            fingerprint(&[String::from("a"), String::from("bc")])
        );
        assert_ne!(
            fingerprint(&lines),
            fingerprint(&[String::from("ab"), String::from("d")])
        );
    }

    #[test]
    fn repeated_chorus_keeps_times_and_blanks() {
        let lines = vec![
            "hello".into(),
            String::new(),
            "hello".into(),
            "world".into(),
        ];
        let (phrases, ids) = unique_ids(&lines);
        assert_eq!(phrases, ["hello", "world"]);
        let translated = vec!["hola".into(), "mundo".into()];
        let lyrics = Lyrics::Synced {
            times: vec![0, 10, 20, 30],
            lines,
        };
        let Lyrics::Synced { times, lines } =
            with_lines(&lyrics, substitute(ids, &translated)).unwrap()
        else {
            panic!("no reconstruyó");
        };
        assert_eq!(times, [0, 10, 20, 30]);
        assert_eq!(lines, ["hola", "", "hola", "mundo"]);
    }

    #[test]
    fn already_in_target_language_stays() {
        let spanish = Item {
            detected_source_language: "ES".into(),
            text: "hello".into(),
        };
        let english = Item {
            detected_source_language: "EN".into(),
            text: "hola".into(),
        };
        assert_eq!(choose(&spanish, "hola", "ES"), "hola");
        assert_eq!(choose(&english, "hello", "ES"), "hola");
        assert!(same_lang("EN", "EN-US"));
        assert!(same_lang("pt", "PT-BR"));
        assert!(!same_lang("EN", "ES"));
    }

    #[test]
    fn batches_respect_count_and_size() {
        let many: Vec<String> = (0..51).map(|i| i.to_string()).collect();
        let batches = chunks(&many);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 50);
        assert_eq!(batches[1].len(), 1);

        let wide = vec!["a".repeat(60_000), "b".repeat(60_000)];
        assert_eq!(chunks(&wide).len(), 2);
        assert!(chunks(&[]).is_empty());
    }

    #[test]
    fn retry_delay_caps_at_twenty_seconds() {
        assert_eq!(next_delay(RETRY_MIN), Duration::from_secs(10));
        assert_eq!(next_delay(Duration::from_secs(10)), RETRY_MAX);
        assert_eq!(next_delay(RETRY_MAX), RETRY_MAX);
    }
}
