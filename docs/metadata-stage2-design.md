# Metadaten-Vertrag Stufe 2

Stand: 2026-10-07; ursprünglicher Stufe-2-Abschluss am 2026-07-11.

Ergänzung 2026-10-06: Lokale Fähigkeiten sind an den gehaltenen Root und dessen
Mount gebunden. Gültige macOS-Volume-Bits beziehungsweise der konkrete Linux-Treiber
mit lesenden ACL-/xattr-Abfragen ersetzen plattformweite Zusagen. Unbekannte
Dateisysteme bleiben unbekannt; nur lesbare Mounts versprechen keine Schreibwege.
Image-Gegenläufe und Grenzen: [Lokale Metadatenfähigkeiten](local-metadata-mounts.md).

Status: Abgeschlossen. Für reguläre Dateien binden Sync-Plan und
Ausführung Owner, ACLs, normale Rechte, Sonderbits, Linux-xattrs und den macOS-Ressourcen-
Fork sowie Sparse-Extents an Quell- und Zielmanifeste und übertragen sie über die
atomare Temp-Datei.
Verzeichnisse erhalten Owner, ACLs, xattrs, Rechte, mtime und Sonderbits gemeinsam post-order
mit Vorprüfung und Abschluss-Readback. `Portable`, `NativeBestEffort` und
`NativeStrict` sind in der GUI wählbar; native Profile erzwingen `ContentHash`, Mirror
bleibt für BestEffort gesperrt und `Portable` bleibt Standard. `NativeStrict` wird
erst nach vollständigem Scan ohne unbekannte Fähigkeit oder pfadgenaue Auslassung
freigegeben. Vorschau und Abschluss bilanzieren Inhalt, Metadatenänderungen,
Hardlinks, Nichtunterstütztes, Rechtefehler und inkompatible ACL-Modelle sowie jede
Klasse pro Pfad. ACL-, xattr-, Fork-, Owner-/Sonderbit-, Sparse- und Hardlink-Proben
sind auf macOS und Linux grün. Der Backup-Hinweis lebt seit 2026-07-23 als
Klartext-Zeile unter der Metadaten-Profilwahl der Sync-Ansicht (statt als
Dauerwarnung in der Seitenleiste) und bleibt dort bis zur separaten Restore-Probe
und zum Realmodus-Gate bestehen.

Abschlussverifikation 2026-07-11: macOS 40 Kern-Modultests, 24 Dateioperations- und
55 Sync-Tests; Linux 41 Kern-Modultests, 24 Dateioperations- und 57 Sync-Tests. Der
Linux-Gegenlauf enthält einen erfolgreichen `NativeStrict`-xattr-Roundtrip sowie die
fail-closed Ablehnung unbekannter Fähigkeiten und inkompatibler ACL-Modelle.

## Ziel und Nicht-Ziel

Stufe 2 erweitert den heutigen Vertrag aus Datenstrom, `mtime` und normalen
POSIX-`rwx`-Bits um:

- numerischen Eigentümer und Gruppe,
- POSIX-Sonderbits (`setuid`, `setgid`, `sticky`),
- Access Control Lists (ACLs),
- Extended Attributes einschließlich Finder-Informationen und macOS-Ressourcen-Fork,
- Sparse-Struktur regulärer Dateien,
- Hardlink-Beziehungen innerhalb eines geplanten Baums.

Der Vertrag verspricht keine magische Übersetzung zwischen inkompatiblen
Sicherheitsmodellen. Eine macOS-NFSv4-ACL kann beispielsweise nicht verlustfrei in eine
Linux-POSIX-ACL umgedeutet werden. Doppeldecker muss Fähigkeiten deshalb vor dem Plan
ermitteln, Unterschiede sichtbar machen und im strikten Modus vor jeder Mutation
abbrechen.

Nicht Teil dieser Stufe sind Dateisystem-Snapshots, APFS-Klone als dauerhaftes
Semantikmerkmal, Linux-Capabilities als ausführbare Berechtigung, SELinux-Policy-
Übersetzung, unveränderliche Dateiflags, Creation Time sowie die Identität gemounteter
oder entfernter Quellen. Diese Punkte bleiben eigene Realmodus-Gates.

## Verbindliche Profile

| Profil | Verhalten | Zulässiger Einsatz |
|---|---|---|
| `Portable` | heutiger Vertrag: Daten, `mtime`, normale `rwx`-Bits | sicherer Standard, Remote-Ziele mit wenigen Fähigkeiten |
| `NativeStrict` | alle an der Quelle vorhandenen Stufe-2-Merkmale müssen am Ziel exakt darstellbar sein | lokales Backup und explizit kompatible Provider |
| `NativeBestEffort` | überträgt darstellbare Merkmale und bilanziert jedes ausgelassene Merkmal | ausdrücklich gewählter Modus mit ContentHash; für Mirror und bidirektionale Paare gesperrt |

`NativeStrict` darf nicht automatisch in `Portable` oder `NativeBestEffort`
zurückfallen. `Mirror` mit einem unvollständigen Metadaten-Scan bleibt komplett
gesperrt, genauso wie heute bei ausgelassenen Symlinks oder Spezialdateien.

## Fähigkeiten statt Plattformnamen

Jeder `Vfs` liefert vor dem Scan ein `MetadataCapabilities`-Objekt. Entscheidungen
dürfen nicht allein auf `cfg(target_os)` beruhen, weil auch lokale Dateisysteme,
Mount-Optionen und Remote-Server unterschiedliche Fähigkeiten haben.

```rust
pub struct MetadataCapabilities {
    pub owner: Capability,
    pub special_mode_bits: Capability,
    pub acl_model: AclModel,
    pub xattrs: Capability,
    pub resource_fork: Capability,
    pub sparse_extents: Capability,
    pub hardlinks: Capability,
}

pub enum Capability {
    Exact,
    ReadOnly,
    Unsupported,
    Unknown,
}

pub enum AclModel {
    Posix,
    Nfs4,
    Unsupported,
    Unknown,
}
```

`Unknown` ist im strikten Profil ein Fehler, keine Einladung zum Probieren während
der Ausführung. Die Fähigkeitenabfrage am gehaltenen lokalen Root ist rein lesend.
Zusätzliche wirkungsbezogene Restore-Proben laufen nur in eigenen Wegwerfobjekten
auf dem konkreten Ziel-Mount; ein Erfolg auf einem anderen Mount genügt nicht.

Aktueller Stand: Auf einem konkret als fähig erkannten Mount kann der lokale Adapter
Owner und Sonderbits descriptor-relativ mit exaktem Readback übertragen; ein
Berechtigungsfehler bricht vor dem Datei-Commit ab. Daraus folgt keine pauschale
`Exact`-Zusage für FAT/exFAT, unbekannte Treiber oder nur lesbare Mounts. Linux-xattrs, plattformeigene ACLs, Sparse-Extents und
der macOS-Ressourcen-Fork besitzen einen Transfer mit Abschluss-Readback; macOS-xattrs
bleiben wegen systemverwalteter Attribute `ReadOnly`, der Ressourcen-Fork ist außerhalb
von macOS `Unsupported`. Lokale Hardlinks werden descriptor-relativ und atomar
wiederaufgebaut und deshalb als `Exact` gemeldet. Der spätere Realmodus verlangt
zusätzlich weiterhin eine Probe auf dem
konkreten Ziel-Mount und für die tatsächlich geplanten numerischen IDs.

## Planrepräsentation

Große xattrs oder Ressourcen-Forks gehören nicht als Bytevektor in den UI-Plan. Der
Scan erzeugt pro Eintrag ein kanonisches Manifest mit Größen und BLAKE3-Digests:

```rust
pub struct MetadataManifest {
    pub owner: Option<Owner>,
    pub mode: u32,
    pub acl: Option<Acl>,
    pub xattrs: Vec<NamedBlobSummary>,
    pub resource_fork: Option<BlobSummary>,
    pub sparse_extents: Option<Vec<DataExtent>>,
    pub hardlink_group: Option<u64>,
    pub digest: [u8; 32],
}
```

Die kanonische Digest-Reihenfolge ist fest: Versionsbyte, Owner, Gruppe, Modus,
ACL-Modell plus serialisierte ACL, xattr-Namen byteweise sortiert mit Länge und Inhalt,
Ressourcen-Fork-Digest, Extent-Liste und Hardlink-Gruppennummer. Plattformabhängige
Strukturen werden vor dem Hash in eigene DTOs übersetzt; rohe C-Strukturen werden nie
gehasht oder gespeichert.

Der bestätigte `SyncPlan` bindet sowohl das Quellmanifest als auch den erwarteten
Zielzustand. `execute()` liest unmittelbar vor jeder Mutation beide Manifeste erneut.
Schon ein geändertes xattr, eine ACL-Änderung oder eine neue Hardlink-Beziehung ist
Plan-Drift und bricht den Lauf ab.

## VFS-Erweiterung

Der bestehende Datenpfad bleibt protokollneutral. Ergänzt werden explizite,
fähigkeitsgeprüfte Methoden statt eines lokalen Sonderwegs in `sync.rs`:

```rust
trait Vfs {
    fn metadata_capabilities(&self, root: &Path) -> io::Result<MetadataCapabilities>;
    fn metadata_manifest(&self, path: &Path) -> io::Result<MetadataManifest>;
    fn open_fork_read(&self, path: &Path, fork: ForkKind) -> io::Result<Box<dyn Read>>;
    fn open_fork_write_new(&self, path: &Path, fork: ForkKind) -> io::Result<Box<dyn Write>>;
    fn apply_metadata(&self, path: &Path, metadata: &MetadataPayload) -> io::Result<()>;
    fn create_hardlink(&self, existing: &Path, new: &Path) -> io::Result<()>;
}
```

Die konkrete Signatur darf beim Implementieren an die bestehenden assoziierten
Reader-/Writer-Typen angepasst werden. Unverhandelbar sind Fähigkeitsprobe,
descriptor-relative lokale Implementierung, Planbindung und ein expliziter
`Unsupported`-Fehler statt stiller Auslassung.

Auch wenn keine native Klasse am Ziel schreibbar ist, bleibt
`NativeBestEffort` für planbindbare Inhalte verfügbar. Das Quellmanifest und die
pfadgenaue Auslassungsbilanz bleiben erhalten; der Plan verspricht keine nativen
Metadatenänderungen. Ein ausgelassener nativer Modus darf nicht durch einen
nachfolgenden portablen `chmod` doch angewandt werden. Lokale Manifeste lassen
nur am gehaltenen Dateisystem ausdrücklich nicht unterstützte Klassen aus;
unbekannte Fähigkeiten und tatsächliche Lesefehler werden weiterhin nicht als
Abwesenheit gewertet. Ein Provider ohne Manifestvertrag erhält dadurch keine
erfundene Metadatenvollständigkeit.

Der native/exFAT-Gegenlauf prüft unter Linux auch Kopieren und Aktualisieren.
Auf macOS-exFAT sind Vorschau und Manifest geprüft; der Inhaltslauf bleibt wegen
fehlendem exklusivem Rename im bisherigen lokalen Commitweg ein offener
VFS-Folgepunkt. Dieser Vorbehalt betrifft die Commitgarantie und wird nicht als
Metadatenerfolg ausgegeben.

## Plattformvertrag

### Eigentümer und Sonderbits

Der lokale VFS-Unterbau und der Sync-Pfad trennen die Phasen verbindlich:
`set_metadata_owner`
nutzt `fchown` und exakten uid/gid-Readback, `set_metadata_normal_mode` löscht bewusst
alle Sonderbits, und `set_metadata_special_bits` setzt ausschließlich `0o7000` ganz
zuletzt mit erneutem Readback. Ungültige `-1`-IDs oder vermischte Bitmasken brechen vor
dem Systemaufruf ab. Ein `fchown`-Fehler entfernt nur die neue Temp-Datei; ein
vorhandenes Ziel bleibt unverändert.

- Erfasst werden numerische `uid`/`gid`; Namen sind nur Anzeige und kein stabiler
  Schlüssel.
- `NativeStrict` darf fremde IDs nur anwenden, wenn eine Zielprobe mit denselben IDs
  erfolgreich war. Fehlende Rechte sind ein Vorprüfungsfehler.
- Reihenfolge: Daten schreiben, `fchown`, ACL/xattrs, normale Rechte, Zeitstempel,
  `fsync`, Sonderbits ganz zuletzt. `chown` darf setuid/setgid löschen; POSIX beschreibt
  dieses Sicherheitsverhalten ausdrücklich. Deshalb wäre „Modus vor Owner“ falsch.
- Sonderbits werden nie auf einer noch unverifizierten Temp-Datei wirksam. Erst nach
  Inhalts- und Metadatenprüfung werden sie gesetzt und die Temp-Datei atomar committed.
- Sticky ist nur für Ordner relevant. Verzeichnismetadaten werden post-order gesetzt,
  nachdem ihre Kinder vollständig angelegt wurden.

Descriptor-basierte `fchown`-/`fchownat`- und `fchmod`-Zugriffe sind in der bereits
verwendeten [`rustix`-API](https://docs.rs/rustix/latest/rustix/fs/index.html) vorhanden.
Der POSIX-Hinweis zum Löschen der Sonderbits bei `chown` und zum Race-Schutz von
`fchownat` ist in der [Open-Group-Spezifikation](https://pubs.opengroup.org/onlinepubs/9699919799.2013edition/functions/chown.html)
dokumentiert.

### Extended Attributes

Der VFS-Unterbau liest Namen und Werte bereits ausschließlich am geöffneten Handle,
wiederholt Größenabfrage und Lesen bei `ERANGE`, verlangt zwei identische Reads unter
einem stabilen ctime-Snapshot und verifiziert exaktes Schreiben per Readback. Linux
besteht den Exact-Roundtrip. Auf
macOS bleibt die Fähigkeit bewusst `ReadOnly`: Das System fügt unter anderem
`com.apple.provenance` selbst hinzu und Apple beschreibt generell, dass das System die
zu synchronisierenden beziehungsweise lokal zu erhaltenden Attribute bestimmt
([Apple File Provider](https://developer.apple.com/documentation/fileprovider/nsfileprovideritemprotocol/extendedattributes?changes=_5)). Ohne dokumentierten stabilen
Übertragungsvertrag wird dieses Sicherheitsattribut weder still ausgelassen noch als
exakt kopierbar behauptet.

- Namen und Werte werden als Bytes behandelt. Die UI darf Namen verlustfrei escaped
  anzeigen, aber nie per verlustbehaftetem UTF-8 Round-Trip verändern.
- Lesen und Schreiben erfolgt am bereits geöffneten Eintrag mit `flistxattr`,
  `fgetxattr` und `fsetxattr`. `rustix` 1.1.4 stellt diese Funktionen bereit.
- `com.apple.ResourceFork` wird auf macOS aus dieser allgemeinen Liste herausgenommen
  und ausschließlich durch den separaten Fork-Inhaltsstrom verarbeitet.
- Der Scan wiederholt Größenabfrage und Lesen bei `ERANGE`; eine fortlaufende Änderung
  endet nach einer kleinen festen Retry-Grenze als Plan-Drift.
- Ein nicht lesbares Attribut ist in `NativeStrict` ein Fehler. Insbesondere Linux-
  Namespaces `security`, `system` und `trusted` haben andere Berechtigungen als `user`;
  sie dürfen nicht still ausgelassen werden. Die Namespace-Regeln und Größenlimits
  beschreibt die [Linux-xattr-Dokumentation](https://man7.org/linux/man-pages/man7/xattr.7.html).
- Ziel-xattrs, die im bestätigten Quellmanifest fehlen, werden bei einem exakten
  Replacement entfernt. Bei `AddOnly` auf einem bereits vorhandenen Eintrag bleibt der
  heutige Vertrag unverändert: keine Metadatenänderung.

### macOS-Ressourcen-Fork

Der VFS-Unterbau ist umgesetzt: `ForkKind::Resource` liest und schreibt auf macOS in
Chunks über die positionsfähigen descriptor-basierten `fgetxattr`-/`fsetxattr`-
Systemaufrufe. Zwei identische BLAKE3-Leseläufe unter stabilem ctime binden Größe und
Inhalt ans Manifest; `com.apple.ResourceFork` bleibt aus der normalen xattr-Liste
ausgesondert. Linux liefert für denselben VFS-Aufruf ausdrücklich `Unsupported`.
Schreiben ist nur als `open_fork_write_new` für einen exklusiv angelegten Temp-Eintrag
vorgesehen; vorhandene Forks werden nicht überschrieben.

Der Ressourcen-Fork wird als zweiter Inhaltsstrom behandelt, nicht als gewöhnliches
kleines Metadatum. Apple trennt ihn ebenfalls von der normalen xattr-Menge und wertet
Änderungen als Inhaltsänderung ([Apple File Provider](https://developer.apple.com/documentation/fileprovider/nsfileprovideritemprotocol/extendedattributes?changes=_5)).

- Lokales Lesen/Schreiben erfolgt descriptor-basiert über den vom System unterstützten
  Fork-Zugriff; ein Pfad `..namedfork` darf nicht an der Sicherheitsgrenze neu
  aufgelöst werden.
- Falls die descriptor-basierte System-API nicht in den bestehenden atomaren
  Temp-Dateipfad integrierbar ist, muss der Implementierungssprint stoppen und einen
  kleinen FFI-Adapter für `fcopyfile(..., COPYFILE_XATTR)` beziehungsweise die
  passenden Fork-Aufrufe bauen. Ein Pfad-Fallback ist ausgeschlossen.
- Der Fork erhält einen eigenen BLAKE3-Digest und wird vor dem Commit zurückgelesen.
  Eine Datei gilt erst dann als inhaltsgleich, wenn Daten- und Ressourcen-Fork passen.

Die lokalen macOS-Manpages `copyfile(3)`, `listxattr(2)` und `getxattr(2)` sind für den
Adapter verbindliche Referenz und werden in den Modultests gegen die laufende
Systemversion geprüft.

### ACLs

ACLs werden in ein typisiertes, geordnetes DTO übersetzt. Textausgaben von `getfacl`
oder Benutzernamen sind kein Persistenzformat.

- Linux: Access-ACL und bei Ordnern Default-ACL; POSIX-Einträge einschließlich Maske.
- macOS: geordnete Allow-/Deny-Einträge, UUID-Prinzipale und Vererbungsflags.
- Gleiches Modell: exakte Übertragung.
- Unterschiedliches Modell: `NativeStrict` meldet `IncompatibleAclModel` vor dem
  ersten Schreiben. `NativeBestEffort` darf die ACL nur nach einer eigenen Bestätigung
  auslassen; es findet keine erfundene Rechteübersetzung statt.
- Modusbits und ACL sind gekoppelt. Nach dem Setzen wird beides erneut gelesen und
  gemeinsam geprüft.

Linux-POSIX-ACLs kennen Access- und Default-ACLs sowie eine wirksame Maske
([`acl(5)`](https://man7.org/linux/man-pages/man5/acl.5.html)); macOS hat ein anderes,
geordnetes Modell. Die vorhandene plattformübergreifende `exacl`-Crate bestätigt diese
Modellunterschiede, arbeitet aber mit Pfaden und wäre deshalb nicht direkt Teil der
Sicherheitsgrenze ([API-Referenz](https://docs.rs/exacl/latest/exacl/)). Implementiert ist
ein eigener `AclAdapter` auf offenen Deskriptoren:

- macOS über die Systemfunktionen `acl_get_fd_np`/`acl_set_fd_np`,
- Linux über die binären `system.posix_acl_access`-/`system.posix_acl_default`-xattrs
  am Deskriptor.

Der isolierte [ACL-Spike](acl-adapter-spike.md) und die spätere Implementierung
belegen den headless Linux-Build ohne zusätzliche globale `libacl`-Buildabhängigkeit.
Linux verwendet die binären ACL-xattrs am Deskriptor; eine pfadbasierte API wäre
kein zulässiger Ersatz.

Der frühere Spike ist inzwischen durch `core/src/acl.rs` ersetzt. Typisierte DTOs,
kanonischer Manifest-Digest, exaktes Schreiben/Entfernen und Readback laufen auf beiden
Plattformen. ACLs werden nach dem Owner und vor xattrs sowie den gekoppelten normalen
Modusbits angewandt; Sonderbits bleiben die letzte Phase.

### Sparse Files

- Der Quelladapter ermittelt Datenbereiche mit `SEEK_DATA`/`SEEK_HOLE` und speichert
  eine sortierte, nicht überlappende Extent-Liste.
- Meldet das Dateisystem keine verlässlichen Extents, ist die Fähigkeit `Unknown` oder
  `Unsupported`; aus langen Nullfolgen werden nie heuristisch Löcher erfunden.
- Das Ziel wird auf die logische Länge gesetzt. Nur Daten-Extents werden geschrieben;
  Löcher bleiben unallokiert. Danach werden logische Länge, kompletter Inhaltsdigest
  und — sofern das Ziel Extents exakt meldet — die Extent-Struktur geprüft.
- Ein Remote-Provider ohne Sparse-Protokollfähigkeit kann im portablen Profil den
  logischen Inhalt übertragen, aber nicht `NativeStrict` erfüllen.

Linux dokumentiert ausdrücklich, dass ein Dateisystem Daten und Löcher konservativ
melden darf; deshalb ist „keine gemeldeten Löcher“ nicht automatisch ein Beweis für
eine dichte Datei ([`lseek(2)`](https://www.man7.org/linux/man-pages/man2/lseek.2.html)).
Dieselbe Einschränkung steht in der lokalen macOS-Manpage `lseek(2)`.

### Hardlinks

- Während des Quellscans bildet `(device, inode)` für reguläre Dateien mit `nlink > 1`
  eine laufbezogene Hardlink-Gruppe.
- Die erste Datei einer Gruppe wird normal übertragen. Weitere Namen erhalten eine
  Planaktion `CreateHardlink { existing, new }`.
- Der lokale Adapter nutzt `linkat` relativ zu bereits geöffneten Elternordnern und
  atomar reservierten Zielnamen. Hardlinks über Dateisystemgrenzen sind nicht möglich.
- Hardlink-Gruppen gelten nur innerhalb der vollständig gescannten Auswahl. Erst
  mindestens zwei gewählte Namen bilden eine zu erhaltende Beziehung; externe oder
  ausgeschlossene Partner werden weder behauptet noch gesucht.
- Die Erfassung dieser Quellgruppen hängt nicht von der Zielunterstützung ab.
  Kann das Ziel keine Hardlinks erhalten, bilanziert `NativeBestEffort` die betroffenen
  Namen als ausgelassen und kopiert sie unabhängig. `NativeStrict` lehnt diesen Lauf
  vor jeder Mutation ab.
- Nach Ausführung werden gleiche Ziel-Geräte/Inodes und die erwartete Linkzahl geprüft.
  Ein später eingefügter Zielname wird niemals ersetzt.

`rustix` stellt `linkat` bereits im gewählten `fs`-Feature bereit; die Linux-Semantik
ist in [`link(2)`](https://man7.org/linux/man-pages/man2/link.2.html) dokumentiert.

## Atomare Reihenfolge pro regulärer Datei

1. Quelle descriptor-relativ öffnen und Identität erfassen.
2. Ziel-Fähigkeiten und erwarteten Zielzustand gegen den bestätigten Plan prüfen.
3. Kollisionsfreie Temp-Datei mit `O_EXCL|O_NOFOLLOW` im gebundenen privaten
   C2-Arbeitsbereich anlegen, soweit verfügbar; andernfalls gilt der ausdrücklich
   begrenzte Temp-/Cleanup-Vertrag des VFS.
4. Daten-Extents und optionalen Ressourcen-Fork streamen.
5. Daten- und Fork-Digests durch Zurücklesen prüfen.
6. Owner/Gruppe setzen.
7. ACLs und xattrs setzen; nicht gewünschte Ziel-xattrs gibt es an der neuen Temp-Datei
   konstruktionsbedingt nicht.
8. Normale `rwx`-Bits und `mtime` setzen, Datei und Elternordner synchronisieren.
9. Sonderbits zuletzt setzen, Metadatenmanifest erneut lesen und Digest vergleichen.
10. Temp-Datei mit dem bestehenden exklusiven beziehungsweise quarantänierenden
    Rename committen.
11. Bei Abweichung nur nachweislich eigene Temp-Dateien sicher bereinigen. Unsicher
    gebundene Reste bleiben mit Wiederherstellungspfad erhalten; bestehende Ziele
    werden vor ihrem bestätigten Commit nicht ersetzt.

Ordner werden zuerst mit sicheren Minimalrechten angelegt. Ihre ACLs, endgültigen
Rechte, Zeiten und Sonderbits folgen post-order. So verhindern restriktive geerbte
Rechte nicht das Anlegen der noch geplanten Kinder.

## Fehlerbilanz und UI

Der Plan zählt getrennt:

- `content_changes`,
- `metadata_changes`,
- `hardlink_actions`,
- `unsupported_metadata`,
- `privilege_failures`,
- `incompatible_acl_models`.

Die Vorschau zeigt pro Eintrag die betroffenen Klassen. Ein pauschales „Metadaten
konnten teilweise nicht kopiert werden“ reicht nicht. Nach dem Lauf enthält die Bilanz
für jedes ausgelassene Merkmal Pfad, Klasse und Systemfehler.

Der Backup-Hinweis in der Sync-Ansicht (früher die Seitenleisten-Warnung
„Noch kein Vollbackup“) darf erst entfallen, wenn:

1. `NativeStrict` ausgewählt und vollständig vorgeprüft wurde,
2. Quelle und Ziel alle vorhandenen Stufe-2-Merkmale exakt unterstützen,
3. die Abschlussverifikation keine Auslassung meldet,
4. eine separate Restore-Probe erfolgreich war.

## Pflicht-Tests

Alle Tests verwenden Temp-Verzeichnisse; privilegierte Fälle sind eigene, klar
markierte Integrationstests und laufen nie gegen Originaldaten.

1. xattr mit leerem, kleinem und großem Binärwert; Name mit Nicht-ASCII-Zeichen.
2. xattr-Plan-Drift nach Vorschau stoppt vor jeder Mutation.
3. macOS: `com.apple.FinderInfo` und echter Ressourcen-Fork überleben Round-Trip und
   haben eigene Digests.
4. Linux: Access- und Default-ACL einschließlich Maske; macOS: Allow/Deny-Reihenfolge
   und Vererbungsflags.
5. Inkompatibles ACL-Modell sperrt `NativeStrict` vor dem ersten Schreiben.
6. `fchown`-Fehler lässt ein vorhandenes Ziel unangetastet.
7. setuid/setgid werden erst nach `chown` gesetzt und durch Abschluss-Readback belegt;
   Sticky wird an einem Ordner erhalten.
8. Sparse-Datei mit Daten vor, zwischen und nach Löchern: Inhalt, logische Länge und
   belegte Blöcke bleiben plausibel; unsupported Dateisystem meldet keinen Scheinerfolg.
9. Zwei und drei Hardlinks werden am Ziel wieder derselbe Inode; gleichnamige späte
   Kollision ersetzt nichts.
10. Hardlink über Ziel-Dateisystemgrenze und unlesbares xattr brechen fail-closed ab.
11. Abbruch in jeder Metadatenphase entfernt nur die neue Temp-Datei.
12. Verzeichnismetadaten werden post-order angewandt und erneut gelesen.
13. `Portable` bleibt rückwärtskompatibel zu den vorhandenen Stufe-1-Tests.
14. Komplette Kern-Suite läuft auf macOS und Linux-Testhost; plattformspezifische Tests
    melden eindeutig `unsupported`, statt still übersprungen zu werden.

## Umsetzungsschnitte

1. [x] DTOs, konservative Fähigkeitsprobe und kanonischen Manifest-Digest ohne
   Schreibpfad ergänzen.
2. [x] Linux-xattrs regulärer Dateien an Quell-/Zielmanifest binden, Drift vor und nach
   dem Transfer prüfen und ausschließlich auf die atomare Temp-Datei schreiben. Leere,
   binäre und zu entfernende Ziel-xattrs sind im exakten Readback enthalten.
3. [x] macOS-Ressourcen-Fork regulärer Dateien als zweiten Inhaltsstrom übertragen,
   separat hashen, zurücklesen und in Fortschritt sowie Transferbilanz aufnehmen.
4. [x] Owner/Sonderbits mit der festgelegten Reihenfolge implementieren. Dateien nutzen
   die atomare Temp-Datei; Verzeichnisse erhalten Owner, xattrs, normale Rechte, mtime
   und Sonderbits gemeinsam post-order mit Vorprüfung und Abschluss-Readback.
5. [x] Typisierte ACL-Übertragung auf macOS und Linux-Testhost: geordnete NFSv4-Allow/Deny-
   Einträge mit UUID-Prinzipalen sowie Linux-Access-/Default-POSIX-ACLs einschließlich
   Maske sind descriptor-relativ, manifestgebunden und gemeinsam mit Modusbits
   rückgelesen. Inkompatible Modelle werden nicht übersetzt.
6. [x] Sparse-Extents in den generischen Transferpfad eingebaut: descriptor-relatives
   `SEEK_DATA`/`SEEK_HOLE`, explizites Hole-Punching, logischer BLAKE3-Digest und
   exakter Extent-Readback vor dem atomaren Commit.
7. [x] Hardlink-Gruppen im Scanner und `CreateHardlink` in Plan/Ausführung ergänzt:
   Daten werden pro Gruppe einmal übertragen, weitere Namen exklusiv per `linkat`
   angelegt; Gruppen-, Inhalts- und Ziel-Drift werden vor der ersten Mutation geprüft.
8. [x] UI-Vorschau und detaillierte Abschlussbilanz ergänzt: Profilwahl, erzwungener
   Inhaltsvergleich, Klassen pro Pfad, getrennte Zähler und Best-Effort-Auslassungen.
9. [x] Der lokale Realmodus führt die wirkungsgebundene Quarantäne-/Restore-Probe
   vor jeder Freigabe aus; Core- und native Sandbox-Belege stehen im
   [Realmodusvertrag](real-mode-design.md). Das ist kein Nachweis eines unabhängigen
   Vollbackups. Der Backup-Hinweis in der Sync-Ansicht bleibt deshalb bestehen.

Jeder Schnitt muss die bestehende atomare Inhaltsübertragung, Planbindung und
Quarantäne unverändert grün halten. Kein Teilschnitt darf den Backup-Hinweis
vorzeitig abschalten.
