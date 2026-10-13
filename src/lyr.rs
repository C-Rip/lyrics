//! Formato `.lyr` para la caché de letras.
//!
//! ```text
//! "LYR" versión(1)   4 bytes sin comprimir
//! tipo               1 byte: 0 sincronizada, 1 sin tiempos, 2 instrumental
//! cuerpo             deflate crudo
//!
//! sincronizada: n_frases, [largo, utf8]...  n_líneas, [Δ centésimas, índice de frase]...
//! sin tiempos:  n_líneas, [largo, utf8]...
//! ```
//!
//! Los números son varints LEB128. Cada frase se guarda una vez aunque se
//! repita (coros) y los tiempos son diferencias con la línea anterior.

use std::collections::HashMap;

use miniz_oxide::deflate::compress_to_vec;
use miniz_oxide::inflate::decompress_to_vec_with_limit;

use crate::lyrics::Lyrics;

const MAGIC: [u8; 4] = *b"LYR\x01";
const SYNCED: u8 = 0;
const PLAIN: u8 = 1;
const INSTRUMENTAL: u8 = 2;
/// Tope al descomprimir, para que un archivo corrupto no agote la memoria.
const MAX_BODY: usize = 1 << 20;

pub fn encode(lyrics: &Lyrics) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    let kind = match lyrics {
        Lyrics::Synced { times, lines } => {
            let mut index: HashMap<&str, usize> = HashMap::new();
            let mut phrases: Vec<&str> = Vec::new();
            let ids: Vec<usize> = lines
                .iter()
                .map(|line| {
                    *index.entry(line).or_insert_with(|| {
                        phrases.push(line);
                        phrases.len() - 1
                    })
                })
                .collect();
            put_strings(&mut body, &phrases);
            put(&mut body, lines.len() as u64);
            let mut prev = 0;
            for (&ms, id) in times.iter().zip(ids) {
                let cs = (ms / 10).max(prev);
                put(&mut body, cs - prev);
                put(&mut body, id as u64);
                prev = cs;
            }
            SYNCED
        }
        Lyrics::Plain(lines) => {
            put_strings(&mut body, lines);
            PLAIN
        }
        Lyrics::Instrumental => INSTRUMENTAL,
        _ => return None,
    };
    let mut out = MAGIC.to_vec();
    out.push(kind);
    out.extend(compress_to_vec(&body, 9));
    Some(out)
}

pub fn decode(bytes: &[u8]) -> Option<Lyrics> {
    let rest = bytes.strip_prefix(&MAGIC)?;
    let (&kind, compressed) = rest.split_first()?;
    let body = decompress_to_vec_with_limit(compressed, MAX_BODY).ok()?;
    let mut r = Reader { buf: &body };
    let lyrics = match kind {
        SYNCED => {
            let phrases = r.strings()?;
            let count = r.len()?;
            let mut times = Vec::with_capacity(count);
            let mut lines = Vec::with_capacity(count);
            let mut cs = 0u64;
            for _ in 0..count {
                cs = cs.checked_add(r.varint()?)?;
                times.push(cs * 10);
                lines.push(phrases.get(r.len()?)?.clone());
            }
            Lyrics::Synced { times, lines }
        }
        PLAIN => Lyrics::Plain(r.strings()?),
        INSTRUMENTAL => Lyrics::Instrumental,
        _ => return None,
    };
    r.buf.is_empty().then_some(lyrics)
}

fn put(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_strings<S: AsRef<str>>(out: &mut Vec<u8>, items: &[S]) {
    put(out, items.len() as u64);
    for item in items {
        let bytes = item.as_ref().as_bytes();
        put(out, bytes.len() as u64);
        out.extend_from_slice(bytes);
    }
}

struct Reader<'a> {
    buf: &'a [u8],
}

impl Reader<'_> {
    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = self.buf.split_first()?;
            self.buf = rest;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    /// Un conteo o largo, acotado por lo que queda del búfer.
    fn len(&mut self) -> Option<usize> {
        let n = usize::try_from(self.varint()?).ok()?;
        (n <= MAX_BODY).then_some(n)
    }

    fn strings(&mut self) -> Option<Vec<String>> {
        let count = self.len()?;
        let mut items = Vec::with_capacity(count.min(self.buf.len()));
        for _ in 0..count {
            let len = self.len()?;
            if len > self.buf.len() {
                return None;
            }
            let (text, rest) = self.buf.split_at(len);
            items.push(std::str::from_utf8(text).ok()?.to_string());
            self.buf = rest;
        }
        Some(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synced() -> Lyrics {
        let lines = ["la la", "chorus", "", "chorus", "すき きらい"];
        Lyrics::Synced {
            times: vec![0, 1500, 3010, 4500, 61_230],
            lines: lines.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn synced_round_trip() {
        let bytes = encode(&synced()).unwrap();
        let Some(Lyrics::Synced { times, lines }) = decode(&bytes) else {
            panic!("no decodificó");
        };
        assert_eq!(times, vec![0, 1500, 3010, 4500, 61_230]);
        assert_eq!(lines[3], "chorus");
        assert_eq!(lines[4], "すき きらい");
    }

    #[test]
    fn plain_and_instrumental_round_trip() {
        let plain = Lyrics::Plain(vec!["a".into(), "b".into()]);
        assert!(matches!(decode(&encode(&plain).unwrap()), Some(Lyrics::Plain(l)) if l == ["a", "b"]));
        assert!(matches!(decode(&encode(&Lyrics::Instrumental).unwrap()), Some(Lyrics::Instrumental)));
        assert!(encode(&Lyrics::NotFound).is_none());
    }

    #[test]
    fn corrupt_files_are_rejected() {
        let bytes = encode(&synced()).unwrap();
        assert!(decode(&bytes[..bytes.len() - 3]).is_none());
        assert!(decode(b"JSON").is_none());
        let mut wrong = bytes.clone();
        wrong[4] = 9;
        assert!(decode(&wrong).is_none());
    }
}
