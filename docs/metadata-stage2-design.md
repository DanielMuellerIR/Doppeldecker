# Metadaten-Vertrag Stufe 2

Stand: 2026-10-31; ursprünglicher Stufe-2-Abschluss am 2026-07-11.

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
    pub acl: Capability,
    pub xattrs: Capability,
    pub resource_fork: Capability,
    pub named_streams: Capability,
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
    WindowsSid,
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
    pub named_streams: Option<Vec<NamedBlobSummary>>,
    pub sparse_extents: Option<Vec<DataExtent>>,
    pub hardlink_group: Option<u64>,
    pub reported_hard_links: Option<u32>,
    pub sparse_read_issue: Option<SparseReadIssue>,
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

### Windows-SIDs und DACLs über SMB

SMB-Dateien und -Ordner liefern im nativen Profil eine getrennte
`WindowsSid`-/`WindowsDacl`-Beobachtung. Das Modell erhält fehlende, NULL- und
leere DACLs getrennt, außerdem ACE-Reihenfolge, Masken, Flags und vollständige SIDs.
Komplexe oder unbekannte ACEs bleiben als vollständige opaque Beobachtung gebunden.
Owner und Group stehen als SIDs neben der DACL; sie werden weder in numerische
UID/GID übersetzt noch übertragen. Der Namensraum gehört zur geprüften
SMB-Serveridentität. Gleiche DACLs versprechen keine gleichen effektiven Rechte.

Zwei vollständige Sicherheits- und Streambeobachtungen sind an gehaltene
Basisobjekte und Eltern gebunden. Abweichende Datei-ID, Typ, ChangeTime,
Strommenge oder Sicherheitsdaten brechen den Scan ab. Die Security-Abfrage liest
Owner, Group und DACL, keine SACL. Fehlendes `READ_CONTROL` bleibt eine konkrete
Beobachtungslücke je Objekt; lesbarer Inhalt bleibt übertragbar. Nicht unterstützte
Abfragen und ungültige Deskriptoren sind getrennte Zustände. Letztere binden den
Hash des Puffers. Ungültige SMB-Hüllen, Transport-, Identitäts- und Close-Fehler
bleiben Fehler. Der Descriptor-/Query-Puffer ist auf 64 KiB begrenzt.

Der allgemeine ACL-Vertrag bleibt `ReadOnly`. Daneben besitzt gewöhnlicher
`NativeBestEffort`-Inhaltssync einen begrenzten Schreibweg für reguläre Dateien:
Wenn Inhalt, ADS oder andere unterstützte Metadaten bereits einen Basistausch
verlangen, kann die neue eigene Temp-Datei ihre Quell-DACL erhalten. Quelle und
Ziel brauchen denselben belegten Servernamensraum. Die ausdrücklich vorbereitete
Providersicht hält Root, Profil, Konto und Sitzung. Ein serialisierter Planbeleg
überträgt diese Backendfreigabe nicht. Bei Zuordnung zum tatsächlichen Queueowner
wird dessen eigener Kontext erneut geprüft und mit der Bestätigung verglichen.

Dieser erste Umfang akzeptiert eine nichtleere, geschützte explizite DACL der
Revision 2 mit einfachen Allow-/Deny-ACEs und ohne Vererbungsflags. Nur bekannte
konkrete Dateirechte sind darstellbar. Eine Allow-ACE muss dem beobachteten Owner
der neuen Ziel-Temp die für Readback und Dateiabschluss benötigten Rechte geben;
Deny-ACEs dürfen ausschließlich `WRITE_OWNER`, `WRITE_DAC`, `FILE_WRITE_DATA`,
`FILE_APPEND_DATA`, `FILE_READ_EA`, `FILE_WRITE_EA` und `FILE_EXECUTE` betreffen,
einzeln oder kombiniert. Frische Abschlusszugriffe verlangen weder EA-Zugriffe
noch Dateiausführung oder erneut `WRITE_DAC`. Vor dem einzigen SET bleibt
`WRITE_DAC` erforderlich. Diese zusätzlichen Denys allein benötigen keinen zusätzlichen
Daten-FLUSH-Handle; Daten-Schreib-/Anhängeverbote behalten ihren vorab gewährten
FLUSH-Zugriff. Der Owner-Allow bleibt
bei `0x001701ff`; beliebige schreibgeschützte Rechteformen werden nicht zugesagt.
Kontextabhängige Trustees wie CREATOR OWNER/GROUP, OWNER RIGHTS und PRINCIPAL SELF
werden nicht umgedeutet. Andere Zustände, Masken und ACE-Formen bleiben mit ihrem
konkreten Grund als Auslassung sichtbar. Bestehende Ordner besitzen den unten
beschriebenen getrennten Wirkungsvertrag. Gleicher Datei-Hauptinhalt wird allein wegen einer
DACL-Abweichung nicht erneut kopiert. Bereits gleiche beobachtete Werte erzeugen
keine Auslassung.

Eine neue eigene Probedatei unter dem gewählten Zielroot prüft vor der Planung
DACL, ACE-Reihenfolge und frisches Öffnen für die nötigen Folgezugriffe. Der
Recoveryintent steht vor CREATE. Die Probe mutiert keine vorhandenen Nutzerdaten;
`plan()` und Fähigkeitenabfragen bleiben lesend. Servernormalisierung ist keine
exakte Übertragung: Nach belegter Bereinigung der eigenen Probe bleibt Inhaltssync
mit DACL-Auslassung möglich. Eine allgemeine Fähigkeit von Samba oder SMB wird
aus dem erfolgreichen Gegenfall nicht abgeleitet.

SET betrifft ausschließlich die DACL und ihren geschützten Zustand. Owner-, Group-
und SACL-Offsets bleiben null; der Writer fordert kein `WRITE_OWNER` an. Er prüft
Identität und den beobachteten Temp-Owner-/Group-Kontext vor SET sowie unveränderte
Owner/Group und exakte DACL danach. Während SET verhindern gehaltene Handles das
Umbenennen der Basis und ihrer gebundenen Eltern. Frische Öffnungen belegen die
Rechte für den späteren Dateiabschluss. Bereinigung und Commit prüfen die gespeicherte
Temp-ID am gehaltenen DELETE-Handle erneut; ein fremder Nachfolger des Tempnamens
bleibt unberührt. Bekannte Fehler vor SET können Inhalt und
ADS mit passender ACL-Auslassung fortsetzen. Ein anderer beobachteter Kontext der
neuen Datei bleibt ebenfalls eine begründete Auslassung. Nach einem möglicherweise
wirksamen SET führen Antwortverlust, abweichender Readback oder unklarer Abschluss
zu `OutcomeUnclear`: Temp und Intent bleiben sichtbar, ohne automatische Löschung
oder Wiederholung.

Für einen Daten-Deny erhält der eigene Temp vor SET zusätzlich ein Handle mit
`WRITE_DATA | READ_ATTRIBUTES | SYNCHRONIZE`, ohne DELETE und mit
Share-Read/Write/Delete. Es wird ausschließlich für Identitätsprüfung, FLUSH und
CLOSE verwendet. Die ursprüngliche exklusive DELETE-Bindung während SET und der
frische Abschlussreadback bleiben erhalten; frische Öffnungen nach SET verlangen
die beiden verweigerten Datenrechte nicht erneut. Ein regulärer SMB2-FLUSH braucht
mindestens WRITE_DATA oder APPEND_DATA im gewährten Zugriff
([MS-SMB2, FLUSH](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/026984f6-38af-4408-8200-50557eb0a286)).

Die nicht persistierte Abschlussressource bindet Root, Tempname, Original-ID,
DACL und Owner-/Group-Kontext. Nach dem normalen Zeitabschluss prüft der FLUSH-Weg
Originalhandle, frisch gehaltene Eltern und ein frisches DELETE-/Securityhandle
auf dieselbe Datei und exakte DACL. Erst nach erfolgreichem FLUSH und geprüftem
Schließen sämtlicher Abschluss-Handles ist der Commit freigegeben. Die normale
Zeitoperation prüft vor ihrem SET ebenfalls die Original-ID. Ein fremder
Nachfolger des Tempnamens erhält damit keine Zeitänderung. Weitere Streamanlagen
nach DACL-SET sowie erneutes FLUSH oder Zeitsetzen nach Verbrauch der Ressource
werden vor jeder Wirkung abgelehnt.

Abbruch, Drop ohne FLUSH, Drift oder Fehler im FLUSH-/CLOSE-Abschluss lassen Temp
und Intent zur Prüfung stehen. Ein möglicherweise gesendeter SET wird weder
wiederholt noch durch einen frischen Daten-OPEN ersetzt. Das gehaltene FLUSH-Handle wird
beim normalen Abschluss oder Drop höchstens einmal geschlossen; ein unklarer
CLOSE wird nicht erneut gesendet. Eine verlorene CLOSE-Antwort beweist keine
serverseitige Freigabe. Handles, Verbindungen und Abschlusszustand gelangen nicht
in Queuejournal, Registry oder Frontendpayload.

Der native Prüfvertrag umfasst 14 Lebensdauerfälle und 24 tatsächliche
Queueausführungen auf frischen künstlichen Samba-Roots. Einzelne Daten-Denys und
ihre Kombination mit WRITE_OWNER durchlaufen Ergänzen, leere Folgeläufe,
Inhaltstausch und Spiegeln. Eine unklare FLUSH-Wirkung erhält das alte Ziel und
sperrt die nächste Vorbereitung mit dem konkreten Recoverybeleg. Unabhängige
Readbacks prüfen originale Commit-ID, Hauptinhalt, ADS, Zeit, DACL und SIDs;
rohe CREATE-Anfragen unterscheiden erlaubte und verweigerte Datenrechte.
Die Ressourcenprobe verwendet READ_DATA und eingeschränkte Share-Write-Freigabe:
reine Metadatenöffnungen würden die bestehende Schreibbindung nicht zuverlässig
anzeigen. Server- und Oberflächengrenzen bleiben Samba 4.19.5 und Headless-Queue;
eine neue Windows-Server- oder native Tauri-/SMB-GUI-Abnahme ist damit nicht belegt.

Die Owner-/Group-Unverändertheit bezieht sich auf die einzelne DACL-Wirkung an der
neuen Temp-Datei. Ein gewöhnlicher Basistausch bewahrt damit nicht automatisch den
Owner der alten Zieldatei; diese getrennte Auslassung bleibt im Bericht. Windows-
DACLs werden nicht an POSIX-Modusbits gekoppelt. Die lokalen POSIX-/NFSv4-Verträge
und `NativeStrict` erhalten keine neue Zusage.

Auch Noop-Beobachtungen speichern ihre Manifestdigests im Plan und werden vor dem
ersten Nutzdaten-Effekt erneut geprüft. Ohne die unten beschriebene Linkzahl bleiben
Windows-Manifeste im V4-Format; bestehende Manifeste ohne Windows-Daten behalten
V2/V3 und ihre JSON-Form. Neue
optionale Planfelder fehlen in alten serialisierten Plänen weiterhin. Der
allgemeine Inplace-Writer außerhalb der unten beschriebenen Klassen, nichtleere neue Ordner und weitere ACE- und
Vererbungsformen sowie SID-Domänenzuordnung bleiben offen.

#### Linkzahlen regulärer SMB-Dateien

Der vollständige Metadatenbeleg übernimmt die bereits gelesene
[`FileStandardInformation.NumberOfLinks`](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fscc/5afa7f66-619c-48f3-955f-68c4ece704ae) als optionale `reported_hard_links`
(`u32`). Sie stammt aus demselben gehaltenen Basisobjekt wie Streams und
Windows-Sicherheit. Beide vollständigen Beobachtungen prüfen neben Datei-ID und
ChangeTime auch die unveränderte Linkzahl. Verzeichnisse liefern dieses Feld
nicht: Ihre Linkzahl kann sich durch normale Kinderarbeit ändern.

Die Zahl ist weder eine Aliasliste noch ein vollständiger Beleg einer
volumengebundenen Hardlink-Gruppe. Sie umfasst auch Namen außerhalb der Auswahl;
nach solchen Namen wird nicht gesucht. Der Adapter erhält dadurch weder eine
Hardlink-Schreibfähigkeit noch einen erfundenen Gerätebezeichner. Bei Linkzahl
größer als 1 und mehreren ausgewählten regulären Dateien bleibt die fehlende
Gruppenzuordnung konkret bilanziert, auch bei
leeren Folgeläufen und reinen DACL-Wirkungen. Linkzahl 0 bleibt eine unvollständige
Beobachtung; Linkzahl 1 braucht keine pauschale Hardlink-Auslassung. Auch eine
einzige ausgewählte Datei mit äußerem Partner bildet keine innerhalb der Auswahl
zu erhaltende Beziehung. Die konkreten
Objektwerte ersetzen für diese Klasse die gröbere Rootfähigkeitsmeldung.

Die neue Beobachtung wählt ausschließlich bei vorhandenem Feld Digestformat V6.
V2/V3/V4/V5 und ihre JSON-Form ohne Feld bleiben bytekompatibel. Auch eine Noop-
Beobachtung bindet den Wert an den bestätigten Plan; eine spätere Abweichung
stoppt vor unabhängigen Wirkungen. Der reine Inhalts-/DACL-Blobbeleg erhält
absichtlich keine zusätzliche Linkzahlsperre: Die objektweite DACL-Klasse erlaubt
weiterhin neu entstehende Aliasnamen unter ihrer ausdrücklichen Bestätigung.

Der gezielte Prüfvertrag umfasst unabhängig vor der Erweiterung erzeugte
V2–V5-Vektoren, Zahlengrenzen, Manipulation, Portable-Noop und Drift sowie
18 echte Samba-Queuefälle für mehrere ausgewählte Namen, einen einzelnen Namen
mit äußerem Alias und eine gewöhnliche Einzeldatei. Ergänzen, Aktualisieren,
reine DACL-Wirkung, Spiegeln und leere Folgeläufe prüfen Bericht und tatsächliche
Inhalte, Streams, Rechte, Zeiten und Datei-IDs. Die vorhandenen objektweiten
DACL-Alias- und Recoverygegenläufe bleiben Teil der Prüfung. Die lokalen
vollständigen Hardlink-Gruppen und die Schreibverträge bleiben unverändert.

#### Lesende Sparse-Bereiche von SMB-Quellen

Gewöhnlicher `NativeBestEffort`-Sync kann die vollständig gemeldeten Datenbereiche
regulärer SMB-Quellen auf ein starkes lokales Sparse-Ziel übertragen. Die
Rootfähigkeit bleibt `ReadOnly` und setzt die tatsächlich gelesene Volumenzusage
`FILE_SUPPORTS_SPARSE_FILES` voraus. Der getrennte begrenzte Zielwriter ist unten
beschrieben; `NativeStrict` erhält keine zusätzliche Freigabe.

[`FSCTL_QUERY_ALLOCATED_RANGES`](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fscc/d2cde38a-d0b9-4412-b966-52011f8cf6cb)
läuft am gehaltenen Basisobjekt innerhalb der beiden vollständigen
Metadatenbeobachtungen. Die Antwort wird an Befehl, Steuerkennung, Handle,
Pufferposition und Länge gebunden. Bereiche müssen geordnet, überlappungsfrei und
innerhalb der vorzeichenbehafteten 64-Bit-Grenzen liegen. Protokollkonforme
Schnittmengen werden auf die angefragten logischen Dateigrenzen beschränkt.
`BUFFER_OVERFLOW` benötigt eine Fortsetzung mit echtem Fortschritt;
`BUFFER_TOO_SMALL` lässt den Puffer begrenzt wachsen. Einzelantworten sind auf
64 KiB, Gesamtbestand auf 65.536 Bereiche und Arbeit auf 1.024 Antworten begrenzt.
Ein unvollständiger Bestand wird niemals zu einer leeren Liste.

Die Bereiche beschreiben die Serverbeobachtung. Insbesondere kann ein Server bei
einer nicht als Sparse markierten Datei einen vollständigen Datenbereich melden,
obwohl sein lokales Dateisystem Löcher besitzt. Daraus werden keine zusätzlichen
Löcher abgeleitet. Leere Dateien haben nach gebundener Größenprüfung eine leere
Bereichsliste; sie benötigen keine zusätzliche FSCTL-Abfrage.

Bekannte Bereiche verwenden das bestehende `sparse_extents`-Feld. Konkrete
Serverabsagen, Puffergrenzen und ungültige Antworten erscheinen als optionales,
typisiertes `sparse_read_issue`; Rechteabsagen bleiben Rechtefehler. Sie schließen
eine vollständige Bereichsliste aus und erhalten eine eigene Auslassung, auch bei
Noop und reiner DACL-Wirkung. Der logische Inhalt bleibt übertragbar. Nur eine
vorhandene Auslassung wählt Digestformat V7; alle bisherigen JSON-Formen und
V2–V6-Digests ohne dieses Feld bleiben kompatibel.

Der Seek-Reader hält Datei und erforderliche Eltern bis `SparseRead::finish()`.
Auch nach EOF darf er noch innerhalb der Datei lesen. Vor dem Temp-Abschluss
prüft `finish()` die gehaltene ID, Größe, ChangeTime und Linkzahl und schließt die
Handles fehlererhaltend genau einmal. Ein unklarer CLOSE wird nicht wiederholt.
Der generische Sparse-Transfer verlangt diesen Abschluss auch bei vollständig
gelochten und leeren Dateien. Ein Fehler oder Abbruch vor dem Commit lässt die betroffene bestehende Zieldatei
unverändert; bereits abgeschlossene andere Dateien bleiben bilanziert. Der
gewöhnliche SMB-Reader behält seinen eigenen EOF-Vertrag.

Der Prüfvertrag umfasst fünf echte Quelldarstellungen, lokale Bereichs- und
Inhaltsvergleiche, AddOnly/Update/Mirror/Noop sowie konkrete Auslassungs-, Drift-,
Abbruch-, READ- und CLOSE-Fälle über die tatsächliche Queue. Die Sparse-Kopie liest
nur gemeldete Datenbereiche; Hash- und Vorschaupfade können weiterhin den gesamten
logischen Inhalt lesen. Die native Samba-Probe belegt Pufferwachstum;
`BUFFER_OVERFLOW`-Paging wird zusätzlich mit synthetischen Antworten geprüft.
Eine Windows-Server- oder zusätzliche Tauri-GUI-Abnahme folgt daraus nicht.

#### Sparse-Temps auf SMB-Zielen

Gewöhnlicher `NativeBestEffort`-Sync kann vollständig beobachtete Sparse-Bereiche
lokaler Quellen auf einem geeigneten SMB-Ziel erhalten. Vor der Planung prüft eine
laufgebundene neue Probe ausschließlich den ausgewählten Zielroot. Volumenzusage,
`SET_SPARSE`, logische Größe, `SET_ZERO_DATA`, Bereichs- und Inhaltsreadback sowie
FLUSH und einmaliger CLOSE müssen zusammen passen. Ein explizit nicht unterstützter
Befehl oder eine Rechteabsage ergibt vor der Planung eine Sparse-Auslassung;
Transport-, Identitäts- und unklare Abschlussfehler bleiben Fehler. Auswahlroot und
Profilroot werden auf Probereste geprüft. Ein fehlgeschlagener Kontext wiederholt
die Probe nicht. `plan()` selbst bleibt lesend.

Der Writer adressiert ausschließlich eine neue, exklusiv angelegte eigene Tempdatei.
Dateiidentität, erforderliche Eltern und Providerbindung bleiben gehalten. Nach
einem Abschlussversuch sind Schreiben, Seek, Größen- und Lochänderungen gesperrt;
ein erneuter erfolgreicher FLUSH bestätigt nur den bereits abgeschlossenen Stand.
CLOSE wird auch bei verlorener Antwort nicht wiederholt. Unklare Temps behalten
ihren vorhandenen Recovery-Intent.

Der geprüfte Samba-Stand kann beim Rename das Sparse-Attribut entfernen, obwohl
die physischen Löcher erhalten bleiben. Dies entspricht dem Archive-Set in
[`smb2_reply.c`](https://github.com/samba-team/samba/blob/samba-4.19.5/source3/smbd/smb2_reply.c#L1772)
und der Attributmaske in
[`dosmode.c`](https://github.com/samba-team/samba/blob/samba-4.19.5/source3/smbd/dosmode.c#L906).
Deshalb hält der eigene Temp einen vorab gewährten, objektgebundenen
Schreib-/Attributhandle bis zum Commit. Fremde Datenwriter bleiben ausgeschlossen;
die eigene Tempbeobachtung teilt diesen Zugriff nur nach ID-Prüfung. Derselbe Handle
schließt die Dateimetadaten ab, stellt das Sparse-Attribut nach Rename wieder her,
führt FLUSH aus und wird einmal geschlossen. Der gemeinsame Kopierpfad prüft danach
die ausgewählten Metadaten am endgültigen Ziel exakt zurück. Vor Rename bindet der
bestehende V3-Intent das Ziel als mögliche unklare Wirkung. Ein verschwundener
Tempname allein gibt einen Folgelauf dadurch nicht frei.

Der begrenzte Vertrag umfasst lokale APFS-/Btrfs-Quellen, leere und vollständig
gelochte Dateien, Dateninseln, dichte Daten sowie Daten- und Lochreste am ungeraden
EOF. Inhalt, logische Größe, serverseitige Bereiche und physischer Zielreadback
werden durch tatsächliche AddOnly-/Update-/Mirror-/Noop-Queuefälle geprüft.
Hardlink-Gruppen werden am SMB-Ziel weiterhin nicht rekonstruiert; die bekannte
Auslassung bleibt auch im leeren Folgelauf sichtbar. Geprüfte Antwortverluste vor
und nach Daten-/Metadaten-FLUSH, CLOSE und Sparse-Wiederherstellung erhalten
konkrete Teilwirkungen und verhindern blindes Wiederholen oder Mirror-Entfernungen.

Mit vorbereitetem Windows-DACL-Zielkontext können auch geeignete SMB-Quellen
Sparse-Bereiche, benannte Datenströme und die bisher unterstützten geschützten
expliziten Datei-DACLs gemeinsam übertragen. Die zulässigen ACE-, SID- und
Vererbungsformen werden dadurch nicht erweitert. Der einzige Sparse-Guard erhält
vor dem SET den benötigten DACL-Zugriff; der DACL-Abschluss leiht diesen FID ohne
einen zweiten Datenwriter und ohne eigenen CLOSE. Lokale Quellen benötigen dafür
keine zusätzlichen ACL-Rechte. Reine DACL-Änderungen erhalten weiterhin die
bestehende Dateiidentität und ihre beobachtete Sparse-Struktur.

Der geprüfte Server kann das Sparse-Attribut auch während des Stream-/
Metadatenabschlusses entfernen. Deshalb stellt der gemeinsame Guard es vor dem
Metadaten-FLUSH und nach Rename wieder her. Beide beabsichtigten Schritte sind
getrennt und einmalig; unklare Antworten werden nicht wiederholt. Identität,
Größe und Dateizeit bleiben bei der Wiederherstellung unverändert. Nach dem
Metadatenabschluss sind weitere Dateizeit- und DACL-Wirkungen dieses Temps gesperrt.
Der DACL-FLUSH-Proof ist erst nach exaktem DACL-Readback vollständig; der gemeinsame
Handle bleibt bis zum geprüften Commitabschluss gehalten.

Die Prüfungen umfassen DACLs ohne Daten-Deny sowie separate und kombinierte
Schreib-/Anhängeverbote, Noop, Inhaltsersatz, reine DACL-Wirkungen und Mirror über
die tatsächliche Queue. Bekannte DACL-Absagen vor SET lassen die anderen
unterstützten Klassen mit konkreter ACL-Auslassung weiterlaufen. Ein unklarer SET,
Abbruch, Tempnamen-Tausch oder verlorener Metadaten-/Commitabschluss erhält dagegen
die konkrete Wirkung und den Intent. Vor dem Commit bleibt das bestehende Ziel
erhalten; nach einem wirksamen Rename wird kein falscher Rollback behauptet.

Eine abgeschlossene konkrete DACL-Zielabsage lässt den unabhängigen Sparse-/
ADS-Transfer inzwischen ebenfalls zu. Der eigene Abschlusszugriff benötigt dann
kein WRITE_DAC; die Sparse-Wiederherstellung vor dem Metadaten-FLUSH bleibt wegen
der Datenströme erforderlich. Die ACL-Auslassung wird gesondert bilanziert. Ein
unvorbereiteter Kontext, fehlender Queueowner oder unbekannter Probeausgang
erteilt diese zusätzliche Fähigkeit nicht.

Ein Zwei-Share-Gegenlauf trennt exakte Quellbeobachtung von einem Ziel, das DACLs
normalisiert, aber Inhalte, Sparse-Bereiche und ADS exakt erhält. Bei konkret
nicht übertragbarer Quell-ACL bleiben Ergänzen, Inhaltsersatz, Mirror und leere
Folgeläufe möglich; die ACL-Auslassung bleibt sichtbar. Für eine übertragbare
Quell-ACL kann ein späterer Lauf dagegen die getrennt bestätigte DACL-Wirkung an
der bestehenden Datei vorschlagen. Normalisiert der Server diese Wirkung nach
SET, bleibt deren Ausgang unklar; daraus wird kein erfolgreicher leerer Folgelauf.
Die Temp-Probe ersetzt diesen getrennten Dateivertrag nicht.

Eine gemeldete SMB-Volumeneinheit ist keine belegte Sparse-Blockgrenze.
[FileFsSizeInformation](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fscc/e13e068c-e3a7-4dd4-94fd-3892b492e6e7)
meldet Sektoren und Allokationseinheiten; Samba kann mit
[`block size`](https://www.samba.org/samba/docs/current/man-html/smb.conf.5)
die Anzeigeeinheit ändern. Ein künstlicher SMB-3.1.1-Gegenlauf meldet damit
64 KiB, während Ergänzen und der leere Folgelauf weiterhin exakte 4-KiB-Datenbereiche
sowie identische Inhalte erhalten. Daraus darf der Planner keine angeblich zu
feinen Quellgrenzen ableiten. Weitere Servergranularitäten brauchen eine eigene
objektgebundene Darstellbarkeitsprüfung.

Portable, NativeStrict, strikter Remote-Sync,
SMB-Hardlink-Schreiben, weitere Servergranularitäten und eine Windows-Server-Abnahme
werden hierdurch nicht erweitert. Native Clientprüfungen auf macOS und Linux
verwenden künstliche Samba-Roots; eine zusätzliche Tauri-/SMB-GUI-Abnahme ist nicht
enthalten.

#### DACLs bestehender regulärer SMB-Dateien

Der gewöhnliche `NativeBestEffort`-Inhaltssync besitzt eine separate
`SetFileDacl`-Aktion für reine DACL-Abweichungen bei gleichem Hauptinhalt und gleicher
vollständig beobachteter ADS-Menge. Beide DACLs sind geschützt, explizit, nichtleer
und Revision 2; Servernamensraum und Owner-/Group-SID-Kontext sind gebunden.
Der neue Encoder verlangt nur `0x00160081` für Inhalts-/Security-Readback und
`WRITE_DAC`. Der eigene Temp-Encoder verlangt weiterhin `0x001701ff` und besitzt
den oben beschriebenen getrennten Daten-Deny-Abschluss; der Ordnerencoder bleibt
unverändert. Es werden weder Hauptdaten noch ADS, Owner, Group oder SACL geschrieben,
keine Namen angelegt, gelöscht oder umbenannt und keine Datei ersetzt.

Dieser Inplace-Vertrag erhält explizite Deny-ACEs für `FILE_WRITE_DATA`,
`FILE_APPEND_DATA`, `FILE_READ_EA`, `FILE_WRITE_EA`, `FILE_EXECUTE`,
`FILE_WRITE_ATTRIBUTES`, `DELETE`, `WRITE_DAC` und `WRITE_OWNER`, einzeln oder kombiniert
(Maske `0x000d013e`). Der Änderungszugriff `0x00160081` wird vor dem einzigen SET
gewährt. Danach liest der frische Abschluss nur Inhalt, Attribute und Security
mit `0x00120081`; er verlangt kein erneutes `WRITE_DAC`. Ein expliziter
Owner-Allow muss weiterhin sämtliche benötigten Rechte `0x00160081` erlauben.
Die Deny-Maske darf keine weiteren Bits enthalten; insbesondere bleiben das
Verzeichnisrecht `FILE_DELETE_CHILD`, generische und unbekannte Bits ausgeschlossen.
ACE-Reihenfolge und Trustees bleiben exakt erhalten. Datei-Temps lassen davon
Daten-/EA-/Ausführungs-Denys sowie WRITE_DAC und WRITE_OWNER zu; Ordner behalten ihren eigenen
Vertrag. Generisches `WRITE` bleibt ausgeschlossen, weil es unter anderem
`SYNCHRONIZE` umfasst und damit auch Leseöffnungen verhindern kann
([Microsoft: Dateizugriffsrechte](https://learn.microsoft.com/en-us/windows/win32/fileio/file-security-and-access-rights)).

Ein Datei-Deny für `DELETE` ist kein allgemeines Löschverbot: Der Server kann
Löschen zusätzlich über `FILE_DELETE_CHILD` des Elternordners erlauben. Der
Inplace-Vertrag überträgt ausschließlich die Datei-DACL und verändert keine
Elternrechte. Diese getrennten Freigabewege gehören zur
[Windows-Dateirechtesemantik](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-deletefile).
Die konkreten Datei- und EA-Rechte beschreibt die
[Microsoft-Rechtetabelle](https://learn.microsoft.com/en-us/windows/win32/fileio/file-access-rights-constants).

Bei bereits gleichen DACLs mit passenden beobachteten SID-Kontexten erzeugt eine
für Inplace unterstützte Rechteform keine ausschließlich aus dem stärkeren
Datei-Tempvertrag abgeleitete ACL-Auslassung mehr. Das gilt auch, wenn ein
Temp-Writer im selben Lauf vorbereitet ist. Die manifestgebundene
Sicherheitsbeobachtung bleibt erhalten; es entsteht keine Schreibwirkung.
Tatsächlich nicht unterstützte Deny-Bits oder Vererbung bleiben außerhalb des
begrenzten Schreibvertrags und behalten ihre erkannten Auslassungen.

Diese Rechtewirkung gilt für das gesamte Dateiobjekt. Ein anderer SMB-Client kann
auch bei gehaltenem Datei-/Elternhandle ohne Share-Delete und sogar bei
Share-Read-only einen weiteren Hardlink anlegen. Eine beobachtete Linkzahl ist
keine Exklusivitätsgarantie. Die ausdrückliche Bestätigung umfasst deshalb alle
bestehenden und künftig angelegten Namen, auch außerhalb der ausgewählten Ordner;
die bekannte Namensliste bleibt ausdrücklich unvollständig. Alle im Scan bekannten
Quell-/Ziel-Aliaspaare geben ihren gewünschten Rechtezustand ab, auch bei Noop,
Skip oder einer anderen geplanten Aktion. Widersprüchliche Sollwerte sperren die
Objektwirkung vor der Vorschau. Identische Sollwerte werden nach gebundenem
Server-/Share-Namensraum und Datei-ID zu genau einer Wirkung zusammengefasst.

Die additive V6-Registry speichert vollständige Objektbindung, Inhalts-/ADS-Belege
und Aliasumfang offener Wirkungen in einem atomaren heißen Ledger. Der
Einzelsnapshot ist ein zusätzlicher Prüfpfad; Verlust oder ein veralteter Snapshot
leeren das Ledger nicht. Initiale
Leaseanlage, Ledgerregistrierung und Generationenwechsel besitzen denselben
Registrylock. Ein dauerhaftes `Applying` steht vor einem einzigen SET. Exakter
Readback erfolgt am selben gehaltenen und anschließend an einem frisch geöffneten
Handle. Ein bestätigtes `Applied` überlebt Abbruch und Queuefehler vor `finish`.
Der synchrone Queuecallback sichert den Bericht vor dem Abschluss des Intents.

Beim Abschluss wird zuerst der vollständige Closed-Beleg in einem privaten
Ownerunterordner dauerhaft geschrieben. Erst danach entfernt ein atomarer heißer
Commit den offenen Record und führt den Ownerzähler fort. Ein im Ledger enthaltener
ausstehender Zählercommit wird unter demselben Lock idempotent beendet; die genau
benannten privaten Metadaten werden zuletzt entfernt. Ein Crash kann dadurch keine
unregistrierte Wirkung und keinen doppelt angerechneten Ownerbeitrag erzeugen.
Closed-Belege bleiben bis zum nachweislich dauerhaften, am konkreten Outcome
gebundenen Queuecheckpoint erhalten. Ohne solchen Checkpoint behält auch ein
direkter Provideraufruf seinen vollständigen Beleg. Anzahl und aggregierte
Prüfsumme pro Owner erkennen verschwundene oder veränderte Closed-Belege bei
Recovery. Ein separater Gesamtzähler erkennt dabei auch fehlende Ownerheader oder
ein verschwundenes gesamtes Beleginventar; normale Guards lesen diese Historie nicht. Der Queuegesamtbericht erhält schon freigegebene bestätigte Wirkungen;
ein erkannter Vorherzustand entfernt nur den exakt zugehörigen unklaren Ausgang.

Monotone Namensraumzähler verbleiben unabhängig vom Retirement im heißen Ledger.
Separate Ownerzähler erlauben während eines Laufs den Abzug eigener Beiträge;
ein verbleibender Wert null entspricht dem bisherigen leeren Zustand. Normale
Guards lesen die abgeschlossene Vollplanhistorie nicht. Die vorhandene V6-Fassung
wird unter demselben Lock über zuerst gesicherte Einzelbelege migriert; V1–V5
bleiben unverändert. `AddOnly` nutzt weder die neue Vorbereitung noch Aliasbindung.

Nach möglichem SET gibt es keine blinde Wiederholung, Rücksetzung oder Bereinigung.
Recovery prüft Identität, SID-Kontext und DACL am selben gehaltenen Handle; nur ein
zum Intent passender Vorher-/Nachherzustand kann bewusst geschlossen werden. Bereits
abgeschlossene Objektwirkungen desselben Laufs bleiben im Gesamtbericht erhalten.
Offene V6-Intents, ein beschädigter heißer Registryzustand und ein verlorener
privater Confignamensraum bleiben auch vor fremden/strikten Scans gesperrt. Fehlende
oder beschädigte aufbewahrte Closed-Belege sperren die betreffende Queue-Recovery. Diese Klasse erweitert weder
`Portable` noch `NativeStrict` oder den strikten Syncvertrag.

Die gezielte Erweiterungsprüfung verwendet bitweise Formatgrenzen, beide
ACE-Reihenfolgen, Owner-/Everyone-Trustees und unveränderte Temp-/Ordnergrenzen.
Die tatsächliche Queue-Matrix ergänzt einzelne und kombinierte Denys der genannten
Dateirechte sowie bekannte Hardlinknamen. Rohe `FILE_OPEN`-Anfragen belegen
verweigerte und weiterhin erlaubte Daten-, EA-, Ausführungs-, Attributschreib- und
DELETE-Zugriffe, ohne einen Daten- oder Metadatenschreibbefehl zu senden. Der
Owner-Allow würde alle kontrollierten Zugriffe ohne Deny erlauben. Die künstliche
Zieleltern-DACL bietet dabei keine alternative `FILE_DELETE_CHILD`-Freigabe;
diese Fixtureänderung gehört nicht zur Produktwirkung. Leere Folgeläufe prüfen
zusätzlich die ACL-Bilanz. Antwortverlust und Abbruch sind auch mit der vollständig
kombinierten Deny-Maske samt rein lesendem Neustartabschluss belegt.

Am 2026-10-31 bestehen 22 echte Queuefälle auf Samba 4.19.5 sowie die vollständigen
Core-, App- und CLI-Suiten, Workspace-Clippy und Formatprüfung auf macOS und Linux.
Die Fuzz-Übersetzung mit `--locked` ist auf macOS bestanden. Die native Fixture
prüft Quell- und Ziel-DACL, SIDs, ADS und mtime unabhängig vor dem Lauf sowie
originale Ziel-ID, SID-Kontext, gewünschte DACL, Hauptinhalt, ADS und mtime nach
normalem Abschluss und nach den neuen Recoveryfällen. Ein Windows-Server oder
eine neue native Tauri-/SMB-GUI-Ausführung ist damit nicht geprüft.

Der echte Provider- und Queue-Gegenlauf verwendet künstliche Bestände auf
Samba 4.19.5 mit `streams_xattr`, `acl_xattr` und
`acl_xattr:ignore system acls=yes`. Er bestätigt ID-/Inhalts-/ADS-/SID-Erhalt,
bestehende und neu angelegte externe Hardlinknamen, Alias-Konflikte, Drift,
konkrete Lesezugriffsverweigerung, Antwortverlust und rein lesenden Wiederanlauf
mit einer neuen SMB-Sitzung. Dies belegt die geprüfte Serverkonfiguration,
keine allgemeine Windows- oder Mehrservergarantie. Core-, CLI-, App-, Clippy- und
Fuzz-Gates bestehen auf macOS und Linux. Browserprüfungen und ein natives
WKWebView-Fixture mit den echten Oberflächendateien prüfen Darstellung und
Bedienung; das Fixture ersetzt keinen echten Tauri-IPC-/SMB-GUI-Lauf.

#### Rechte selbst neu angelegter leerer SMB-Unterordner

Der additive V7-Vertrag gilt nur für gewöhnlichen `NativeBestEffort`-Inhaltssync.
Die Quelle muss im vollständigen ungefilterten Listing tatsächlich leer sein und
eine vollständig beobachtete leere `named_streams`-Menge besitzen. Excludes
machen einen nichtleeren Ordner nicht geeignet. Quelle und Strommenge werden
frisch vor CREATE sowie vor der optionalen Rechtewirkung erneut geprüft.
Nichtleere Ordner und nichtleere oder unbekannte Strommengen sind konkrete
Implementierungsauslassungen; sie begründen kein allgemeines SMB-Protokollverbot.

Die Einzelwirkung endet im `CreateDir`-Zweig: einmaliger exklusiver normaler
CREATE, gewöhnliche Metadaten, optionale Rechtewirkung, dauerhafter Queuecheckpoint
und Finish. Die CREATE-Maske bleibt `0x00100080` (`READ_ATTRIBUTES | SYNCHRONIZE`),
mit den bisherigen normalen Attributen und Shareflags. Der originale Responsehandle
bleibt gehalten; zusätzliche Securityrechte werden ihm nicht zugeschrieben.
Der getrennte Securityhandle öffnet erst nach gewöhnlichen Metadaten und muss
gegen originale Objekt-ID sowie frische Eltern-/Rootbindung dasselbe Objekt
bezeichnen. Es gibt keinen langfristigen Receiptpool oder beliebige Dateizahlgrenze.
Alle zusätzlichen Handles schließen, bevor die nächste normale Mutation beginnt.

Der mtime-Adapter unterscheidet Fehler vor SET, möglicherweise gesendeten SET und
bestätigten SET mit unklarem Readback. Bestehende Aufrufer behalten ihre Semantik.
Scheitert vor dem ersten normalen Zeit-SET die zusätzliche Öffnung oder die nur
für die Objektbindung benötigte ID-Abfrage, darf der Receipt nach bestätigter
Handlefreigabe endgültig aufgegeben werden. Der bisherige Zeitweg schreibt danach
genau einmal. Erkannter Identitätswechsel und Abbruch bleiben Fehler; ein sicher
vor SET auftretendes `Interrupted` wird unverändert weitergereicht. Nach möglichem
SET gibt es keinen Mutationsretry; ein rein lesender Abschluss darf ausschließlich
lesen. Ein bekannter Objekt- oder Quelltausch bleibt Plan-Drift, keine
Metadatenauslassung.

`CreatedDirectoryDaclSelection` bindet gewünschte geschützte explizite Rechte und
den vorbereiteten Probe-Kontext. Ein nicht serialisierbarer CREATE-Receipt liefert
erst zur Laufzeit die tatsächliche Objekt-ID. Die Before-DACL samt Owner und Group
wird am zusätzlich gebundenen Securityhandle beobachtet; sie darf geerbt oder
ungeschützt sein. Nur After muss die begrenzte geschützte explizite Revision-2-Klasse
mit konkreten Allow-/Deny-ACEs erfüllen. Ihr Ownerzugriff bleibt `0x001600a1`;
Owner-, Group-, SACL-, Daten-, Kind- und Streammutationen gehören nicht zu diesem SET.
Eine abweichende tatsächliche Targetgruppe bleibt eine konkrete Auslassung gegenüber
der Probe. Sie wird weder aus der Parentgruppe noch aus der Sourcegruppe erfunden.

Der dauerhafte V7-Plan besitzt `ExclusiveMinimalCreate` als Herkunft, Objekt-ID,
Server-/Share-Namensraum, tatsächlichen SID-Kontext und beide DACLs. Vor dem einzigen
SET steht `Applying` dauerhaft; gehaltener und frisch geöffneter Readback müssen
Objekt-ID, Kontext und After-DACL exakt bestätigen. Dann folgen dauerhaftes
`Applied`, synchroner Queuecheckpoint und Finish. Ein eigener EffectKey erhält
Herkunft und Objekt-ID im normalen CREATE-Teilbericht; dessen Metadatenmarkierung
behauptet noch keine DACL-Wirkung. Bereits früh abgeschlossene Metadaten werden aus
dem globalen Ordnerdurchgang ausgenommen. V7 nimmt außerdem nicht am allgemeinen
ACL-Schreiben oder dessen generischer ACL-Verifikation teil.

Die privaten V7-Belege verwenden getrennte heiße und geschlossene Daten sowie
Ownerbeiträge. Vollpläne wachsen nicht mit abgeschlossener Historie im heißen
Ledger. Nur ein nachweislich dauerhafter Queuecheckpoint gibt den exakten
Objekt-/Herkunftsbeleg frei; direkte Providertransaktionen behalten ihn.
Offene Rechtewirkungen blockieren abhängige Scans und Wirkungen auch anderer
Fenster. Eine Disjunktheitsbehauptung braucht echte Root- und Vorfahren-IDs;
unterschiedliche Namensschreibweisen allein reichen nicht. Ungeklärte Rootpaare
bleiben gesperrt. Anders als V6 besitzt V7 keine pauschale Share-weite
Dateihardlink-Wirkung.

Queuejournalversion 36 schützt sämtliche neuen Plan-, Teilbericht-, Fortschritts-
und Recoveryformen. Bestätigte und unklare Rechtewirkungen bleiben getrennt in
Queue, Audit, CLI und Oberfläche. Nach CREATE- oder mtime-Unklarheit greift der
normale Vertrag; daraus entsteht weder ein DACL-Retry noch ein rekonstruierter
Receipt. Nach möglichem DACL-SET ist nur bewusste beobachtende Recovery zulässig:
gleiche Identität und Kontext, erkannter Before-/Afterzustand, keine erneute
Mutation, Rücksetzung oder Löschung. Drittzustände und fehlende Belege bleiben offen.
Die alte V5-ExistingDirectoryExact-Klasse sowie V1–V6-Formate und Digests bleiben
semantisch unverändert; `Portable` und `NativeStrict` erhalten keine neue Fähigkeit.

Der echte Queue-Gegenlauf auf Samba 4.19.5 mit `streams_xattr`, `acl_xattr` und
`acl_xattr:ignore system acls=yes` besteht alle 24 Fälle. Er kombiniert echte
SMB-/Queuewirkungen und eine echte `WRITE_DAC`-Absage mit synthetisch injizierten
Ressourcen-, Beobachtungs- und Readbackfehlern sowie Antwortverlusten. Geprüft sind
Update, AddOnly und Noop, normaler CREATE-/Metadatenfortschritt, konkrete
Rechteauslassungen bei weiterlaufenden Inhalten, Drift und Abbruch sowie der rein
lesende Abschluss mit neu geöffneter Queue und neuer SMB-Sitzung. Dies belegt die
geprüfte Serverkonfiguration, keine allgemeine Windows- oder Mehrservergarantie.
Core-, CLI-, App-, Clippy- und Fuzz-Gates bestehen auf macOS und Linux.
Browserprüfungen und eine native WKWebView-Fixture mit den echten Oberflächendateien
prüfen Darstellung und Bedienung; sie ersetzen keinen echten Tauri-IPC-/SMB-GUI-Lauf.

#### DACLs bestehender SMB-Unterordner

Gewöhnlicher `NativeBestEffort`-Sync darf die DACL eines bereits bestehenden,
manifestgebundenen Unterordners im selben belegten Servernamensraum ändern.
Der gewählte Root bleibt ausgenommen. Sowohl vorherige als auch gewünschte DACL
müssen zur geschützten expliziten Revision-2-Klasse ohne Vererbungsflags gehören.
Die vorbereitete private Ordnerprobe muss exakten Roundtrip und frischen Zugriff
belegen; Owner und Group des vorhandenen Zielordners müssen zu ihrem Kontext passen.
Der gehaltene Zugriff verlangt `WRITE_DAC`, jedoch weder DELETE noch `WRITE_OWNER`.
Die allgemeine ACL-Fähigkeit bleibt lesend. Neue Ordner, komplexe oder erbende
DACLs und abweichende Kontexte erhalten konkrete Auslassungen bei weiter möglichem
Inhaltssync. Die Rechteklasse erweitert `NativeStrict` nicht.

Der Plan bindet vorherige und gewünschte DACL samt starker Zielidentität. Vor der
ersten unabhängigen Wirkung werden alle Beobachtungen erneut geprüft. SET läuft
in einem eigenen letzten Durchgang nach Kindern, Entfernungen, Streams, Zeiten und
übrigen Metadaten. Die Wirkung ändert ausschließlich DACL und Schutzstatus am
gehaltenen Ordner; Identität, Owner/Group, Kinder und Streams bleiben erhalten.
Eine reine Ordner-DACL-Änderung benötigt keine Dateikopie und keine Transferbytes.
Sie wird als Metadatenänderung und im gesonderten DACL-Bericht gezählt.

Die Vorschau nennt alle betroffenen Zielordner und die endgültige Wirkung ohne
Quarantäne oder automatische Wiederherstellung. Ein typisierter V5-Intent speichert
Owner, Endpunkt-, Root- und Ordneridentität, SID-Kontext und beide DACL-Digests.
`Applying` wird vor SET dauerhaft geschrieben; `Applied` verlangt exakten Readback
und belegte Folgezugriffe. Möglicherweise wirksame Fehler behalten einen unklaren
Bericht und offenen Intent. Fortschritts- und Queuecheckpoints erhalten bestätigte
Teilwirkungen auch bei späterem Abbruch. Offene Prepared-Intents benötigen denselben
besonderen Abschluss, selbst wenn noch kein Wirkungsbericht vorliegt.

Der ausdrückliche Abschluss in Oberfläche beziehungsweise
`acknowledge-directory-dacl <operation-id> --reviewed` verbindet sich mit dem
ursprünglich gespeicherten SMB-Profil. Ein gemeinsamer gehaltener Lesehandle liefert
Ordner-ID, Owner/Group und DACL. Nur dieselbe Identität mit beobachtetem Vorher- oder
Nachherwert kann den eigenen Intent schließen. Fremde Identität, ein dritter Wert
oder unlesbare Sicherheitsdaten bleiben blockiert. Der Abschluss sendet keinen
SET und löscht oder restauriert nichts am Server. Ein aktueller Vorherwert beweist
nicht, dass historisch nie ein SET stattgefunden hat. Ein bereits belegtes Applied
bleibt deshalb als bestätigte Teilwirkung erhalten. Der Queueeintrag endet als
bewusst abgeschlossenes `Cancelled`; ein neuer Lauf braucht eine frische Vorschau.
Unabhängige Recoverypfade bleiben bestehen. Ältere V1–V4-Intents erhalten dadurch
keine neue Bedeutung.

### WebDAV-Sicherheitsbeobachtungen

Native Manifeste lesen `DAV:acl` und `DAV:owner` mit einer ergänzenden
Depth-0-Abfrage. Die üblichen Listing- und Portable-Abfragen bleiben unverändert.
HTTP-Abfragestatus, ein ausdrücklicher Ressourcenstatus und die Statuswerte der
beiden Properties bleiben getrennt. Eine fehlende Antwort, Property-403,
Property-404 und ein ausdrücklich leerer 200-Wert bedeuten unterschiedliche Dinge.
Auch ein erfolgreicher ACL-Wert ist hier nur opak beobachtet: Der Server kann
lediglich einen sichtbaren Teil seiner ACL liefern. Benutzerprivilegien und
anbieterspezifische Ownerfelder ersetzen keine vollständige ACL oder UID/GID-Domäne.

Erfolgreiche Werte werden über expandierte XML-Namen, sortierte Attribute und
geordnete Inhalte gehasht. Rohwerte und Principal-URLs werden nicht persistiert.
Der Namensraum bindet Endpunkt, Providerroot, Konto und TLS-Identität. Vor und nach
der Abfrage werden Ressource und Sitzung erneut geprüft. Die Antwort ist auf
1 MiB begrenzt; widersprüchliche Ressourcenpfade, XML-/Transportfehler und Drift
bleiben Fehler. Eine zusätzliche HTTP-Absage 403/404/405/501 wird nur als konkrete
Beobachtungslücke behandelt, wenn die getrennten Ressourcenprüfungen das weiterhin
bestehende Objekt bestätigen. Die bestehende Best-Effort-Grenze für Collections
bleibt erhalten. Optionale DAV-Fehlerdetails und Location-Felder werden nicht als
Ressourcenadresse oder ACL interpretiert und nie verfolgt; sie ändern den
beobachteten Status nicht ([RFC 4918, Abschnitt 14](https://www.rfc-editor.org/rfc/rfc4918.html#section-14)).

Quell- und Zielbeobachtungen erscheinen im pfadgenauen ACL-/Owner-Bericht auch
bei einem leeren Folgelauf als Auslassung. Berechtigungsfehler werden getrennt
gezählt. Lesbarer Inhalt bleibt übertragbar; reine Beobachtungsunterschiede
verursachen keinen Basistausch. Die bestätigten Manifestdigests werden vor der
ersten Wirkung erneut geprüft, auch wenn die betreffende Datei keine Aktion hat.
Neue DAV-Beobachtungen verwenden V5. Ohne sie bleiben V2/V3/V4 und ihre JSON-Form
bytekompatibel. Ein allgemeines DAV-ACL-Modell, ACL-Schreiben oder eine neue
`NativeStrict`-Zusage ist damit nicht verbunden.

### Benannte SMB-Datenströme regulärer Dateien

Der gewöhnliche Inhaltssync kann im Profil `NativeBestEffort` benannte
SMB-Datenströme lesen und auf einem dafür geeigneten SMB-Ziel erhalten. Die
Fähigkeit stammt aus `FILE_NAMED_STREAMS` des konkreten Roots und den beobachteten
Zugriffsrechten. Schreibzugriff gilt ausschließlich für eigene neue Datei-Temps;
die allgemeine SMB-Sicht erhält dadurch keine strikte Schreibfreigabe.
`Portable` behält seinen Vertrag ohne diese zusätzliche Klasse.

Das Manifest bindet die vollständige sortierte Namensmenge, Größen und
BLAKE3-Digests. `None` bedeutet unbeobachtet, `Some([])` nachweislich leer.
Manifeste ohne dieses Feld behalten ihre bisherige JSON-Form und den V2-Digest;
beobachtete Mengen verwenden V3. Bestehende Queue- und Laufjournale bleiben
lesbar. Eine inzwischen zusätzliche Beobachtung darf eine alte Vorbereitung als
Drift stoppen, ohne ihren Ausführungsvertrag umzudeuten.

Auch bei unverändertem Hauptinhalt löst eine abweichende Streammenge ein Update
aus. Hauptinhalt und sämtliche Streams werden in eine exklusive neue Basisdatei
geschrieben und vollständig zurückgelesen. Erst danach wird die ganze Datei
übernommen. Eine beobachtet leere Quelle entfernt damit alte Zielstreams;
unbeobachtete Quellstreams werden niemals als leere Menge behandelt. Größen und
Fortschritt enthalten die tatsächlich übertragenen Streambytes. Fehler vor dem
Commit erhalten das Altziel, ein fehlgeschlagener Abschluss danach meldet
`OutcomeUnclear` mit den zu prüfenden Pfaden.

Die SMB-Lesesicht prüft vollständige `FileStreamInformation`-Antworten und zwei
übereinstimmende Mengen-/Hashbeobachtungen unter gehaltenen Datei- und
Elternhandles. Eine wirkliche Umbenennungsbindung benötigt an Verzeichnissen
`FILE_LIST_DIRECTORY` beziehungsweise `FILE_READ_DATA`; reine Attributhandles
sind nur Identitätszeugen. Die öffentliche starke Lesesicht hält die vollständige
benötigte Kette mit echten Lesehandles. Der gewöhnliche Inhaltssync bindet damit
seine gewählte Wurzel und deren Nachfahren, für die ohnehin vollständige Listings
erforderlich sind. Für höhere Vorfahren verlangt er keine zusätzlichen
Auflistungsrechte: Deren Identitäten werden frisch geprüft, die serverseitige
Pfadauflösung bleibt dort sichtbar Best Effort. Das gewährt keinen strikten
Remote-Vertrag.

Die lesende Dateisystem-/Fähigkeitsabfrage verlangt nur Attributrechte.
Eine Zielrolle prüft zusätzlich gezielt `FILE_ADD_FILE`, ohne Löschrechte
anzufordern. So kollidiert eine zweite lesende Sitzung nicht allein wegen
unnötiger Rechte mit gehaltenen Handles. Nur eine ausdrückliche Ablehnung des
geprüften Schreibrechts belegt `ReadOnly`; eine vorübergehende Sharesperre oder
ein Verbindungsfehler bleibt ein Fehler.

Die derzeitige Antwortgrenze beträgt 64 KiB; Overflow und
unvollständige Antworten sind Fehler. Ungültige UTF-16-Namen, nicht unterstützte
Streamtypen und kollidierende Lookupnamen werden nicht als vollständiger Bestand
ausgegeben. Ein Streamwriter bestätigt erst nach FLUSH und CLOSE den Abschluss.
Die Antwortstruktur folgt [Microsoft FileStreamInformation](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fscc/f8762be6-3ab9-411e-a7d6-5cc68f70c78d).

Verzeichnisstreams werden an einer ausdrücklich als Ordner geöffneten Basis
vollständig gelesen und manifestiert. Eine erfolgreiche leere Antwort bedeutet
beobachtet leer; Fehler oder fehlende Unterstützung bleiben davon getrennt.
Nur ausdrücklich nicht unterstützte Verzeichnisabfragen ergeben eine
unbeobachtete Menge nach sauberem Handleabschluss; der Inhalt bleibt im
Best-Effort-Profil übertragbar. Rechte-, Verbindungs-, Parser-, Puffer- und
Abschlussfehler bleiben Fehler. `NativeStrict` verlangt bei angekündigter
Lesbarkeit auf beiden vorhandenen Seiten eine vollständige Beobachtung, auch
ohne geplante Schreibwirkung.
Der gewöhnliche `NativeBestEffort`-Schreibweg erhält Verzeichnisstreams durch
einzeln bestätigte Ergänzungen, Ersetzungen und Entfernungen am gehaltenen
Ordner. Ein exklusiver Creator bleibt vom Anlegen des Streamtemps bis zum
Commitreadback geöffnet. Vollständige Mengen und Hashes werden vor und nach jeder
Wirkung geprüft; nur der exakt eigene Live-Creator ersetzt dabei einen separaten
Reader. Basis-IDs gelten nicht als Besitzbeleg eines Streams. Zusammengeführte
Ordner und normale Kinder werden nicht ausgetauscht.

Ersetzen verwendet einen einzelnen Rename mit frischer Prüfung und vollständigem
Readback. Es gibt keinen atomaren Vergleich gegen die zuvor geplante Zielfassung;
parallele Änderungen im Best-Effort-Fenster können endgültig ersetzt werden.
Entfernungen verwenden einen frischen, danach nicht umbenannten Zielhandle und
bestätigte Abwesenheit. Ersetzungen und Entfernungen besitzen keine Quarantäne.
Alle Streamkopien laufen vor Entfernungen, bestätigte Teilwirkungen werden vor dem
Intentabschluss dauerhaft im Fensterjournal gesichert. Fehler und Neustarts
behalten typisierte Temp-/Finalhinweise und bekannte beziehungsweise unklare
Einzelwirkungen; daraus entsteht weder ein automatisches Löschrecht noch ein Retry.

AddOnly lässt vorhandene Ordnerstreams unverändert; neue Ordner erhalten die
Quellmenge. Update und Mirror stellen die konkret bestätigte Menge her. Identische
Mengen bleiben ohne Schreibaktion. Fehlende Unterstützung und unbeobachtete Mengen
bleiben auf ungeeigneten Zielen pfadgenau als Auslassung sichtbar, auch im leeren
Folgelauf. Die getrennte strikte Klasse erhält keine neue Schreibfähigkeit.
Nichtleere Streams direkt an einer Freigabewurzel mit intern leerem Basisnamen
sind noch nicht adressierbar. Die gewählten Syncwurzeln dienen als Container;
ihre eigenen Metadaten werden nicht auf die Zielwurzel übertragen.

Lokale Adapter besitzen keinen generischen
Schreibweg für diese SMB-Klasse; xattrs oder Ressourcen-Forks werden nicht als
Ersatz erfunden. Auch andere nicht darstellbare Ziele erhalten eine konkrete
Auslassung bei weiterhin möglichem Inhaltstransfer. `NativeStrict` wird nie
herabgestuft. Server- und Dateisystemgrenzen gelten zusätzlich, insbesondere bei
Sambas [Speicherung in xattrs](https://www.samba.org/samba/docs/current/man-html/vfs_streams_xattr.8.html): Ein abgelehnter nativer Streamtransfer wird nicht
als vollständige Inhaltskopie ausgegeben.

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

- Der lokale Quelladapter ermittelt Datenbereiche mit `SEEK_DATA`/`SEEK_HOLE` und speichert
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

#### Lokale Allokationsgrenzen

Ein grundsätzlich Sparse-fähiges Ziel kann nicht jede feinere Quellstruktur exakt
abbilden. Für APFS, ext4 und Btrfs liest der lokale Adapter die konkrete
Allokationsblockgröße am gehaltenen Ziel-Elternordner. Noch fehlende Eltern werden
bis zum nächsten vorhandenen Ordner innerhalb des Roots zurückverfolgt;
Symlinks werden nicht betreten. Mountidentität und Blockgröße werden rein lesend
geprüft. Andere Treiber erhalten aus dieser Abfrage keine Geometriezusage.

Nur tatsächliche Lochgrenzen benötigen ganze Zielblöcke. Angrenzende Datenstücke
bilden einen Datenlauf; das logische Dateiende darf im letzten Datenblock liegen.
Ist eine gemeldete Struktur nachweisbar feiner, bilanziert `NativeBestEffort` diese
Sparse-Klasse bereits in der Vorschau als ausgelassen und kopiert den logischen
Inhalt über den normalen geprüften Datenstrom. Andere Metadaten und
Hardlink-Gruppen bleiben unabhängig erhalten. Die Entscheidung gilt auch für den
abgesicherten lokalen beziehungsweise CLI-Pfad dieses Profils; dessen Permit- und
Mirror-Gates bleiben unverändert. `NativeStrict` verweigert eine bekannte nicht
darstellbare Struktur vor jeder Mutation. Unbekannte Geometrie behält den
bisherigen exakten Versuch samt zwingendem Readback.

Diese Entscheidung ist kein generisches Ignorieren von Schreib- oder
Abschlussfehlern. Quell- und Zielidentität, Hash, Reader-Abschluss, Flush, Close,
Quarantäne und Commit bleiben unverändert. Ein echter leerer Folgelauf erhält die
konkrete Auslassung, ohne denselben Inhalt allein wegen unmöglicher Sparse-Grenzen
wieder zu kopieren. Vorhandene Noop-Metadaten sind auch vor unabhängigen
Mirror-Entfernungen digestgebunden.

Beim Sparse-Transfer einer neuen Temp wird der letzte EOF-Lochbereich bis zur
nächsten ganzen Zielblockgrenze deallokiert. Die logische Dateigröße bleibt dabei
unverändert; nach EOF liegen keine zu erhaltenden Daten. Nur den Nullrest
unberührt zu lassen genügt auf APFS nicht, weil er weiterhin als belegter
Datenblock gemeldet werden kann. Der exakte Metadaten- und Inhaltsreadback bleibt
verpflichtend. Linux beschreibt ebenfalls, dass partielle
Blöcke bei `PUNCH_HOLE` nur genullt und ganze Blöcke deallokiert werden
([`fallocate(2)`](https://www.man7.org/linux/man-pages/man2/fallocate.2.html)); die
macOS-`fcntl(2)`-Manpage verlangt für `F_PUNCHHOLE` Blockalignment.

Die gezielten Gegenläufe verwenden wirkliche lokale Ziele, eine ausdrücklich
emulierte feinere Quellstruktur und echte lokale beziehungsweise Samba-Queues.
Der Prüfvertrag umfasst Inhaltsvergleich, Noop-Konvergenz, Hardlink-Identität,
ungerades EOF und Drift vor Entfernungen. Daraus folgt keine universelle
Darstellbarkeitszusage für andere Dateisysteme, Kompressions- oder Speicherschichten.

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


## Rechte neuer nichtleerer SMB-Unterordner

V8 ergänzt ausschließlich gewöhnlichen `NativeBestEffort`-Inhaltssync mit
`ContentHash` um frühe Rechte für selbst exklusiv neu angelegte nichtleere Ordner.
`CreatedDirectoryBeforeChildrenSelection`,
`WindowsDaclTransfer::CreatedDirectoryBeforeChildrenExact` und
`ExclusiveMinimalCreateBeforeChildren` bilden eine eigene Klasse. V7-Auswahl,
Origin, Encoder-Mindestmaske `0x001600a1`, Payloads und Digests bleiben erhalten.
V8 verlangt eine geschützte explizite Revision-2-DACL ohne ACE-Vererbung und eine
konservative explizite Owner-Allow-Maske mindestens `0x001601e7`. Dies berechnet
keine effektiven Gruppenrechte, verwendet kein `MAXIMUM_ALLOWED` und behauptet
keine isolierte Notwendigkeit einzelner Maskenbits. Owner, Group und SACL werden
nicht geschrieben.

Ein kompakter direkter Quellbeleg bindet die vollständige ungefilterte Menge roher
Kindnamen und Typen, einschließlich ausgeblendeter Kinder. Die normale Planung
verantwortet weiterhin Kinderinhalte und deren Hashes; es entsteht kein zusätzlicher
Vollbaumsnapshot. Vor CREATE und unmittelbar vor dem Rechte-SET müssen dieser
Beleg, Quellmanifest und vollständig beobachtet leere Ordnerstreams frisch passen.
Fehlende optionale Zusatzbeobachtungen lassen nur die Rechtewirkung aus. Bekannte
Namens-/Typdrift, Abbruch und Identitätswechsel bleiben Fehler.

Der normale Minimal-CREATE liefert das genuine Originalreceipt. Seine Beobachtung
`created(false)` wird vor der zusätzlichen Rechtearbeit dauerhaft an die Queue
übergeben. `Prepared` und `Applying` gehen dem einzigen DACL-SET voraus; `Applied`
folgt auf gehaltenen und frischen exakten Readback. Ein synchroner dauerhafter
Queuecheckpoint geht dem bestätigten Schließen aller Original-, Security- und
Parenthandles sowie dem Intentfinish voraus. Erst danach folgen Kinder. Es gibt
keine frühe mtime und keinen Eintrag in der V7-Menge früh abgeschlossener Ordner.

Nur der skalare exakte EffectKey bleibt für den normalen postorder-Abschluss.
Original-ID und bestätigte frühe DACL/SIDs werden vor der finalen Zeitmutation
frisch geprüft. Auch der tatsächliche Zeit-OPEN bindet dieselbe ID; danach folgen
frischer ID-/Zeit- und Rechte-/SID-Readback. Nach möglichem Zeit-SET erfolgt keine
Wiederholung. Eine konkrete OPEN-Absage vor SET darf ausschließlich im erlaubten
Best-Effort-Profil als typisierte Mtime-/PrivilegeFailure-Auslassung enden; die
frühe bestätigte DACL bleibt erhalten und gewöhnliche Metadaten bleiben als nicht
vollständig bestätigt markiert. Historische gleiche Pfade ersetzen keinen aktuellen
Originalbeleg. Der Marker des gewöhnlichen Metadatenabschlusses umfasst nicht den
separaten frühen DACL-Bericht und keine Zusage vollständiger Kinderinhalte.

V8 überträgt `file_id` in EffectKeys und Outcomes als positive kanonische
Dezimalzeichenfolge. Dadurch bleiben auch benachbarte u64-IDs oberhalb 2^53 in
JavaScript unterscheidbar. Numerische V8-Werte, führende Nullen und Überläufe werden
zurückgewiesen. Intern und an den Identitätsprüfungen bleibt die ID ein `u64`.
V7 behält in Persistenz und Digests seine bisherige numerische Wireform und deren
Feldreihenfolge. Die getrennte Tauri-Anzeigeprojektion überträgt seine Objekt-IDs
in Fortschritt, Queueantworten und Fehlerberichten ebenfalls als exakte
Dezimalzeichenfolgen. Sie erteilt keine neue Backendfreigabe und verändert keine
Journalbytes.

V8 nutzt eigene Versionen, Digestdomains, Intent-/Ledger-/Owner-/Closedpfade. Die
nicht serialisierte geschlossene Storeauswahl teilt nur die passenden bestehenden
Persistenz-, Lock-, Checkpoint- und Retirementalgorithmen. Die Objekt-Namespace-
Identität bleibt für V7 und V8 gemeinsam vergleichbar; offene Ansprüche desselben
Server-/Share-/Objekts dürfen sich nicht gegenseitig unsichtbar werden. Ohne
sichtbare V8-Beiträge bleiben insbesondere V7-Envelopebytes und Epoch erhalten.

J37 weist V8-Daten in älteren Journalen zurück: alle acht gespeicherten Planarten,
`PlanChanged.current_plan`, Anlage-/Rechteberichte, Progress und Recovery. Jede
V8-Planwirkung muss einzeln `ContentSync`/`CreateDir`/`NativeBestEffort`/`ContentHash`
erfüllen. Reine historische Anlagebeobachtungen bleiben am ContentSync erhalten,
auch wenn ein neu bestätigter Folgeplan keinen V8-CREATE mehr enthält. Rechtebelege
und offene Recovery behalten ihre ursprüngliche passende Absicht. Eine aus allen
validierten Storephasen abgeleitete reine RAM-Herkunft schützt dabei auch
`Prepared` und geschlossenes `Prepared`, ohne eine Rechtewirkung zu erfinden.

Echte Folgeprüfungen nutzen einen geeigneten geschützten expliziten Root mit
passendem tatsächlichem SID-Kontext, normale native Datei-Temps, beide exakten
Datei-DACLs, benannte Streams, verschachtelte Ordner und die originale finale
Parent-ID/-Zeit. Ein abweichender geerbter Probe-Groupkontext gegenüber späteren
Kindtemps bleibt eine konkrete Datei-DACL-Auslassung. Source-/Targetgruppen werden
nicht gleichgesetzt und Providerproben nicht still umgebunden. Fehlermatrizen
prüfen insbesondere spätes Namens-/Typdrift bei gleicher mtime, Cancel nach Applied,
SET-Antwortverlust, fehlgeschlagenen Queuecheckpoint, Kindhashdrift, Zeitabsage vor
SET und Fehler nach SET sowie Austausch direkt vor dem finalen Zeit-OPEN. Frühe
Rechte bleiben als bestätigt oder unklar erhalten; frischer Queueabschluss ist
rein lesend und erzeugt weder SET, Restore, Delete noch Kinderfortsetzung.
