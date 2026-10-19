# Realmodus-Sicherheitsvertrag

Stand: 2026-10-19

Ergänzung 2026-10-18: Der [normale lokale AddOnly-/Update-Start](local-sync.md)
verwendet dieselben technischen Root-/Listing-/Digest- und Einmal-Permitprüfungen
mit wirkungsbezogener Zustimmung. Reine Additionen benötigen keine Restore-Probe
oder Quarantäne. Sein separates lokales Wirkungsprofil bindet außerdem
Neunamencommit und Flushzusagen; begrenzte Best-Effort-Ersetzungen bestätigen
fehlende Wiederherstellung ausdrücklich. Dieses Profil lockert keine strikte
Remote-, Mirror- oder bidirektionale Vorbereitung. Die folgenden Zufallsphrasen-, Backup- und Mirror-Gates gelten
weiter für den getrennten Assistenten; sie sind keine allgemeine Eintrittsschranke
für den normalen lokalen Start.

Status: Verbindlicher und lokal vollständig umgesetzter Sicherheitsvertrag.
FTPS, WebDAV und SMB sind providerweise angebunden; die jeweils fehlenden
Dauerhaftigkeits- oder Quarantänegates bestimmen die vollständig abgesicherten
Zielmodi. Für schwächere, aber begrenzbare Providerwirkungen gilt zusätzlich der
Best-Effort-Vertrag aus [`product-principles.md`](product-principles.md).
SFTP v3 erlaubt interaktives Best-Effort-Listing sowie begrenzte Inhalts-Snapshots
und additive lokale Downloads über eine eigene beobachtete Inhaltsklasse.
Strikte Planung und Remote-Mutationen bleiben ohne atomare Rootbindung gesperrt.
Die [Fähigkeitsmatrix](function-audit.md) beschreibt die heutigen Richtungen.

## Ziel

Der Realmodus darf niemals nur „Dry-Run aus“ bedeuten. Er ist eine einmalige,
plan- und wurzelgebundene Erlaubnis für genau einen Lauf. Quelle, Ziel, Mount/Server,
vollständige Auflistung, Inhaltszustand, Quarantäne und Restore-Fähigkeit müssen
gemeinsam nachweisbar sein.

Dieser Vertrag entstand für unbeaufsichtigte Ersetzungen und rekursive
Mirror-Entfernungen mit großem Wirkungsradius. Er ist nicht die allgemeine
Produktidentität und darf nicht unverändert als Eintrittsschranke für eine einzelne,
interaktiv ausgelöste Dateioperation verwendet werden. Ein fehlender atomarer
No-Replace-Commit kann beispielsweise einen Sync-Zielmodus begrenzen, ohne einen
normalen, sichtbar als Best Effort ausgeführten Rename vollständig zu verbieten.

Vier Fehlerklassen sollen damit fail-closed enden:

1. Ein Pfad zeigt nach Vorschau oder Rückfrage auf ein anderes Volume, einen anderen
   Server oder einen ausgetauschten Ordner.
2. Eine unvollständige Quell- oder Zielauflistung sieht irrtümlich wie „leer“ oder
   „überzählig“ aus.
3. Ein veralteter Plan ersetzt oder entfernt zwischenzeitlich geänderte Daten.
4. Die Quarantäne existiert zwar, ist aber auf dem konkreten Ziel nicht dauerhaft
   beschreibbar oder nicht wiederherstellbar.

„Fail-closed“ bedeutet hier: Die davon abhängige destruktive Wirkung startet nicht
unter falschen Annahmen. Es bedeutet nicht, dass Browse, Additionen oder unabhängig
bestätigbare Einzelaktionen des Providers verschwinden. Planung und UI müssen
Wirkungen getrennt bilanzieren und die stärkste noch ausführbare Klasse anbieten.

Der Vertrag ist kein Vollbackup-Versprechen. Der Metadaten-Vertrag Stufe 2 ist lokal
umgesetzt; der Backup-Hinweis in der Sync-Ansicht („Ein Sync ersetzt kein Backup“,
seit 2026-07-23 dort statt als Seitenleisten-Dauerwarnung) bleibt dennoch bestehen,
weil Doppeldecker weder ein unabhängiges Backup noch dessen externe Restore-Probe
technisch verifiziert und keine Dateisystem-Snapshots verspricht.

## Technisch getrennte Ausführungsarten

Die frühere Kombination aus `dry_run: bool` und optionaler Sandbox ist durch einen
geschlossenen Typ ersetzt. Tests erhalten zusätzlich das nur über den ausdrücklich
benannten Testkonstruktor erreichbare `HeadlessTestPermit`:

```rust
pub enum ExecutionMode {
    DryRun,
    TestSandbox(TestSandboxPermit),
    Real(Box<RealRunPermit>),
    HeadlessTest(HeadlessTestPermit),
}
```

- `DryRun` besitzt keine Schreibmethoden.
- `TestSandboxPermit` entsteht im Kern nur nach Marker- und Gefahrenpfad-Prüfungen.
  `execute()` akzeptiert es ausschließlich zusammen mit Quell- und Ziel-VFS, die an
  denselben gehaltenen Sandbox-Deskriptor gebunden sind.
- `HeadlessTestPermit` hat private Felder, ist nur für isolierte Kerntests bestimmt und
  existiert in Release-Builds nicht.
- `RealRunPermit` entsteht ausschließlich aus `PreparedLocalRealRun::confirm` nach
  vollständiger Vorprüfung, exakter Zufallsphrase und gegebenenfalls Backup-Häkchen.

`Safety::default()` ist garantiertes Dry-Run. Headless-Tests erhalten explizite
Test-Permits; „kein Sandboxpfad“ erlaubt im Kern keine implizite Mutation mehr. Eine
direkte Tauri-Command-Ausführung oder ein manipuliertes Frontend kann damit keinen
Realmodus erfinden. `run_real_sync` nimmt weder Pfade noch Optionen oder Permitdaten
vom Frontend entgegen, sondern konsumiert nur die backend-gehaltene Vorbereitung.

## Einmaliges `RealRunPermit`

Ein Permit enthält mindestens:

```rust
struct RealRunPermit {
    nonce: [u8; 32],
    plan_digest: [u8; 32],
    source_identity: RootIdentity,
    target_identity: RootIdentity,
    quarantine_identity: RootIdentity,
    mode: SyncMode,
    compare: Compare,
    issued_at: SystemTime,
    expires_at: SystemTime,
    consumed: AtomicBool,
}
```

Verbindliche Eigenschaften:

- genau einmal konsumierbar,
- maximal zehn Minuten zwischen letzter Vorprüfung und Start,
- nur für exakt den bestätigten Plan-Digest,
- jede Änderung an Quelle, Ziel, Richtung, Modus oder Vergleich invalidiert es,
- nicht serialisierbar und nicht vom Frontend lieferbar,
- im Backend-`AppState` gehalten; ein App-Neustart löscht jedes Permit,
- bestehende Single-Flight-Schranke bleibt zusätzlich aktiv.

## Lokale Wurzelidentität

Ein kanonisierter Pfadstring genügt nicht. Der lokale Provider öffnet Quelle, Ziel und
Quarantänebasis komponentenweise mit `O_NOFOLLOW|O_DIRECTORY` und hält die Root-
Deskriptoren vom letzten Scan bis zum Ende der Ausführung offen. Alle Kindzugriffe
werden relativ zu diesen Handles ausgeführt.

Für Test-Sandbox und lokalen Realmodus ist dieser Vertrag umgesetzt: Der frische,
planbindende Zweitscan
und `execute()` verwenden dieselbe `LocalVfs::rooted`-Instanz. Jeder Zugriff wird relativ
zum gehaltenen Sandbox-Handle ausgeführt; Pfade außerhalb werden abgelehnt. Vor den
mutierenden Phasen vergleicht der Kern `st_dev` und `st_ino` des sichtbaren Pfads mit
dem Handle und bricht bei Austausch ab. Der Realmodus bindet zusätzlich Mount-ID,
Dateisystemtyp, Mountflags, FSID und auf macOS die Laufzeit-Volume-ID.

`LocalRootIdentity` enthält:

- Root-`st_dev`, `st_ino` und Dateityp,
- Mount-Identität,
- Dateisystemtyp und relevante Mountflags,
- den nur zur Anzeige dienenden absoluten Pfad.

Linux verwendet zusätzlich `statx(..., STATX_MNT_ID_UNIQUE)`, falls der Kernel es
liefert, sonst `STATX_MNT_ID` plus Geräte-ID. `STATX_MNT_ID_UNIQUE` wird während der
laufenden Systeminstanz nicht wiederverwendet; die normale Mount-ID entspricht
`/proc/self/mountinfo` ([`statx(2)`](https://man7.org/linux/man-pages/man2/statx.2.html)).

macOS kombiniert `fstat`, `fstatfs().f_fsid` und die Laufzeit-Volume-ID. Apples
`NSURLVolumeIdentifierKey` ist ausdrücklich nur innerhalb der laufenden
Systeminstanz stabil und nicht über Neustarts hinweg
([Apple-Dokumentation](https://developer.apple.com/documentation/foundation/nsurlvolumeidentifierkey?changes=__4__2&language=objc)).
Das passt zum absichtlich kurzlebigen Permit; es ist kein persistenter Datenträgername.

Vor jeder mutierenden Phase werden die Identitäten auf den **gehaltenen** Deskriptoren
erneut geprüft. Ein sichtbarer Rename oder Symlink-Swap kann so höchstens einen klaren
Abbruch auslösen, aber nie auf einen neu aufgelösten Fremdpfad umlenken.

## Remote-Wurzelidentität

Jeder Provider liefert eine `RemoteRootIdentity` und hält dieselbe authentisierte
Verbindung von finaler Vorschau bis Ausführungsende. Ein Reconnect invalidiert das
Permit und verlangt eine neue Vorschau.

### SFTP

Die folgenden Identitätsanforderungen bleiben notwendig, reichen aber nicht für
eine aktuelle Sync-Freigabe. SFTP v3 bleibt ohne rootgebundenen Namespace gesperrt.

- Endpoint, Benutzerkonto, kanonischer Remote-Root und SSH-Hostkey-Fingerprint.
- Der Hostkey ist Teil der SSH-Serverauthentisierung; RFC 4253 beschreibt die
  Verifikation des öffentlichen Hostkeys im Schlüsselaustausch
  ([RFC 4253](https://www.rfc-editor.org/info/rfc4253/)).
- Hostkey-Wechsel ist kein gewöhnlicher Dialog im laufenden Sync, sondern harter
  Abbruch und separate Vertrauensentscheidung.
- Fehlen stabile Datei-IDs, sind BLAKE3 und wiederholte vollständige Verzeichnislisten
  für Update/Mirror Pflicht.

### FTP/FTPS

- Endpoint, Benutzerkonto, kanonischer Root, Transportmodus und Verbindungssitzung.
- Explizites TLS verwendet private Kontroll-/Datenkanäle und einen exakten
  Zertifikatspin. Öffentliche WebPKI mit Hostname, extern geprüfter Pin und bewusst
  ungeprüfter Zertifikatspin sind getrennte sichtbare Zustände. Ein Zertifikatwechsel
  ist kein stiller Reconnect; ein unveränderter Pin behauptet im ungeprüften Modus
  keine bestätigte Serveridentität.
- Ausdrückliches Klartext-FTP bindet Endpunkt, Root, FEAT- und Bannerdigest, aber
  keine kryptografische Serveridentität. `ftps://` fällt nie auf Klartext zurück.
- Vollständiges MLSD mit Pflichtfakten ist erforderlich. Ein `Unique`-Fact ist keine
  ewige Identität. Ziel-Update/Mirror bleiben ohne Quarantäne-/Restore-Vertrag
  gesperrt; AddOnly verlangt zusätzlich die No-Replace-Probe.

### WebDAV

- TLS-validierte Origin, Konto, normalisierter Collection-Root.
- Jede Collection wird mit `Depth: 1` per `PROPFIND` vollständig traversiert; Depth
  infinity wird nicht vorausgesetzt.
- Starke ETags binden Dateirepräsentationen. RFC 4918 verlangt, dass ein ETag für eine
  URL nicht für unterschiedliche GET-Repräsentationen wiederverwendet wird, weist aber
  auch darauf hin, dass nicht sichtbare Properties aus PROPFIND fehlen dürfen
  ([RFC 4918](https://www.rfc-editor.org/info/rfc4918/)).
- Sobald ein Server Zugriffsfehler, unklare Pagination, fehlende starke ETags oder eine
  möglicherweise gefilterte Auflistung liefert, bleibt Mirror gesperrt.
- Umgesetzt: ETags sind Teil des Listing-Proofs; jede WebDAV-Quelle verlangt zusätzlich
  `ContentHash`, jeder GET ist per `If-Match` gebunden. Strikte WebDAV-Zielmutationen
  und ihre künstlichen Proben bleiben inzwischen passiv gesperrt: Die Bedingungen
  binden nicht die gesamte Root-/Elternkette gemeinsam. Gewöhnliche additive
  Uploads verwenden ihren eigenen Best-Effort-Vertrag; daraus entsteht keine
  Sync-Zielfreigabe. Siehe [Mutationsgrenze](webdav-mutation-boundary.md).

### SMB

- Server-GUID, Share-Name/Tree-ID, Konto, normalisierter Root, ausgehandelter Dialekt
  und Signing-/Encryption-Status.
- SMB2 führt den `ServerGUID` ausdrücklich als globale Serveridentität
  ([MS-SMB2](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/f388d7e0-9bc3-4d9c-98e5-71f8e36b3c4f)).
- Entry-Planbindung nutzt stabile File-IDs, soweit der Server sie anbietet. Ändern sich
  Server-GUID, Share oder Dialekt bei einem Reconnect, ist der Plan ungültig.
- Umgesetzt: SMB 3.1.1, Signing und Verschlüsselung sind Pflicht. Vollständiges
  Paging, Root-File-ID, Tree-ID und Sitzungsgeneration gehören zum Proof. Ein
  SMB-Ziel erhält AddOnly nach Create-/Datei-FLUSH-/Rename-Proben. Update/Mirror
  bleiben ohne den vollständigen identitätsgebundenen Restore-Proofvertrag
  gesperrt; ein Verzeichnis-FLUSH allein genügt dafür nicht.

## Vollständigkeitsnachweis der Auflistung

Jeder Scan erzeugt neben Einträgen einen `ListingProof`:

```rust
struct ListingProof {
    root_identity: RootIdentity,
    directories_started: u64,
    directories_completed: u64,
    entries_seen: u64,
    listing_digest: [u8; 32],
    provider_token: Option<Vec<u8>>,
    errors: Vec<ListingError>,
    complete: bool,
}
```

`complete` ist nur wahr, wenn:

- jede betretene Verzeichnisliste einen eindeutigen erfolgreichen Abschluss hatte,
- keine Seite, kein Continuation-Token und keine PROPFIND-/MLSD-Antwort fehlt,
- Namen innerhalb eines Verzeichnisses eindeutig und sicher dekodierbar sind,
- die Root-Identität während des ganzen Scans gleich blieb,
- kein Timeout, Berechtigungs-, I/O- oder Protokollfehler als leere Liste behandelt
  wurde,
- der Digest alle relativen Namen, Typen, IDs und planrelevanten Metadaten enthält.

Vor **jeder** Mirror-Entfernung wird nach allen Kopien ein zweiter vollständiger
Quell- und Zielscan durchgeführt. Beide Proofs müssen vollständig sein; Root-Identität,
Quellbestand und die noch zu entfernenden Zielzustände müssen zum bestätigten Plan
passen. Ein einziges unsicheres Verzeichnis sperrt alle Entfernungen des Laufs.

Eine leere Quelle allein ist niemals Löschbeweis. Der erste Real-Mirror unterstützt
keine vollständig leere Quelle. Ein späterer Sonderfall bräuchte eine eigene
Wiederherstellungsprobe, erneuten Scan und eine deutlich getrennte Freigabe.

## Verbindlicher Inhaltsvergleich

- `AddOnly`: schneller Vergleich bleibt zulässig, weil vorhandene Ziele unberührt
  bleiben.
- `Update` und `Mirror`: `ContentHash` ist im Realmodus Pflicht.
- Quell-, Ziel- und `DeleteDir`-Digests aus v0.1.5 gehören zum Plan-Digest.
- Provider mit starker, dokumentierter Inhalts-ID dürfen diese als zusätzliche
  Drift-Schranke verwenden, nicht als stillen Ersatz für die BLAKE3-Erstfreigabe.
- Die Übertragungsprüfung liest das fertige Ziel wie heute zurück.

## Quarantäne und Speicherprobe

Update/Mirror benötigen eine Quarantäne auf demselben Dateisystem beziehungsweise im
selben atomaren Rename-Namespace wie das Ziel. Copy-and-delete ist kein Ersatz.

Für lokale Ziele liegt sie als reservierter Geschwisterordner des Zielroots unter
dessen Elternhandle. Dateisystemwurzel, Benutzerordner, Repo-Wurzel und ein Ziel ohne
sicher beschreibbaren Geschwisterbereich sind im ersten Realmodus unzulässig.

Vor der Freigabe prüft Doppeldecker:

1. freien Platz für neue Daten **plus** maximal zu quarantänierende vorhandene Daten,
2. exklusives Anlegen, `fsync`, Rename und Wiederöffnen im Zielbereich,
3. exklusives Verschieben in die Quarantäne,
4. Wiederherstellen ohne Überschreiben,
5. BLAKE3-Gleichheit nach dem Restore,
6. sauberes Entfernen ausschließlich der selbst erzeugten Probe.

Die Probe verwendet zufällige, von Doppeldecker erzeugte Bytes und berührt keine
Nutzerdaten. Ihr Erfolg belegt nur den internen Quarantäne-/Restore-Pfad. Zusätzlich
verlangt die UI vor Update/Mirror die bewusste Bestätigung, dass ein unabhängiges
Backup existiert; Doppeldecker behauptet nicht, dieses externe Backup technisch
verifiziert zu haben.

Jeder reale Quarantänelauf behält das in v0.1.4 eingeführte Sidecar-/Intent-Journal.
Das Aufbewahrungsziel bleibt 30 Tage ohne Auto-Löschung. Endgültiges Leeren bleibt pro
Lauf separat bestätigt.

Der Restore-Claim bindet neben Inhalt und Objektidentität auch das vollständig
darstellbare Metadatenmanifest, Sparse-Extents und die Hardlinktopologie. Ältere
Version-2-Records ohne diesen Claim bleiben als physisch vorhandene Daten sichtbar,
sind aber nur manuell rettbar; der automatische Restore bleibt für sie gesperrt. Ein
bereits sichtbares Restore-Ziel erhält bei der Recovery erst nach Synchronisierung
seiner vollständigen Elternkette bis zum Zielroot und der noch vorhandenen
Quarantäne-Eltern bis zur Basis den dauerhaften Manifestabschluss. Ein Fehler
erhält den offenen Intent als `outcome_unclear`; es wird nicht erneut umbenannt. Ein
reservierter Pending-Lauf ohne Ordner und Records wird beim Leeren ausschließlich
durch seinen dauerhaften Purge-Intent abgeschlossen. Doppeldecker legt dafür keinen
neuen Laufordner an, dessen Identität zwischen Anlage und Wiederöffnung austauschbar
wäre.

## Mengen- und Wirkungsgrenzen

Die Vorschau zählt getrennt:

- neue Dateien/Bytes,
- zu ersetzende Dateien/Bytes,
- reine Metadatenänderungen,
- zu quarantänierende Dateien, rekursive Einträge und Bytes,
- erwarteten Platzbedarf für Ziel und Quarantäne.

Erste Realmodus-Grenzen:

- `AddOnly`: keine Ersetzungen und keine Entfernungen.
- `Update`: höchstens 100 Ersetzungen pro Lauf; jede alte Version muss vorher
  quarantänierbar sein.
- `Mirror`: in der ersten freigegebenen Stufe höchstens 50 entfernte Einträge und
  höchstens 5 % des vollständigen Zielbestands. Beide Grenzen gelten gleichzeitig.
- Leere Quelle, ausgelassene Einträge, unvollständige Metadaten im strikten Profil oder
  ungenügender Platz sperren jede Entfernung.
- Grenzen sind nicht über einen simplen UI-Schalter deaktivierbar. Größere Änderungen
  werden in neue, erneut geprüfte Läufe aufgeteilt.

Diese Grenzen definieren den vollständig abgesicherten Realmodus. Provider, die eine
einzelne Garantie nicht liefern, sollen zusätzlich einen klar getrennten
Best-Effort-Lauf anbieten können: frische Vorschau, konkrete Risikobilanz,
wirkungsgerechte Bestätigung, Readback und Audit. Ein solcher Lauf darf keine
stärkere Atomarität, Dauerhaftigkeit oder Wiederherstellbarkeit behaupten, als der
Provider tatsächlich liefert.

## Aktivierungsablauf in der UI

Der Test-Sandbox-Schalter wird nie zum Realmodus-Schalter umgedeutet. Der Realmodus
besitzt einen getrennten Assistenten und eine dauerhaft rote Fensterkennzeichnung.

1. Exakte Quelle und Zielwurzel mit Volume-/Serveridentität anzeigen.
2. Fähigkeiten, Metadaten-Grenzen und vollständige Listing-Proofs anzeigen.
3. Für Update/Mirror BLAKE3, Quarantäneplatz und Restore-Probe erfolgreich prüfen.
4. Unabhängiges Backup und dessen jüngste Restore-Probe bewusst bestätigen lassen.
5. Alle Ersetzungen/Entfernungen und Bytes anzeigen.
6. Zielbasename plus zufällige Laufphrase eingeben, beispielsweise
   `REAL Archiv K7M4Q2`.
7. Einmaliges Permit erzeugen; danach sind Pfad-, Richtungs- und Modusfelder gesperrt.
8. Unmittelbar vor Start Root-/Serveridentität und Plan-Digest erneut prüfen.

Die rote Kennzeichnung enthält Modus, Ziel und verbleibende Permit-Zeit. Nach Erfolg,
Fehler, Abbruch, zehn Minuten oder irgendeiner Auswahländerung fällt die App automatisch
in Dry-Run zurück.

## Audit und Geheimnisse

Ein lokales, append-only JSONL-Laufprotokoll enthält Zeit, Providerart, gehashte
Root-Identitäten, Plan-Digest, Aktions-/Bytezahlen, Probe-Ergebnis, Quarantänelauf und
Abschlussstatus. Es enthält keine Kennwörter, Tokens, privaten Schlüssel, kompletten
Remote-URLs mit Credentials oder Dateiinhalte.

Remote-Hostkeys, Server-GUIDs und Zertifikatsinformationen werden nur so weit
gespeichert, wie Identitätsprüfung und Nutzeranzeige es benötigen. Secret-Zugriffe
bleiben außerhalb des Kernprotokolls.

## Crash- und Fehlervertrag

- Fehler vor Permit: keine geplante Nutzdatenwirkung. Eine bereits begonnene
  künstliche Capability-/Restore-Probe wird nach ihrem eigenen sicheren Vertrag
  bilanziert und aufgeräumt; unsichere Reste bleiben sichtbar.
- Lokale Proben bereinigen ausschließlich den nach erfolgreicher Anlage oder
  bestätigtem Rename bekannten Ort. Eine fremde Kollision an einem noch nicht
  erreichten Namen bleibt erhalten. Unklare Anlagen oder Renames behalten ihre
  möglichen Orte als strukturierte Recoverypfade; sie lösen keinen blinden
  Cleanup aus. Dies behauptet keine lokale atomare Inode-Compare-and-Unlink-
  Primitive und übernimmt nicht den stärkeren Remote-Restore-Proofvertrag.
- Fehler nach Probe, vor erster Aktion: nur selbst erzeugte Probe wurde entfernt.
- Fehler während Kopien: vorhandene Ziele bleiben durch Temp-Datei-Commit unangetastet.
- Fehler nach Quarantäne-Intent: UI zeigt den unvollständigen Lauf; Wiederherstellung
  bleibt möglich.
- Provider-Reconnect, Mount-ID-Wechsel oder Wurzelverlust: keine weiteren Aktionen,
  insbesondere keine Entfernungen.
- Abbruch: keine neue Löschphase beginnen; bereits committed Kopien und journalisierte
  Quarantänen exakt bilanzieren.
- Audit-/Journal-`fsync`-Fehler: fail-closed vor der nächsten Nutzerdatenmutation.

## Pflicht-Tests

1. Kein Kernaufruf mutiert mit Default-/fehlendem Permit.
2. Permit ist einmalig, läuft nach zehn Minuten ab und bindet Plan, Richtung und Modi.
3. Root-Rename plus Symlink-Ersatz trifft nur die gehaltenen Deskriptoren.
4. Linux-Mount-ID beziehungsweise macOS-FSID wechselt: Abbruch vor Mutation.
5. Quelle oder Ziel verschwindet und wird am gleichen Pfad neu gemountet: Plan ungültig.
6. Jede Art unvollständiger/paginierter Liste liefert `complete=false`; Mirror entfernt
   im gesamten Lauf nichts.
7. Zweitscan erkennt neue, fehlende und inhaltsveränderte Einträge mit unveränderter
   Größe/mtime.
8. Restore-Probe belegt Rename, `fsync`, BLAKE3 und kollisionsfreie Rückkehr.
9. Zu wenig Platz für Ziel plus Quarantäne sperrt vor erster Mutation.
10. Direkter Tauri-Command mit erfundenem Frontend-Permit scheitert.
11. App-Neustart, Pfadwechsel, Moduswechsel und Provider-Reconnect verwerfen das Permit.
12. SFTP-Hostkey-, SMB-ServerGUID-, FTPS-Sitzungs- und WebDAV-Origin-Wechsel brechen ab.
13. WebDAV-403/partielle PROPFIND-, FTP-ohne-MLSD- und SMB-Paging-Fehler sperren Mirror.
14. Abbruch/Crash an jeder Intent-/Rename-Grenze liefert eine korrekte Quarantänebilanz.
15. Audit enthält keine Secrets und meldet Teilfortschritt korrekt.
16. Alle lokalen Tests laufen auf macOS und Linux-Testhost; Remote-Provider nutzen
    deterministische Mock-Server mit Identitäts- und Pagination-Fehlern.

Mount-Wechsel-Tests dürfen zunächst über injizierte Provider-Identitäten laufen. Echte
Loopback-Mounts oder Disk-Images kommen erst in einen separat freigegebenen
Integrationstest; sie sind keine Voraussetzung, um den Kern fail-closed zu bauen.

Die lokalen Pflichtfälle sind auf macOS und Linux-Testhost automatisiert abgedeckt,
einschließlich injiziertem Mountwechsel, nicht-UTF-8-/unvollständigem Listing,
Inhaltsdrift bei gleicher Größe/mtime, Platzmangel, Auditfehler vor Mutation sowie
Quarantäne-Restore. Die Punkte 12 und 13 sowie der Reconnect-Teil von Punkt 11 bleiben
absichtlich den jeweiligen Remote-Provider-Meilensteinen zugeordnet.

## Stufenweise Freigabe

0. **Strukturhärtung (umgesetzt):** `ExecutionMode`, private Test-Permits,
   Dry-Run-Default und descriptor-gehaltene Root-Handles sind vorhanden.
1. **Lokal AddOnly (umgesetzt):** echte Daten nur hinzufügen; keine Ersetzung, keine
   Entfernung; schneller Vergleich bleibt zulässig.
2. **Lokal Update (umgesetzt):** höchstens 100 Ersetzungen, zwingender ContentHash,
   vollständige Listings, Platzbilanz und Quarantäne-/Restore-Probe.
3. **Lokal Mirror (umgesetzt):** Metadatenvertrag Stufe 2, ContentHash-Zweitscan nach
   allen Kopien, höchstens 50 Einträge und zugleich höchstens 5 %, keine leere Quelle
   oder Auslassung. Der echte GUI-Agententest mit Nutzerdaten bleibt bewusst aus;
   Plattformtests verwenden ausschließlich temporäre künstliche Bäume.
4. **Remote AddOnly (providerweise umgesetzt):** FTP/FTPS und SMB sind als strikte
   Ziele capability-geprüft angebunden. WebDAV-Ziele bleiben im strikten Sync
   passiv gesperrt; gewöhnliche additive Uploads besitzen einen eigenen Vertrag.
   SFTP bleibt für Sync ohne rootgebundenen Namespace gesperrt. Die sichtbare
   Transportstufe ist davon getrennt.
5. **Remote Update/Mirror (providerweise umgesetzt):** FTPS-, WebDAV- und
   SMB-Quellen unterstützen alle Modi gegen ein lokales Ziel. Remote-Ziele erhalten
   im vollständig abgesicherten Modus
   die durch Identitäts-, Listing-, Quarantäne-, Dauerhaftigkeits- und
   Reconnect-Proben belegten Wirkungen. Fehlende Einzelgarantien werden künftig
   wirkungsbezogen als Best Effort angeboten, sofern Ziel, Umfang und Restrisiko
   verlässlich gebunden und bestätigt werden können.

Keine Stufe wird automatisch durch das Vorhandensein einer Umgebungsvariable aktiv.
Jede neue Remote-Stufe braucht grüne Plattformtests, aktualisierte öffentliche
Warnungen und eine ausdrückliche Produktfreigabe.
