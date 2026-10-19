//! Fortschritts-Meldung und Abbruch für lang laufende Datei-Vorgänge (v. a. Sync).
//!
//! Beides ist bewusst simpel gehalten und ohne Async-Runtime umgesetzt, damit der
//! Kern auf jeder Plattform gleich funktioniert. Die GUI-Schicht (Tauri) übersetzt
//! [`Progress`]-Callbacks später in Events an das Web-Frontend.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Abbruch-Signal für einen laufenden Vorgang.
///
/// Wird von außen (z. B. „Abbrechen"-Button in der UI) über [`Cancel::cancel`] gesetzt
/// und im Sync-Loop regelmäßig mit [`Cancel::is_cancelled`] abgefragt. Klonbar — alle
/// Klone teilen sich dasselbe Flag (per `Arc`).
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>, Option<std::time::Instant>);

impl Cancel {
    /// Neues, noch nicht ausgelöstes Abbruch-Signal.
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)), None)
    }

    /// Den Vorgang zum Abbruch markieren. Wirkt beim nächsten Check im Loop.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Wurde Abbruch angefordert?
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
            || self
                .1
                .is_some_and(|deadline| std::time::Instant::now() >= deadline)
    }

    /// Eine Vorbereitungsfrist gilt nur für diesen geliehenen Teilvorgang. Ihr
    /// Ablauf setzt das gemeinsame Nutzerflag nicht und benötigt keinen Timerthread.
    pub(crate) fn until(&self, deadline: std::time::Instant) -> Self {
        Self(
            self.0.clone(),
            Some(self.1.map_or(deadline, |old| old.min(deadline))),
        )
    }
}

/// Momentaufnahme des Fortschritts eines Sync-Vorgangs.
///
/// Wird per Callback nach außen gemeldet (mehrfach pro Sekunde bei großen Dateien).
/// Alle Zähler sind kumulativ über den gesamten Vorgang.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Progress {
    /// Anzahl zu schreibender Dateien insgesamt (Kopien + Aktualisierungen).
    pub files_total: u64,
    /// Bereits fertig geschriebene Dateien.
    pub files_done: u64,
    /// Insgesamt zu übertragende Bytes (Summe der zu kopierenden/aktualisierenden Dateien).
    pub bytes_total: u64,
    /// Bereits übertragene Bytes.
    pub bytes_done: u64,
    /// Aktuell bearbeiteter relativer Pfad (für die Statuszeile in der UI).
    pub current: String,
    /// Aktuelle Phase als stabiler Bezeichner. Die Oberfläche übersetzt ihn in einen
    /// deutschen Text; ein hier ergänzter Wert, den sie nicht kennt, erscheint roh in
    /// der Statuszeile. Deshalb die vollständige Liste:
    ///
    /// - `"copy"` — Kopieren oder Aktualisieren
    /// - `"move"` — Verschieben
    /// - `"metadata"` — Metadaten übertragen oder anpassen
    /// - `"delete"` — endgültiges Entfernen
    /// - `"quarantine"` — Entfernen über die Quarantäne, also wiederherstellbar
    /// - `"done"`, `"cancelled"` — Abschluss
    /// - `"failed"`, `"interrupted"`, `"completion_unclear"` — Abbruchzustände eines
    ///   Queue-Eintrags
    /// - `"recovery"`, `"recovery_acknowledged"` — Wiederaufnahme nach einem Abbruch
    ///
    /// `"delete"` und `"quarantine"` sind ausdrücklich verschiedene Wirkungen und
    /// dürfen in der Anzeige nicht denselben Text bekommen.
    pub phase: String,
}
