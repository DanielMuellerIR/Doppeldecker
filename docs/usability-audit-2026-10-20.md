# Produktaudit: Alltagstauglichkeit und Sicherheitsgrenzen

Stand: 2026-10-20. Untersuchte Basis: `4797a04130a046e1d1195bd6e60716b22ad9c739` (`4797a04`).

## Gegenstand und Aussagekraft

Doppeldecker soll komfortable, schnelle und umfassende Dateiarbeit mit zwei Panes auf macOS und Linux ermöglichen. Dieser Audit untersucht, wo Sicherheitsprüfungen dieses Produktziel unterstützen und wo sie gewöhnliche Arbeit verhindern oder unverhältnismäßig verlangsamen.

Die Bestandsaufnahme beruht auf Quellcode, Projektverträgen und der Repositoryhistorie. Sie ist keine Laufzeitmessung und kein erneuter vollständiger Sicherheitsreview. Zeilenangaben beziehen sich auf die genannte Basis; Dateipfade sind relativ zum Repository. Aus dem Code abgeleitete Datenmengen sind als solche gekennzeichnet.

Die Befundabschnitte dokumentieren den damaligen Bestand. Die folgenden
Änderungen beschreiben den aktuellen Implementierungsstand; alte Zahlen und
Absagen in den Befundbelegen sind keine weiterhin geltenden Produktregeln.

## Umsetzungsstand

| Punkt | Änderung |
| --- | --- |
| 1 | Gewöhnliche SFTP-Schreibklasse für Upload, mkdir, Rename, Move, Ersetzen und Löschen. |
| 2 | Dauerhaftes Löschen vollständiger Auswahlen auch auf FTPS, WebDAV und SMB. |
| 3 | Standardsnapshot mit einem Remote-Inhaltsdurchlauf, Streaminghash und Attributcheck; vollständiger Zusatzreadback optional. |
| 4 | Keine festen Datei-, Auswahl-, Baumgesamtgrößen-, Tiefen- oder pauschalen Vorbereitungszeitgrenzen der alten Inhaltsklasse. |
| 5 | Lokale WebDAV-Quellen halten Root und nur das aktuelle Dateihandle statt aller ausgewählten Dateien. |
| 6 | Reguläres WebDAV-/SMB-Ersetzen als sichtbare Best-Effort-Wirkung ohne erfundene Quarantäne. |
| 7 | Hashbildung und frische Quellversionsprüfung im Transfer statt vollständigem Vorhashen. |
| 8 | Update ohne feste 100-Ersetzungs-Sperre; Umfang und Wiederherstellungsweg bleiben sichtbar. |
| 9 | Mirror warnt bei hohen Mengen/Anteilen und leerer Quelle; vollständige Scans und Quarantäne bleiben erforderlich. |
| 10 | Git-/HOME-Ziele mit konkreten Warnungen; HOME-Quarantäne im reservierten, auf beiden Seiten ausgeschlossenen Unterordner. `/` bleibt gesperrt. |
| 11 | Ein Konfliktdialog mit nachladenden Möglichkeiten und einer endgültigen Entscheidung. |
| 12 | Gewöhnliche FTPS-/SMB-Aktionen ohne künstliche Fähigkeitsproben; echte Temp-/Ordner-/Renamewirkung zuerst. |
| 13 | Normale Syncstarts, Queue-Rückfragen und kleine endgültige Löschungen mit Wirkungsbestätigung statt Routinephrasen oder mehrfacher Startauswahl. |
| 14 | Originalformat, passive Ansicht und explizites konfliktgeprüftes Zurückspeichern einer extern bearbeiteten Remote-Arbeitskopie. |

Ressourcen-, Parser- und Requestgrenzen bleiben erhalten. Sie werden als konkrete
Fehler gemeldet, nicht als erfundene Vollständigkeit. Endgültiges Löschen von mehr
als 1000 Einträgen oder 10 GiB und endgültiges Quarantäneleeren behalten eine
exakte Phrase. Strikte Remote-Sync- und Restorefähigkeiten werden durch normale
Dateiaktionen nicht automatisch freigegeben.

## Prüfstand der Umsetzung

Die abschließenden Prüfungen am 2026-10-20 bestehen auf macOS und Linux:

| Prüfung | macOS | Linux |
| --- | --- | --- |
| Core | 1181 erfolgreich | 1203 erfolgreich einschließlich des separat nativ ausgeführten Mounttests |
| CLI | 80 erfolgreich | 81 erfolgreich |
| App-Schicht | 229 erfolgreich | 220 erfolgreich |
| Format, Workspace-Clippy mit allen Targets, gesperrter Fuzz-Build | erfolgreich | erfolgreich |
| Nativer App-Build | erfolgreich | erfolgreich |

413 Browserfälle einschließlich CSP-Prüfung und sechs Pane-Tests sowie 149
Tooltests bestehen. Die Linux-Kopie wurde über SHA-256 gegen denselben Quellstand
abgeglichen. Ein vom Container-Mountlayout abhängiger Test lief mit genau einem
Filtertreffer außerhalb des Containers erfolgreich; er wurde nicht still ausgelassen.

Der zusätzliche echte Samba-Gegenlauf unter Linux belegt auch gewöhnliches
Ersetzen, rekursives Dauerlöschen, Rename, Kollisionen und begrenztes Temp-Cleanup.
Unter macOS wurden der einzelne nachladende Konfliktdialog und Remote-Bearbeitung
nativ geprüft: Öffnen in der externen Anwendung, Speichern, ausdrücklicher
Rückupload über die Fensterqueue und identischer Serverinhalt. Nachbardateien
blieben unverändert. Der notarisierte Auslieferungsbuild wurde anschließend in
derselben markierten Sandbox gestartet; seine kompakte Bearbeitungsübersicht ist
visuell geprüft.

Die Standardsuiten lassen 22 Core-Opt-ins je Plattform aus; der Samba-Fall wurde
unter Linux zusätzlich ausgeführt. Weitere Mount-, LAN- und externe Provider-Opt-ins
wurden für diesen Ausbau nicht erneut ausgeführt. Die beiden zusätzlichen
Linux-App-Anzeigefälle benötigen eine interaktive Displayabnahme; eine neue native
Linux-GUI-Abnahme wird hier nicht behauptet. Die übrigen ignorierten CLI-/App-Fälle
sind ausdrücklich separat gestartete Kindprozesshelfer. Diese Prüfungen belegen
keine atomare Remote-Pfadbindung, keinen Stromausfallschutz jedes Servers und
keine vollständige Komfortparität aller Provider.

Details:
[SFTP-Inhalte und Bearbeitung](sftp-contents.md), [SFTP-Schreibvertrag](sftp-writes.md),
[lokaler Sync](local-sync.md), [Providerübersicht](function-audit.md).

## Herkunft: Sync-Vorschau und gewöhnliche Dateiaktionen

Der Initialcommit `44d5ca6` vom 2026-07-09 beschreibt im damaligen `README.md` einen plattformübergreifenden Dual-Pane-Dateimanager nach dem Vorbild von ForkLift. Vorschau, Dry-Run und die Trennung von `plan()` und `execute()` werden ausdrücklich für Synchronisation beschrieben. Das damalige `AGENTS.md` begründet die Bestätigung löschender Läufe insbesondere mit Mirror.

`71b92d1` vom 2026-07-10 ergänzt grundlegende Dateioperationen; die README-Aussage über Vorschau bleibt auf Sync bezogen. `52c44a0` vom selben Tag dokumentiert einen fail-closed Realmodus mit einmaligen, plan- und rootgebundenen Freigaben. `caf3b79` vom 2026-07-29 führt anschließend die praktischen Produktprinzipien und abgestufte Ausführungsklassen ein.

Diese Quellen belegen keine ursprüngliche allgemeine Vorschaupflicht vor jeder Dateiaktion. Sie erlauben auch keine Aussage darüber, wer eine solche Verallgemeinerung erstmals formuliert hat. Im untersuchten Frontend sind konfliktfreie Best-Effort-Additionen ausdrücklich ohne modale Einzelfrage vorgesehen (`frontend/file-actions.js:350–360`). Backend-Vorbereitung und eine vom Nutzer zu bestätigende Vorschau sind dabei verschiedene Dinge.

## Vergleichsmaßstab

Die offizielle [ForkLift-Anleitung](https://binarynights.com/manual) beschreibt direkte Copy-/Move-Aktionen, Remote-Move per Drag-and-drop, Konfliktentscheidungen für Ersetzen und Beide behalten, dauerhaftes Remote-Löschen sowie Sync nach Größe und Änderungszeit. Die offizielle [FileZilla-Anleitung zum Remote-Löschen](https://filezillapro.com/docs/v3/getting-started/delete-files/) beschreibt das Löschen einer Auswahl per Tastatur oder Kontextmenü; die [FileZilla-CLI-Anleitung](https://filezillapro.com/docs/v3/filezilla-pro-cli/rename-remote-local-file-cli/) normales Remote-Rename/Move.

Diese Primärquellen belegen übliche Arbeitsabläufe. Sie belegen keine Gleichheit der internen Sicherheitsgarantien. Ziel ist ein vergleichbar brauchbarer Funktionsumfang mit eigenständiger Umsetzung und verständlich benannten Restrisiken.

## Priorisierung

Priorität 1 betrifft fehlende Grundfunktionen und besonders große Transferhindernisse. Priorität 2 betrifft häufige Arbeitsunterbrechungen, Skalierung und vermeidbare Vorbereitung. Priorität 3 betrifft zusätzliche Bedienung und spezialisierte Abläufe. Die Reihenfolge ist eine Produktbewertung, keine Einstufung von Sicherheitslücken. Änderungen sollten je Wirkung eingeführt und passend geprüft werden; eine gemeinsame Abschaltung sämtlicher Schutzregeln folgt daraus nicht.

### 1. SFTP-Schreibfunktionen fehlen weitgehend — Priorität 1

**Bestand und Beleg:** `core/src/remote_real_mode.rs:996–1014` beschränkt die interaktive SFTP-Quellklasse auf additive lokale Downloads und sperrt Remote-Ziel, Move und Ersetzen. `core/src/sftp.rs:675–709` und `docs/remote-providers-design.md:24–29` dokumentieren die darüber hinausgehende strikte Mutationssperre wegen fehlender atomarer Root-/Elternbindung.

**Nutzerwirkung:** SFTP dient weitgehend zum Listing und Download. Gewöhnliches Hochladen, Umorganisieren und Aktualisieren von Serverdateien benötigt einen anderen Client. Selbst ein Download darf bestehende lokale Ziele in dieser Klasse nicht ersetzen.

**Empfehlung:** Für gewöhnliche interaktive Aktionen einen praktikablen Best-Effort-Vertrag bereitstellen: eindeutige Auswahl, geprüfte Verbindung, frischer Bestandscheck, passende Tempdatei und Ergebnisprüfung. Der neue [Schreibvertrag](sftp-writes.md) setzt diese Empfehlung um; sein Prüfstand ist separat dokumentiert. Strikter Sync kann getrennte Anforderungen behalten.

**Restrisiko:** SFTP bindet einen Pfadbefehl nicht atomar an alle zuvor geprüften Eltern. Ein gleichzeitiger oder böswilliger Namespacewechsel kann eine andere Datei treffen; bei rekursiven Mutationen auch einen anderen Unterbaum. Dieses Risiko muss nach Wirkung beschrieben werden und darf nicht als vollständig beseitigt gelten.

### 2. Gewöhnliches Remote-Löschen ist gesperrt — Priorität 1

**Bestand und Beleg:** `core/src/remote_real_mode.rs:2310–2380` verweigert bei FTPS, SFTP und SMB ohne starken Restorevertrag Quarantäne und dauerhaftes Löschen. Der aktuelle providerübergreifende Stand ist in `docs/remote-providers-design.md:48–53` festgehalten; auch WebDAV besitzt dort keine allgemeine Freigabe. Einzelne Move-Quellentfernungen sind davon getrennte Verträge.

**Nutzerwirkung:** Überflüssige Serverdateien, fehlgeschlagene Uploadreste oder eigene leere Verzeichnisse lassen sich nicht wie in einem üblichen Dateimanager beseitigen.

**Empfehlung:** Explizit dauerhaftes Löschen anbieten, zuerst für einzelne Dateien und leere Ordner. Rekursive Löschungen benötigen eine verständliche Umfangsanzeige und Teilbilanz. Quarantäne bevorzugen, aber ihr Fehlen nicht zur allgemeinen Löschsperre machen.

**Restrisiko:** Inhalte können unwiederbringlich verloren gehen. Ein zwischen Check und Befehl ersetzter Name kann ein anderes Objekt bezeichnen; ein Elternwechsel kann den Wirkungsbereich verschieben. Unvollständige Rekursionsbelege und unklare Ausgänge verlangen weiterhin besondere Behandlung. Nach Antwortverlust darf keine blinde Wiederholung erfolgen.

### 3. SFTP-Öffnen und Dateivergleich lesen viermal remote — Priorität 1

**Bestand und Beleg:** `core/src/sftp_read.rs:472–512` liest am Ende eines Streams denselben Dateiinhalt nochmals vollständig. `core/src/remote_snapshot.rs:150–163` verlangt zusätzlich einen unabhängigen Remote-Hashdurchlauf. Beide Schichten zusammen ergeben vier vollständige Remote-Lesedurchläufe; `docs/sftp-contents.md:98–99` benennt dies ausdrücklich.

**Nutzerwirkung:** Eine 4-GiB-Datei verursacht daraus abgeleitet ungefähr 16 GiB Remote-Inhalt vor dem Öffnen, zusätzlich zu lokalen Prüfungen. Bei langsamen Leitungen ist dies ein erheblicher vermeidbarer Zeit- und Datenverbrauch. Es handelt sich um eine Codeableitung, nicht um einen gemessenen Benchmark.

**Empfehlung:** Normales Ansehen mit einem Remote-Durchlauf, Transferhash und abschließendem Größen-/Attributcheck ermöglichen. Zusätzliche Vollverifikation gezielt wählbar machen. Für eine anschließende Quellentfernung dürfen stärkere Anforderungen gelten.

**Restrisiko:** Gleichzeitig veränderte Inhalte können einen gemischten Stand ergeben. Gleich große Änderungen mit unverändert erscheinender Zeit werden durch einen einfachen Attributcheck nicht sicher erkannt. Auch vier Durchläufe erzeugen keinen atomaren Server-Snapshot.

### 4. Harte SFTP-Datei- und Baumgrenzen — Priorität 1

**Bestand und Beleg:** 8 GiB je Datei und 100 direkte Auswahleinträge stehen in `core/src/sftp_read.rs:19–22`, `:215–236` und `:335`. 1000 Baumeinträge, 16 Ebenen, 16 GiB Baumgesamtinhalt und 60 Sekunden Vorbereitung stehen in `core/src/sftp_tree.rs:8–11`, `:114–116`, `:172–173` und `:197`. Hinzu kommen Pfadgrenzen von 4096 Bytes und 64 Komponenten sowie begrenzte Listingantworten.

**Nutzerwirkung:** Große Videos oder Images scheitern grundsätzlich. Gewöhnliche Projekt- oder Fotoverzeichnisse können bereits die Eintragsgrenze überschreiten. Auch eine langsame, funktionierende Verbindung kann an der gesamten Vorbereitungsfrist scheitern. Konservative Behandlung potenziell lokal gleichwertiger Namen erschwert zusätzliche Fälle; Namen dürfen jedoch nicht still zusammenfallen.

**Empfehlung:** Produktgrößen durch begrenzte Ressourcen ersetzen: iterative Verarbeitung, begrenzte Puffer und Handles, Fortschritt und Abbruch. Request-Timeouts von einer Gesamtlaufzeit unterscheiden. Tatsächliche Grenzen des Ziel-Dateisystems und Namenskollisionen pfadgenau behandeln.

**Restrisiko:** Große Aufträge benötigen mehr Zeit und Platz und können größere Teilwirkungen hinterlassen. Streaming, Platzprüfungen und eine vollständige Abschlussbilanz begrenzen dies. Höhere Bytegrenzen verschärfen nicht automatisch das zugrunde liegende Pfadrennen; niedrigere Grenzen lösen es ebenfalls nicht.

### 5. WebDAV-Uploads besitzen dieselben Größenlimits und ein Handlelimit — Priorität 1

**Bestand und Beleg:** `core/src/local_read.rs:23–26`, `:68–85`, `:179–201` und `:381` begrenzt auf 1000 Baumeinträge, 16 Ebenen, 16 GiB Gesamtinhalt, 8 GiB je Datei, 60 Sekunden Vorbereitung und 128 gehaltene Handles. Der Auswahlvertrag steht in `docs/webdav-upload.md:28–33`.

**Nutzerwirkung:** Die Handlegrenze kann bereits vor der Eintragsgrenze greifen. Größere Sammlungen und Dateien müssen aufgeteilt werden oder sind überhaupt nicht übertragbar.

**Empfehlung:** Quellen beim jeweiligen Transfer binden und nur aktuell benötigte Datei-/Elternhandles halten, statt alle Quellen vorab dauerhaft offen zu halten. Harte Ressourcenbudgets behalten, aber nicht unmittelbar als Produktgrößenlimits abbilden.

**Restrisiko:** Eine später begonnene Datei kann sich seit der ersten Auswahl verändert haben. Der aktuelle Stand muss dann erkennbar neu gebunden oder die betroffene Wirkung erneut bestätigt werden. Bereits abgeschlossene Dateien benötigen eine ehrliche Teilbilanz.

### 6. WebDAV- und SMB-Ersetzen fehlen im gewöhnlichen Arbeitsweg — Priorität 1

**Bestand und Beleg:** `core/src/remote_real_mode.rs:1002–1009` lehnt gewöhnliches lokales WebDAV-Ersetzen bereits vor Providerzugriff ab. `core/src/webdav.rs:51–52` begründet dies mit fehlender gemeinsamer atomarer Bindung aller Ressourcen. SMB-Ersetzen hängt an einem starken Quarantäne-/Restorevertrag (`core/src/remote_real_mode.rs:2226–2280`).

**Nutzerwirkung:** Ein geändertes Dokument kann nicht einfach über seine bisherige Serverdatei hochgeladen werden. Der verbreitete Ablauf „lokal bearbeiten, wieder hochladen“ ist unterbrochen.

**Empfehlung:** Gewöhnliches Ersetzen als ausdrücklich beschriebene Best-Effort-Wirkung ermöglichen. Vorhandene Protokollbedingungen nutzen; Serverversionierung oder Sicherung bevorzugen. Der stärkere garantierte Wiederherstellungsvertrag bleibt eine getrennte Klasse.

**Restrisiko:** Die Altversion kann dauerhaft verloren gehen und eine späte Konkurrenzänderung überschrieben werden. WebDAV-Bedingungen binden nicht sämtliche Vorfahren gemeinsam. Die Oberfläche muss diese konkrete Schwäche zeigen und darf keine Quarantäne versprechen, die nicht existiert.

### 7. WebDAV verlangt vollständiges Vorhashen innerhalb von 60 Sekunden — Priorität 2

**Bestand und Beleg:** `core/src/local_read.rs:294–333` liest den vollständigen lokalen Inhalt vor dem Transfer. Die Laufzeitgrenze wird dabei zwischen I/O-Schritten geprüft. `:476` prüft den Transferinhalt; `core/src/webdav_upload.rs:706–740` enthält weitere Zielprüfung.

**Nutzerwirkung:** Der Upload startet erst nach vollständigem Lesen der ausgewählten Inhalte. Ein großes Dokument auf langsamem Datenträger kann bereits vor dem eigentlichen Upload an der Frist scheitern.

**Empfehlung:** Hashbildung und Quellversionsprüfung soweit möglich in den Transfer integrieren. Zusätzliche vollständige Zielverifikation nach gewähltem Prüfprofil oder konkreter Wirkung ausführen, statt sie unterschiedslos zu stapeln.

**Restrisiko:** Änderungen während des Lesens können einen gemischten Quellstand erzeugen. Gehaltene Datei und Versionschecks bleiben sinnvoll; ein vollständiger Vorhash verhindert für sich ebenfalls keinen atomaren Snapshotverlust.

### 8. Update-Sync erlaubt höchstens 100 Ersetzungen — Priorität 2

**Bestand und Beleg:** `core/src/real_mode.rs:23` und `:1551–1573`; die Geltung für den normalen lokalen Einstieg ist in `docs/local-sync.md:91` festgehalten.

**Nutzerwirkung:** 101 geänderte Dateien in einem gewöhnlichen Projektbaum können nicht in einem Lauf aktualisiert werden.

**Empfehlung:** Die feste Sperre durch eine skalierende Übersicht und eine passende Wirkungsbestätigung ersetzen. Platzbedarf, Sicherungsweg und Teilbilanz bleiben erforderlich.

**Restrisiko:** Eine falsche Richtungs- oder Rootwahl kann mehr Dateien betreffen. Quelle, Ziel, Zahl und Sicherungsbedarf müssen klar sichtbar sein. Der Wert 100 ist keine technische Grenze für korrekte Ersetzung.

### 9. Mirror scheitert an 50 Einträgen, 5 Prozent und leerer Quelle — Priorität 2

**Bestand und Beleg:** `core/src/real_mode.rs:24–25` und `:1585–1625` sperrt eine leere Quelle sowie mehr als 50 zu entfernende Einträge oder mehr als 5 Prozent des Zielbestands.

**Nutzerwirkung:** Schon eine Entfernung aus zehn Zieldateien überschreitet mit 10 Prozent die Grenze. Auch bewusstes Spiegeln einer leeren Quelle ist unmöglich.

**Empfehlung:** Schwellen als Warn- und Bestätigungsschwellen verwenden. Hohe Anteile und leere Quellen besonders sichtbar machen; Quarantäne und Platzbilanz beibehalten. Keine pauschale Freigabe unvollständig gescannter Quellen.

**Restrisiko:** Eine falsche oder unerwartet leere Quelle kann den gesamten Zielbestand in Quarantäne bewegen. Dies verursacht Unterbrechung und Platzbedarf, auch wenn Wiederherstellung möglich bleibt. Ein Scanfehler darf weiterhin niemals als leere Quelle gelten.

### 10. Git-Unterbäume und HOME sind in starken Sync-Klassen pauschal gesperrt — Priorität 2

**Bestand und Beleg:** `core/src/real_mode.rs:1510–1548` lehnt Dateisystemwurzel, Benutzerordner und Ziele innerhalb eines Git-Repositorys ab. `docs/local-sync.md:112–118` grenzt die heutige Geltung ab: Reine Additionen und ausdrücklich dauerhafte schwache Ersetzungen sind inzwischen ausgenommen.

**Nutzerwirkung:** Projektdateien lassen sich in der abgesicherten Ersetzungsklasse nicht regulär synchronisieren. Der Speicherort im Repository ist dabei entscheidend, nicht die konkret ausgewählte Wirkung.

**Empfehlung:** Git-/HOME-Pauschalsperren durch konkrete Bereichswarnungen, Ausschlussmöglichkeiten und sichtbare Richtung ersetzen. Ein Ganzbaumziel `/` verlangt eine gesonderte Bewertung und sollte nicht beiläufig freigeschaltet werden.

**Restrisiko:** Arbeitskopie, Konfiguration oder `.git` können überschrieben werden. Konkrete Ausschlüsse und klar begrenzte Ziele steuern dieses Risiko genauer als ein pauschales Verbot sämtlicher Repository-Unterordner.

### 11. Remote-Konflikte benötigen teilweise zwei Dialogrunden — Priorität 2

**Bestand und Beleg:** `frontend/file-actions.js:315–325` bietet zunächst „Ersetzen prüfen“ oder „Beide behalten prüfen“. Nach weiterer Backend-Vorbereitung folgt bei `:385–406` die konkrete Entscheidung.

**Nutzerwirkung:** Ein gewöhnlicher Namenskonflikt verlangt inhaltlich ähnliche Entscheidungen mehrfach; dazwischen kann erneut Provider-I/O stattfinden.

**Empfehlung:** Einen Konfliktdialog mit nachladenden Möglichkeiten verwenden. Notwendige Prüfungen dürfen Optionen vorübergehend deaktivieren; die endgültige Wirkung und ihre Folgen gehören in denselben verständlichen Ablauf.

**Restrisiko:** Die Zusammenlegung erzeugt keine zusätzliche Datenverlustgefahr, wenn die Backendentscheidung weiterhin an die angezeigte Wirkung gebunden bleibt und Konkurrenzänderungen frisch geprüft werden. Eine Frontendentscheidung allein ersetzt diesen Vertrag nicht.

### 12. Künstliche FTPS-/SMB-Proben verlängern gewöhnliche Arbeit — Priorität 2

**Bestand und Beleg:** `core/src/remote_real_mode.rs:2110–2120`, `:2136–2141` und `:2188–2217` führt vor Dateioperationen Capability-Proben aus. `docs/remote-providers-design.md:155–169` beschreibt separate FTPS-Verbindungen für Proben und spätere Mutation. Die Proben gelten dadurch nicht automatisch als Beweis für denselben Serverknoten.

**Nutzerwirkung:** Zusätzliche Dateien und Ordner werden erstellt, umbenannt oder gelöscht; Verbindungen und Roundtrips kommen vor dem eigentlichen Auftrag hinzu. Probevoraussetzungen können eine fachlich mögliche Operation blockieren. Abbrüche können eigene Reste hinterlassen.

**Empfehlung:** Wo möglich die echte Operation mit ihrer ohnehin nötigen Tempdatei als ersten Fähigkeitsnachweis verwenden. Normales mkdir direkt ausführen und prüfen. Proben nur behalten, wenn ihr Ergebnis eine konkrete zusätzliche Entscheidung rechtfertigt.

**Restrisiko:** Die erste echte Wirkung kann scheitern oder einen unklaren Ausgang haben. Tempintents, begrenztes Cleanup und Teilbilanz müssen dies beherrschen. Eine Probe auf einer anderen Verbindung beseitigt dieses Risiko ebenfalls nicht.

### 13. Laufphrasen und mehrfacher Sync-Start erschweren Routinejobs — Priorität 3

**Bestand und Beleg:** `frontend/real-mode-ui.js:249–267` trennt Direktstart und Queue; `:317–359` verlangt bei den betroffenen Läufen exakte Phrase und gegebenenfalls eine externe Backup-/Restore-Bestätigung. `frontend/bidirectional-ui.js:339–362` enthält die entsprechende Queue-/Phrasenbedienung. Normales lokales AddOnly/Update ist bereits ein vereinfachter separater Einstieg.

**Nutzerwirkung:** Routineübertragungen im strikten oder Remote-Weg besitzen mehrere Bestätigungsschritte. Einreihen kann eine spätere betreute erneute Bestätigung erfordern.

**Empfehlung:** Normale Additionen ohne Phrase und Routine-Update mit einer konkreten Wirkungsbestätigung anbieten. Phrasen auf außergewöhnlich große dauerhafte Löschwirkungen und endgültiges Quarantäneleeren beschränken. Backend-Permit und Driftprüfung können unabhängig davon bestehen bleiben.

**Restrisiko:** Weniger Reibung kann eine unaufmerksame Fehlbedienung erleichtern. Richtung, Ziel und Wirkung bleiben deshalb sichtbar. Das Abschreiben einer Phrase ist jedoch kein technischer Nachweis eines Backups oder eines korrekten Plans.

### 14. SFTP-Öffnen verändert Formatbehandlung und bietet keinen Rückschreibweg — Priorität 3

**Bestand und Beleg:** `docs/sftp-contents.md:87–96` beschreibt passive Dateiendungen für unter anderem HTML, SVG und XML sowie den fehlenden Rückschreibweg. `core/src/sftp_read.rs:353` setzt die Inhaltsklasse auf 0600 für Dateien und 0700 für Ordner; Ausführungsrechte werden nicht übernommen.

**Nutzerwirkung:** Webdateien öffnen als Text statt in ihrer normalen Anwendung. Remote-Bearbeiten mit anschließendem Zurückspeichern und das Herunterladen ausführbarer Programme bleiben eingeschränkt.

**Empfehlung:** „Sicher ansehen“ und bewusstes „Mit Anwendung öffnen/bearbeiten“ als unterschiedliche Wirkungen anbieten. Originalformat und darstellbare Rechte im passenden Arbeitsmodus erhalten. Einen Rückupload als eigene, konfliktgeprüfte Wirkung behandeln.

**Restrisiko:** Eine externe Anwendung kann aktive Inhalte mit ihren eigenen Rechten ausführen. Ein Rückupload kann zwischenzeitliche Remoteänderungen überschreiben. Dafür sind eine konkrete Format-/Ausführungsentscheidung und frische Konflikterkennung sinnvoller als eine universelle passive Behandlung.

## Sinnvolle verbleibende Regeln

Der Abbau unnötiger Bedienungs- und Funktionsgrenzen sollte folgende Regeln erhalten:

- Quelle, Ziel und angeforderte Wirkung müssen eindeutig sein. Lokale Symlinkwechsel dürfen keinen Zugriff außerhalb des gebundenen Bereichs erlauben.
- Secrets gehören weder in Kommandoargumente noch in URLs, persistierte Operationsdaten oder Audittexte.
- Vorhandene Ziele dürfen nur aufgrund einer verständlichen Überschreibentscheidung ersetzt werden.
- Eine fehlende Garantie muss konkret beschrieben werden; Erfolg, Vollständigkeit, Dauerhaftigkeit und Wiederherstellbarkeit dürfen nicht erfunden werden.
- Ein mutierender Befehl darf nach verloren gegangener Antwort nicht blind wiederholt werden. Unklare Ausgänge bleiben sichtbar.
- Rekursive Entfernung braucht einen passenden Bestandsbeleg. Fehlerhafte oder unvollständige Listings sind keine leeren Verzeichnisse.
- Abbruch und Teilwirkungen müssen ehrlich bilanziert werden. Eigene Tempdateien brauchen begrenzte Aufbewahrung und einen nachvollziehbaren Wiederherstellungs- oder Bereinigungsweg.
- Unerwartete Server-, Konto- oder Hostkeywechsel dürfen nicht still als dieselbe Verbindung fortgesetzt werden. Erwartbare Zertifikatserneuerungen benötigen einen eigenständigen verständlichen Aktualisierungsweg.

Diese Regeln sind mit einem schnellen normalen Dateimanagerweg vereinbar. Stärkere Vollverifikation und strengere Wiederherstellungsverträge können zusätzlich angeboten werden, ohne sämtliche gewöhnlichen Aktionen von ihnen abhängig zu machen.
