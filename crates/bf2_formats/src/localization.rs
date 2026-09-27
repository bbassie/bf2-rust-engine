//! Localized strings from `mods/<mod>/Localization/<Language>/*.utxt`.
//!
//! UTF-16 text with one entry per line: the key, padding, then the text between pairs of
//! escape characters: `CPNAME_SK_16_hotel      \x1b\x1bHotel\x1b\x1b`.

use std::{collections::HashMap, path::Path};

use crate::install::Bf2Install;

/// Case-insensitive key to text.
#[derive(Debug, Clone, Default)]
pub struct Localization {
    strings: HashMap<String, String>,
}

impl Localization {
    /// Every `.utxt` of `language` (e.g. `english`) in every mod. Later files override
    /// earlier ones, so patch files win.
    pub fn load(install: &Bf2Install, language: &str) -> Self {
        let mut localization = Self::default();
        for mod_name in install.mods() {
            let Some(dir) = child_dir(&install.mod_dir(&mod_name), "localization")
                .and_then(|dir| child_dir(&dir, language))
            else {
                continue;
            };
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut files: Vec<_> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("utxt")))
                .collect();
            files.sort_by_key(|p| p.to_string_lossy().to_ascii_lowercase());
            for file in files {
                if let Ok(bytes) = std::fs::read(&file) {
                    localization.parse(&decode_utf16(&bytes));
                }
            }
        }
        localization
    }

    pub fn parse(&mut self, text: &str) {
        for line in text.split(['\n', '\r']) {
            let Some((key, rest)) = line.split_once('\x1b') else {
                continue;
            };
            let key = key.trim();
            let value = rest.trim_start_matches('\x1b');
            let value = value.split("\x1b\x1b").next().unwrap_or(value);
            if !key.is_empty() {
                self.strings.insert(key.to_ascii_lowercase(), value.to_string());
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.strings.get(&key.trim().to_ascii_lowercase()).map(String::as_str)
    }

    /// The text for `key`, or `key` itself if there is none.
    pub fn resolve(&self, key: &str) -> String {
        self.get(key).unwrap_or(key).to_string()
    }

    pub fn len(&self) -> usize {
        self.strings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.strings.is_empty()
    }
}

fn child_dir(parent: &Path, name: &str) -> Option<std::path::PathBuf> {
    std::fs::read_dir(parent)
        .ok()?
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name) && e.path().is_dir())
        .map(|e| e.path())
}

fn decode_utf16(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xFF, 0xFE]).unwrap_or(bytes);
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_entries() {
        let mut loc = Localization::default();
        loc.parse(
            "ID_LANGUAGE        \x1b\x1bEnglish\x1b\x1b\r\n\
             CPNAME_SK_16_hotel \x1b\x1bHotel\x1b\x1b\r\n\
             EMPTY_TEXT         \x1b\x1b\x1b\x1b\n",
        );
        assert_eq!(loc.get("cpname_sk_16_HOTEL"), Some("Hotel"));
        assert_eq!(loc.get("EMPTY_TEXT"), Some(""));
        assert_eq!(loc.resolve("missing"), "missing");
    }

    #[test]
    fn decodes_utf16_with_bom() {
        let bytes: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("Hi".encode_utf16().flat_map(|u| u.to_le_bytes()))
            .collect();
        assert_eq!(decode_utf16(&bytes), "Hi");
    }
}
