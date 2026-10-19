//! Interaktive Umbenennung mit wirkungsspezifischer Ausführungsklasse.
//!
//! Dieses Modul trennt die gewöhnliche Dateimanager-Umbenennung (F2/CLI) vom
//! strikten Sync-Commit-Vertrag. Ein Provider, dessen Rename ein vorhandenes Ziel
//! ersetzen könnte (z. B. FTP-Server mit überschreibendem RNTO), verliert dadurch
//! nicht mehr die gesamte Umbenennung. Stattdessen läuft sie als sichtbarer
//! Best-Effort-Weg:
//!
//! 1. unmittelbar vorher ein frisches, vollständiges Elternlisting,
//! 2. sichtbare Kollision, wenn der Zielname bereits existiert (keine Mutation),
//! 3. genau eine Rename-Mutation — niemals ein blinder Wiederholversuch,
//! 4. anschließend ein Listing-Readback, der das Ergebnis belegt,
//! 5. nach einem Verbindungsverlust höchstens ein erneutes Listing zur reinen
//!    Zustandsklärung; bleibt der Zustand unklar, meldet der stabile Fehlercode
//!    [`ErrorCode::OutcomeUnclear`] genau das.
//!
//! Die Resultate sind eindeutig: erfolgreich ([`RenameOutcome::Renamed`]),
//! kollidiert ([`RenameOutcome::Collision`]), unverändert (Fehler mit geklärtem
//! Bestand) oder Ausgang unklar ([`ErrorCode::OutcomeUnclear`]).
//!
//! Reichweite der Ausführungsklassen: Sie beschreiben ausschließlich, wie gut das
//! ZIEL des Umbenennens geschützt ist. Der Vorgang spricht die Quelle in beiden
//! Klassen über ihren Pfad an — genau wie jede andere Datei- oder Löschoperation
//! des Vfs-Modells. Zwischen dem frischen Elternlisting und der Mutation kann ein
//! gleichnamiger Eintrag ausgetauscht worden sein; dann benennt der Vorgang den
//! neuen Eintrag um und meldet Erfolg. Diese klassenunabhängige Einschränkung
//! steht als [`SOURCE_PATH_BINDING_LIMITATION`] in jedem Fähigkeits- und
//! Ergebnisbericht, damit „abgesichert" nicht mehr verspricht, als der Code hält.

use std::io;
use std::path::{Path, PathBuf};

use crate::error::{coded_error, error_code, ErrorCode};
use crate::fileops;
use crate::remote_real_mode::ProviderKind;
use crate::vfs::{CompleteDirectoryListing, ProofListEntry, Vfs};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EntryType {
    is_dir: bool,
    is_file: bool,
    is_symlink: bool,
}

fn entry_type(entry: &crate::vfs::Entry) -> EntryType {
    EntryType {
        is_dir: entry.is_dir,
        is_file: entry.is_file,
        is_symlink: entry.is_symlink,
    }
}

/// Klassenunabhängige Einschränkung jeder Umbenennung: Die Quelle ist über ihren
/// Pfad gebunden, nicht über einen Providerbeleg des konkreten Eintrags.
///
/// Kein Provider des Vfs-Modells bietet ein Rename, das an einen Versionsbeleg des
/// Quelleintrags gebunden werden kann: SFTP und FTP/FTPS liefern zu einem Eintrag
/// überhaupt keinen Token, der SMB-Token ändert sich beim Umbenennen selbst
/// (`ChangeTime`), und nur WebDAV kennt mit `If-Match` eine bedingte Operation —
/// die es in `rename_noreplace` bereits nutzt, allerdings mit einer unmittelbar
/// vorher selbst gelesenen ETag. Deshalb wird die Einschränkung ausgewiesen statt
/// scheinbar behoben (Entscheidung 2026-08-03).
pub const SOURCE_PATH_BINDING_LIMITATION: &str = "Der umzubenennende Eintrag ist über seinen Pfad gebunden: Zwischen dem frischen Elternlisting und der Umbenennung könnte ein gleichnamiger Eintrag ausgetauscht worden sein.";

/// Ausführungsklasse einer konkreten, frisch geprüften Umbenennung.
///
/// Beide Klassen beschreiben ausschließlich den Schutz des ZIELS. Für die Bindung
/// der Quelle gilt in beiden [`SOURCE_PATH_BINDING_LIMITATION`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenameExecutionClass {
    /// Der Provider beweist ein atomares No-Replace-Rename: Eine späte Kollision
    /// kann kein vorhandenes Ziel ersetzen. Die Zusage gilt genau dafür — sie sagt
    /// nichts darüber, ob die Quelle beim Zugriff noch derselbe Eintrag ist wie im
    /// frischen Elternlisting.
    Assured,
    /// Das gewöhnliche Provider-Rename ist bewiesen, aber ein vorhandenes Ziel
    /// könnte bei einer späten Kollision ersetzt werden. Die Bestandsprüfung
    /// unmittelbar vorher begrenzt dieses Fenster, schließt es aber nicht atomar.
    BestEffort,
}

/// Für genau eine verbundene Sitzung geprüfte Umbenennungs-Fähigkeit samt der
/// konkreten Einschränkungen und Blocker. GUI und CLI zeigen diese Daten an,
/// statt eigene Provider-Sperrlisten zu erfinden.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ProviderRenameCapabilities {
    pub provider: ProviderKind,
    /// Atomares No-Replace-Rename ist für diese Sitzung bewiesen.
    pub assured: bool,
    /// Das gewöhnliche Rename samt Readback ist bewiesen, ohne No-Replace-Garantie.
    pub best_effort: bool,
    /// Konkrete Einschränkungen der stärksten verfügbaren Klasse. Sobald überhaupt
    /// eine Klasse verfügbar ist, steht hier mindestens
    /// [`SOURCE_PATH_BINDING_LIMITATION`]; die klassenspezifischen Einschränkungen
    /// stehen davor. Leer ist die Liste nur, wenn Umbenennen gar nicht möglich ist —
    /// den Grund nennt dann `blockers`.
    pub limitations: Vec<String>,
    /// Warum keine Ausführungsklasse verfügbar ist (leer, sobald eine existiert).
    pub blockers: Vec<String>,
}

impl ProviderRenameCapabilities {
    /// Stärkste verfügbare Ausführungsklasse; `None` heißt „nicht ausführbar".
    pub fn execution_class(&self) -> Option<RenameExecutionClass> {
        if self.assured {
            Some(RenameExecutionClass::Assured)
        } else if self.best_effort {
            Some(RenameExecutionClass::BestEffort)
        } else {
            None
        }
    }
}

/// Eindeutiges, rückgelesenes Ergebnis einer Umbenennung.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RenameOutcome {
    /// Die Umbenennung wurde ausgeführt und per Readback bestätigt. Der Readback
    /// belegt Namen und Typ des Ergebnisses; dass die Quelle beim Zugriff noch
    /// derselbe Eintrag war wie im Listing, kann er nicht belegen (siehe
    /// [`SOURCE_PATH_BINDING_LIMITATION`]).
    Renamed,
    /// Der Zielname existiert bereits; es wurde nichts verändert. Der Aufrufer
    /// entscheidet sichtbar, wie es weitergeht — ein stilles Ersetzen gibt es nicht.
    Collision,
}

/// Ergebnisbericht einer interaktiven Umbenennung. „Unverändert" und „Ausgang
/// unklar" laufen bewusst als Fehler mit stabilem Code, damit kein Aufrufer einen
/// Teilerfolg übersehen kann.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RenameReport {
    pub outcome: RenameOutcome,
    pub class: RenameExecutionClass,
    /// Einschränkungen der verwendeten Klasse samt der klassenunabhängigen
    /// [`SOURCE_PATH_BINDING_LIMITATION`], für Anzeige in GUI und CLI. Die
    /// klassenspezifische Einschränkung steht vorn, damit eine Oberfläche, die nur
    /// den ersten Eintrag zeigt, weiterhin das Wesentliche zeigt.
    pub limitations: Vec<String>,
    /// Bei [`RenameOutcome::Renamed`]: der vollständige neue Pfad.
    pub new_path: Option<PathBuf>,
    /// Bei [`RenameOutcome::Collision`]: ob der vorhandene Eintrag ein Ordner ist.
    pub existing_is_dir: Option<bool>,
    /// `true`, wenn die Mutationsantwort oder der erste Readback die Verbindung
    /// verlor und erst das klärende Listing nach dem identitätsgeprüften
    /// Neuverbinden den Erfolg belegt hat.
    pub verified_after_reconnect: bool,
}

/// Führt die Umbenennung in der übergebenen Ausführungsklasse aus.
///
/// `reclaim` stellt nach einem Verbindungsverlust höchstens einmal eine frische,
/// identitätsgeprüfte Sitzung her — ausschließlich damit ein Listing den echten
/// Zielzustand klären kann. Die Mutation selbst wird niemals wiederholt.
pub(crate) fn rename_with_class(
    vfs: &dyn Vfs,
    old_abs: &Path,
    new_name: &str,
    sandbox: &Option<PathBuf>,
    class: RenameExecutionClass,
    limitations: Vec<String>,
    reclaim: Option<&dyn Fn() -> io::Result<()>>,
) -> io::Result<RenameReport> {
    let new_name = fileops::validate_name(new_name)?;
    let parent = old_abs.parent().ok_or_else(|| {
        coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Umbenennen benötigt einen Elternordner",
        )
    })?;
    let old_name = old_abs
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            coded_error(
                io::ErrorKind::InvalidInput,
                ErrorCode::InvalidInput,
                "Der bisherige Name ist nicht sicher dekodierbar",
            )
        })?
        .to_string();
    if new_name == old_name {
        return Err(coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Der neue Name ist mit dem bisherigen identisch",
        ));
    }
    let new_abs = parent.join(&new_name);
    fileops::guard_vfs(vfs, sandbox, old_abs)?;
    fileops::guard_vfs(vfs, sandbox, &new_abs)?;

    // Schritt 1: frische, nachweislich vollständige Bestandsprüfung des Elternordners.
    // Ohne vollständiges Listing lässt sich weder die Kollision noch der spätere
    // Readback ehrlich belegen — dann ist die Umbenennung nicht ausführbar.
    let listing = complete_parent_listing(vfs, parent)?;
    if class == RenameExecutionClass::BestEffort {
        // Ohne No-Replace-Garantie trägt allein diese Bestandsprüfung das Verbot
        // des stillen Ersetzens. Ein Name, der sich nicht sicher dekodieren lässt,
        // kann dem Zielnamen serverseitig gleichen, ohne dass wir das erkennen —
        // dann ist die Kollisionsprüfung nicht ehrlich belegbar.
        if let Some(unsafe_entry) = listing.entries.iter().find(|item| !item.safely_decoded) {
            return Err(coded_error(
                io::ErrorKind::InvalidData,
                ErrorCode::InvalidData,
                format!(
                    "Der Zielordner enthält einen nicht sicher dekodierbaren Namen ({:?}); die Kollisionsprüfung kann nicht ehrlich belegt werden",
                    String::from_utf8_lossy(&unsafe_entry.raw_name)
                ),
            ));
        }
    }
    // Dieser gemeinsame Koordinator übernimmt nur den Bestandseintrag. Das VFS
    // besitzt keinen allgemeinen Rename-mit-Token-Aufruf. Provider dürfen ihre
    // stärkere Bindung innerhalb der Mutation ergänzen: WebDAV setzt eine
    // ETag-Bedingung; seine private gewöhnliche Rename-Sicht hält zusätzlich den
    // Quelltoken aus genau diesem Listing. Andere Provider bleiben pfadbasiert,
    // ausgewiesen als SOURCE_PATH_BINDING_LIMITATION.
    let source_entry = listing
        .entries
        .iter()
        .find(|item| item.entry.name == old_name)
        .map(|item| item.entry.clone())
        .ok_or_else(|| {
            coded_error(
                io::ErrorKind::NotFound,
                ErrorCode::NotFound,
                format!("„{old_name}“ steht nicht im frischen Elternlisting"),
            )
        })?;
    // Schritt 2: sichtbare Kollision — keine Mutation, der Aufrufer entscheidet.
    // Der Vergleich ist bewusst konservativ: Viele Server setzen Groß-/Klein-
    // schreibung oder Unicode-Normalformen (NFC/NFD) gleich. Ein nur darüber
    // „freier" Name gilt deshalb ebenfalls als Kollision — lieber eine sichtbare
    // falsche Kollision auf einem strikten Server als ein stilles Ersetzen auf
    // einem gleichsetzenden. Der eigene bisherige Name ist ausgenommen, damit
    // eine reine Groß-/Kleinschreibungs-Korrektur desselben Eintrags möglich bleibt.
    if let Some(existing) = listing
        .entries
        .iter()
        .find(|item| item.entry.name != old_name && names_may_collide(&item.entry.name, &new_name))
    {
        return Ok(RenameReport {
            outcome: RenameOutcome::Collision,
            class,
            limitations,
            new_path: None,
            existing_is_dir: Some(existing.entry.is_dir),
            verified_after_reconnect: false,
        });
    }

    // Schritt 3: genau eine Mutation in der stärksten verfügbaren Klasse.
    let mutation = match class {
        RenameExecutionClass::Assured => vfs.rename_noreplace(old_abs, &new_abs),
        RenameExecutionClass::BestEffort => vfs.rename_plain_once(old_abs, &new_abs),
    };
    match mutation {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            // Späte Kollision: Der Server hat sie selbst erkannt und nichts ersetzt.
            return Ok(RenameReport {
                outcome: RenameOutcome::Collision,
                class,
                limitations,
                new_path: None,
                existing_is_dir: None,
                verified_after_reconnect: false,
            });
        }
        Err(error) if error_code(&error) == ErrorCode::ConnectionLost => {
            // Die Antwort ging verloren; ob der Server die Mutation vorher noch
            // ausgeführt hat, ist offen. Es folgt ausschließlich Zustandsklärung.
            return classify_after_connection_loss(
                vfs,
                parent,
                &old_name,
                &new_name,
                &new_abs,
                entry_type(&source_entry),
                class,
                limitations,
                reclaim,
                error,
            );
        }
        // Jeder andere Fehler einer No-Replace- bzw. sauber abgelehnten Mutation
        // bedeutet: nichts wurde verändert. Der Code des Providers bleibt erhalten.
        Err(error) => return Err(error),
    }

    // Schritt 4: Listing-Readback. Erst der frische Bestand macht aus der
    // Serverantwort einen belegten Erfolg.
    let (readback, verified_after_reconnect) = match complete_parent_listing(vfs, parent) {
        Ok(listing) => (listing, false),
        Err(error) if error_code(&error) == ErrorCode::ConnectionLost => {
            let Some(reclaim) = reclaim else {
                return Err(crate::error::with_recovery_context(
                    coded_error(
                        io::ErrorKind::Other,
                        ErrorCode::OutcomeUnclear,
                        format!(
                            "Der Server bestätigte die Umbenennung, der Rücklese-Beleg verlor aber die Verbindung und es ist keine Zustandsklärung verfügbar: {error}"
                        ),
                    ),
                    &[&error],
                    &[],
                ));
            };
            reclaim().map_err(|reconnect_error| {
                crate::error::with_recovery_context(
                    coded_error(
                        io::ErrorKind::Other,
                        ErrorCode::OutcomeUnclear,
                        format!(
                            "Der Server bestätigte die Umbenennung, die Zustandsklärung nach dem abgebrochenen Readback scheiterte aber: {reconnect_error}"
                        ),
                    ),
                    &[&error, &reconnect_error],
                    &[],
                )
            })?;
            let listing = complete_parent_listing(vfs, parent).map_err(|readback_error| {
                crate::error::with_recovery_context(
                    coded_error(
                        io::ErrorKind::Other,
                        ErrorCode::OutcomeUnclear,
                        format!(
                            "Der Server bestätigte die Umbenennung, auch der klärende Rücklese-Beleg scheiterte aber: {readback_error}"
                        ),
                    ),
                    &[&error, &readback_error],
                    &[],
                )
            })?;
            (listing, true)
        }
        // Eine private Providersicht kann den unklaren Readback bereits samt
        // maschinenlesbaren Recoverypfaden einordnen. Diese Hülle erhalten,
        // statt ihre Pfade nur als Text in einen neuen Fehler einzuschließen.
        Err(error) if error_code(&error) == ErrorCode::OutcomeUnclear => return Err(error),
        Err(error) => {
            return Err(crate::error::with_recovery_context(
                coded_error(
                    io::ErrorKind::Other,
                    ErrorCode::OutcomeUnclear,
                    format!(
                        "Der Server bestätigte die Umbenennung, der Rücklese-Beleg scheiterte aber: {error}"
                    ),
                ),
                &[&error],
                &[],
            ))
        }
    };
    classify_readback(
        &readback,
        &old_name,
        &new_name,
        &new_abs,
        entry_type(&source_entry),
        class,
        limitations,
        verified_after_reconnect,
    )
}

/// Konservative Namensäquivalenz: NFC-Normalform plus Unicode-Kleinschreibung.
/// Sie bildet nach, dass viele Server Groß-/Kleinschreibung oder NFC/NFD
/// gleichsetzen. Exotischere Gleichsetzungen (etwa historische Kurznamen) kann
/// kein Client vorhersehen; dieses Restrisiko bleibt Teil der sichtbaren
/// Best-Effort-Einschränkung.
pub(crate) fn folded_name(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    name.nfc().collect::<String>().to_lowercase()
}

/// True, wenn ein Server die beiden Namen als denselben Eintrag behandeln könnte.
pub(crate) fn names_may_collide(existing: &str, candidate: &str) -> bool {
    existing == candidate || folded_name(existing) == folded_name(candidate)
}

/// Sucht den Zieleintrag im Readback: zuerst byte-exakt, sonst über die
/// konservative Äquivalenz — ein normalisierender Server darf den neuen Namen in
/// seiner eigenen Normalform listen, ohne dass der belegte Erfolg verlorengeht.
/// Ein Äquivalenztreffer kann kein fremder Eintrag sein: Vor der Mutation war
/// jeder äquivalente Name eine sichtbare Kollision. Der unveränderte alte Name
/// selbst ist vom Äquivalenz-Fallback ausgenommen, damit eine gescheiterte reine
/// Groß-/Kleinschreibungs-Korrektur als „unverändert" erkannt wird und nicht als
/// Scheinerfolg.
fn find_new_entry<'a>(
    listing: &'a CompleteDirectoryListing,
    old_name: &str,
    new_name: &str,
) -> Option<&'a ProofListEntry> {
    listing
        .entries
        .iter()
        .find(|item| item.entry.name == new_name)
        .or_else(|| {
            listing.entries.iter().find(|item| {
                item.entry.name != old_name && names_may_collide(&item.entry.name, new_name)
            })
        })
}

/// Holt das Elternlisting und besteht auf dem Vollständigkeitsnachweis.
fn complete_parent_listing(vfs: &dyn Vfs, parent: &Path) -> io::Result<CompleteDirectoryListing> {
    let listing = vfs.list_complete(parent)?;
    if !listing.complete {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Das Elternlisting ist nicht nachweislich vollständig; Umbenennen benötigt den vollständigen Bestand",
        ));
    }
    Ok(listing)
}

/// Ordnet einen frischen Bestand einem eindeutigen Umbenennungsresultat zu.
#[allow(clippy::too_many_arguments)]
fn classify_readback(
    listing: &CompleteDirectoryListing,
    old_name: &str,
    new_name: &str,
    new_abs: &Path,
    source_type: EntryType,
    class: RenameExecutionClass,
    limitations: Vec<String>,
    verified_after_reconnect: bool,
) -> io::Result<RenameReport> {
    let old_present = listing
        .entries
        .iter()
        .any(|item| item.entry.name == old_name);
    let new_entry = find_new_entry(listing, old_name, new_name);
    match (old_present, new_entry) {
        (false, Some(target)) if entry_type(&target.entry) == source_type => Ok(RenameReport {
            outcome: RenameOutcome::Renamed,
            class,
            limitations,
            new_path: Some(new_abs.to_path_buf()),
            existing_is_dir: None,
            verified_after_reconnect,
        }),
        (false, Some(_)) => Err(coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            "Der neue Name existiert, hat aber einen anderen Typ als der umbenannte Eintrag",
        )),
        (true, None) => Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Das frische Listing zeigt den unveränderten Bestand — die Umbenennung wurde nicht ausgeführt",
        )),
        (true, Some(_)) => Err(coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            "Alter und neuer Name existieren gleichzeitig; der Bestand muss geprüft werden",
        )),
        (false, None) => Err(coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            "Weder alter noch neuer Name stehen im frischen Listing; der Bestand muss geprüft werden",
        )),
    }
}

/// Klärt nach einem Verbindungsverlust ausschließlich den Zustand — die Mutation
/// wird unter keinen Umständen wiederholt.
#[allow(clippy::too_many_arguments)]
fn classify_after_connection_loss(
    vfs: &dyn Vfs,
    parent: &Path,
    old_name: &str,
    new_name: &str,
    new_abs: &Path,
    source_type: EntryType,
    class: RenameExecutionClass,
    limitations: Vec<String>,
    reclaim: Option<&dyn Fn() -> io::Result<()>>,
    lost: io::Error,
) -> io::Result<RenameReport> {
    if let Some(reclaim) = reclaim {
        // Höchstens ein identitätsgeprüfter Neuaufbau. Schlägt er fehl oder meldet
        // er eine andere Serveridentität, bleibt der Ausgang ehrlich unklar.
        if let Err(reconnect_error) = reclaim() {
            return Err(crate::error::with_recovery_context(
                coded_error(
                    io::ErrorKind::Other,
                    ErrorCode::OutcomeUnclear,
                    format!(
                        "Verbindung brach während der Umbenennung ab ({lost}); die Zustandsklärung scheiterte: {reconnect_error}"
                    ),
                ),
                &[&lost, &reconnect_error],
                &[],
            ));
        }
    }
    let listing = match complete_parent_listing(vfs, parent) {
        Ok(listing) => listing,
        Err(listing_error) => {
            return Err(crate::error::with_recovery_context(
                coded_error(
                    io::ErrorKind::Other,
                    ErrorCode::OutcomeUnclear,
                    format!(
                        "Verbindung brach während der Umbenennung ab ({lost}); das klärende Listing scheiterte: {listing_error}"
                    ),
                ),
                &[&lost, &listing_error],
                &[],
            ))
        }
    };
    let old_present = listing
        .entries
        .iter()
        .any(|item| item.entry.name == old_name);
    let new_present = find_new_entry(&listing, old_name, new_name).is_some();
    if old_present && !new_present {
        // Der Bestand ist eindeutig unverändert: Die Mutation hat den Server nie
        // erreicht oder wurde abgelehnt. Das ist das Resultat „unverändert".
        return Err(crate::error::with_recovery_context(
            coded_error(
                io::ErrorKind::ConnectionAborted,
                ErrorCode::ConnectionLost,
                format!(
                    "Verbindung brach ab, bevor die Umbenennung wirkte; das frische Listing bestätigt den unveränderten Bestand ({lost})"
                ),
            ),
            &[&lost],
            &[],
        ));
    }
    classify_readback(
        &listing,
        old_name,
        new_name,
        new_abs,
        source_type,
        class,
        limitations,
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::Entry;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn entry(name: &str, is_dir: bool) -> ProofListEntry {
        ProofListEntry {
            entry: Entry {
                name: name.into(),
                is_dir,
                is_file: !is_dir,
                is_symlink: false,
                size: 0,
                mtime: None,
                created: None,
                owner: None,
                group: None,
                extension: None,
                mode: None,
                device: None,
                inode: None,
                hard_links: None,
            },
            raw_name: name.as_bytes().to_vec(),
            safely_decoded: true,
            provider_token: None,
        }
    }

    fn symlink_entry(name: &str) -> ProofListEntry {
        let mut item = entry(name, false);
        item.entry.is_file = false;
        item.entry.is_symlink = true;
        item
    }

    fn listing(names: &[(&str, bool)], complete: bool) -> CompleteDirectoryListing {
        CompleteDirectoryListing {
            entries: names.iter().map(|(name, dir)| entry(name, *dir)).collect(),
            provider_token: None,
            complete,
        }
    }

    /// Skriptbarer Test-Provider: Listings in Aufrufreihenfolge plus ein
    /// vorbestimmtes Mutationsergebnis. Zählt Mutationen, damit die Tests den
    /// Exakt-einmal-Vertrag belegen können.
    struct ScriptedVfs {
        listings: Mutex<Vec<io::Result<CompleteDirectoryListing>>>,
        mutation: Mutex<Option<io::Result<()>>>,
        mutations_executed: AtomicUsize,
        actual_rename: Option<(PathBuf, PathBuf)>,
    }

    impl ScriptedVfs {
        fn new(
            listings: Vec<io::Result<CompleteDirectoryListing>>,
            mutation: io::Result<()>,
        ) -> Self {
            Self {
                listings: Mutex::new(listings),
                mutation: Mutex::new(Some(mutation)),
                mutations_executed: AtomicUsize::new(0),
                actual_rename: None,
            }
        }

        fn with_actual_rename(mut self, from: &Path, to: &Path) -> Self {
            self.actual_rename = Some((from.to_owned(), to.to_owned()));
            self
        }

        fn mutation_count(&self) -> usize {
            self.mutations_executed.load(Ordering::SeqCst)
        }

        fn run_mutation(&self) -> io::Result<()> {
            self.mutations_executed.fetch_add(1, Ordering::SeqCst);
            if let Some((from, to)) = &self.actual_rename {
                std::fs::rename(from, to)?;
            }
            self.mutation
                .lock()
                .unwrap()
                .take()
                .expect("Mutation darf höchstens einmal laufen")
        }
    }

    impl Vfs for ScriptedVfs {
        fn list(&self, _path: &Path) -> io::Result<Vec<Entry>> {
            unreachable!("Tests verwenden ausschließlich list_complete")
        }

        fn list_complete(&self, _path: &Path) -> io::Result<CompleteDirectoryListing> {
            self.listings.lock().unwrap().remove(0)
        }

        fn stat(&self, _path: &Path) -> io::Result<Entry> {
            unreachable!()
        }

        fn exists(&self, _path: &Path) -> bool {
            false
        }

        fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn create_dir(&self, _path: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn open_read(&self, _path: &Path) -> io::Result<Box<dyn Read + Send>> {
            unreachable!()
        }

        fn open_write(&self, _path: &Path) -> io::Result<Box<dyn Write + Send>> {
            unreachable!()
        }

        fn open_write_new(&self, _path: &Path) -> io::Result<Box<dyn Write + Send>> {
            unreachable!()
        }

        fn sync_file(&self, _path: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn sync_dir(&self, _path: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn set_mtime(&self, _path: &Path, _mtime: i64) -> io::Result<()> {
            unreachable!()
        }

        fn set_mode(&self, _path: &Path, _mode: u32) -> io::Result<()> {
            unreachable!()
        }

        fn remove_file(&self, _path: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn remove_dir_all(&self, _path: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn rename_noreplace(&self, _from: &Path, _to: &Path) -> io::Result<()> {
            self.run_mutation()
        }

        fn rename_plain_once(&self, _from: &Path, _to: &Path) -> io::Result<()> {
            self.run_mutation()
        }
    }

    fn connection_lost() -> io::Error {
        io::Error::new(io::ErrorKind::UnexpectedEof, "Antwort ging verloren")
    }

    #[test]
    fn readbackfehler_erhaelt_pruefpfade_nach_echter_umbenennung() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("alt.txt");
        let target = root.path().join("neu.txt");
        let retained = root.path().join("vorhandener-pruefpfad.txt");
        std::fs::write(&source, b"echt umbenannte Bytes").unwrap();
        std::fs::write(&retained, b"weiter pruefen").unwrap();
        let readback_error = crate::error::with_recovery_paths(
            coded_error(
                io::ErrorKind::PermissionDenied,
                ErrorCode::PermissionDenied,
                "Listing verweigert",
            ),
            &[retained.to_string_lossy().into_owned()],
        );
        let vfs = ScriptedVfs::new(
            vec![
                Ok(listing(&[("alt.txt", false)], true)),
                Err(readback_error),
            ],
            Ok(()),
        )
        .with_actual_rename(&source, &target);
        let error = rename_with_class(
            &vfs,
            &source,
            "neu.txt",
            &None,
            RenameExecutionClass::Assured,
            Vec::new(),
            None,
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(vfs.mutation_count(), 1);
        assert!(!source.exists());
        assert_eq!(std::fs::read(target).unwrap(), b"echt umbenannte Bytes");
        assert_eq!(std::fs::read(&retained).unwrap(), b"weiter pruefen");
        assert!(
            crate::error::recovery_paths(&error).contains(&retained.to_string_lossy().into_owned()),
            "Nach bestätigtem Rename ging der belegte Prüfpfad aus dem Readbackfehler verloren"
        );
    }

    #[test]
    fn kollision_im_frischen_listing_verhindert_jede_mutation() {
        let vfs = ScriptedVfs::new(
            vec![Ok(listing(&[("alt.txt", false), ("neu.txt", false)], true))],
            Ok(()),
        );
        let report = rename_with_class(
            &vfs,
            Path::new("/pane/alt.txt"),
            "neu.txt",
            &None,
            RenameExecutionClass::BestEffort,
            Vec::new(),
            None,
        )
        .unwrap();
        assert_eq!(report.outcome, RenameOutcome::Collision);
        assert_eq!(report.existing_is_dir, Some(false));
        assert_eq!(vfs.mutation_count(), 0);
    }

    #[test]
    fn erfolgreiches_best_effort_rename_wird_rueckgelesen() {
        let vfs = ScriptedVfs::new(
            vec![
                Ok(listing(&[("alt.txt", false)], true)),
                Ok(listing(&[("neu.txt", false)], true)),
            ],
            Ok(()),
        );
        let report = rename_with_class(
            &vfs,
            Path::new("/pane/alt.txt"),
            "neu.txt",
            &None,
            RenameExecutionClass::BestEffort,
            vec!["Testeinschränkung".into()],
            None,
        )
        .unwrap();
        assert_eq!(report.outcome, RenameOutcome::Renamed);
        assert_eq!(report.new_path.as_deref(), Some(Path::new("/pane/neu.txt")));
        assert!(!report.verified_after_reconnect);
        assert_eq!(vfs.mutation_count(), 1);
    }

    #[test]
    fn readback_verwechselt_regulaere_datei_nicht_mit_symlink() {
        let vfs = ScriptedVfs::new(
            vec![
                Ok(listing(&[("alt.txt", false)], true)),
                Ok(CompleteDirectoryListing {
                    entries: vec![symlink_entry("neu.txt")],
                    provider_token: None,
                    complete: true,
                }),
            ],
            Ok(()),
        );

        let error = rename_with_class(
            &vfs,
            Path::new("/pane/alt.txt"),
            "neu.txt",
            &None,
            RenameExecutionClass::Assured,
            Vec::new(),
            None,
        )
        .expect_err("Der Rename-Readback muss den vollständigen Eintragstyp binden");

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
    }

    #[test]
    fn verbindungsverlust_im_readback_wird_einmal_lesend_geklaert() {
        let vfs = ScriptedVfs::new(
            vec![
                Ok(listing(&[("alt.txt", false)], true)),
                Err(connection_lost()),
                Ok(listing(&[("neu.txt", false)], true)),
            ],
            Ok(()),
        );
        let reconnects = AtomicUsize::new(0);
        let reclaim = || -> io::Result<()> {
            reconnects.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        let report = rename_with_class(
            &vfs,
            Path::new("/pane/alt.txt"),
            "neu.txt",
            &None,
            RenameExecutionClass::Assured,
            Vec::new(),
            Some(&reclaim),
        )
        .expect("Nach erfolgreicher Mutation darf ein identischer Lesereconnect klären");

        assert_eq!(report.outcome, RenameOutcome::Renamed);
        assert!(report.verified_after_reconnect);
        assert_eq!(vfs.mutation_count(), 1);
        assert_eq!(reconnects.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unvollstaendiges_listing_verhindert_die_ausfuehrung() {
        let vfs = ScriptedVfs::new(vec![Ok(listing(&[("alt.txt", false)], false))], Ok(()));
        let error = rename_with_class(
            &vfs,
            Path::new("/pane/alt.txt"),
            "neu.txt",
            &None,
            RenameExecutionClass::BestEffort,
            Vec::new(),
            None,
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::InvalidData);
        assert_eq!(vfs.mutation_count(), 0);
    }

    #[test]
    fn servermeldung_ohne_wirkung_ist_als_unveraendert_belegt() {
        let vfs = ScriptedVfs::new(
            vec![
                Ok(listing(&[("alt.txt", false)], true)),
                Ok(listing(&[("alt.txt", false)], true)),
            ],
            Ok(()),
        );
        let error = rename_with_class(
            &vfs,
            Path::new("/pane/alt.txt"),
            "neu.txt",
            &None,
            RenameExecutionClass::BestEffort,
            Vec::new(),
            None,
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::InvalidData);
        assert_eq!(vfs.mutation_count(), 1);
    }

    #[test]
    fn verbindungsverlust_mit_belegtem_erfolg_liefert_renamed() {
        let vfs = ScriptedVfs::new(
            vec![
                Ok(listing(&[("alt.txt", false)], true)),
                Ok(listing(&[("neu.txt", false)], true)),
            ],
            Err(connection_lost()),
        );
        let reconnects = AtomicUsize::new(0);
        let reclaim = || -> io::Result<()> {
            reconnects.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let report = rename_with_class(
            &vfs,
            Path::new("/pane/alt.txt"),
            "neu.txt",
            &None,
            RenameExecutionClass::BestEffort,
            Vec::new(),
            Some(&reclaim),
        )
        .unwrap();
        assert_eq!(report.outcome, RenameOutcome::Renamed);
        assert!(report.verified_after_reconnect);
        assert_eq!(vfs.mutation_count(), 1, "keine blinde Wiederholung");
        assert_eq!(reconnects.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn verbindungsverlust_mit_unveraendertem_bestand_meldet_connection_lost() {
        let vfs = ScriptedVfs::new(
            vec![
                Ok(listing(&[("alt.txt", false)], true)),
                Ok(listing(&[("alt.txt", false)], true)),
            ],
            Err(connection_lost()),
        );
        let error = rename_with_class(
            &vfs,
            Path::new("/pane/alt.txt"),
            "neu.txt",
            &None,
            RenameExecutionClass::BestEffort,
            Vec::new(),
            Some(&(|| Ok(())) as &dyn Fn() -> io::Result<()>),
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::ConnectionLost);
        assert_eq!(vfs.mutation_count(), 1);
    }

    #[test]
    fn scheiternde_zustandsklaerung_meldet_outcome_unclear() {
        let vfs = ScriptedVfs::new(
            vec![
                Ok(listing(&[("alt.txt", false)], true)),
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "immer noch weg",
                )),
            ],
            Err(connection_lost()),
        );
        let error = rename_with_class(
            &vfs,
            Path::new("/pane/alt.txt"),
            "neu.txt",
            &None,
            RenameExecutionClass::BestEffort,
            Vec::new(),
            Some(&(|| Ok(())) as &dyn Fn() -> io::Result<()>),
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(vfs.mutation_count(), 1);
    }

    #[test]
    fn doppelte_namen_nach_verbindungsverlust_bleiben_unklar() {
        let vfs = ScriptedVfs::new(
            vec![
                Ok(listing(&[("alt.txt", false)], true)),
                Ok(listing(&[("alt.txt", false), ("neu.txt", false)], true)),
            ],
            Err(connection_lost()),
        );
        let error = rename_with_class(
            &vfs,
            Path::new("/pane/alt.txt"),
            "neu.txt",
            &None,
            RenameExecutionClass::BestEffort,
            Vec::new(),
            Some(&(|| Ok(())) as &dyn Fn() -> io::Result<()>),
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
    }

    #[test]
    fn spaete_serverkollision_wird_als_kollision_gemeldet() {
        let vfs = ScriptedVfs::new(
            vec![Ok(listing(&[("alt.txt", false)], true))],
            Err(io::Error::new(io::ErrorKind::AlreadyExists, "belegt")),
        );
        let report = rename_with_class(
            &vfs,
            Path::new("/pane/alt.txt"),
            "neu.txt",
            &None,
            RenameExecutionClass::Assured,
            Vec::new(),
            None,
        )
        .unwrap();
        assert_eq!(report.outcome, RenameOutcome::Collision);
        assert_eq!(report.existing_is_dir, None);
        assert_eq!(vfs.mutation_count(), 1);
    }

    #[test]
    fn casefold_kollision_verhindert_die_mutation() {
        // Der Server könnte „NEU.TXT" und „neu.txt" gleichsetzen; ein
        // überschreibendes RNTO würde dann still ersetzen. Konservativ: Kollision.
        let vfs = ScriptedVfs::new(
            vec![Ok(listing(&[("alt.txt", false), ("NEU.TXT", false)], true))],
            Ok(()),
        );
        let report = rename_with_class(
            &vfs,
            Path::new("/pane/alt.txt"),
            "neu.txt",
            &None,
            RenameExecutionClass::BestEffort,
            Vec::new(),
            None,
        )
        .unwrap();
        assert_eq!(report.outcome, RenameOutcome::Collision);
        assert_eq!(vfs.mutation_count(), 0);
    }

    #[test]
    fn unicode_normalform_kollision_verhindert_die_mutation() {
        // NFD („u" plus kombinierender Trema) gegen NFC („ü"): Auf einem
        // normalisierenden Server ist das derselbe Name.
        let vfs = ScriptedVfs::new(
            vec![Ok(listing(
                &[("alt.txt", false), ("u\u{0308}bung", false)],
                true,
            ))],
            Ok(()),
        );
        let report = rename_with_class(
            &vfs,
            Path::new("/pane/alt.txt"),
            "\u{00fc}bung",
            &None,
            RenameExecutionClass::BestEffort,
            Vec::new(),
            None,
        )
        .unwrap();
        assert_eq!(report.outcome, RenameOutcome::Collision);
        assert_eq!(vfs.mutation_count(), 0);
    }

    #[test]
    fn reine_gross_kleinschreibungs_korrektur_bleibt_moeglich() {
        // Der eigene Eintrag zählt nicht als Kollision: „alt.txt" → „ALT.TXT".
        let vfs = ScriptedVfs::new(
            vec![
                Ok(listing(&[("alt.txt", false)], true)),
                Ok(listing(&[("ALT.TXT", false)], true)),
            ],
            Ok(()),
        );
        let report = rename_with_class(
            &vfs,
            Path::new("/pane/alt.txt"),
            "ALT.TXT",
            &None,
            RenameExecutionClass::BestEffort,
            Vec::new(),
            None,
        )
        .unwrap();
        assert_eq!(report.outcome, RenameOutcome::Renamed);
        assert_eq!(vfs.mutation_count(), 1);
    }

    #[test]
    fn gescheiterte_gross_kleinschreibungs_korrektur_ist_unveraendert() {
        // Der Server meldet Erfolg, listet aber weiter nur den alten Namen: Der
        // Äquivalenz-Fallback darf den alten Eintrag nicht als Erfolg werten.
        let vfs = ScriptedVfs::new(
            vec![
                Ok(listing(&[("alt.txt", false)], true)),
                Ok(listing(&[("alt.txt", false)], true)),
            ],
            Ok(()),
        );
        let error = rename_with_class(
            &vfs,
            Path::new("/pane/alt.txt"),
            "ALT.TXT",
            &None,
            RenameExecutionClass::BestEffort,
            Vec::new(),
            None,
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::InvalidData);
    }

    #[test]
    fn unsicher_dekodierbarer_name_sperrt_nur_best_effort() {
        let unsafe_listing = || {
            let mut entries = listing(&[("alt.txt", false)], true);
            let mut raw = entry("kaputt", false);
            raw.safely_decoded = false;
            raw.raw_name = vec![0xff, 0xfe];
            entries.entries.push(raw);
            entries
        };
        // Best Effort: Die Kollisionsprüfung ist nicht ehrlich belegbar → Absage.
        let vfs = ScriptedVfs::new(vec![Ok(unsafe_listing())], Ok(()));
        let error = rename_with_class(
            &vfs,
            Path::new("/pane/alt.txt"),
            "neu.txt",
            &None,
            RenameExecutionClass::BestEffort,
            Vec::new(),
            None,
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::InvalidData);
        assert_eq!(vfs.mutation_count(), 0);
        // Abgesichert: Das atomare No-Replace trägt die Kollisionsgarantie selbst;
        // der unabhängige Eintrag blockiert die Umbenennung nicht.
        let assured = ScriptedVfs::new(
            vec![
                Ok(unsafe_listing()),
                Ok({
                    let mut entries = listing(&[("neu.txt", false)], true);
                    let mut raw = entry("kaputt", false);
                    raw.safely_decoded = false;
                    entries.entries.push(raw);
                    entries
                }),
            ],
            Ok(()),
        );
        let report = rename_with_class(
            &assured,
            Path::new("/pane/alt.txt"),
            "neu.txt",
            &None,
            RenameExecutionClass::Assured,
            Vec::new(),
            None,
        )
        .unwrap();
        assert_eq!(report.outcome, RenameOutcome::Renamed);
    }

    #[test]
    fn gleicher_name_wird_vor_jeder_mutation_abgelehnt() {
        let vfs = ScriptedVfs::new(Vec::new(), Ok(()));
        let error = rename_with_class(
            &vfs,
            Path::new("/pane/alt.txt"),
            "alt.txt",
            &None,
            RenameExecutionClass::Assured,
            Vec::new(),
            None,
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::InvalidInput);
        assert_eq!(vfs.mutation_count(), 0);
    }
}
