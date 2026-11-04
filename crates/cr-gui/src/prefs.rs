//! Saved preferences and the online services' credentials.
//!
//! Preferences live in `prefs.json` in the user's config folder (`%APPDATA%\CleanGroove` on
//! Windows, `~/Library/Application Support/CleanGroove` on macOS, `~/.config/cleangroove` on Linux).
//! The Discogs token is never written there: it goes to the system keychain (Windows
//! Credential Manager, the macOS Keychain, the Linux kernel keyring), and `DISCOGS_TOKEN`
//! in the environment is used when the keychain has none.

use std::path::PathBuf;

use crossbeam_channel::{Receiver, TryRecvError};
use serde::{Deserialize, Serialize};

const KEYCHAIN_SERVICE: &str = "CleanGroove";
const KEYCHAIN_DISCOGS: &str = "discogs-token";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Email address or web page sent to MusicBrainz in the User-Agent, as it asks.
    pub musicbrainz_contact: String,
    /// Tracks export: the folder albums go in, the name template, 16 or 24 bit (0: as the
    /// rip), the cover embedded and as cover.jpg.
    pub export_root: String,
    pub export_template: String,
    pub export_bits: u16,
    pub embed_cover: bool,
    pub cover_jpg: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            musicbrainz_contact: String::new(),
            export_root: String::new(),
            export_template: cr_tracks::export::DEFAULT_TEMPLATE.to_string(),
            export_bits: 0,
            embed_cover: true,
            cover_jpg: true,
        }
    }
}

/// The folder preferences are kept in.
pub fn config_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        env("APPDATA").map(|d| d.join("CleanGroove"))
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Application Support/CleanGroove"))
    } else {
        env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config"))).map(|d| d.join("cleangroove"))
    }
}

impl Prefs {
    pub fn load() -> Self {
        config_dir()
            .and_then(|d| std::fs::read_to_string(d.join("prefs.json")).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = config_dir().ok_or("no config folder (APPDATA / HOME not set)")?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("prefs.json"), json + "\n").map_err(|e| e.to_string())
    }
}

/// Where the Discogs token came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    Keychain,
    Environment,
    None,
}

fn keychain_entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_DISCOGS).map_err(|e| e.to_string())
}

/// The Discogs token: the keychain's, else `DISCOGS_TOKEN`.
pub fn discogs_token() -> (Option<String>, TokenSource) {
    if let Some(t) = keychain_entry().ok().and_then(|e| e.get_password().ok()).filter(|t| !t.trim().is_empty()) {
        return (Some(t.trim().to_string()), TokenSource::Keychain);
    }
    match std::env::var("DISCOGS_TOKEN").ok().filter(|t| !t.trim().is_empty()) {
        Some(t) => (Some(t.trim().to_string()), TokenSource::Environment),
        None => (None, TokenSource::None),
    }
}

pub fn set_discogs_token(token: &str) -> Result<(), String> {
    keychain_entry()?.set_password(token.trim()).map_err(|e| e.to_string())
}

pub fn clear_discogs_token() -> Result<(), String> {
    match keychain_entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// A Discogs client with the saved token, or without one (release numbers still work).
pub fn discogs_client() -> cr_discogs::Client {
    match discogs_token().0 {
        Some(t) => cr_discogs::Client::new(t),
        None => cr_discogs::Client::anonymous(),
    }
}

/// Settings window state for the online services.
pub struct ServicesState {
    pub prefs: Prefs,
    /// Typed into the token box; cleared once saved.
    pub token_input: String,
    pub status: String,
    /// Where the Discogs token comes from; read once, and again when it is saved or removed.
    pub token_source: TokenSource,
    test_rx: Option<Receiver<String>>,
}

impl Default for ServicesState {
    fn default() -> Self {
        Self::new()
    }
}

impl ServicesState {
    pub fn new() -> Self {
        ServicesState { prefs: Prefs::load(), token_input: String::new(), status: String::new(), token_source: discogs_token().1, test_rx: None }
    }

    pub fn has_token(&self) -> bool {
        self.token_source != TokenSource::None
    }

    pub fn save_token(&mut self) {
        self.status = match set_discogs_token(&self.token_input) {
            Ok(()) => {
                self.token_input.clear();
                "Token saved in the system keychain".to_string()
            }
            Err(e) => format!("Could not save the token: {}", e),
        };
        self.token_source = discogs_token().1;
    }

    pub fn remove_token(&mut self) {
        self.status = match clear_discogs_token() {
            Ok(()) => "Token removed from the keychain".to_string(),
            Err(e) => format!("Could not remove the token: {}", e),
        };
        self.token_source = discogs_token().1;
    }

    /// Checks the saved token by asking Discogs whose it is.
    pub fn test_token(&mut self) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let msg = match discogs_token().0 {
                None => "No token saved".to_string(),
                Some(t) => match cr_discogs::Client::new(t).identity() {
                    Ok(id) => format!("Token works: Discogs user {}", id.username),
                    Err(e) => format!("Discogs did not accept the token: {}", e),
                },
            };
            let _ = tx.send(msg);
        });
        self.test_rx = Some(rx);
        self.status = "Asking Discogs...".to_string();
    }

    pub fn poll(&mut self) {
        if let Some(rx) = &self.test_rx {
            match rx.try_recv() {
                Ok(msg) => {
                    self.status = msg;
                    self.test_rx = None;
                }
                Err(TryRecvError::Disconnected) => self.test_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
    }

    pub fn is_working(&self) -> bool {
        self.test_rx.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prefs_round_trip_and_unknown_fields() {
        let p = Prefs { musicbrainz_contact: "me@example.com".into(), export_bits: 16, ..Default::default() };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<Prefs>(&json).unwrap(), p);
        // Older or newer files still load, with the defaults for what they lack.
        let old = serde_json::from_str::<Prefs>(r#"{"musicbrainz_contact": "x", "other": 1}"#).unwrap();
        assert_eq!(old, Prefs { musicbrainz_contact: "x".into(), ..Default::default() });
        assert!(old.embed_cover && old.cover_jpg && old.export_template.contains("$num(%track%,2)"));
    }
}
