# Bidirektionaler Sync

Stand: 2026-10-18

Status: Verbindlicher Implementierungsvertrag. Der lokale Baseline-, Konflikt-,
Journal-, Resume-/Rollback- und Typwechselvertrag ist umgesetzt. Remote-Endpunkte
verwenden dieselbe Planung und leiten jede Zielwirkung aus den tatsächlich
vorhandenen Identitäts-, Quarantäne- und Dauerhaftigkeitsbelegen ab.

Aktuelle Grenze: SFTP v3 liefert keinen rootgebundenen Snapshot und bleibt schon
bei der Sicherheitsplanung gesperrt. FTPS, WebDAV und SMB dürfen als Quellen
bestätigte Wirkungen auf ein starkes lokales Ziel liefern. Wirkungen auf ein
Remote-Ziel bleiben im bidirektionalen Lauf ohne den vollständigen gebundenen
Restore-Proofvertrag gesperrt, auch wenn gewöhnliches AddOnly-Kopieren dorthin
möglich ist. Ein bloßer erfolgreicher Rename- oder Flush-Test genügt nicht.

Die früheren SFTP-Freigaben der Stufen 6b–6d und 6g–6h beschreiben historische
Implementierungsschritte; sie sind keine aktuelle Freigabe. Die heutige
[Fähigkeitsmatrix](function-audit.md) und `ProviderEndpoint` in
`core/src/remote_real_mode.rs` sind maßgeblich. Queue-Journal 21 enthält die
geheimnisfreien Endpunkt- und Profilbindungen; ältere Versionen werden nur nach
ihrem jeweiligen Migrationsvertrag gelesen. POSIX-Modi werden nur gebunden, wenn
beide Seiten sie lesen und schreiben können. Secretwerte, Sitzungen, Handles und
Permits werden nicht persistiert.

## Grundsatz und Sicherheitsinvarianten

Bidirektionaler Sync ist kein zweimal ausgeführter einseitiger Sync. Er benötigt eine
persistente, bestätigte Baseline, damit „auf einer Seite gelöscht“ von „auf der anderen
Seite neu“ unterschieden werden kann.

Die aktuelle Ausführung unterstützt ausschließlich Portable-Metadaten und liest
alle Dateiinhalte per BLAKE3. Einseitige AddOnly-/Update-/Mirror-Optionen, der schnelle
Größe-/Datum-Vergleich und native Profile gehören nicht zum bidirektionalen Request.
Die Oberfläche zeigt stattdessen den geltenden Vertrag; beim Zurückwechseln in eine
Richtung bleiben die vorherigen Einwegeinstellungen erhalten. Ersetzungen und
Entfernungen entstehen aus Baseline und ausdrücklich bestätigten Entscheidungen.

Unverhandelbar sind:

1. Ohne gültige Baseline gibt es keine automatische Gewinnerseite und keine
   Löschpropagierung.
2. Beide rekursiven Listings, Root-Identitäten und alle Dateiinhalte werden vor der
   Planung vollständig gebunden. Bidirektional verwendet immer `ContentHash`.
3. Kein Konflikt wird anhand von mtime, Providerreihenfolge oder „neuer gewinnt“
   automatisch entschieden.
4. Alle Kopien und Konfliktduplikate laufen vor Ersetzungen und Löschpropagierungen.
5. Ersetzungen und Entfernungen laufen auf jeder Zielseite ausschließlich über deren
   geprüfte Quarantäne. Copy-and-delete ist kein Fallback.
6. Die alte Baseline bleibt bis zum vollständigen Commit beider Seiten gültig.
7. Nach einem Teilcommit ist das Paar blockiert, bis derselbe Lauf sicher fortgesetzt
   oder rückwärts wiederhergestellt wurde. Ein neuer Normalplan darf ihn nicht
   überschreiben.
8. Secrets, Sitzungen, Handles und Permits werden weder in Baseline noch Journal oder
   Queue persistiert.

## Paar und dauerhaft gebundene Endpunkte

Ein Paar besitzt eine zufällige 128-Bit-`pair_id`, einen Anzeigenamen und genau zwei
Rollen `left` und `right`. Die Rollen bleiben stabil, auch wenn die UI die Panes
tauscht.

Die persistente `EndpointBinding` enthält nur öffentliche Identitätsdaten:

- lokal: kanonischer Root, Gerät/Inode, FSID, Dateisystemtyp und vorhandene
  Volume-ID; ändert sich ein Feld nach Neustart oder Remount, ist eine sichtbare
  Neuverknüpfung nötig,
- SFTP: Host, Port, Hostkey, Konto und kanonischer Server-Root,
- FTP/FTPS: Host, Port, Transportmodus, optionales Zertifikat, Konto, Root sowie
  FEAT-/Begrüßungsdigest,
- WebDAV: HTTPS-Origin, Zertifikat, Konto und Collection-Root,
- SMB: Host, Port, Server-GUID, Share, Konto, Root, Dialekt und
  Signing-/Encryption-Algorithmen.

Sitzungsgeneration, Tree-ID, Linux-Mount-ID und andere kurzlebige Werte gehören in den
jeweiligen Lauf-Proof, nicht in die dauerhafte Paaridentität. Jeder Lauf vergleicht
zuerst die dauerhafte Bindung und danach die kurzlebige `RootIdentity`. Eine Abweichung
ist kein normaler Konflikt, sondern ein hartes Gate ohne Mutation.

Baseline-Version 2 und Laufjournal-Version 8 verwenden für FTPS und WebDAV
Endpunktbindungsschema 2. Es bindet die längengekennzeichneten Rohbytes des
Willkommenstexts beziehungsweise des optionalen `Server`-Headers. Eine Bindung aus
Schema 1 wird ausschließlich nach einer frischen authentisierten Verbindung gegen
den gleichzeitig berechneten alten Digest geprüft. Passt sie, schreibt der bestätigte
Lauf die aktuelle Schema-2-Bindung auch bei einem No-op atomar in die Baseline; ohne
diesen Live-Beleg bleibt Resume, Rollback und Neuplanung fail-closed. Baseline v1,
Laufjournal v7 und schemafreie Registry-Schlüssel werden mit ihren damaligen
kanonischen DTOs geprüft. Die Registry schlüsselt einen so authentisierten Eintrag
atomar um und behält seine `pair_id`, damit Baseline- und Recoverypfade stabil bleiben.

## Baseline-Format

Die Baseline ist ein versionsgebundenes, kanonisch serialisiertes Dokument:

```text
BidirectionalBaseline {
  version,
  pair_id,
  generation,
  left_binding,
  right_binding,
  metadata_profile,
  entries: map<BinaryRelativePath, BaselineEntry>,
  tombstones: map<BinaryRelativePath, Tombstone>,
  body_digest,
}
```

`BinaryRelativePath` ist eine Liste roher Namenskomponenten. Absolute Pfade,
Traversal, leere Komponenten und verlustbehaftet dekodierte Namen sind unzulässig.
Damit können zwei unterschiedliche lokale Byte-Namen nie in denselben Baseline-Key
fallen. Remote-Provider liefern weiterhin verlustfreies UTF-8.

`BaselineEntry` bindet:

- Typ (`file` oder `directory`; Symlinks und Spezialdateien sind nicht baselinefähig),
- logische Größe und mtime als Diagnosewerte,
- BLAKE3-Inhaltsdigest jeder Datei,
- kanonisches Metadatenmanifest entsprechend dem gewählten Profil,
- vorhandene stabile Provider-ID nur als zusätzliche Drift-Schranke.

Die Baseline wird unter einer app-ordnerweiten Prozesssperre mit `0600`,
Write→`fsync`→atomarem Rename→Eltern-`fsync` ersetzt. Ein falscher Versionswert,
Digest, doppelter Pfad oder eine Größenüberschreitung stoppt fail-closed. Migrationen
erzeugen zuerst ein neues Dokument; sie überschreiben nie eine unbekannte Version.

Der lesende Snapshot bindet nicht nur die gemeldete Dateigröße: Jeder Hashlauf liest
höchstens diese Größe plus ein Prüfbyte und lehnt kürzere oder längere Streams ab.
Baseline und Laufjournal werden descriptor-relativ ohne Symlinkfolgen geöffnet;
Dateityp, Identität und Größenlimit gelten auch für das tatsächlich geöffnete Objekt.

`NativeBestEffort` ist für bidirektionale Paare gesperrt: eine absichtliche Auslassung
würde sonst bei jedem Lauf als Gegenänderung pendeln. `Portable` ist Standard.
`NativeStrict` bleibt derzeit ebenfalls gesperrt. Seine spätere Freigabe verlangt,
dass beide Endpunkte für jede vorkommende Klasse exakt dieselbe les- und schreibbare
Semantik melden; dieser symmetrische Fähigkeitsvertrag ist noch nicht implementiert.

## Erster Lauf

Ohne Baseline gibt es zwei sichere Wege:

1. Sind beide vollständig gescannten Bäume einschließlich des gewählten
   Metadatenprofils identisch, wird nach ausdrücklicher Bestätigung nur Generation 1
   geschrieben.
2. Jede Abweichung wird als Initialkonflikt angezeigt. Erst vollständige
   pfadbezogene Entscheidungen erzeugen einen ausführbaren Initialplan.

Eine leere Seite bedeutet ohne Baseline niemals automatisch „die andere Seite hierher
spiegeln“. Dafür muss der Nutzer jeden betroffenen Initialkonflikt oder einen sichtbar
gesonderten, wirkungsbilanzierten Initialimport bestätigen.

Einseitige Erstkonflikte bieten Skip mit konkretem Grund nur deaktiviert an.
Beide behalten setzt zwei vorhandene Versionen voraus. Eine fehlende Gewinnerseite
benennt die Entfernung der Gegenseite und bleibt bei verletzten Löschgrenzen
deaktiviert. Die erneute Vorbereitung muss zum ursprünglichen ungeklärten
Plan-Digest passen; Drift verlangt einen neuen Dialog. Bestehende Baselines und
zulässiges Skip bleiben einschließlich Neustart erhalten.

## Vollständiger Snapshot

Vor jeder Planung entsteht pro Seite ein `TreeSnapshot` aus demselben gehaltenen VFS,
das später ausgeführt wird:

- Root-Identität vor und nach dem Scan identisch,
- jedes Verzeichnis bis zum providerseitig eindeutigen Ende aufgelistet,
- eindeutige rohe Namen,
- kein Symlink, Reparse-Point, Spezialtyp oder Typfehler,
- BLAKE3 jeder Datei und passendes Metadatenmanifest,
- Grenzwerte für Pfadzahl, Pfadtiefe, Einzeldatei- und Gesamtscanbytes,
- kanonischer Snapshot-Digest unabhängig von Listingreihenfolge.

Ein einziger ausgelassener oder nicht sicher lesbarer Eintrag sperrt den gesamten
bidirektionalen Plan. Anders als ein einseitiges AddOnly darf dieser Modus keine
partielle Sicht als Erfolg behandeln.

## Drei-Wege-Entscheidung

Für jeden Pfad werden links und rechts mit der letzten Baseline verglichen. „Geändert“
meint Inhalt, Typ oder die im Profil gebundenen Metadaten:

| Links | Rechts | Ergebnis |
|---|---|---|
| unverändert | unverändert | keine Aktion |
| geändert | unverändert | links → rechts |
| unverändert | geändert | rechts → links |
| identisch geändert | identisch geändert | neue gemeinsame Baseline |
| unterschiedlich geändert | unterschiedlich geändert | Konflikt |
| gelöscht | unverändert | bestätigbare Löschung rechts |
| unverändert | gelöscht | bestätigbare Löschung links |
| gelöscht | geändert | Konflikt; nie automatisch löschen |
| geändert | gelöscht | Konflikt; nie automatisch löschen |
| gelöscht | gelöscht | gemeinsamer Tombstone |

Ein nach der Baseline nur auf einer Seite neu entstandener Pfad wird zur anderen Seite
kopiert. Sind auf beiden Seiten verschiedene neue Einträge unter demselben Namen
entstanden, ist das ein Konflikt. Ein Typwechsel ist immer ein Konflikt, auch wenn die
andere Seite unverändert scheint; die Entscheidung muss die Wirkung auf den ganzen
Unterbaum sichtbar machen.

Umbenennungen werden in Version 1 bewusst als Löschung plus neuer Pfad behandelt.
Inhaltsbasierte Rename-Erkennung darf später nur eine Anzeigehilfe sein und niemals
ohne Bestätigung eine Löschung freischalten.

## Tombstones

Nach einer erfolgreich auf beide Seiten propagierten oder beidseitig beobachteten
Löschung wird der alte Eintrag durch einen `Tombstone` ersetzt. Er enthält Pfad,
letzten Entry-Digest, Baseline-Generation, Löschrichtung und Commitzeit.

Taucht ein Tombstone-Pfad erneut auf einer oder beiden Seiten auf, gilt er als
`RecreatedAfterDeletion`-Konflikt. Er wird nicht als normaler neuer Pfad kopiert; so
kann eine lange offline gewesene Seite gelöschte Daten nicht unbemerkt wiederbeleben.

Aufbewahrungsziel sind 90 Tage, aber Zeit allein löscht weder Tombstone noch Daten.
Bereinigung ist eine separate bestätigte Wartungsaktion und nur erlaubt, wenn beide
Seiten den Pfad weiterhin vollständig als abwesend beweisen, kein Recovery-Journal
offen ist und mindestens eine neuere erfolgreiche Baseline existiert.

## Konflikte und Entscheidungen

Jeder Konflikt trägt beide Entry-Digests und den Baseline-Digest. Seine Anzeige
verwendet beide gebundenen Endpunkte, Vorhandensein, Typ, bekannte Dateigröße und
optionale Änderungszeit aus denselben Snapshots. Der Core liefert die zulässigen
Entscheidungen und aktualisiert sie nach jeder Wahl einschließlich kumulierter
Mengengrenzen. [Bedienvertrag](conflict-decisions.md):

- `KeepLeft`: linke Version am Originalpfad auf beiden Seiten,
- `KeepRight`: rechte Version am Originalpfad auf beiden Seiten,
- `KeepBoth`: beide Versionen auf beiden Seiten; der Originalpfad behält die explizit
  gewählte Hauptseite, die andere erhält einen vorab kollisionsgeprüften Namen,
- `Skip`: keine Mutation und keine Baseline-Änderung für diesen Pfad.

Die lokale Grenze von 100 Typwechseln zählt frühere Entscheidungen mit. Eine
weitere solche Wahl erscheint gesperrt, auch bei `KeepBoth`; zulässiges
Überspringen bleibt erreichbar. Anzeige und spätere Vorbereitung verwenden
dieselbe Grenze.

`KeepBoth` erzeugt den Konfliktnamen bereits bei der Vorschau aus Basename,
Seitenrolle, UTC-Zeit und Zufallssuffix. Der exakte Name und seine Abwesenheit auf
beiden Seiten gehören zur Planbindung. Bei Verzeichnis-/Typkonflikten wird zuerst der
vollständige Verlierer-Unterbaum unter dem Konfliktnamen materialisiert; erst danach
darf der Originalpfad ersetzt werden. Lange Namen werden an UTF-8-Grenzen gekürzt;
Erweiterungen bleiben im verfügbaren Namensbudget erhalten. Case-/Unicode-äquivalente
Namen gelten bereits bei der Vorschau als Kollisionen.
Das gilt auch zwischen mehreren neu gewählten Konfliktnamen, die noch in keinem
Snapshot stehen. Zwei äquivalente neue Geschwisternamen werden vor Journalanlage
und Projektionsvorbereitung abgelehnt. Bereits gewählte Namen bleiben auch bei
schrittweisen Entscheidungen reserviert und werden bei weiteren Vorschlägen
ausgespart.

Für einen lokalen Datei↔Verzeichnis-Wechsel stellt der VFS-Kern dafür einen
descriptor-relativen atomaren Namensaustausch bereit. Linux verwendet
`renameat2(RENAME_EXCHANGE)`, macOS `renameatx_np(RENAME_SWAP)`. Die lokale Ausführung
materialisiert den Gewinner zuerst unter einem planmäßig als fehlend gebundenen Namen,
tauscht diesen Namen atomar mit dem Original und bewegt den nun am Staging-Namen
liegenden Verlierer journalisiert in die Quarantäne. Der dauerhafte Zweiphasenschritt
erkennt Vor-, Zwischen- und Nachzustand; Resume tauscht einen reinen Zwischenzustand
zuerst atomar zurück und fordert danach ein frisches Permit. Ein Fallback mit zeitweise
fehlendem Originalpfad existiert nicht.

Entscheidungen sind pfadbezogen. Eltern- und Kindentscheidung dürfen sich nicht
überlappen; eine Elternentscheidung bilanziert den ganzen Unterbaum. Text-Merge,
globale automatische Gewinnerregeln und stilles „neuer gewinnt“ gehören nicht zu
Version 1.

## Plan und Digest

`BidirectionalPlan` enthält:

- Pair-ID, Baseline-Generation und Baseline-Digest,
- beide dauerhaften und kurzlebigen Root-Identitäten,
- beide vollständigen Snapshot-Digests,
- Optionen und Metadatenprofil,
- sortierte Aktionen samt Richtung, erwarteten Quell-/Zielzuständen und
  Konfliktentscheidung,
- getrennte Byte-, Ersetzungs-, Lösch- und Quarantänebilanz pro Seite,
- die dynamisch geprüften Providerfähigkeiten,
- einen kanonischen BLAKE3-Plan-Digest.

Offene Konflikte machen den Plan nicht ausführbar. Jede nachträgliche Entscheidung
erzeugt einen neuen Digest und eine neue Zufallsphrase.

## Provider- und Richtungsfähigkeiten

Jede konkrete Aktion prüft die Zielseite; die bloße Providerart schaltet nichts frei:

- neue Datei/Ordner: `exclusive_create` und Datei-Dauerhaftigkeit,
- Ersetzung oder `KeepLeft`/`KeepRight`: atomarer Replace-Rename, Quarantäne,
  Restore-Probe, Platz und `ContentHash`,
- Löschpropagierung: zusätzlich vollständiger Zweitscan und starke
  Verzeichnis-Dauerhaftigkeit,
- `KeepBoth`: auf beiden Seiten mindestens sichere AddOnly-Fähigkeit; die spätere
  Hauptkopie benötigt gegebenenfalls Update.

Auch die Ermittlung dieser Schreibfähigkeiten folgt den tatsächlichen Zielseiten.
Eine leere Erstbaseline und eine reine Quelle starten keine mutierenden
Capability-Proben. Das gilt ebenso für Resume und Rollback: Deren Wirkungen liegen
auf den ursprünglichen Zielseiten. Eine unveränderte Seite erhält keine erfundene
positive Schreibfähigkeit; die vorhandenen Zielgates bleiben vollständig wirksam.
Eigene Probenreste werden niemals durch einen neuen Snapshot als bestätigter
Bestand übernommen. Ein künstlicher FTPS-Gegenlauf
prüft diese Grenze mit einer verlorenen MKD-Antwort und einem echten Quelldownload.

Damit kann ein Paar asymmetrisch sein. FTPS, WebDAV oder SMB dürfen beispielsweise
eine bestätigte Löschung nach lokal propagieren; die umgekehrte Remote-Zielwirkung
bleibt ohne gebundenen Quarantäne-/Restore-Vertrag gesperrt. Das betrifft auch
additive Projektionen wegen ihres möglichen Rollbacks. SFTP bleibt bereits beim
Quellsnapshot gesperrt. Die UI zeigt nicht ausführbare Wirkungen als
Capability-Konflikte statt einen schwächeren Fallback zu verwenden.

Additionen in einem bidirektionalen Lauf benötigen wegen eines möglichen späteren
Rollback bereits Copy-Commit plus Quarantäne; bloßes AddOnly reicht dafür nicht.
Reguläre Dateiersetzungen benötigen den Update-/Restore-Vertrag.
Verzeichnisersetzungen, Typwechsel und Löschpropagierung benötigen den stärkeren
Mirror-/Namespace-Durability-Vertrag. Die Bestätigung zeigt diese konkrete Matrix für
beide Seiten an.

Provider↔Provider-Transfers verwenden weiterhin ausschließlich generisches
`open_read`/`open_write_new`; kein Protokoll erhält einen pfadbasierten Sonderweg.

## Wiederverwendung der Einweg-Engine

Die bidirektionale Engine implementiert keinen zweiten atomaren Kopierkern. Sie bildet
jede Richtung als `ProjectedVfs` ab:

- ausgewählte Quellpfade stammen aus der Gewinnerseite,
- nicht ausgewählte Pfade spiegeln die jeweilige Zielseite und erscheinen deshalb
  unverändert,
- ausgewählte Löschungen fehlen nur in der Projektion,
- gemischte Elternverzeichnisse werden aus beiden vollständigen Snapshots kanonisch
  zusammengesetzt,
- die View-Identität bindet beide echten Root-Identitäten und den Selektionsdigest.

Darauf erzeugt die bestehende Engine einen normalen AddOnly-, Update- oder
Mirror-`SyncPlan` und führt ihn mit denselben Temp-, Hash-, Metadaten-, Quarantäne- und
Driftprüfungen aus. Eine Projektion darf keine Schreibmethode implementieren. Ihr
Listing ist nur vollständig, wenn alle dafür benötigten echten Listings vollständig
waren.

Ausführungsreihenfolge:

1. additive Konfliktkopien beider Richtungen,
2. normale neue Kopien beider Richtungen,
3. Ersetzungen beider Richtungen,
4. Metadatenphasen,
5. nach erneutem beidseitigem Vollscan bestätigte Löschpropagierungen,
6. finaler Vollscan beider Seiten,
7. atomarer Baseline-Commit.

Aktionen derselben Originalpfade dürfen nie in beiden Richtungsplänen vorkommen.

## Laufjournal und Recovery

Vor der ersten Probe entsteht ein versionsgebundenes `BidirectionalRunJournal` im
App-Datenordner. Es bindet Pair-ID, Plan-/Baseline-Digest, beide Root-Bindungen,
Quarantäneläufe und pro Projektion `Prepared`, `Running` oder `Committed`.

FTPS prüft die fehlende Quarantänegarantie passiv vor jeder Dateiprobe. Eine
dadurch gesperrte bidirektionale Zielwirkung verändert keinen entfernten Bestand.
Leere Baselines und reine Quellen bleiben ebenfalls ohne mutierende Proben.
Tatsächlich benötigte gewöhnliche FTPS-Vorbereitungen besitzen ihren eigenen
dauerhaften fenstergebundenen Queueintent vor der ersten Wirkung; ein späteres
Laufjournal ersetzt diesen Vorabvertrag nicht.
[STOU-/Intentvertrag](ftps-stou-transport.md#wirkungsbezogene-vorbereitung-und-dauerhafte-intents).

Jeder Intent wird vor der zugehörigen Mutation atomar und dauerhaft geschrieben.
Meldet der Journalwriter nach dem Rename einen Fehler, genügt ein sichtbarer neuer
Readback nicht: Die gebundene Datei und ihr Elternordner werden erneut synchronisiert
und der Stand nochmals geprüft. Ein weiterer Fehler liefert `outcome_unclear` und
beendet die Ausführung vor der nächsten Projektion; das Journal bleibt für Recovery.
Neue, in einem Teillauf bereits committed Einträge werden beim Rollback ebenfalls in
eine laufbezogene Quarantäne verschoben, nicht blind gelöscht. Ersetzte oder entfernte
Originale bleiben in der bestehenden Quarantäne.

Nach Crash oder Fehler gibt es nur:

- `Resume`: ausschließlich wenn frische Root-Identitäten und Snapshots exakt zu allen
  committed und noch offenen Journalzuständen passen,
- `Rollback`: in umgekehrter Commitreihenfolge, ohne irgendeinen vorhandenen Namen zu
  ersetzen; eine Kollision stoppt sichtbar und lässt das Journal offen.

Bis Resume oder Rollback vollständig abgeschlossen ist, bleibt das Paar gesperrt. Die
alte Baseline wird weder fortgeschrieben noch aus dem beobachteten Teilzustand
„repariert“. Ein Journal wird erst nach finalem Baseline-`fsync` als `Completed`
markiert und danach separat entfernt.

Normale Mehrpfad-Projektionen laufen beim Rollback nach Pfadtiefe rückwärts. Ein
additiver Ordner ist eine atomare Unterbaumeinheit; ein gelöschter Ordner wird als
ganzer manifestierter Quarantäneeintrag restauriert. Bei zusammengeführten Ordnern
werden Kinddateien vor dem Elternmodus zurückgesetzt. Journal v7 bindet Projektion,
Einheitenindex, Pfad und Phase vor jedem Rename. Die Recovery prüft auch den
vollständigen Inhalt eines bereits verschobenen Recovery-Quarantäne-Unterbaums.

Beim Typwechsel-Rollback darf die manifestierte Altversion den besetzten Originalpfad
nicht direkt ersetzen. Sie wird per No-Replace unter exakt dem gebundenen Stagingnamen
wiederhergestellt und vollständig nachgelesen. Erst danach tauscht derselbe lokale
Kernelvertrag beide Namen atomar zurück. Damit entspricht der Baum wieder dem Präfix
vor der Typwechselprojektion; die davor liegende additive Stagingprojektion übernimmt
anschließend die normale Quarantänisierung der Neuversion. Ein Crash nach Staging-
Restore oder Namenstausch wird aus Original- und Staging-Unterbaumform erkannt.

### Drei Fallen beim Laden und Binden (belegt 2026-08-25)

**`Path::exists()` entscheidet nicht über „vorhanden".** In Journal- und
Baseline-Ladern liefert ein selbstreferenzieller Symlink `false`. Ein vorhandenes,
aber unlesbares Recoveryjournal gilt damit als fehlend — und ein neuer Lauf wird
zugelassen, obwohl die Sperre greifen müsste. Existenz deshalb über das
tatsächliche Öffnen beziehungsweise `symlink_metadata` entscheiden, nie über
`exists()`.

**Die Größenbegrenzung muss auch während des Lesens gelten.** Baselines sind auf
64 MiB, Laufjournale auf 256 MiB begrenzt. Eine allein vor dem Öffnen anhand der
Metadaten geprüfte Grenze genügt nicht, wenn die Datei während des Lesens wächst.
Die Leseoperation selbst muss deshalb begrenzt sein.

**POSIX kennt kein bedingtes Rename.** Eine Primitive „Rename nur, wenn an diesem
Namen noch genau dieser Inode liegt" gibt es für lokale Ziele nicht. Deshalb muss
nach dem exklusiven Claim am Quarantänenamen Identität und Inhalts- beziehungsweise
Baumhash geprüft werden; eine Abweichung bleibt als `OutcomeUnclear` für die
Recovery sichtbar. Nur Provider mit einer echten bedingten Rename-Primitive dürfen
vorher mutationsfrei mit `PlanDrift` abbrechen.

## Permit, Bestätigung und Queue

Eine Vorbereitung bleibt im Backend-RAM, gilt einmalig und höchstens zehn Minuten.
Sie bindet Baseline, beide Snapshots, alle Konfliktentscheidungen, Providerfähigkeiten,
Quarantäneproben, Platzbilanz und Plan-Digest.

Jeder Lauf verlangt eine exakte Zufallsphrase `BEIDE <Paarname> <Nonce>`.
Sobald Ersetzungen, Löschungen oder Konfliktauflösungen vorkommen, ist außerdem die
unabhängige Backup-Bestätigung Pflicht. Die bekannten 100-Ersetzungs- sowie
50-Einträge-/5-%-Löschgrenzen gelten pro Zielseite und können nicht per Frontend-Bool
abgeschaltet werden.

Die persistente Queue speichert nur Pair-ID, erwartete Baseline-/Plan-Digests und die
geheimnisfreien Konfliktentscheidungen. Vor Ausführung verbindet sie beide Provider
frisch, wiederholt alle Proofs und fordert eine neue Laufphrase. Nach App-Neustart ist
jedes Permit verloren; ein bereits mutierendes Journal wird als Recovery-Fall statt
als normaler Queue-Eintrag geladen.

## UI

Die Toolbar erhält einen klar getrennten Modus `Bidirektional`; die bestehenden
Einwegmodi bleiben unverändert. Der Assistent zeigt:

- Paarname, linke/rechte dauerhafte Identität und Baseline-Generation,
- Scanstatus und Snapshot-Digests,
- Aktionen und Bytes getrennt nach Richtung,
- Ersetzungen, Löschungen und Quarantäne getrennt pro Seite,
- jeden Konflikt mit erlaubten Entscheidungen und Capability-Begründung,
- offene Recovery-Journale vor jeder neuen Vorschau.

Fokus, Tastaturbedienung und Screenreader-Rollen gehören zur Abnahme. Ein
unaufgelöster Konflikt, fehlendes Backup-Häkchen oder falsche Phrase deaktiviert den
Start technisch im Backend und nicht nur optisch.

## Abnahmekriterien

Pflichtfälle auf macOS und Linux:

1. identischer Initiallauf schreibt nur die Baseline,
2. abweichender Initiallauf mutiert ohne vollständige Entscheidungen nichts,
3. nur links/rechts geänderte und identisch beidseitig geänderte Dateien,
4. parallele unterschiedliche Änderungen mit allen vier Konfliktentscheidungen,
5. Löschung gegen unverändert, Löschung gegen Änderung und beidseitige Löschung,
6. Tombstone-Wiederbelebung nach langer Offline-Phase,
7. Typwechsel und Verzeichnis-Unterbaumkonflikt,
8. Umbenennung als Löschen+Neu,
9. nicht-UTF-8, Symlink, Spezialdatei und unvollständiges Providerlisting,
10. Provideridentitäts- und Reconnect-Wechsel,
11. späte Kollision, Quota-Fehler und Abbruch vor/nach jedem Teilcommit,
12. Resume und kollisionsfreier Rollback nach Prozessabbruch,
13. Baseline-/Journal-Korruption, unbekannte Version und Prozesssperre,
14. Queue-Neustart ohne Secret-/Permitpersistenz,
15. lokale, Lokal↔Remote- und erlaubte Remote↔Remote-Capability-Matrix,
16. Mock-UI und echte Sandbox-GUI ohne Konsolen-, Fokus- oder CSP-Fehler.

Property-Tests prüfen zusätzlich: erfolgreicher Lauf ergibt zwei gleiche Snapshots und
eine passende Baseline; erneute Planung ist idempotent; ein Fehler vor dem
Baseline-Commit kann die alte Baseline nie verändern. Journal- und Baseline-Parser
erhalten eigene Fuzz-Targets.

## Umsetzungsstufen

1. persistentes Baseline-/Snapshot-/Drei-Wege-Modul samt Korruptions- und
   Property-Tests, noch ohne Mutation,
2. generische Provider↔Provider-Einwegvorbereitung und dynamische Capability-Matrix
   (abgeschlossen 2026-07-13),
3. lokale additive/Update-Projektionen mit atomarem Baseline-Commit
   (abgeschlossen 2026-07-13),
4. Konfliktentscheidungen, Tombstones und beidseitige Quarantäne
   (lokal abgeschlossen 2026-07-13),
5. Journal, Resume/Rollback, Queue und Crash-Injektion
   (lokaler Lauf einschließlich Queue/CLI/Tauri abgeschlossen 2026-07-13),
6. Remote-Paare providerweise freischalten (6a: dauerhafte Bindung und reine
   Providerplanung; 6b: additive Local↔SFTP-Ausführung; 6c: reguläre
   Dateiersetzungen samt Resume/Rollback; 6d: SFTP→lokal-Löschpropagierung; 6e:
   FTPS/WebDAV→lokal sowie dynamische SMB-Matrix und echter SMB-Wiederholungslauf
   abgeschlossen 2026-07-13; 6f: Remote-Queue/Tauri/UI
   abgeschlossen 2026-07-13; 6g: Remote-Quelltypwechsel in lokale Ziele abgeschlossen
   2026-07-13; 6h: Remote-Recovery und SFTP-Verzeichniswirkung nach lokal abgeschlossen
   2026-07-13),
7. UI, Mock-Suite und echter Sandbox-Abschluss (lokale und Remote-Queue-UI samt
   Mock-Recovery abgeschlossen; macOS-Sandbox-Abschluss nach ES-Modul-, A11y- und
   Virtualisierungsumbau am 2026-07-13 erneut bestanden; Linux separat offen).

Keine Stufe wird durch eine Umgebungsvariable oder ein Frontendfeld vorzeitig
freigeschaltet.


## Reviewnachtrag 2026-10-07: Rollback und Dauerhaftigkeit

Bei Paaren ohne POSIX-Modusvertrag bleibt der explizite Auslassungsdigest auch
beim lokalen Rollback-Readback maßgeblich. Inhalt, Typ, Größe und ein tatsächlich
gebundener Modus werden weiterhin exakt geprüft; die Ausnahme gilt ausschließlich
für den gespeicherten Auslassungsdigest und verändert die Planung nicht.

Ein bereits erfolgter Rename oder Moduswechsel mit anschließendem fsync- oder
Readbackfehler meldet `outcome_unclear` mit strukturierten Recoverypfaden.
Recovery darf einen sichtbaren Zwischen- oder Nachzustand erst nach erneuten
Dauerhaftigkeitsbarrieren als abgeschlossene Journalphase übernehmen. Ein weiterer
fsync-Fehler lässt den offenen Schritt bestehen. Das gilt auch für atomare Typwechsel.

Auch ein nach Fehler sichtbarer Baseline-Rename wird erst nach erneutem Datei- und
Eltern-fsync übernommen. Der Abschluss-Cleanup synchronisiert die exakt gebundene
Baseline nochmals, bevor das Laufjournal entfernt werden darf. Scheitert diese
Barriere, bleiben Journal und Recoverypfade erhalten.
