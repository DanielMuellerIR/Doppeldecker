//! Dateisystem-Abstraktion (Virtual File System).
//!
//! Sync und Dateioperationen verwenden denselben Vertrag für lokale und entfernte
//! Adapter. Fähigkeiten unterscheiden lesende Operationen, interaktive Mutationen
//! und stärkere Sync-/Recoverygarantien. Nicht implementierte optionale Wirkungen
//! bleiben ausdrücklich Unsupported; das Frontend erfindet keine Zusagen.

use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};

use crate::metadata::{ForkKind, MetadataCapabilities, MetadataManifest, XattrValue};

/// Seek-fähiger Quellstrom für Sparse-Dateien. Der Sync springt damit über Löcher,
/// statt sie als lange Folgen physischer Nullbytes zu lesen.
pub trait SparseRead: Read + Seek + Send {}

impl<T: Read + Seek + Send> SparseRead for T {}

/// Seek-fähiger Zielstrom für Sparse-Dateien. `set_len` legt die logische Dateigröße
/// fest, ohne die nicht beschriebenen Bereiche physisch zu allokieren.
pub trait SparseWrite: Write + Seek + Send {
    fn set_len(&mut self, size: u64) -> io::Result<()>;
    /// Einen bestätigten Lochbereich explizit deallokieren. Nur seek-basiertes
    /// Überspringen reicht etwa auf APFS nicht aus, um ein echtes Loch zu garantieren.
    fn punch_hole(&mut self, offset: u64, length: u64) -> io::Result<()>;
}

impl SparseWrite for std::fs::File {
    fn set_len(&mut self, size: u64) -> io::Result<()> {
        std::fs::File::set_len(self, size)
    }

    fn punch_hole(&mut self, offset: u64, length: u64) -> io::Result<()> {
        if length == 0 {
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        {
            rustix::fs::fallocate(
                self,
                rustix::fs::FallocateFlags::PUNCH_HOLE | rustix::fs::FallocateFlags::KEEP_SIZE,
                offset,
                length,
            )?;
            return Ok(());
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;
            let offset = i64::try_from(offset).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "Sparse-Offset ist zu groß")
            })?;
            let length = i64::try_from(length).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "Sparse-Länge ist zu groß")
            })?;
            let mut hole = libc::fpunchhole_t {
                fp_flags: 0,
                reserved: 0,
                fp_offset: offset,
                fp_length: length,
            };
            let result = unsafe { libc::fcntl(self.as_raw_fd(), libc::F_PUNCHHOLE, &mut hole) };
            if result == -1 {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
        #[allow(unreachable_code)]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Plattform unterstützt kein exaktes Sparse-Punching",
        ))
    }
}

/// Öffentlicher Wirkungsbeleg einer begrenzten interaktiven Inhaltsauswahl. Der Digest
/// bindet beobachtete Namen/Attribute, keine atomare Namespace- oder Syncgarantie.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InteractiveSourceTree {
    pub digest: String,
    pub entries: u64,
    pub files: u64,
    pub directories: u64,
    pub bytes: u64,
    pub omitted: Vec<SourceReadOmission>,
}

impl InteractiveSourceTree {
    pub fn limitations(&self) -> Vec<String> {
        std::iter::once(format!("Begrenzter Quellbaum: {} Datei(en), {} Ordner, {} Byte; {} ausgelassene(r) Eintrag/Teilbaum. Höchstens 1000 Einträge, 16 Unterordnerebenen und 16 GiB Gesamtinhalt; Vorbereitung höchstens 60 Sekunden zuzüglich laufendem Request/Cleanup.", self.files, self.directories, self.bytes, self.omitted.len()))
            .chain(self.omitted.iter().map(|item| format!("Nicht übertragen: {} — {}", item.path, item.reason)))
            .collect()
    }

    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.digest.len() != 64
            || !self.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            || self.entries == 0
            || self.entries > 1000
            || (self.directories == 0 && self.omitted.is_empty())
            || self.directories > self.entries
            || self.files > self.entries
            || self.omitted.len() > 1000
            || self.files + self.directories + self.omitted.len() as u64 != self.entries
            || self.bytes > 16 * 1024 * 1024 * 1024
            || self.omitted.iter().any(|item| {
                item.path.len() > 4096 || item.reason.is_empty() || item.reason.len() > 8192
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Ungültiger begrenzter interaktiver Baumbeleg",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourceReadOmission {
    pub path: String,
    pub reason: String,
}

/// Ein Eintrag (Datei, Ordner oder Symlink) in einem Verzeichnis.
///
/// `mtime` ist die Änderungszeit als **Unix-Sekunden** (statt `SystemTime`), weil das
/// protokollübergreifend am einfachsten zu vergleichen und zu übertragen ist.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Entry {
    /// Reiner Name ohne Pfad (z. B. `"foto.jpg"`).
    pub name: String,
    /// Ist es ein Verzeichnis? (Bei Symlinks: zeigt das Ziel auf ein Verzeichnis?)
    pub is_dir: bool,
    /// Ist es eine reguläre Datei? Sockets, FIFOs und Geräte sind bewusst weder Datei
    /// noch Ordner und werden vom Sync ausgelassen.
    pub is_file: bool,
    /// Ist der Eintrag selbst ein symbolischer Link? Sync überspringt Symlinks in v0.1.
    pub is_symlink: bool,
    /// Größe in Bytes (bei Verzeichnissen 0).
    pub size: u64,
    /// Änderungszeit in Unix-Sekunden, falls ermittelbar.
    pub mtime: Option<i64>,
    /// Erstellungszeit (Birth-Time) in Unix-Sekunden. Lokale Dateisysteme sowie
    /// Provider mit einem echten Erstellungszeitfeld liefern den Wert; `ctime`
    /// wird ausdrücklich nicht als Ersatz verwendet.
    pub created: Option<i64>,
    /// Anzeigename des Eigentümers und der Gruppe. Lokal werden UID/GID über die
    /// Systemdatenbank aufgelöst und fallen notfalls auf die exakte Zahl zurück.
    /// Provider ohne belastbare Unix-Identität liefern `None` statt erfundener Werte.
    pub owner: Option<String>,
    pub group: Option<String>,
    /// Sichere letzte Dateiendung ohne Punkt. Verzeichnisse, Spezialdateien und
    /// Namen ohne eindeutige Endung liefern `None`.
    pub extension: Option<String>,
    /// Normale POSIX-`rwx`-Rechtebits (`0o777`, einschließlich Executable-Bit).
    /// Der portable Pfad überträgt nur diese Bits; das native Profil bindet Owner und
    /// Sonderbits separat über das Manifest. Remote-Provider dürfen `None` liefern,
    /// wenn sie keine verlässliche Unix-Semantik haben.
    pub mode: Option<u32>,
    /// Lokale Identität für die spätere laufbezogene Hardlink-Gruppierung. Remote-
    /// Provider dürfen `None` liefern, wenn sie keine stabilen IDs anbieten.
    pub device: Option<u64>,
    pub inode: Option<u64>,
    pub hard_links: Option<u64>,
}

/// Letzte Dateiendung für die Darstellungs-Spalte. Die Funktion ist bewusst im
/// Core zentralisiert, damit lokale und Remote-Listings dieselbe Semantik nutzen.
pub(crate) fn entry_extension(name: &str, is_file: bool) -> Option<String> {
    if !is_file {
        return None;
    }
    Path::new(name)
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Kurzlebige Identität eines lokalen Mounts. Sie ist absichtlich nur für einen
/// vorbereiteten Lauf gültig und kein über Neustarts hinweg gespeicherter
/// Datenträgername. Eine persistierte Erwartung muss frisch verglichen werden;
/// nach Remount oder Neustart darf sie erneut Zustimmung verlangen.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LocalMountIdentity {
    /// Linux-Mount-ID beziehungsweise ein aus der macOS-FSID abgeleiteter Laufwert.
    pub mount_id: u64,
    /// Linux kennzeichnet, ob `STATX_MNT_ID_UNIQUE` statt der wiederverwendbaren ID
    /// geliefert wurde. macOS verwendet zusätzlich `fsid` und `volume_id`.
    pub unique_mount_id: bool,
    pub fsid: Vec<u8>,
    pub volume_id: Option<[u8; 16]>,
    pub fs_type: String,
    pub mount_flags: u64,
}

/// Descriptor-gebundene Identität einer konkreten lokalen Operationswurzel.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LocalRootIdentity {
    pub device: u64,
    pub inode: u64,
    pub is_directory: bool,
    pub mount: LocalMountIdentity,
    /// Nur für die Anzeige; Sicherheitsvergleiche verwenden die Felder darüber.
    pub display_path: PathBuf,
}

/// Ein Eintrag einer nachweislich abgeschlossenen Verzeichnisauflistung. Der rohe Name
/// verhindert, dass zwei verschiedene nicht-UTF-8-Namen durch verlustbehaftete Anzeige
/// irrtümlich zusammenfallen.
#[derive(Clone, Debug)]
pub struct ProofListEntry {
    pub entry: Entry,
    pub raw_name: Vec<u8>,
    pub safely_decoded: bool,
    /// Providergebundener Versionsbeleg des Eintrags, etwa eine starke WebDAV-ETag.
    pub provider_token: Option<Vec<u8>>,
}

/// Vollständige, nicht persistierbare Bindung für einen Quarantäne-Claim über
/// zwei Elternordner. Eine Request-Struktur hält die Trait-Signatur lesbar und
/// verhindert, dass Quell- und Zielbelege beim Aufruf vertauscht werden.
pub struct BoundParentsRenameRequest<'a> {
    pub from: &'a Path,
    pub to: &'a Path,
    pub expected: &'a ProofListEntry,
    pub from_parent: &'a Path,
    pub expected_from_parent: &'a crate::real_mode::RootIdentity,
    pub to_parent: &'a Path,
    pub expected_to_parent: &'a crate::real_mode::RootIdentity,
    /// Zusätzlicher unveränderlicher Inhaltsbeleg, der in derselben
    /// Providerwirkung geprüft werden muss. Quarantäne-Claims binden damit den
    /// unmittelbar zuvor geschriebenen Manifest-Intent; Restore-Proben benötigen
    /// keinen zusätzlichen Inhalt und übergeben `None`.
    pub required_content: Option<BoundContentDigest<'a>>,
}

#[derive(Clone, Copy)]
pub struct BoundContentDigest<'a> {
    pub path: &'a Path,
    pub digest: [u8; 32],
}

/// Lokaler, descriptor-gebundener Restore aus einem bereits zufällig geclaimten
/// Quarantänelauf. Der erwartete Laufroot stammt aus demselben Readback, der zuvor
/// gegen das Manifest geprüft wurde; die Implementierung öffnet ihn erneut und
/// vergleicht die Identität am gehaltenen Handle, bevor sie Kindnamen verwendet.
pub struct BoundQuarantineRestoreRequest<'a> {
    pub run_root: &'a Path,
    pub expected_run_root: &'a crate::real_mode::RootIdentity,
    pub source: &'a Path,
    pub target_root: &'a Path,
    pub expected_target_root: &'a crate::real_mode::RootIdentity,
    pub target: &'a Path,
    pub expected_source: &'a ProofListEntry,
    /// Manifest, das den exakten Zielpfad und die stabile Quellidentität bereits vor
    /// dem Rename festhält. Die lokale Wirkung hält die Datei geöffnet und prüft
    /// Identität sowie Inhalt nach dem Rename erneut.
    pub intent_manifest: &'a Path,
    pub expected_intent_manifest: &'a ProofListEntry,
    pub expected_intent_digest: [u8; 32],
}

/// Descriptor-gebundenes Leeren eines Quarantänelaufs mit dauerhaft sichtbarem
/// Purge-Intent. Die Implementierung muss Lauf und Intent vor der ersten Entfernung
/// an die erwarteten Identitäten binden und den Intent nach der Wirkung erneut lesen.
pub struct BoundQuarantinePurgeRequest<'a> {
    pub run_root: &'a Path,
    pub expected_run_root: &'a crate::real_mode::RootIdentity,
    pub intent: &'a Path,
    pub expected_intent: &'a ProofListEntry,
    pub expected_intent_digest: [u8; 32],
}

/// Inhaltsgebundener Austausch einer kleinen Sidecar-Datei. Der erwartete Digest
/// wird aus den aktuell sichtbaren Bytes gebildet; `replacement` ersetzt diese
/// Bytes nur in derselben atomaren Providerwirkung.
pub struct ConditionalContentReplaceRequest<'a> {
    pub path: &'a Path,
    pub expected_digest: [u8; 32],
    pub replacement: &'a [u8],
}

/// Providerantwort für genau ein vollständig abgeschlossenes Verzeichnis. Remote-
/// Provider müssen Pagination und Tokens später ausdrücklich implementieren; der
/// Default gilt deshalb bewusst als unvollständig.
#[derive(Clone, Debug)]
pub struct CompleteDirectoryListing {
    pub entries: Vec<ProofListEntry>,
    pub provider_token: Option<Vec<u8>>,
    pub complete: bool,
}

/// Fehlerphase einer Provideraktion, die bei Erfolg optional einen späteren
/// Entfernungsbeleg liefert.
///
/// Ein gewöhnliches `io::Result` reicht hier nicht: Manche Server können die
/// Mutation ausführen und erst danach die Antwort verlieren. Nur `Unchanged`
/// erlaubt dem Aufrufer, so zu tun, als sei nichts angelegt oder verschoben worden.
#[derive(Debug)]
pub enum MutationFailure {
    /// Der Provider garantiert, dass die fehlgeschlagene Aktion nichts verändert hat.
    Unchanged(io::Error),
    /// Die Aktion kann bereits gewirkt haben; der sichtbare Pfad ist Recoverybestand.
    OutcomeUnclear(io::Error),
}

impl MutationFailure {
    pub fn into_error(self) -> io::Error {
        match self {
            Self::Unchanged(error) | Self::OutcomeUnclear(error) => error,
        }
    }
}

/// Opaquer Providerbeleg für die spätere Entfernung einer Move-Quelle.
///
/// Für eine einzelne Datei bindet der Token Inhalt und Metadaten an die bedingte
/// Entfernung. `recursive` beschreibt lediglich eine mögliche stärkere
/// Providerfähigkeit; der interaktive Move-Pfad verwendet sie nicht für ein
/// rekursives Root-Delete, sondern baut Bäume manifestgebunden Blatt für Blatt ab.
/// Jede Änderung bis zur bedingten Wirkung muss den Beleg ungültig machen.
#[derive(Clone, Debug)]
pub struct MoveSourceRemovalProof {
    pub provider_token: Vec<u8>,
    pub content_digest: [u8; 32],
    pub recursive: bool,
}

/// Wie ein Provider eine vollständig geschriebene Temp-Datei an ihren endgültigen
/// Namen hängt. Die Klasse stammt aus der konkreten Sitzung beziehungsweise
/// lokalen Volumeabfrage. Vorschau und Ausführung nutzen denselben Vertrag; eine
/// schwächere tatsächliche Klasse muss die Queue vor jeder Wirkung frisch binden.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileCommitClass {
    /// Atomarer No-Replace-Commit ist bewiesen; eine späte Kollision kann kein
    /// vorhandenes Ziel ersetzen.
    Assured,
    /// Mindestens eine stärkere Garantie fehlt: atomarer No-Replace-Commit oder
    /// dauerhaftes Datei-Flush. Der Commit prüft den frischen Elternbestand, führt
    /// genau ein Rename aus und liest das Ergebnis zurück; die Provider-Vorschau
    /// benennt die konkret fehlende Garantie.
    BestEffort,
}

pub const LOCAL_FILE_COMMIT_LIMITATION: &str = "Neue Dateien laufen auf diesem Volume als Best Effort: Es bietet kein atomar kollisionsfreies Rename. Doppeldecker prüft den vollständigen Zielbestand unmittelbar vor dem Commit und liest den Inhalt danach zurück. Ein gleichzeitig neu angelegtes Ziel kann im kurzen Fenster dazwischen ersetzt werden. Nicht darstellbare POSIX-Rechte werden nicht übertragen. Bei unklarem Ausgang erfolgt kein automatischer Wiederholversuch.";

/// Wie eine bereits vollständig geschriebene Temp-Datei eine ausdrücklich
/// bestätigte vorhandene Zieldatei ersetzt. Die Klasse ist getrennt vom Commit
/// neuer Namen: Ein Provider kann No-Replace sicher beherrschen, aber keine
/// wiederherstellbare Altversion anbieten.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileReplaceClass {
    /// Das Ziel wird über den normalen atomaren Replace-/Quarantänevertrag ersetzt.
    Assured,
    /// Genau die bestätigte Zieldatei darf ohne Wiederherstellungsweg ersetzt werden;
    /// frischer Zielcheck, genau eine Mutation und Readback liegen beim Aufrufer.
    BestEffort,
}

/// Abstraktion über ein Dateisystem — lokal oder (später) remote.
///
/// Kopiert wird nie „intern" im Provider, sondern generisch über [`Vfs::open_read`] /
/// [`Vfs::open_write`]: die Sync-Engine pumpt die Bytes von der Quelle zum Ziel. So
/// funktioniert auch ein protokollübergreifender Transfer (z. B. lokal → SFTP), ohne
/// dass ein Provider den anderen kennen muss.
pub trait Vfs: Send + Sync {
    /// Optionaler Abschlussbeleg einer gehaltenen Quelle vor dem Zielcommit.
    fn verify_transferred_source(&self, _path: &Path, _hash: &blake3::Hash) -> io::Result<()> {
        Ok(())
    }

    /// Diese private Zielsicht erlaubt additive Inhalte, weder Overwrite noch Move.
    fn additive_content_target_only(&self) -> bool {
        false
    }

    /// Pfadgenaue Metadatenauslassungen des aktuellen Dateikontexts.
    fn interactive_write_limitations(&self) -> Vec<String> {
        Vec::new()
    }

    /// Diese private Lesesicht darf ausschließlich neue lokale Inhalte speisen.
    fn interactive_additive_only(&self) -> bool {
        false
    }
    /// Optionaler lokaler Laufkontext. Remote-Provider behalten ihren Vertrag.
    /// Der Kontext hält Ziel-Elternordner und privaten Staging-Namensraum getrennt.
    fn staging_scope(
        &self,
        _target: &Path,
        _sandbox: Option<&Path>,
    ) -> io::Result<Option<Box<dyn Vfs>>> {
        Ok(None)
    }

    fn local_parent_inheritance(
        &self,
        _path: &Path,
    ) -> io::Result<Option<crate::metadata::LocalParentInheritance>> {
        Ok(None)
    }

    fn staging_recovery_path(&self) -> Option<&Path> {
        None
    }

    fn finish_staging(&self) -> io::Result<()> {
        Ok(())
    }

    fn has_private_staging(&self) -> bool {
        false
    }

    fn cleanup_staged_backup(&self, _path: &Path, _proof: &ProofListEntry) -> io::Result<bool> {
        Ok(false)
    }

    fn staged_backup_is_retained(&self, _path: &Path, _proof: &ProofListEntry) -> io::Result<bool> {
        Ok(false)
    }

    /// Enger Rückweg aus einem privaten lokalen Staging. Kein allgemeines
    /// mutationsfreies Compare-and-Rename-Versprechen für sichtbare Namen.
    fn restore_staged_backup(
        &self,
        _from: &Path,
        _to: &Path,
        _proof: &ProofListEntry,
    ) -> Result<bool, MutationFailure> {
        Ok(false)
    }

    fn staging_candidate(&self, target: &Path, role: &str) -> io::Result<PathBuf> {
        crate::atomic_file::sibling_candidate(target, role)
    }

    /// Ein privater Kontext darf ausschließlich seine eigenen Namen entfernen:
    /// lokal mit gehaltenem Namensraum, remote mit Anlagebeleg und bedingtem
    /// Readback. false verlangt Rest-Erhaltung, kein pfadbasiertes Remove.
    fn cleanup_owned_temp(&self, _path: &Path) -> io::Result<bool> {
        Ok(false)
    }

    /// Nur lokale Implementierungen unterliegen der optionalen lokalen Test-Sandbox.
    /// Remote-Pfade sind virtuelle absolute Pfade und dürfen niemals versehentlich
    /// gegen das Dateisystem des App-Rechners kanonisiert werden.
    fn is_local(&self) -> bool {
        false
    }

    /// Digest der aktuell verhaltensgeprüften Schreibfähigkeiten. Lokale Garantien
    /// sind durch Mount/Handles gebunden und benötigen keinen Zusatzdigest; Remote-
    /// Provider überschreiben dies, damit ein Capability-Reset das Permit sperrt.
    fn safety_capabilities_digest(&self) -> io::Result<Option<[u8; 32]>> {
        Ok(None)
    }

    /// Bezeichnet ein Name im Listing genau ein Objekt — byteweise, ohne dass der
    /// Provider Groß-/Kleinschreibung oder Unicode-Normalformen zusammenfasst?
    ///
    /// Nur dann darf ein byte-exakter Treffer als eindeutiger Beleg gelten. Der
    /// lokale Provider liefert den exakten Rohnamen des Dateisystems und antwortet
    /// deshalb mit `true`; auf ext4 sind `datei.txt` und `Datei.txt` zwei
    /// verschiedene Einträge, und eine Kopie mit „Ersetzen" muss dort neben dem
    /// Nachbarn funktionieren.
    ///
    /// Für Remote-Provider bleibt es bei `false`: Ein Server kann beim Schreiben
    /// normalisieren und trotzdem beide Schreibweisen listen. Ein exakter Treffer
    /// wäre dann allein kein Beweis, welches Objekt die Mutation getroffen hat.
    /// Ohne zusätzliche objektbezogene Namens- und Inhaltsbelege bleibt der
    /// Rückleseweg dort unklar; ein einzelner bestätigter Lauf ändert diese
    /// globale Providereigenschaft nicht.
    fn has_exact_names(&self) -> bool {
        false
    }

    /// Manche Remote-Protokolle kennen keine portable POSIX-Rechtesemantik. Der
    /// Inhaltsvertrag darf dann nicht so tun, als ließen sich `rwx`-Bits schreiben.
    fn supports_mode_write(&self) -> bool {
        true
    }

    /// POSIX-Rechte am konkreten Zielvolume; `parent` muss ein bestehender Ordner sein.
    fn supports_mode_write_at(&self, _parent: &Path) -> io::Result<bool> {
        Ok(self.supports_mode_write())
    }

    /// Manche Remote-Protokolle liefern eine mtime, bieten aber keinen verlässlichen
    /// standardisierten Schreibbefehl dafür. Der Kopierpfad lässt sie dann sichtbar
    /// providerverwaltet, statt eine nicht nachprüfbare Änderung zu behaupten.
    fn supports_mtime_write(&self) -> bool {
        true
    }

    /// Stärkste für diese Sitzung bewiesene Commit-Klasse neuer Dateien. Der
    /// Default ist bewusst `Assured`: Provider ohne atomaren No-Replace-Beweis
    /// sperren das Schreiben ohnehin an ihren eigenen Gates, und nur ein Provider,
    /// der ausdrücklich den Best-Effort-Vertrag erfüllt (frische Bestandsprüfung
    /// durch den Aufrufer, genau ein Rename, Readback), meldet `BestEffort`.
    fn file_commit_class(&self) -> FileCommitClass {
        FileCommitClass::Assured
    }

    /// Commit-Klasse im konkreten Ziel-Elternordner. Lokale Mounts können sich
    /// innerhalb desselben VFS unterscheiden; eine prozessweite Zusage genügt nicht.
    fn file_commit_class_at(&self, _parent: &Path) -> io::Result<FileCommitClass> {
        Ok(self.file_commit_class())
    }

    /// Datei- und Namespace-Dauerhaftigkeit bleiben von No-Replace getrennt.
    /// Diese Abfrage erteilt keine Freigabe für einen schwächeren Lauf.
    fn file_flush_guarantees_at(&self, _parent: &Path) -> io::Result<(bool, bool)> {
        Ok((true, true))
    }

    /// Stärkste für diese Sitzung bewiesene Klasse beim Ersetzen einer vorhandenen
    /// regulären Datei. Provider dürfen `BestEffort` erst nach einer echten
    /// Ersetzungsprobe melden; die UI muss die fehlende Wiederherstellbarkeit vorab
    /// bestätigt haben.
    fn file_replace_class(&self) -> FileReplaceClass {
        FileReplaceClass::Assured
    }

    /// Baut nach einem Verbindungsabbruch höchstens einmal eine frische,
    /// identitätsgeprüfte Sitzung auf — ausschließlich damit ein anschließendes
    /// Listing den tatsächlichen Zielzustand klären kann. Meldet der Server eine
    /// andere Identität, muss die Methode ablehnen. Implementierungen dürfen dabei
    /// ihre verhaltensgeprüften Fähigkeiten zurücksetzen; der Aufrufer darf nach
    /// der Klärung unter keinen Umständen eine Mutation wiederholen. Für lokale
    /// Ziele ist nichts zu tun.
    fn reclaim_verification_session(&self) -> io::Result<()> {
        Ok(())
    }

    /// Verzeichnis auflisten (nicht rekursiv). Reihenfolge ist unerheblich.
    fn list(&self, path: &Path) -> io::Result<Vec<Entry>>;

    /// Auflistung samt Vollständigkeitsnachweis und rohen Namen. Ein Provider, der
    /// diese Methode nicht überschreibt, darf niemals für einen Real-Mirror gelten.
    fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
        Ok(CompleteDirectoryListing {
            entries: self
                .list(path)?
                .into_iter()
                .map(|entry| ProofListEntry {
                    raw_name: entry.name.as_bytes().to_vec(),
                    entry,
                    safely_decoded: true,
                    provider_token: None,
                })
                .collect(),
            provider_token: None,
            complete: false,
        })
    }

    /// Metadaten eines einzelnen Pfads ermitteln, **ohne** einem Symlink zu folgen.
    fn stat(&self, path: &Path) -> io::Result<Entry>;

    /// Enger Beleg einer schwachen interaktiven regulären Inhaltsquelle. Nur die
    /// Quellseite eines Konfliktvergleichs darf ihn samt vollständigem Hash nutzen.
    /// Er ersetzt weder ein Ziel-Elternlisting noch eine Planner-Rootidentität.
    fn interactive_regular_file_source(&self, _path: &Path) -> io::Result<Option<Entry>> {
        Ok(None)
    }

    /// Ausschließlich für additive interaktive Kopien und deren Quellvorschau.
    /// Eine private Baumvorbereitung kann ihren begrenzten beobachteten Bestand
    /// liefern; Planner, Move und Zielbelege verwenden weiterhin list_complete.
    fn interactive_copy_children(&self, _path: &Path) -> io::Result<Option<Vec<ProofListEntry>>> {
        Ok(None)
    }

    fn interactive_source_tree(&self) -> io::Result<Option<InteractiveSourceTree>> {
        Ok(None)
    }

    /// Existiert der Pfad?
    fn exists(&self, path: &Path) -> bool;

    /// Verzeichnis inkl. aller fehlenden Elternverzeichnisse anlegen.
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;

    /// Genau ein Verzeichnis anlegen. Existiert der Name bereits, muss die Operation
    /// atomar mit `AlreadyExists` scheitern.
    fn create_dir(&self, path: &Path) -> io::Result<()>;

    /// Legt genau ein Verzeichnis an und liefert nur dann einen später nutzbaren
    /// Entfernungsbeleg, wenn der Provider ihn ohne Namensrennen an die Anlage bindet.
    ///
    /// Ein nachträgliches `stat` oder Listing reicht nicht: Ein anderer Prozess kann
    /// den neuen Namen dazwischen austauschen. Der sichere Default legt das Verzeichnis
    /// deshalb normal an, liefert aber keinen Beleg. Ein Aufrufer darf es dann bei einem
    /// späteren Rollback nicht automatisch entfernen. `Unchanged` darf ein Provider
    /// nur melden, wenn er garantiert, dass bei diesem Fehler nichts angelegt wurde;
    /// jeder unklare Fehler gehört in `OutcomeUnclear`.
    fn create_dir_with_removal_proof(
        &self,
        path: &Path,
    ) -> Result<Option<ProofListEntry>, MutationFailure> {
        match self.create_dir(path) {
            Ok(()) => Ok(None),
            Err(error) => Err(MutationFailure::OutcomeUnclear(error)),
        }
    }

    /// Datei zum Lesen öffnen (für den Kopier-Datenstrom).
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>>;

    /// Datei zum Schreiben öffnen/anlegen (überschreibt vorhandene). Der Aufrufer
    /// stellt sicher, dass das Elternverzeichnis existiert.
    fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>>;

    /// Eine garantiert neue Datei zum Schreiben anlegen. Existiert der Pfad bereits,
    /// muss die Operation atomar mit `AlreadyExists` scheitern. Der Sync nutzt das für
    /// temporäre Dateien, damit weder Nutzerdaten noch fremde Temp-Dateien überschrieben
    /// werden können.
    fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>>;

    /// Legt eine neue Datei an und bindet optional schon in derselben Provideraktion
    /// einen Beleg für eine spätere sichere Entfernung.
    ///
    /// Ein erst nach dem Öffnen ausgeführtes Listing wäre kein solcher Beleg: Der
    /// Tempname könnte dazwischen ausgetauscht werden. Der Default liefert deshalb
    /// nur den Schreibstrom und ausdrücklich keinen Entfernungsbeleg. `Unchanged`
    /// setzt bei Fehlern einen garantiert mutationsfreien Providerpfad voraus;
    /// andernfalls muss der Provider `OutcomeUnclear` liefern.
    fn open_write_new_with_removal_proof(
        &self,
        path: &Path,
    ) -> Result<(Box<dyn Write + Send>, Option<ProofListEntry>), MutationFailure> {
        match self.open_write_new(path) {
            Ok(writer) => Ok((writer, None)),
            Err(error) => Err(MutationFailure::OutcomeUnclear(error)),
        }
    }

    /// Reguläre Datei seek-fähig lesen. Provider ohne exakte Sparse-Semantik müssen
    /// ausdrücklich ablehnen; ein normaler Strom würde Löcher unbemerkt verdichten.
    fn open_sparse_read(&self, _path: &Path) -> io::Result<Box<dyn SparseRead>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt keine Sparse-Leseströme",
        ))
    }

    /// Garantiert neue Sparse-Datei seek-fähig anlegen.
    fn open_sparse_write_new(&self, _path: &Path) -> io::Result<Box<dyn SparseWrite>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt keine Sparse-Schreibströme",
        ))
    }

    /// Bereits geschriebene Dateidaten mit der stärksten für die gemeldete
    /// [`FileCommitClass`] verfügbaren Garantie abschließen. `Assured` verlangt ein
    /// dauerhaftes Flush oder einen gleichwertigen verifizierten Commit. Ein
    /// ausdrücklich als `BestEffort` eingestufter interaktiver Provider darf
    /// stattdessen den geschlossenen Datenstrom rücklesen. Nur der versiegelte
    /// normale lokale Sync darf seine bestätigte schwächere Klasse übernehmen;
    /// strikte Remote-, Mirror- und bidirektionale Läufe behalten ihre Gates.
    fn sync_file(&self, path: &Path) -> io::Result<()>;

    /// Stärkste vom Provider angebotene Namespace-Barriere nach Create/Rename/Unlink.
    /// Für lokale Ziele ist das echtes `fsync` auf dem ohne Symlink-Folgen geöffneten
    /// Ordner. Remote-Provider ohne entsprechende Protokolloperation müssen die
    /// bestätigte Sitzung plus Mutation und Readback binden und dürfen stärkere Modi
    /// über ihre Fähigkeiten nicht freischalten.
    fn sync_dir(&self, path: &Path) -> io::Result<()>;

    /// Änderungszeit (Unix-Sekunden) setzen, damit Ziel == Quelle wird.
    fn set_mtime(&self, path: &Path, mtime: i64) -> io::Result<()>;

    /// POSIX-Rechte setzen. Auf macOS/Linux geschieht das am bereits ohne Symlink-Folgen
    /// geöffneten Handle; spätere Remote-Provider dürfen `Unsupported` liefern.
    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()>;

    /// Numerischen Owner/Gruppe am geöffneten Eintrag setzen. Diese Stufe ist vom
    /// Portable-Modus getrennt, weil `fchown` Sonderbits sicherheitsbedingt löschen kann.
    fn set_metadata_owner(&self, _path: &Path, _owner: crate::metadata::Owner) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt keine numerischen Eigentümer",
        ))
    }

    /// Normale Rechte exakt setzen und vorhandene Sonderbits bewusst löschen. Im
    /// Stufe-2-Ablauf folgt dies nach Owner/ACL/xattrs und vor den Sonderbits.
    fn set_metadata_normal_mode(&self, _path: &Path, _mode: u32) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt keine exakten POSIX-Rechte",
        ))
    }

    /// Ausschließlich setuid/setgid/sticky als letzte Metadatenphase exakt setzen.
    fn set_metadata_special_bits(&self, _path: &Path, _bits: u32) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt keine POSIX-Sonderbits",
        ))
    }

    /// Metadatenfähigkeiten am konkreten Root. Ohne Providerimplementierung bleibt
    /// alles `Unknown`, damit ein striktes Profil sicher vor jeder Mutation stoppt.
    fn metadata_capabilities(&self, _root: &Path) -> io::Result<MetadataCapabilities> {
        Ok(MetadataCapabilities::unknown())
    }

    /// Lesendes kanonisches Metadatenmanifest. Provider müssen nicht unterstützte
    /// Stufe-2-Funktionen ausdrücklich melden, statt Merkmale still auszulassen.
    fn metadata_manifest(&self, _path: &Path) -> io::Result<MetadataManifest> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt noch keine Metadatenmanifeste",
        ))
    }

    /// Typisierte ACL am sicher geöffneten Zieleintrag exakt ersetzen oder entfernen.
    fn write_acl_exact(&self, _path: &Path, _acl: Option<&crate::metadata::Acl>) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt keine typisierten ACLs",
        ))
    }

    /// Bytegenaue xattrs lesen. Der Default ist ein expliziter Fehler, damit ein
    /// Provider fehlende Unterstützung niemals als „keine Attribute“ ausgibt.
    fn read_xattrs(&self, _path: &Path) -> io::Result<Vec<XattrValue>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt keine Extended Attributes",
        ))
    }

    /// xattrs eines bereits sicher geöffneten Zieleintrags exakt ersetzen. Aufrufer
    /// verwenden das im atomaren Sync-Pfad ausschließlich an einer neuen Temp-Datei.
    fn write_xattrs_exact(&self, _path: &Path, _xattrs: &[XattrValue]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt keine Extended Attributes",
        ))
    }

    /// Separaten Fork als Inhaltsstrom öffnen. Fehlende Providerfähigkeit ist ein
    /// expliziter Fehler; ein leerer Datenstrom darf sie nicht vortäuschen.
    fn open_fork_read(&self, _path: &Path, _kind: ForkKind) -> io::Result<Box<dyn Read + Send>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt keine separaten Fork-Inhaltsströme",
        ))
    }

    /// Neuen Fork an einem bereits exklusiv angelegten Temp-Eintrag öffnen. Ein
    /// vorhandener Fork muss atomar mit `AlreadyExists` scheitern.
    fn open_fork_write_new(
        &self,
        _path: &Path,
        _kind: ForkKind,
    ) -> io::Result<Box<dyn Write + Send>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt keine separaten Fork-Inhaltsströme",
        ))
    }

    /// Einzelne Datei löschen.
    fn remove_file(&self, path: &Path) -> io::Result<()>;

    /// Verzeichnis samt Inhalt löschen.
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;

    /// Ein nachweislich leeres Verzeichnis entfernen. Providerfähige rekursive
    /// Löschläufe verwenden bewusst diese primitive Operation, damit ein parallel
    /// neu entstandener Kindname nicht serverseitig rekursiv mitgelöscht wird.
    fn remove_dir(&self, _path: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt kein sicheres Entfernen leerer Verzeichnisse",
        ))
    }

    /// Entfernt genau den durch `expected` gebundenen Eintrag oder verändert nichts.
    ///
    /// Ein vorheriges `stat` mit anschließendem Löschen nach Pfad erfüllt diesen
    /// Vertrag ausdrücklich nicht: Ein anderer Prozess kann den Namen dazwischen
    /// austauschen. Eine Implementierung braucht eine atomare Provider-Bedingung
    /// (etwa einen starken Versions-Token) oder eine gleichwertige Entfernung über
    /// einen bereits gebundenen Handle. Bei einem Verzeichnis darf diese Primitive
    /// ausschließlich den nachweislich leeren Eintrag selbst entfernen; rekursives
    /// Löschen braucht einen eigenen Manifestvertrag und ist hier verboten. Auch ein
    /// Fehler muss mutationsfrei sein.
    fn remove_if_matches(&self, _path: &Path, _expected: &ProofListEntry) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt keine identitätsgebundene bedingte Entfernung",
        ))
    }

    /// Datei/Ordner umbenennen bzw. INNERHALB desselben Dateisystems verschieben.
    /// Auf gleichem Volume ist das ein schneller, atomarer Vorgang; über Volume-Grenzen
    /// scheitert es (z. B. `EXDEV`) — der Aufrufer weicht dann auf Kopieren+Löschen aus.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Wie [`Vfs::rename`], aber das Ziel darf nicht existieren. Die Existenzprüfung
    /// muss zusammen mit dem Rename atomar sein, damit ein paralleler Erzeuger nicht
    /// unbemerkt überschrieben wird.
    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Verschiebt einen Eintrag exklusiv und liefert nur dann einen späteren
    /// Entfernungsbeleg, wenn der Provider ihn in derselben atomaren Operation an
    /// genau den verschobenen Eintrag bindet.
    ///
    /// Ein nachträgliches Listing am neuen Namen schließt das Rennen nicht. Der
    /// sichere Default führt das Rename aus, liefert aber keinen Beleg; der Eintrag
    /// muss dann als sichtbarer Recoverybestand stehen bleiben. Auch hier ist
    /// `Unchanged` nur bei garantiert mutationsfreiem Fehler zulässig; ein Fehler
    /// nach möglicher Rename-Wirkung ist `OutcomeUnclear`.
    fn rename_noreplace_with_removal_proof(
        &self,
        from: &Path,
        to: &Path,
    ) -> Result<Option<ProofListEntry>, MutationFailure> {
        match self.rename_noreplace(from, to) {
            Ok(()) => Ok(None),
            Err(error) => Err(MutationFailure::OutcomeUnclear(error)),
        }
    }

    /// Benennt nur dann exklusiv um, wenn `from` noch genau den gebundenen Eintrag
    /// bezeichnet. Prüfung, No-Replace-Rename und Fehlerfreiheit müssen eine einzige
    /// Providerwirkung bilden; jeder Fehler garantiert einen unveränderten Bestand.
    fn rename_noreplace_if_matches(
        &self,
        _from: &Path,
        _to: &Path,
        _expected: &ProofListEntry,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt kein identitätsgebundenes exklusives Rename",
        ))
    }

    /// Meldet nur dann `true`, wenn [`Vfs::rename_noreplace_if_matches`] wirklich
    /// als eine atomare, mutationsfreie Bedingung implementiert ist.
    ///
    /// Ein erfolgreich getestetes normales Rename reicht nicht: Zwischen einem
    /// getrennten Identitätscheck und der Wirkung könnte ein anderer Prozess den
    /// Quellnamen austauschen. Der sichere Default bleibt deshalb `false`.
    fn supports_rename_noreplace_if_matches(&self) -> bool {
        false
    }

    /// Benennt einen gebundenen Eintrag exklusiv um und bindet in derselben
    /// atomaren Providerwirkung zusätzlich genau einen beteiligten Elternordner.
    /// `bound_parent` muss der Elternordner von `from` oder `to` sein. Jeder Fehler
    /// garantiert, dass weder Eintrag noch Name verändert wurden.
    ///
    /// Diese strengere Form schützt Kindmutationen unter einer bereits vorhandenen
    /// Quarantänebasis: Ein getrenntes Listing würde einen Austausch der Basis
    /// zwischen Prüfung und Rename nicht schließen. Der Default bleibt deshalb
    /// ausdrücklich mutationsfrei `Unsupported`.
    fn rename_noreplace_if_matches_in_bound_parent(
        &self,
        _from: &Path,
        _to: &Path,
        _expected: &ProofListEntry,
        _bound_parent: &Path,
        _expected_parent: &ProofListEntry,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt kein exklusives Rename unter einem identitätsgebundenen Elternordner",
        ))
    }

    /// Meldet nur dann `true`, wenn die Elternprüfung und das bedingte Rename der
    /// vorigen Methode wirklich atomar sowie im Fehlerfall mutationsfrei sind.
    fn supports_rename_noreplace_if_matches_in_bound_parent(&self) -> bool {
        false
    }

    /// Benennt einen gebundenen Eintrag exklusiv um und bindet in derselben
    /// Providerwirkung beide beteiligten Elternordner. Das ist der Mindestvertrag
    /// für einen Quarantäne-Claim: Ein Angreifer darf weder den bestätigten
    /// Quellroot noch den bereits manifestierten Laufordner zwischen Prüfung und
    /// Rename austauschen. Jeder Fehler garantiert einen unveränderten Bestand.
    fn rename_noreplace_if_matches_in_bound_parents(
        &self,
        _request: BoundParentsRenameRequest<'_>,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt kein exklusives Rename mit zwei identitätsgebundenen Elternordnern",
        ))
    }

    /// Meldet nur dann `true`, wenn Quellprüfung, beide Elternprüfungen und Rename
    /// atomar zusammengehören, ein optionaler Inhaltsbeleg in derselben Wirkung
    /// geprüft wird und jeder Fehler garantiert mutationsfrei bleibt.
    fn supports_rename_noreplace_if_matches_in_bound_parents(&self) -> bool {
        false
    }

    /// Verschiebt einen manifestierten Quarantäneeintrag aus einem gehaltenen
    /// Laufroot an ein exklusives Ziel. Das ist die lokale Descriptor-Variante des
    /// Providervertrags mit gebundenen Eltern; der Default bleibt mutationsfrei.
    fn restore_from_bound_quarantine_run(
        &self,
        _request: BoundQuarantineRestoreRequest<'_>,
    ) -> Result<(), MutationFailure> {
        Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt keinen descriptor-gebundenen Quarantäne-Restore",
        )))
    }

    fn supports_restore_from_bound_quarantine_run(&self) -> bool {
        false
    }

    /// Leert ausschließlich den Inhalt des bereits gehaltenen Laufordners. Der
    /// sichtbare Claim-Name und das Sidecar werden absichtlich vom Aufrufer getrennt
    /// behandelt, weil POSIX den finalen `rmdir` nicht an Gerät/Inode binden kann.
    fn purge_bound_quarantine_run_contents(
        &self,
        _request: BoundQuarantinePurgeRequest<'_>,
    ) -> Result<(), MutationFailure> {
        Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt kein descriptor-gebundenes Leeren eines Quarantänelaufs",
        )))
    }

    fn supports_purge_bound_quarantine_run_contents(&self) -> bool {
        false
    }

    /// Belegt nach einer Purge-Wirkung über denselben descriptor-gebundenen Vertrag,
    /// dass Lauf und Intent noch dieselben Objekte sind und der Lauf leer ist.
    fn verify_bound_quarantine_run_empty(
        &self,
        _request: BoundQuarantinePurgeRequest<'_>,
    ) -> Result<(), MutationFailure> {
        Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt keinen descriptor-gebundenen Quarantäne-Leerheitsbeleg",
        )))
    }

    /// Ersetzt eine kleine Datei nur dann, wenn ihr aktueller Inhalt noch exakt
    /// dem erwarteten Digest entspricht. `Ok` garantiert außerdem, dass neue Bytes
    /// und Elternverzeichnis soweit vom Provider unterstützt dauerhaft bestätigt
    /// sind. Ein `Unchanged`-Fehler garantiert, dass die Datei unverändert blieb;
    /// nach `OutcomeUnclear` muss der Aufrufer den sichtbaren Bestand als
    /// Recoveryfall behandeln.
    fn replace_file_if_content_matches(
        &self,
        _request: ConditionalContentReplaceRequest<'_>,
    ) -> Result<(), MutationFailure> {
        Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt keinen inhaltsgebundenen Dateiaustausch",
        )))
    }

    /// Meldet nur dann `true`, wenn Inhaltsprüfung, Austausch und Durability als
    /// ein Vertrag implementiert sind und Fehler die oben beschriebene
    /// Mutationsklasse zuverlässig erhalten.
    fn supports_replace_file_if_content_matches(&self) -> bool {
        false
    }

    /// Meldet nur dann `true`, wenn eine neue Datei zusammen mit einem später
    /// verwendbaren Entfernungsbeleg in derselben Erzeugungswirkung gebunden wird.
    fn supports_open_write_new_with_removal_proof(&self) -> bool {
        false
    }

    /// Meldet nur dann `true`, wenn ein neuer Ordner zusammen mit einem später
    /// verwendbaren Entfernungsbeleg in derselben Erzeugungswirkung gebunden wird.
    fn supports_create_dir_with_removal_proof(&self) -> bool {
        false
    }

    /// Meldet nur dann `true`, wenn [`Vfs::remove_if_matches`] Prüfung und Wirkung
    /// atomar verbindet und jeder Fehler garantiert mutationsfrei bleibt.
    fn supports_remove_if_matches(&self) -> bool {
        false
    }

    /// Vollständiger Vertrag für die künstliche Restore-Probe. Ein einzelnes
    /// positives Teilmerkmal reicht nicht: Probe und Cleanup brauchen alle
    /// identitätsgebundenen Wirkungen einschließlich des dauerhaften Manifest-CAS.
    fn supports_bound_restore_probe(&self) -> bool {
        self.supports_open_write_new_with_removal_proof()
            && self.supports_create_dir_with_removal_proof()
            && self.supports_rename_noreplace_if_matches()
            && self.supports_rename_noreplace_if_matches_in_bound_parent()
            && self.supports_rename_noreplace_if_matches_in_bound_parents()
            && self.supports_replace_file_if_content_matches()
            && self.supports_remove_if_matches()
    }

    /// Bindet eine vollständig gelesene Move-Quelle atomar an ihren Inhaltsdigest.
    /// Ohne provideratomaren Versionsbeleg liefert der sichere Default `None`.
    fn bind_move_source_for_removal(
        &self,
        _path: &Path,
        _content_digest: [u8; 32],
        _recursive: bool,
    ) -> io::Result<Option<MoveSourceRemovalProof>> {
        Ok(None)
    }

    /// Entfernt die gebundene Move-Quelle oder verändert gar nichts. Der normale
    /// Dateioperationspfad ruft diese Primitive nur für `recursive == false` auf.
    /// Ein rekursives Pfad-Delete nach bloßem Root-Vergleich erfüllt den Vertrag
    /// ausdrücklich nicht; Bäume werden über vollständige Manifeste blattweise und
    /// Verzeichnisse ausschließlich leer entfernt.
    fn remove_move_source_if_matches(
        &self,
        _path: &Path,
        _expected: &MoveSourceRemovalProof,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt keine atomar gebundene Move-Quellentfernung",
        ))
    }

    /// Entfernt genau ein zuvor unmittelbar frisch geprüftes Blatt im begrenzten
    /// Best-Effort-Quellabbau eines interaktiven Moves.
    ///
    /// Diese Methode ist kein Identitätsbeleg: Zwischen Readback und Wirkung kann
    /// ein Provider ohne bedingtes Entfernen den Namen austauschen. Der Aufrufer
    /// muss dieses Restrisiko sichtbar machen und darf hier niemals einen ganzen
    /// Baum übergeben. Provider dürfen den engen Pfad unabhängig von ihren stärkeren
    /// Papierkorb-/Quarantäneverträgen freigeben.
    fn remove_move_leaf_best_effort(&self, path: &Path) -> io::Result<()> {
        self.remove_file(path)
    }

    /// Entfernt genau einen zuvor vollständig und frisch als leer gelisteten Ordner
    /// im begrenzten Best-Effort-Quellabbau eines interaktiven Moves.
    ///
    /// Der Default ruft bewusst nur `remove_dir` auf. Ein rekursives Entfernen würde
    /// die Manifestgrenze umgehen und könnte nach einem Namensaustausch fremde
    /// Inhalte treffen.
    fn remove_move_empty_dir_best_effort(&self, path: &Path) -> io::Result<()> {
        self.remove_dir(path)
    }

    /// Führt das gewöhnliche Protokoll-Rename des Providers genau einmal aus, ohne
    /// No-Replace-Garantie: Ein serverseitig gleichzeitig entstandenes Ziel kann
    /// dabei ersetzt werden. Diese Methode trägt ausschließlich die interaktive
    /// Best-Effort-Umbenennung, deren Aufrufer unmittelbar vorher den vollständigen
    /// Elternbestand geprüft und danach einen Listing-Readback ausgeführt hat.
    /// Nach einem Verbindungsfehler darf kein Aufrufer diese Mutation wiederholen;
    /// erlaubt ist nur ein erneutes Listing zur Zustandsklärung.
    ///
    /// Vertrag für Implementierungen: Ein Fehler der Art `AlreadyExists` (ebenso
    /// wie bei [`Vfs::rename_noreplace`]) darf ausschließlich eine mutationsfreie
    /// Ablehnung bedeuten — der Aufrufer wertet ihn als sichtbare Kollision ohne
    /// jede Bestandsänderung. Ein Provider, der das nicht garantieren kann, muss
    /// stattdessen einen anderen Fehler liefern.
    fn rename_plain_once(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider bietet kein geprüftes Best-Effort-Rename",
        ))
    }

    /// Dieselbe Einzelwirkung mit belegter Fehlerphase. Nur ein Provider, der
    /// vor dem Versand gestoppt hat, darf weitere Fehler als Unchanged einordnen.
    fn rename_plain_once_with_phase(&self, from: &Path, to: &Path) -> Result<(), MutationFailure> {
        self.rename_plain_once(from, to).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                MutationFailure::Unchanged(error)
            } else {
                MutationFailure::OutcomeUnclear(error)
            }
        })
    }

    /// Ersetzt genau eine vorhandene reguläre Zieldatei oder einen bestätigten Symlink
    /// als Verzeichniseintrag mit einer fertigen Temp-Datei und führt die
    /// Provider-Mutation exakt einmal aus. Kein Fallback, kein Retry und keine
    /// rekursive Wirkung. Der Aufrufer bindet unmittelbar vorher den Zielbeleg und
    /// liest den Zustand anschließend zurück.
    fn rename_replace_plain_once(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider bietet kein geprüftes Best-Effort-Ersetzen",
        ))
    }

    /// Zwei vorhandene Namen atomar austauschen. Lokale macOS-/Linux-Ziele nutzen
    /// dafür den jeweiligen Kernel-`EXCHANGE`/`SWAP`-Rename; Provider ohne exakt
    /// dieselbe Garantie müssen ablehnen. Das ist die Grundlage für sichere
    /// Datei↔Ordner-Typwechsel ohne zwischenzeitlich fehlenden Originalnamen.
    fn rename_exchange(&self, _left: &Path, _right: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt keinen atomaren Namensaustausch",
        ))
    }

    /// Einen zweiten Namen für dieselbe reguläre Datei anlegen. Das Ziel darf nicht
    /// existieren; die Kollisionsprüfung muss atomar im Provider erfolgen.
    fn create_hard_link_noreplace(&self, _existing: &Path, _new: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider unterstützt keine Hardlinks",
        ))
    }

    /// Gehaltene lokale Sicherheitswurzel, falls dieser Provider descriptor-verankert
    /// arbeitet. Remote-Provider liefern später ihre eigene Identität über Permits.
    fn anchored_root(&self) -> Option<&Path> {
        None
    }

    /// Geräte-/Inode-Identität des gehaltenen lokalen Root-Handles.
    fn anchored_identity(&self) -> Option<(u64, u64)> {
        None
    }

    /// Prüft, ob der sichtbare Pfad noch auf dieselbe gehaltene Wurzel zeigt. Die
    /// Standardeinstellung gilt für Provider ohne lokale Root-Verankerung.
    fn anchor_is_current(&self) -> io::Result<bool> {
        Ok(true)
    }

    /// Identität eines konkreten lokalen Verzeichnisroots. Nichtlokale Provider
    /// liefern ihre eigene Identität über den späteren Providervertrag.
    fn local_root_identity(&self, _path: &Path) -> io::Result<LocalRootIdentity> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider besitzt keine lokale Root-Identität",
        ))
    }

    /// Providerneutrale Identität eines konkreten Roots. Lokale Implementierungen
    /// verwenden den descriptor-/Mount-gebundenen Wert; Remote-Provider überschreiben
    /// die Methode mit Server-, Root- und Sitzungsidentität.
    fn root_identity(&self, path: &Path) -> io::Result<crate::real_mode::RootIdentity> {
        self.local_root_identity(path)
            .map(crate::real_mode::RootIdentity::Local)
    }

    /// Im gehaltenen lokalen Mount verfügbare Bytes. Fehlende Information ist im
    /// Realmodus ein Fehler und wird nicht als unbegrenzt behandelt.
    fn available_bytes(&self, _path: &Path) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider meldet keinen verlässlichen freien Speicher",
        ))
    }
}

/// Lokales Dateisystem (die Platte des jeweiligen Rechners).
///
/// Standardmäßig arbeiten alle Methoden mit absoluten Pfaden. Für schreibende
/// Sandbox-Syncs hält `rooted` zusätzlich einen Root-Deskriptor; dann werden sämtliche
/// Kindzugriffe relativ zu genau diesem Handle ausgeführt.
#[derive(Clone, Default)]
pub struct LocalVfs {
    root: Option<crate::local_fs::RootHandle>,
    file_commit_root: Option<std::sync::Arc<crate::local_fs::RootHandle>>,
    staging: Option<std::sync::Arc<crate::local_staging::LocalStaging>>,
    staging_owner: Option<crate::local_staging::StagingOwner>,
    sync_binding: Option<std::sync::Arc<LocalSyncVfsBinding>>,
    verified_reads: Option<std::sync::Arc<crate::local_read::VerifiedLocalFiles>>,
}

#[derive(Clone, Default)]
struct LocalSyncVfsBinding {
    require_temp_recovery: bool,
    execution: Option<crate::real_mode::LocalSyncExecution>,
    parents: std::sync::Arc<Vec<crate::local_fs::RootHandle>>,
    neighbor: Option<std::sync::Arc<crate::local_staging::NeighborStaging>>,
}

fn unsupported_sync(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Unsupported
        || error.raw_os_error().is_some_and(|code| {
            code == libc::ENOTSUP || code == libc::EOPNOTSUPP || code == libc::ENOSYS
        })
}

impl LocalVfs {
    pub(crate) fn with_verified_reads(
        mut self,
        items: &[PathBuf],
        cancel: &crate::Cancel,
    ) -> io::Result<Self> {
        let root = self
            .root
            .as_ref()
            .ok_or_else(|| io::Error::other("Uploadquelle benötigt einen gehaltenen Root"))?;
        self.verified_reads = Some(std::sync::Arc::new(
            crate::local_read::VerifiedLocalFiles::bind(root, items, cancel)?,
        ));
        Ok(self)
    }
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn with_staging_owner(mut self, queue: &Path, operation: &str) -> Self {
        self.staging_owner = Some(crate::local_staging::StagingOwner {
            queue: queue.to_path_buf(),
            operation: operation.into(),
        });
        self
    }

    /// Auch ein begrenzter SFTP-Download kann vor dem lokalen Commit abstürzen.
    /// Sein Tempintent nutzt denselben privaten Recoveryindex wie schwacher Sync;
    /// daraus entsteht weder ein Wiederholungs- noch ein Löschrecht.
    pub(crate) fn with_required_temp_recovery(self, queue: &Path, operation: &str) -> Self {
        let mut value = self.with_staging_owner(queue, operation);
        std::sync::Arc::make_mut(value.sync_binding.get_or_insert_with(Default::default))
            .require_temp_recovery = true;
        value
    }

    /// Bindet die konkrete Zielvolume-Abfrage zusätzlich zu einem äußeren
    /// Sandboxroot. Das erteilt keine Schreibfreigabe; ohne diese getrennte
    /// Bindung würde schon eine lesende Vorschau einen eigenen Submount mit
    /// dem äußeren Sandbox-Mount vergleichen und fälschlich als Drift ablehnen.
    pub fn with_file_commit_root(mut self, target: &Self) -> Self {
        self.file_commit_root = target.root.clone().map(std::sync::Arc::new);
        self
    }

    fn sync_execution(&self) -> Option<&crate::real_mode::LocalSyncExecution> {
        self.sync_binding
            .as_deref()
            .and_then(|binding| binding.execution.as_ref())
    }

    fn sync_parents(&self) -> &[crate::local_fs::RootHandle] {
        self.sync_binding
            .as_deref()
            .map_or(&[], |binding| binding.parents.as_slice())
    }

    fn neighbor_staging(&self) -> Option<&crate::local_staging::NeighborStaging> {
        self.sync_binding
            .as_deref()
            .and_then(|binding| binding.neighbor.as_deref())
    }

    pub(crate) fn with_sync_execution(
        mut self,
        execution: Option<crate::real_mode::LocalSyncExecution>,
    ) -> Self {
        std::sync::Arc::make_mut(self.sync_binding.get_or_insert_with(Default::default))
            .execution = execution;
        self
    }

    pub(crate) fn with_sync_parents(
        mut self,
        target: &Path,
        plan: &crate::SyncPlan,
    ) -> io::Result<Self> {
        let mut paths = std::collections::BTreeSet::new();
        for row in &plan.comparison.rows {
            if row.action == crate::sync::ComparisonAction::Skip {
                continue;
            }
            let path = crate::path::checked_join(target, &row.path)?;
            for parent in path.ancestors().take_while(|p| p.starts_with(target)) {
                match self.stat(parent) {
                    Ok(entry) if entry.is_dir && !entry.is_symlink => {
                        paths.insert(parent.to_path_buf());
                        break;
                    }
                    Ok(_) => continue,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                }
            }
        }
        let mount = self.local_root_identity(target)?.mount;
        let parents = paths.into_iter().map(|path| {
            let handle = match self.root_for(&path) {
                Some(root) => crate::local_fs::RootHandle::open_beneath(root, &path)?,
                None => crate::local_fs::RootHandle::open(&path)?,
            };
            if handle.current_mount_identity()? != mount {
                return Err(crate::real_mode::plan_drift("Eine lokale Sync-Wirkung liegt auf einem anderen Mount als der bestätigte Zielroot"));
            }
            Ok(handle)
        }).collect::<io::Result<Vec<_>>>()?;
        std::sync::Arc::make_mut(self.sync_binding.get_or_insert_with(Default::default)).parents =
            std::sync::Arc::new(parents);
        Ok(self)
    }

    fn finish_sync(&self, result: io::Result<()>, assured: bool) -> io::Result<()> {
        match result {
            Err(error)
                if !assured && self.sync_execution().is_some() && unsupported_sync(&error) =>
            {
                Ok(())
            }
            other => other,
        }
    }

    fn root_for(&self, path: &Path) -> Option<&crate::local_fs::RootHandle> {
        if let Some(stage) = self.neighbor_staging() {
            if path.starts_with(stage.parent.path()) {
                return Some(&stage.parent);
            }
        }
        if let Some(stage) = &self.staging {
            if path.starts_with(stage.directory.path()) {
                return Some(&stage.directory);
            }
            if path.starts_with(stage.parent.path()) {
                return Some(&stage.parent);
            }
        }
        self.sync_parents()
            .iter()
            .filter(|root| path.starts_with(root.path()))
            .max_by_key(|root| root.path().components().count())
            .or_else(|| {
                self.file_commit_root
                    .as_deref()
                    .filter(|root| path.starts_with(root.path()))
            })
            .or(self.root.as_ref())
            .or_else(|| self.staging.as_ref().and_then(|stage| stage.outer()))
    }

    fn staged_rename(
        &self,
        from: &Path,
        to: &Path,
        flags: Option<rustix::fs::RenameFlags>,
    ) -> io::Result<()> {
        if self.staging.is_none() {
            if self.file_commit_root.is_some() {
                return crate::local_fs::rename_between(
                    self.root_for(from),
                    from,
                    self.root_for(to),
                    to,
                    flags,
                );
            }
            return match (&self.root, flags) {
                (Some(root), None) => crate::local_fs::rename_beneath(root, from, to),
                (None, None) => crate::local_fs::rename(from, to),
                (Some(root), Some(rustix::fs::RenameFlags::NOREPLACE)) => {
                    crate::local_fs::rename_noreplace_beneath(root, from, to)
                }
                (None, Some(rustix::fs::RenameFlags::NOREPLACE)) => {
                    crate::local_fs::rename_noreplace(from, to)
                }
                (Some(root), Some(rustix::fs::RenameFlags::EXCHANGE)) => {
                    crate::local_fs::rename_exchange_beneath(root, from, to)
                }
                (None, Some(rustix::fs::RenameFlags::EXCHANGE)) => {
                    crate::local_fs::rename_exchange(from, to)
                }
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Unbekannte Renameflags",
                )),
            };
        }
        if let Some(stage) = &self.staging {
            stage.verify_parent()?;
            stage.record("rename_intent", from, Some(to))?;
        }
        crate::local_fs::rename_between(self.root_for(from), from, self.root_for(to), to, flags)?;
        if let Some(stage) = &self.staging {
            stage.verify_parent().map_err(|error| crate::coded_error(io::ErrorKind::Other, crate::ErrorCode::OutcomeUnclear, format!("Rename wirkte bereits; Ziel-Elternbindung änderte sich ({error}). Recoveryjournal: {}", stage.directory.path().display())))?;
            stage
                .record("rename_applied", from, Some(to))
                .map_err(|error| {
                    crate::coded_error(
                        io::ErrorKind::Other,
                        crate::ErrorCode::OutcomeUnclear,
                        format!("Rename ausgeführt; Journalbestätigung fehlgeschlagen: {error}"),
                    )
                })?;
        }
        Ok(())
    }

    /// Lokales VFS fest an eine vorhandene Verzeichniswurzel binden. Pfade außerhalb
    /// der Wurzel werden anschließend von jeder Operation abgelehnt.
    pub fn rooted(root: &Path) -> io::Result<Self> {
        Ok(Self {
            root: Some(crate::local_fs::RootHandle::open(root)?),
            file_commit_root: None,
            staging: None,
            staging_owner: None,
            sync_binding: None,
            verified_reads: None,
        })
    }
}

impl Vfs for LocalVfs {
    fn interactive_source_tree(&self) -> io::Result<Option<InteractiveSourceTree>> {
        self.verified_reads
            .as_ref()
            .map_or(Ok(None), |files| files.tree())
    }
    fn interactive_copy_children(&self, path: &Path) -> io::Result<Option<Vec<ProofListEntry>>> {
        self.verified_reads
            .as_ref()
            .map_or(Ok(None), |files| files.children(path))
    }
    fn verify_transferred_source(&self, path: &Path, hash: &blake3::Hash) -> io::Result<()> {
        if let Some(files) = &self.verified_reads {
            files.verify_transfer(path, hash)?;
        }
        Ok(())
    }

    fn file_flush_guarantees_at(&self, parent: &Path) -> io::Result<(bool, bool)> {
        let caps = crate::local_fs::file_capabilities(self.root_for(parent), parent)?;
        Ok((caps.file_flush, caps.directory_flush))
    }
    fn supports_mode_write_at(&self, parent: &Path) -> io::Result<bool> {
        Ok(crate::local_fs::file_capabilities(self.root_for(parent), parent)?.mode_write)
    }

    fn file_commit_class(&self) -> FileCommitClass {
        self.root
            .as_ref()
            .or(self.file_commit_root.as_deref())
            .map_or(FileCommitClass::BestEffort, |root| {
                self.file_commit_class_at(root.path())
                    .unwrap_or(FileCommitClass::BestEffort)
            })
    }

    fn file_commit_class_at(&self, parent: &Path) -> io::Result<FileCommitClass> {
        if !self.anchor_is_current()? {
            return Err(crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::PlanDrift,
                "Der gebundene Zielroot wurde während der Übertragung ausgetauscht",
            ));
        }
        let class = crate::local_fs::file_capabilities(self.root_for(parent), parent)?.commit_class;
        if class == FileCommitClass::BestEffort {
            if let Some(root) = self.file_commit_root.as_deref().or(self.root.as_ref()) {
                let current = self.local_root_identity(parent)?;
                if current.mount != root.current_mount_identity()? {
                    return Err(crate::coded_error(io::ErrorKind::Other, crate::ErrorCode::PlanDrift,
                        "Der Zielordner liegt auf einem anderen Mount als der bestätigte Best-Effort-Root"));
                }
            }
        }
        Ok(class)
    }

    fn staging_scope(
        &self,
        target: &Path,
        sandbox: Option<&Path>,
    ) -> io::Result<Option<Box<dyn Vfs>>> {
        // Ein privates Recoveryjournal benötigt echte Dauerhaftigkeit. Bei einer
        // bestätigten schwächeren Daten-/Namespaceklasse bleiben stattdessen
        // Nachbartemps mit ehrlicher Recoverybilanz erhalten.
        let weak_flush = self
            .sync_execution()
            .is_some_and(|p| !p.file_flush || !p.directory_flush);
        if self
            .staging
            .as_ref()
            .is_some_and(|stage| target.parent() == Some(stage.parent.path()))
        {
            return Ok(None);
        }
        let stage = if weak_flush {
            Ok(None)
        } else {
            crate::local_staging::LocalStaging::create(
                self.root
                    .as_ref()
                    .or_else(|| self.staging.as_ref().and_then(|stage| stage.outer())),
                self.root_for(target),
                target,
                sandbox,
                self.staging_owner.as_ref(),
                self.staging.as_ref().map(|stage| stage.anchor()),
            )
        };
        let stage = match stage {
            Ok(stage) => stage,
            Err(error) if error.kind() == io::ErrorKind::Unsupported => None,
            Err(error) => return Err(error),
        };
        let require_recovery = self
            .sync_binding
            .as_ref()
            .is_some_and(|binding| binding.require_temp_recovery);
        let neighbor = if stage.is_none() && (self.sync_execution().is_some() || require_recovery) {
            self.staging_owner
                .as_ref()
                .map(|owner| {
                    crate::local_staging::NeighborStaging::create(
                        owner,
                        target,
                        self.root_for(target),
                    )
                })
                .transpose()?
                .flatten()
        } else {
            None
        };
        if stage.is_none()
            && neighbor.is_none()
            && self.staging_owner.is_some()
            && (require_recovery
                || self
                    .sync_execution()
                    .is_some_and(|profile| profile.is_best_effort()))
        {
            return Err(crate::coded_error(io::ErrorKind::PermissionDenied, crate::ErrorCode::PermissionDenied,
                "Dieser Lauf benötigt für seine Temp-Recovery einen gegen fremde Schreibzugriffe geschützten Queueordner"));
        }
        if stage.is_none() && self.staging.is_none() && neighbor.is_none() {
            return Ok(None);
        }
        Ok(Some(Box::new(Self {
            root: self.root.clone(),
            file_commit_root: self.file_commit_root.clone(),
            staging: stage.map(std::sync::Arc::new),
            staging_owner: self.staging_owner.clone(),
            verified_reads: self.verified_reads.clone(),
            sync_binding: Some(std::sync::Arc::new(LocalSyncVfsBinding {
                neighbor: neighbor.map(std::sync::Arc::new),
                ..self.sync_binding.as_deref().cloned().unwrap_or_default()
            })),
        })))
    }

    fn local_parent_inheritance(
        &self,
        path: &Path,
    ) -> io::Result<Option<crate::metadata::LocalParentInheritance>> {
        crate::local_staging::parent_inheritance(self.root_for(path), path).map(Some)
    }

    fn staging_recovery_path(&self) -> Option<&Path> {
        self.staging
            .as_ref()
            .map(|stage| stage.directory.path())
            .or_else(|| self.neighbor_staging().map(|stage| stage.path()))
    }

    fn finish_staging(&self) -> io::Result<()> {
        if let Some(stage) = &self.staging {
            stage.finish()
        } else if let Some(stage) = self.neighbor_staging() {
            stage.finish()
        } else {
            Ok(())
        }
    }

    fn has_private_staging(&self) -> bool {
        self.staging.is_some()
    }

    fn cleanup_staged_backup(&self, path: &Path, proof: &ProofListEntry) -> io::Result<bool> {
        let Some(stage) = self.staging.as_ref().filter(|stage| stage.contains(path)) else {
            return Ok(false);
        };
        stage.verify_parent()?;
        // Ein fremder, vor dem Parken geöffneter Unterverzeichnis-FD kann einen
        // Altbaum weiter verändern. Daher niemals rekursiv als eigenen Temp löschen.
        if !proof.entry.is_file || proof.entry.is_symlink {
            return Ok(false);
        }
        let current = crate::local_fs::stat_beneath(&stage.directory, path)?;
        if current.device != proof.entry.device
            || current.inode != proof.entry.inode
            || !current.is_file
            || current.is_symlink
        {
            return Err(crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::PlanDrift,
                "Geparkte Sicherungsidentität änderte sich",
            ));
        }
        self.cleanup_owned_temp(path)
    }

    fn staged_backup_is_retained(&self, path: &Path, proof: &ProofListEntry) -> io::Result<bool> {
        let Some(stage) = self.staging.as_ref().filter(|stage| stage.contains(path)) else {
            return Ok(false);
        };
        stage.verify_parent()?;
        if !stage.directory.is_private()? {
            return Ok(false);
        }
        match crate::local_fs::stat_beneath(&stage.directory, path) {
            Ok(current) => Ok(current.device == proof.entry.device
                && current.inode == proof.entry.inode
                && current.is_dir == proof.entry.is_dir
                && current.is_file == proof.entry.is_file
                && current.is_symlink == proof.entry.is_symlink),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn restore_staged_backup(
        &self,
        from: &Path,
        to: &Path,
        proof: &ProofListEntry,
    ) -> Result<bool, MutationFailure> {
        let Some(stage) = self.staging.as_ref().filter(|stage| stage.contains(from)) else {
            return Ok(false);
        };
        let check = (|| -> io::Result<()> {
            if !stage.directory.is_private()? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Staging verlor seine privaten Rechte",
                ));
            }
            let current = crate::local_fs::stat_beneath(&stage.directory, from)?;
            if current.device != proof.entry.device
                || current.inode != proof.entry.inode
                || current.is_dir != proof.entry.is_dir
                || current.is_file != proof.entry.is_file
                || current.is_symlink != proof.entry.is_symlink
            {
                return Err(crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::PlanDrift,
                    "Sicherungsidentität änderte sich vor dem Rückweg",
                ));
            }
            Ok(())
        })();
        check.map_err(MutationFailure::Unchanged)?;
        self.rename_noreplace(from, to).map_err(|error| {
            if crate::error_code(&error) == crate::ErrorCode::OutcomeUnclear {
                MutationFailure::OutcomeUnclear(error)
            } else {
                MutationFailure::Unchanged(error)
            }
        })?;
        Ok(true)
    }

    fn staging_candidate(&self, target: &Path, role: &str) -> io::Result<PathBuf> {
        let path = match &self.staging {
            Some(stage) => stage.candidate(target, role),
            None => crate::atomic_file::sibling_candidate(target, role),
        }?;
        if let Some(stage) = self.neighbor_staging() {
            stage.candidate(&path)?;
        }
        Ok(path)
    }

    fn cleanup_owned_temp(&self, path: &Path) -> io::Result<bool> {
        let Some(stage) = self.staging.as_ref().filter(|stage| stage.contains(path)) else {
            return Ok(false);
        };
        if !stage.directory.is_private()? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Staging ist nicht mehr privat",
            ));
        }
        stage.record("cleanup_intent", path, None)?;
        crate::local_fs::remove_file_beneath(&stage.directory, path)?;
        crate::local_fs::sync_dir_beneath(&stage.directory, stage.directory.path())?;
        stage.record("cleaned", path, None)?;
        Ok(true)
    }

    fn is_local(&self) -> bool {
        true
    }

    /// Das lokale Listing liefert den exakten Rohnamen des Dateisystems. Ob der
    /// Mount selbst case-insensitiv ist, spielt dabei keine Rolle: Erscheinen zwei
    /// Namen nebeneinander, sind es zwei verschiedene Einträge.
    fn has_exact_names(&self) -> bool {
        true
    }

    fn file_replace_class(&self) -> FileReplaceClass {
        // Das lokale Rename ersetzt die sichtbare Datei atomar, kann die Altversion
        // ohne bedingten Backup-Cleanup aber nicht wiederherstellbar halten. Diese
        // begrenzte Wirkung ist deshalb sichtbar Best Effort statt „Assured“.
        FileReplaceClass::BestEffort
    }

    fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
        match self.root_for(path) {
            Some(root) => crate::local_fs::list_beneath(root, path),
            None => crate::local_fs::list(path),
        }
    }

    fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
        let entries = match self.root_for(path) {
            Some(root) => crate::local_fs::list_complete_beneath(root, path)?,
            None => crate::local_fs::list_complete(path)?,
        };
        Ok(CompleteDirectoryListing {
            entries,
            provider_token: None,
            complete: true,
        })
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        if let Some(files) = &self.verified_reads {
            return files.stat(path);
        }
        match self.root_for(path) {
            Some(root) => crate::local_fs::stat_beneath(root, path),
            None => crate::local_fs::stat(path),
        }
    }

    fn exists(&self, path: &Path) -> bool {
        // Descriptor-relatives stat folgt auch dem finalen Symlink NICHT; dadurch zählt
        // weiterhin selbst ein toter Symlink als existierender Eintrag.
        self.stat(path).is_ok()
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        match self.root_for(path) {
            Some(root) => crate::local_fs::create_dir_all_beneath(root, path),
            None => crate::local_fs::create_dir_all(path),
        }
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        if let Some(stage) = &self.staging {
            stage.verify_parent()?;
            stage.record("mkdir_intent", path, None)?;
        }
        match self.root_for(path) {
            Some(root) => crate::local_fs::create_dir_beneath(root, path),
            None => crate::local_fs::create_dir(path),
        }?;
        if let Some(stage) = &self.staging {
            stage
                .verify_parent()
                .and_then(|()| stage.record("mkdir_applied", path, None))
                .map_err(|error| {
                    crate::coded_error(
                        io::ErrorKind::Other,
                        crate::ErrorCode::OutcomeUnclear,
                        format!(
                            "Ordneranlage hatte mögliche Wirkung ({error}); Recovery: {}",
                            stage.directory.path().display()
                        ),
                    )
                })?;
        }
        Ok(())
    }

    fn create_dir_with_removal_proof(
        &self,
        path: &Path,
    ) -> Result<Option<ProofListEntry>, MutationFailure> {
        self.create_dir(path).map(|()| None).map_err(|error| {
            if crate::error_code(&error) == crate::ErrorCode::OutcomeUnclear {
                MutationFailure::OutcomeUnclear(error)
            } else {
                MutationFailure::Unchanged(error)
            }
        })
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        if let Some(files) = &self.verified_reads {
            return files.open(path);
        }
        let file = match self.root_for(path) {
            Some(root) => crate::local_fs::open_read_beneath(root, path)?,
            None => crate::local_fs::open_read(path)?,
        };
        Ok(Box::new(file))
    }

    fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let file = match self.root_for(path) {
            Some(root) => crate::local_fs::open_write_beneath(root, path)?,
            None => crate::local_fs::open_write(path)?,
        };
        Ok(Box::new(file))
    }

    fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let file = match self.root_for(path) {
            Some(root) => crate::local_fs::open_write_new_beneath(root, path)?,
            None => crate::local_fs::open_write_new(path)?,
        };
        if let Some(stage) = self.staging.as_ref().filter(|stage| stage.contains(path)) {
            stage
                .inherit_file(&file)
                .and_then(|()| stage.record("created", path, None))
                .map_err(|error| {
                    crate::coded_error(
                        io::ErrorKind::Other,
                        crate::ErrorCode::OutcomeUnclear,
                        format!(
                            "Temp angelegt, Vorbereitung nicht bestätigt ({error}); Recovery: {}",
                            path.display()
                        ),
                    )
                })?;
        }
        Ok(Box::new(file))
    }

    fn open_write_new_with_removal_proof(
        &self,
        path: &Path,
    ) -> Result<(Box<dyn Write + Send>, Option<ProofListEntry>), MutationFailure> {
        self.open_write_new(path)
            .map(|writer| (writer, None))
            .map_err(|error| {
                if crate::error_code(&error) == crate::ErrorCode::OutcomeUnclear {
                    MutationFailure::OutcomeUnclear(error)
                } else {
                    MutationFailure::Unchanged(error)
                }
            })
    }

    fn open_sparse_read(&self, path: &Path) -> io::Result<Box<dyn SparseRead>> {
        let file = match self.root_for(path) {
            Some(root) => crate::local_fs::open_read_beneath(root, path)?,
            None => crate::local_fs::open_read(path)?,
        };
        Ok(Box::new(file))
    }

    fn open_sparse_write_new(&self, path: &Path) -> io::Result<Box<dyn SparseWrite>> {
        let file = match self.root_for(path) {
            Some(root) => crate::local_fs::open_write_new_beneath(root, path)?,
            None => crate::local_fs::open_write_new(path)?,
        };
        if let Some(stage) = self.staging.as_ref().filter(|stage| stage.contains(path)) {
            stage
                .inherit_file(&file)
                .and_then(|()| stage.record("created", path, None))
                .map_err(|error| {
                    crate::coded_error(
                        io::ErrorKind::Other,
                        crate::ErrorCode::OutcomeUnclear,
                        format!(
                            "Temp angelegt, Vorbereitung nicht bestätigt ({error}); Recovery: {}",
                            path.display()
                        ),
                    )
                })?;
        }
        Ok(Box::new(file))
    }

    fn sync_file(&self, path: &Path) -> io::Result<()> {
        let result = match self.root_for(path) {
            Some(root) => crate::local_fs::sync_file_beneath(root, path),
            None => crate::local_fs::sync_file(path),
        };
        self.finish_sync(result, self.sync_execution().is_none_or(|p| p.file_flush))
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        let result = match self.root_for(path) {
            Some(root) => crate::local_fs::sync_dir_beneath(root, path),
            None => crate::local_fs::sync_dir(path),
        };
        self.finish_sync(
            result,
            self.sync_execution().is_none_or(|p| p.directory_flush),
        )
    }

    fn set_mtime(&self, path: &Path, mtime: i64) -> io::Result<()> {
        // std kann mtime nicht setzen → filetime-Crate am sicher geöffneten Handle.
        // atime bleibt unverändert.
        match self.root_for(path) {
            Some(root) => crate::local_fs::set_mtime_beneath(root, path, mtime),
            None => crate::local_fs::set_mtime(path, mtime),
        }
    }

    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        match self.root_for(path) {
            Some(root) => crate::local_fs::set_mode_beneath(root, path, mode),
            None => crate::local_fs::set_mode(path, mode),
        }
    }

    fn set_metadata_owner(&self, path: &Path, owner: crate::metadata::Owner) -> io::Result<()> {
        match self.root_for(path) {
            Some(root) => crate::local_fs::set_metadata_owner_beneath(root, path, owner),
            None => crate::local_fs::set_metadata_owner(path, owner),
        }
    }

    fn set_metadata_normal_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        match self.root_for(path) {
            Some(root) => crate::local_fs::set_metadata_normal_mode_beneath(root, path, mode),
            None => crate::local_fs::set_metadata_normal_mode(path, mode),
        }
    }

    fn set_metadata_special_bits(&self, path: &Path, bits: u32) -> io::Result<()> {
        match self.root_for(path) {
            Some(root) => crate::local_fs::set_metadata_special_bits_beneath(root, path, bits),
            None => crate::local_fs::set_metadata_special_bits(path, bits),
        }
    }

    fn metadata_capabilities(&self, root: &Path) -> io::Result<MetadataCapabilities> {
        crate::local_fs::metadata_capabilities(self.root_for(root), root)
    }

    fn metadata_manifest(&self, path: &Path) -> io::Result<MetadataManifest> {
        match self.root_for(path) {
            Some(root) => crate::local_fs::metadata_manifest_beneath(root, path),
            None => crate::local_fs::metadata_manifest(path),
        }
    }

    fn write_acl_exact(&self, path: &Path, acl: Option<&crate::metadata::Acl>) -> io::Result<()> {
        match self.root_for(path) {
            Some(root) => crate::local_fs::write_acl_exact_beneath(root, path, acl),
            None => crate::local_fs::write_acl_exact(path, acl),
        }
    }

    fn read_xattrs(&self, path: &Path) -> io::Result<Vec<XattrValue>> {
        match self.root_for(path) {
            Some(root) => crate::local_fs::read_xattrs_beneath(root, path),
            None => crate::local_fs::read_xattrs(path),
        }
    }

    fn write_xattrs_exact(&self, path: &Path, xattrs: &[XattrValue]) -> io::Result<()> {
        match self.root_for(path) {
            Some(root) => crate::local_fs::write_xattrs_exact_beneath(root, path, xattrs),
            None => crate::local_fs::write_xattrs_exact(path, xattrs),
        }
    }

    fn open_fork_read(&self, path: &Path, kind: ForkKind) -> io::Result<Box<dyn Read + Send>> {
        match kind {
            ForkKind::Resource => match self.root_for(path) {
                Some(root) => crate::local_fs::open_resource_fork_read_beneath(root, path),
                None => crate::local_fs::open_resource_fork_read(path),
            },
        }
    }

    fn open_fork_write_new(
        &self,
        path: &Path,
        kind: ForkKind,
    ) -> io::Result<Box<dyn Write + Send>> {
        match kind {
            ForkKind::Resource => match self.root_for(path) {
                Some(root) => crate::local_fs::open_resource_fork_write_new_beneath(root, path),
                None => crate::local_fs::open_resource_fork_write_new(path),
            },
        }
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        match self.root_for(path) {
            Some(root) => crate::local_fs::remove_file_beneath(root, path),
            None => crate::local_fs::remove_file(path),
        }
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        match self.root_for(path) {
            Some(root) => crate::local_fs::remove_dir_all_beneath(root, path),
            None => crate::local_fs::remove_dir_all(path),
        }
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        match self.root_for(path) {
            Some(root) => crate::local_fs::remove_dir_beneath(root, path),
            None => crate::local_fs::remove_dir(path),
        }
    }

    fn remove_if_matches(&self, _path: &Path, _expected: &ProofListEntry) -> io::Result<()> {
        // `unlinkat` arbeitet auch bei einem descriptor-gehaltenen Elternordner am
        // Namen. Der Kernel bietet auf macOS und Linux dabei keine Bedingung
        // „Gerät/Inode müssen noch dem geprüften Eintrag entsprechen“. Ein zweites
        // `statat` würde deshalb nur ein neues Rennen eröffnen. Bis eine echte
        // handle-gebundene Primitive verfügbar ist, bleibt die Sicherung unangetastet.
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Lokales Dateisystem bietet keine atomare bedingte Entfernung nach Inode",
        ))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.staged_rename(from, to, None)
    }

    fn rename_plain_once(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.staged_rename(from, to, None)?;
        crate::atomic_file::sync_rename_parents(self, from, to).map_err(|error| {
            crate::error::with_recovery_context(
                crate::coded_error(io::ErrorKind::Other, crate::ErrorCode::OutcomeUnclear,
                    format!("Best-Effort-Rename wirkte bereits, aber der Eltern-Sync scheiterte ({error})")),
                &[&error], &[from, to],
            )
        })
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.staged_rename(from, to, Some(rustix::fs::RenameFlags::NOREPLACE))
    }

    fn restore_from_bound_quarantine_run(
        &self,
        request: BoundQuarantineRestoreRequest<'_>,
    ) -> Result<(), MutationFailure> {
        let crate::real_mode::RootIdentity::Local(expected_run_root) = request.expected_run_root
        else {
            return Err(MutationFailure::Unchanged(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Lokaler Quarantäne-Restore erhielt keine lokale Laufidentität",
            )));
        };
        crate::local_fs::restore_from_bound_quarantine_run(
            self.root.as_ref(),
            request.run_root,
            expected_run_root,
            request.source,
            request.target_root,
            request.expected_target_root,
            request.target,
            request.expected_source,
            request.intent_manifest,
            request.expected_intent_manifest,
            request.expected_intent_digest,
        )
    }

    fn supports_restore_from_bound_quarantine_run(&self) -> bool {
        true
    }

    fn purge_bound_quarantine_run_contents(
        &self,
        request: BoundQuarantinePurgeRequest<'_>,
    ) -> Result<(), MutationFailure> {
        let crate::real_mode::RootIdentity::Local(expected_run_root) = request.expected_run_root
        else {
            return Err(MutationFailure::Unchanged(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Lokales Quarantäne-Leeren erhielt keine lokale Laufidentität",
            )));
        };
        crate::local_fs::purge_bound_quarantine_run_contents(
            self.root.as_ref(),
            request.run_root,
            expected_run_root,
            request.intent,
            request.expected_intent,
            request.expected_intent_digest,
        )
    }

    fn supports_purge_bound_quarantine_run_contents(&self) -> bool {
        true
    }

    fn verify_bound_quarantine_run_empty(
        &self,
        request: BoundQuarantinePurgeRequest<'_>,
    ) -> Result<(), MutationFailure> {
        let crate::real_mode::RootIdentity::Local(expected_run_root) = request.expected_run_root
        else {
            return Err(MutationFailure::Unchanged(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Lokaler Quarantäne-Leerheitsbeleg erhielt keine lokale Laufidentität",
            )));
        };
        crate::local_fs::verify_bound_quarantine_run_empty(
            self.root.as_ref(),
            request.run_root,
            expected_run_root,
            request.intent,
            request.expected_intent,
            request.expected_intent_digest,
        )
    }

    fn rename_replace_plain_once(&self, from: &Path, to: &Path) -> io::Result<()> {
        // Dieser Pfad wird ausschließlich nach dem vollständigen frischen
        // Elternlisting und der ausdrücklichen Best-Effort-Bestätigung aufgerufen.
        // Das descriptor-relative Rename ist genau eine atomare Namenswirkung; es
        // verspricht bewusst keine wiederherstellbare Sicherung der Altdatei.
        self.rename(from, to)
    }

    fn rename_noreplace_with_removal_proof(
        &self,
        from: &Path,
        to: &Path,
    ) -> Result<Option<ProofListEntry>, MutationFailure> {
        self.rename_noreplace(from, to).map_err(|error| {
            if crate::error_code(&error) == crate::ErrorCode::OutcomeUnclear {
                MutationFailure::OutcomeUnclear(error)
            } else {
                MutationFailure::Unchanged(error)
            }
        })?;
        if self
            .staging
            .as_ref()
            .is_some_and(|stage| stage.contains(to))
        {
            let entry = self.stat(to).map_err(MutationFailure::OutcomeUnclear)?;
            return Ok(Some(ProofListEntry {
                entry,
                raw_name: to.file_name().unwrap().as_encoded_bytes().to_vec(),
                safely_decoded: true,
                provider_token: None,
            }));
        }
        Ok(None)
    }

    fn rename_exchange(&self, left: &Path, right: &Path) -> io::Result<()> {
        self.staged_rename(left, right, Some(rustix::fs::RenameFlags::EXCHANGE))
    }

    fn create_hard_link_noreplace(&self, existing: &Path, new: &Path) -> io::Result<()> {
        if self.staging.is_some() {
            crate::local_fs::hard_link_between(
                self.root_for(existing),
                existing,
                self.root_for(new),
                new,
            )
        } else {
            match &self.root {
                Some(root) => {
                    crate::local_fs::create_hard_link_noreplace_beneath(root, existing, new)
                }
                None => crate::local_fs::create_hard_link_noreplace(existing, new),
            }
        }
    }

    fn anchored_root(&self) -> Option<&Path> {
        self.root.as_ref().map(crate::local_fs::RootHandle::path)
    }

    fn anchored_identity(&self) -> Option<(u64, u64)> {
        self.root
            .as_ref()
            .map(crate::local_fs::RootHandle::identity)
    }

    fn anchor_is_current(&self) -> io::Result<bool> {
        for root in [self.root.as_ref(), self.file_commit_root.as_deref()]
            .into_iter()
            .flatten()
            .chain(self.sync_parents().iter())
            .chain(self.neighbor_staging().map(|stage| &stage.parent))
        {
            if !root.is_current()? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn local_root_identity(&self, path: &Path) -> io::Result<LocalRootIdentity> {
        match self.root_for(path) {
            Some(root) => crate::local_fs::root_identity_beneath(root, path),
            None => crate::local_fs::root_identity(path),
        }
    }

    fn available_bytes(&self, path: &Path) -> io::Result<u64> {
        match self.root_for(path) {
            Some(root) => crate::local_fs::available_bytes_beneath(root, path),
            None => crate::local_fs::available_bytes(path),
        }
    }
}

#[cfg(test)]
mod local_sync_flush_tests {
    use super::*;

    #[test]
    fn bestaetigter_flushverzicht_akzeptiert_nur_nichtunterstuetzt() {
        let weak = crate::real_mode::LocalSyncExecution {
            new_file_commit: FileCommitClass::Assured,
            file_flush: false,
            directory_flush: false,
            irreversible_replacements: 0,
        };
        let vfs = LocalVfs::new().with_sync_execution(Some(weak));
        for code in [libc::ENOTSUP, libc::EOPNOTSUPP, libc::ENOSYS] {
            assert!(vfs
                .finish_sync(Err(io::Error::from_raw_os_error(code)), false)
                .is_ok());
            assert!(vfs
                .finish_sync(Err(io::Error::from_raw_os_error(code)), true)
                .is_err());
            assert!(LocalVfs::new()
                .finish_sync(Err(io::Error::from_raw_os_error(code)), false)
                .is_err());
        }
        for code in [libc::EIO, libc::ENOSPC, libc::EACCES] {
            assert_eq!(
                vfs.finish_sync(Err(io::Error::from_raw_os_error(code)), false)
                    .unwrap_err()
                    .raw_os_error(),
                Some(code)
            );
        }
        let unclear = crate::coded_error(
            io::ErrorKind::Other,
            crate::ErrorCode::OutcomeUnclear,
            "Rename wirkte bereits; Flush scheiterte",
        );
        assert_eq!(
            crate::error_code(&vfs.finish_sync(Err(unclear), false).unwrap_err()),
            crate::ErrorCode::OutcomeUnclear
        );
    }
}
