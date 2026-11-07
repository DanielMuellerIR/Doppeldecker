# Code-Review und Alltagstauglichkeit 2026-10-16

Stand: 2026-10-16. Ausgangspunkt `74d7c04`; Produktversion unverändert `0.1.123`.

## Ergebnis

Die Architektur bleibt passend: Datei- und Providerwirkungen im GUI-freien Core,
eine dünne Desktop-Anbindung und ein modularer, ereignisgesteuerter Frontendzustand.
Ein Frameworkwechsel würde die heutigen Probleme nicht lösen. Der größte
Produktengpass ist die Verbindung aus unnötig blockierter Bedienung, fehlenden
alltäglichen Dateiaktionen und teilweise zu weit gekoppelten Providerfähigkeiten.

Die Korrekturen dieses Reviews beseitigen konkrete Fehlabläufe, Panics,
Interoperabilitätsfehler und GUI-Blockaden. Sie schalten keine bislang gesperrte
Providerwirkung pauschal frei. Die nachfolgend beschriebenen größeren
Produktänderungen waren zum Reviewabschluss offen. Der anschließende Ausbau wird
in der [Roadmap](roadmap.md#beauftragter-ausbau-der-alltagstauglichkeit) fortgeschrieben.

## Behobene Fehler und Gegenbelege

| Bereich | Fehler und korrigiertes Verhalten | Beleg |
| --- | --- | --- |
| Kontextmenü | Ein Rechtsklick auf Leerfläche verwendete die unmarkierte Cursordatei. Dateiaktionen erscheinen dort jetzt nur für eine tatsächliche Auswahl; der Tastaturweg behält seinen Cursorbezug. | Browser: Leerfläche, Datei und Tastaturmenü. |
| Teilfehler | Datei-, einseitige und bidirektionale Syncfehler ließen bereits veränderte Panes stehen. Die aktuellen Paneorte werden auch nach Fehlern frisch gelesen. | Browser: alle drei Terminalfehler, tatsächliche Listingaufrufe. |
| Sync-Vorschau | Eine verspätete Vorschau konnte einen neueren Plan ersetzen. Antworten sind an die Revision der Ansicht, Optionen, Endpunkte und Tabs gebunden. | Verzögerte alte und neue Planantwort; alter Stand bleibt verworfen. |
| Sync-Start | Geschlossene oder umgeschaltete Ansichten konnten einen alten lokalen Lauf einreihen beziehungsweise alte bidirektionale Links-/Rechtsentscheidungen anzeigen. Jeder weitere Vorbereitungsschritt prüft seinen ursprünglichen Bedienkontext. | Alte lokale Vorbereitung, Pane-Tausch während bidirektionaler Vorbereitung, neue korrekte Rollenentscheidung. |
| Vorbereitungen | Eine verspätete Remotevorbereitung konnte den nächsten Start blockieren oder von dessen Cleanup getroffen werden. Vorbereiten und Verwerfen besitzen jetzt eine feste Reihenfolge und einen Besitzer. | Verzögerte FTP-/FTPS-Vorbereitung: vorbereiten, verwerfen, neu vorbereiten. |
| Doppelklick und Enqueue | Zwei Realmodus-Klicks konnten konkurrierende Vorbereitungen starten; eine späte Enqueue-Antwort konnte eine neu geöffnete Ansicht schließen. Startbesitz und Ansichtsrevision verhindern beides. | Browser: genau eine Vorbereitung; neue Ansicht bleibt offen. |
| Sync-Bestätigung | Links und rechts waren im Ausführungsdialog nicht durch vollständige Wurzeln erklärt. Beide Pfade erscheinen jetzt, auch lange Pfade mit Umbruch. | Nativer Dialog gelesen; dabei entdeckte Abschneidung korrigiert. Langer Browserfall prüft vollständigen Text und horizontale Geometrie. |
| Einstellungen | Eine gescheiterte Migration konnte lokale Einstellungen durch Defaults ersetzen. Ein fehlgeschlagener Sitzungswrite konnte denselben Stand fälschlich als bereits gespeichert behandeln. | Rote Migrations- und tatsächliche Dateischreibfehler; unveränderter Retry speichert anschließend korrekt. |
| Fensterende | Sitzungs-fsync lief beim Schließen auf dem GUI-Thread. Es läuft jetzt im Hintergrund; Beenden wartet auf ausstehende Abschlusswrites. Geschlossene Fenster werden durch späte Erinnerungsereignisse nicht wiederbelebt. | App-Tests für mehrere ausstehende Writes und späte Erinnerung; nativer Zwei-Fenster-Test mit sofortigem Beenden und Neustart. |
| Einstellungsabfragen | Drei lesende Commands warteten auf dem GUI-Thread hinter demselben Mutex wie laufende Sitzungswrites. Auch diese Abfragen laufen jetzt in Blocking-Workern. | App-Suiten und Workspace-Clippy auf beiden Plattformen; Erfolgsantworten bleiben unverändert. |
| Profiltransaktionen | Ein zweiter Profilworker wartete mit gehaltener Exit-Barriere auf den ersten und konnte damit erneut das GUI-Ende blockieren. Er liefert jetzt unmittelbar einen erklärten Busy-Status. | Tatsächlich roter Zweithread-Gegenlauf, danach grün; vorhandene Exit- und Secrettransaktionstests bleiben erhalten. |
| Providercommands | 32 FTP-/FTPS-, WebDAV- und SMB-Commands führten Netzwerk-, Schlüsselbund-, Profil- oder Journal-I/O unmittelbar im GUI-Thread aus. Sie verwenden jetzt Blocking-Worker mit dort gebundenem App-State. | Vollständiger Diffreview der Bodies und vorhandenen Reservierungs-/Profil-/Exitassertionen; App-Suiten beider Plattformen. |
| Datei-Commands | Zwölf lokale und providerneutrale Commands führten Konfliktscan, Capability-Proben, Queuewrites, mkdir, Rename oder Papierkorb-I/O auf dem GUI-Thread aus. Sie laufen jetzt in Blocking-Workern. | Alle zwölf fachlichen Bodies einschließlich Reservierung und Queuegates unverändert; App-Suiten/Clippy beidseitig und native Dateiaktionen. |
| Pane-Darstellung | Die Antwort eines Hidden-Listings setzte zwischenzeitlich gewählte Spalten, Dichte und Schriftgröße zurück. Neuere Darstellungsoptionen bleiben beim Refresh desselben Orts erhalten. | Gehaltenes echtes Mocklisting, sichtbare Hidden-Datei und gleichzeitig bewahrte Anzeigeoptionen. |
| Tastaturzugang | Der Informationsdialog fokussierte bei laufender Größenmessung einen deaktivierten Knopf. Der Planvergleich verwendete eine nicht definierte Fokusfarbe. | Browser: Fokus im Modal, Escape vor Messende; sichtbarer Fokusrahmen im Vergleich. |
| CLI-Listing | Die Textausgabe meldete nur eine Zusammenfassung. Sie nennt jetzt Namen, Typ und Größe, mit escaped Steuerzeichen; JSONL bleibt unverändert. | Echte isolierte CLI mit Datei, Ordner und Symlink. |
| CLI-Baseline | Ein noch fehlender Baseline-Unterordner hinter einem Symlink konnte die Rootprüfung umgehen. Der vorhandene Präfix wird jetzt kanonisch aufgelöst; andere Fehler bleiben Fehler. | Fehlende Unterordner hinter einem Rootalias werden vor einer Mutation abgelehnt. |
| SFTP | Hostkey-/Signaturfehler wurden als Verbindungsverlust eingeordnet. Ein hängender Primärhost verbrauchte außerdem die gesamte Frist vor der Alternative. Identitätsfehler stoppen; Verbindungsalternativen erhalten einen Anteil der bestehenden Gesamtfrist. | Echte Produktionsschleife mit hängendem ersten Kandidaten; Identitätsfehler versucht keinen zweiten Server. |
| FTP/FTPS | Eine nicht-ASCII-MLSD-Zeitangabe konnte beim Stringslice paniken. Die Prüfung arbeitet jetzt auf Bytes und liefert einen Datenfehler. | Tatsächlicher Panic mit `2026091012345é`, danach regulärer Fehler. |
| DELETE-Readback | Gleicher Typ und Inhalt nach verlorener DELETE-Antwort bewiesen keine unveränderte Identität. Ein vorhandener Nachfolger benötigt nun denselben bestätigten Provider-Token. | Datei und Ordner, anderer/fehlender Token, unvollständiges/mehrdeutiges/fehlerhaftes Listing; genau eine Mutation. Ein unsicher benannter Nachbar blockiert den unabhängigen Beleg nicht. |
| WebDAV-Löschauswahl | Eine bereits bekannte fehlende oder schwache Collection-ETag wurde erst nach einer zuvor ausgewählten Dateilöschung abgelehnt. Alle benötigten Tokens des ausgewählten Plans werden jetzt vor dessen erster Mutation geprüft. | Echter HTTPS-Gegenlauf mit unabhängiger Datei plus schwachem beziehungsweise tokenlosem Ordner. Die alleinige schwache Collection war bereits durch ihre Elternprüfung geschützt. |
| WebDAV-Navigation | IPv6-Discovery verwendete URI-Klammern als TLS-Hostnamen. Slashlose Collectionanfragen scheiterten an normgerechten Weiterleitungen. | IPv6-/IPv4-/DNS-Test; HTTPS-Server verlangt Slashform für OPTIONS/PROPFIND. Genau eine Weiterleitung auf dieselbe kodierte URL plus Slash; keine Mutationswiederholung. |
| WebDAV-XML | Standardoptionales `responsedescription` machte gültige Listings unlesbar. Es ist jetzt ein begrenztes Skalarfeld an den drei erlaubten Strukturstellen. | Text, CDATA, Entities und leeres Feld; falsche Eltern, verschachtelte Pflichtfelder und überlange Werte bleiben abgelehnt. |
| DMG-Bau | Ein Neubau entfernte das bisherige fertige Image vor Erfolg. Der Build arbeitet jetzt in einer eigenen Stage und ersetzt das fertige Paket erst nach erfolgreicher Signatur. Bei ungeklärtem Mount bleibt das Arbeitsimage erhalten. | Vorhandenes Image bleibt bei belegtem Mount und fehlgeschlagenem Neubau bytegleich erhalten. |
| DMG-Prüfung | Der Fehlercleanup hing einen Mountpfad statt des eigenen Devices aus und konnte im Erfolg erneut detach aufrufen. Er bindet ausschließlich Devices aus seiner eigenen Attach-Antwort. | Das echte Shellskript mit künstlichen Apple-Werkzeugen: Fehler, fehlende Mountzuordnung und Erfolg; alle drei Gegenfälle gegen die alte Fassung rot. |
| Testprozesse | Zwei Timeoutwege beendeten den SFTP-Runner hart und umgingen dessen EXIT-Cleanup. Eigener Prozessbesitz bleibt jetzt bis TERM/Wait erhalten; Timeoutphasen werden vorher begrenzt diagnostiziert. | Tatsächlich roter TERM-Sentinel; danach grün. Keine aus Statusdateien gelesene PID wird signalisiert. |

Die Änderungen führen keine zusätzliche Bestätigung für Klartext-FTP oder gepinntes
FTPS ein. Unklare Serverwirkungen bleiben sichtbar; Mutationen werden nach einem
Verbindungsfehler weiterhin nicht blind wiederholt.

## Alltagstauglichkeit im Vergleich zu ForkLift

ForkLift verbindet den Zweipane-Aufbau mit Datei-Zwischenablage, Hintergrundaktivität,
Konfliktvarianten, Suche, Vorschau, Remote-Bearbeitung, Archiven und
Stapelumbenennung. Diese konkreten Abläufe sind der sinnvollere Vergleichsmaßstab
als die Zahl der unterstützten Protokollnamen.
Quellen: [ForkLift-Handbuch](https://www.binarynights.com/manual) und
[Funktionsübersicht](https://www.binarynights.com/).

| Alltagssituation | Stand von Doppeldecker | Priorität |
| --- | --- | --- |
| Während einer großen Kopie weiterarbeiten | `input-controller.js`, `operation-ui.js` und `file-actions.js` sperren Navigation beziehungsweise weitere Aufträge im selben Fenster. Ein zweites Fenster hilft, ersetzt aber keine gute Hintergrundqueue. Lokale Enqueues und providerseitige mutierende Vorbereitungen besitzen unterschiedliche Backend-Schranken; die Frontendsperre allein zu entfernen genügt nicht. | Zuerst beheben. Die laufende Operation muss ihre eigenen gebundenen Endpunkte behalten, während die sichtbaren Panes weiter navigieren. |
| Dateien wie gewohnt kopieren und einfügen | F5/F6 und Drag-and-drop sind vorhanden; eine Datei-Zwischenablage mit den üblichen Kürzeln fehlt. Textbearbeitungsbefehle sind kein Ersatz dafür. | Zuerst ergänzen, einschließlich Textfeldabgrenzung und frischer Quellenprüfung. |
| SFTP im täglichen Gebrauch | Interaktives Listing ist verfügbar; Öffnen, Download, mkdir, Rename, Upload und Sync sind gesperrt. Das ist noch kein brauchbarer Ersatz für einen SFTP-Dateimanager. | Eigene begrenzte Best-Effort-Ausführungsklassen entwerfen, zunächst für lesende Inhalte und additive Übertragungen. Keine pauschale Freischaltung aller VFS-Methoden. |
| Standard-WebDAV benutzen | Schreiben hängt an einer künstlichen nichtrekursiven Collection-DELETE-Probe und starken Collectionidentitäten. Ein Standardserver kann deshalb trotz möglichem PUT/MKCOL wesentliche Dateiaktionen verlieren. | Fähigkeiten gewöhnlicher Dateiaktionen von rekursivem Löschen und Probenbereinigung entkoppeln. |
| Einen Ordner mit einzelnen problematischen Namen kopieren | `require_complete_safe_listing` in `core/src/fileops.rs` kann wegen eines nicht sicher dekodierten Nachbarn den ganzen Ordner beziehungsweise eine unabhängige Ersetzung sperren. Der historische Querschnitts-Fund bleibt offen. | Vollständigkeit, Namenskollision und Auswahlwirkung getrennt behandeln; übersprungene Einträge sichtbar bilanzieren. Keine pauschale Lockerung für Mirror oder rekursives Löschen. |
| Lokalen Sync starten | Der normale GUI-Weg bleibt außerhalb einer validierten Sandbox ein Testlauf; der Realmodus besitzt einen eigenen Assistenten. Die Beschriftung erklärt diesen Unterschied nicht durchgehend direkt am Startknopf. | Vorschau und echten Lauf deutlich unterscheiden. Additive Übertragung mit passender, einfacher Bestätigung anbieten; Mirror separat behandeln. |
| Konflikte entscheiden | Ersetzen/Überspringen/Abbrechen sind vorhanden. Bidirektionale Konflikte zeigen noch keine ausreichend vergleichbaren Daten beider Versionen; beim Erstabgleich werden auch einseitige Pfade als Konflikt behandelt. Überspringen kann dort erst später an der Baselinegrenze scheitern. | Beide Endpunkte, Größen, Zeitstände und tatsächliche zulässige Entscheidungen anzeigen. Danach Beide-behalten mit backendseitig vergebenem Kollisionsnamen ergänzen. |
| Einen Namen finden | Type-ahead springt zu Einträgen. Schnellfilter und rekursive Suche fehlen. | Schnellfilter vor einer vollständigen Inhaltssuche; vorhandene Virtualisierung weiterverwenden. |
| Dateien beurteilen und bearbeiten | Informationsdialog und externer Vergleich sind vorhanden. Eine integrierte Vorschau und komfortables Remote-Bearbeiten mit ausdrücklich geprüftem Rückupload fehlen. | Nach den grundlegenden Dateiabläufen. Ein Snapshot allein ist noch kein Bearbeitungsworkflow. |
| Viele ähnliche Dateiaufgaben erledigen | Tabs, Favoriten, Spalten, Ordnergrößen und persistente Queue sind vorhanden. Stapelumbenennung, Duplizieren und Archivnavigation fehlen. | Anschließender Komfortausbau; kein Grund, eine ehrliche erste Preview bis zur vollständigen ForkLift-Parität zurückzuhalten. |

Die WebDAV-Grenze ist technisch konkret: RFC 4918 erlaubt für Collection-DELETE nur
`Depth: infinity`. Das Entfernen des heutigen `Depth: 0`-Headers wäre deshalb keine
sichere Reparatur: Ein nach der Bestätigung hinzugekommenes Kind könnte rekursiv
mitgelöscht werden. Gewöhnliche Dateiaktionen brauchen einen unabhängigen Vertrag.
Siehe [RFC 4918 §9.6.1](https://www.rfc-editor.org/rfc/rfc4918#section-9.6.1).

SFTP und FTP sollten ebenfalls nach derselben Produktlogik bewertet werden. Eine
fehlende descriptor-relative Elternbindung ist ein konkretes Restrisiko; sie ist
kein überzeugender pauschaler Grund, bei einem Protokoll schon den Download zu
verbieten und beim anderen gewöhnliche pfadbasierte Aktionen anzubieten. Die
Korrektur muss die erlaubte Wirkung und ihre Grenze beschreiben, statt eine strikte
Syncfähigkeit unter einem anderen Namen wiederzuverwenden.

## Was gegen einen GitHub-Release spricht

Ein klar bezeichnetes Preview kann mit weniger Komfort sinnvoll sein. Ein als
alltagstauglicher ForkLift-Ersatz beworbener stabiler Release wäre derzeit zu weit.
Die fehlenden Basisabläufe und die SFTP-Grenze müssten sichtbar erklärt werden.

Vor einem ersten öffentlichen Paket bleiben erforderlich:

1. **Aktuellen Linux-Kandidaten interaktiv abnehmen.** Der alte Gegenlauf von
   `0.1.123` stammt vom 2026-08-06. Core, Provider, Queue und Fenster haben sich
   seitdem wesentlich geändert. AppImage und DEB benötigen aktuellen Start-,
   Installations-, Upgrade-, Deinstallations-, Mehrfenster- und Recoverybeleg.
   Ein erfolgreicher Cargo-Test ersetzt diese Abnahme nicht.
2. **Den tatsächlich auszuliefernden Kandidaten prüfen.** App, DMG beziehungsweise
   Linuxpaket, CLI-Sidecar und Updater müssen dieselbe Version und den beabsichtigten
   Quellstand enthalten. Signatur, Notarisierung, Tickets, Architektur, Prüfsummen,
   Paketextraktion und bytegleicher Sandbox-Smoke gelten für genau dieses Paket.
3. **Außenwirkung prüfen.** README, Release Notes, aktuelle Funktionsmatrix und
   ausgelieferte Git-Historie müssen die echten Grenzen nennen und frei von privaten
   Pfaden, Hosts, Kontakten und Secrets sein. Ein neuer sauberer Commit allein prüft
   die bestehende Historie nicht.
4. **Veröffentlichungsvoraussetzungen festlegen.** Im bestehenden Releaseplan sind das öffentliche Repository
   und sein Remote noch als ausstehend geführt. Support-, Datenschutz- und Impressumslinks
   sind laut bestehendem Releaseplan noch festzulegen; der beabsichtigte öffentliche
   Snapshot und die private Sicherung der vollständigen Historie brauchen eine
   konkrete Freigabe. Diese organisatorischen Punkte sind keine Codefehler.
5. **Repräsentative echte Dateiabläufe durchführen.** Lokale Kopie, Move, Rename,
   Konflikt, Abbruch, Wiederanlauf und mindestens ein realistischer Remoteablauf
   müssen mit künstlichen Daten funktionieren. Eine rein simulierte Browserbrücke
   belegt keine Serverfähigkeit.

Eine vollständige Komfortparität, zusätzliche Cloudanbieter, ein Git-Client oder
ein weiterer Architekturumbau sind keine Voraussetzung für das erste Preview.
Ebenso ist ein offener Vollreviewbereich allein kein pauschales Releaseverbot.
Maßgeblich bleiben konkrete Defekte, zugesagte Funktionen und Kandidatenbelege.

## Prüfstand und Grenzen

- macOS: Core **910 bestanden, 15 gezielt ignoriert**, CLI **65**, App **179**;
  Workspace-Clippy mit `-D warnings`, Format und Fuzz-Check mit gesperrtem Lockfile.
- Linux: Core **929 bestanden, 15 gezielt ignoriert**, CLI **65**, App **175**;
  Workspace-Clippy, Format und Fuzz-Check. Isolierte Kopie mit den geprüften Quellen.
  Zusätzlich bestanden die **42** SFTP-Runner- und DMG-Werkzeugtests. Die vollständige
  Python-Werkzeugsuite ist dort mangels Node.js nicht belegt.
- Werkzeugsuite: **134 Tests bestanden** nach dem Timeout-Cleanup-Fix. Im ersten
  Abschlusslauf scheiterten zwei bestehende SFTP-Helper an Zeitgrenzen. Die
  ursprüngliche Timingursache ist nicht nachgewiesen; sie wurde nicht durch eine
  Fristerhöhung oder einen Interpreterwechsel verdeckt.
- Browser: **249 Mock-UI-Fälle**, CSP-Prüfung und **sechs Pane-Tab-Modultests**
  bestanden, einschließlich gezielter roter und grüner Gegenbelege. Das
  Mock-Listing verwendet die echte Oberfläche und deren CSP.
- Native macOS-Sandbox: zwei Fenster, Einstellungen, Close unmittelbar vor Quit,
  Neustart mit genau einem erhaltenen Fenster, echte Kopie mit Inhaltsreadback,
  Konfliktabbruch und bidirektionale Vorbereitung ohne Ausführung. Die dabei
  entdeckte Abschneidung langer Wurzeln ist korrigiert; Browser und abschließender
  nativer Dialog zeigen beide vollständigen Pfade mit sauberem Umbruch. Der letzte
  Datei-Worker-Nachtrag besteht native F2-/F5-/F6- und mkdir-Läufe mit tatsächlichem
  Inhalts-/Pfadreadback. Auch der finale Start-/Einstellungs-IPC ist geprüft.
- Aktuelle Developer-ID-App gebaut, von Apple akzeptiert, Ticket angeheftet und
  validiert; Gatekeeper- und Privacyprüfung bestanden. Aus dieser App ein natives
  Test-DMG gepackt: Signatur, eingebettetes App-Ticket, Prüfsumme und bytegleiche
  App-/CLI-Inhalte bestanden. Das Test-DMG selbst ist nicht notarisiert; ein
  Updaterpaket wurde in diesem Review nicht hergestellt oder abgenommen. Die
  installierte App wurde nicht ersetzt, und es wurde kein GitHub-Release veröffentlicht.
- Die gezielt ignorierten externen OpenSSH-/Samba- und Spezialtests gelten hier
  nicht als bestanden. FTP/FTPS und WebDAV wurden gegen die künstlichen
  Loopbackserver ihrer Core-E2Es ausgeführt. Keine neue interaktive Linuxabnahme,
  keine Stromausfallprobe und kein vollständiger öffentlicher Historienaudit.

## Abdeckung und Fortsetzung

Der alte AP-025-Rest ist geklärt: `sync_tests.rs` wurde vollständig gelesen; die
übrigen acht Ganzdateibelege wurden gegen ihre aktuellen Hashes und Deltas
abgeglichen. `acf7768` begrenzt die zuvor offenen regulären Streams bereits. Der
bedingte Provider-Claim verspricht nach seinem VFS-Vertrag unveränderte Wirkung
bei jedem Fehler und besitzt derzeit keine produktive Providerimplementierung.
Daraus folgt kein neuer bestätigter Fehler.

Von **26** registrierten CodeQA-Bereichen besitzen jetzt **18** einen aktuellen
Abschlussbeleg; **acht** Ganzbereiche und der abschließende Querschnitt bleiben
offen. Die Zahl ist Bereichsabdeckung, keine Aussage über 18/26 aller Codezeilen.
Fünf als offen geführte ältere Kandidaten waren bereits durch vorhandene Fixes
überholt: private Papierkorb-Claims, lokale Sicherungsbilanz, exFAT-Commitklasse,
Recoverypfade über Teilaktionen und Defaults nach Einstellungslesefehlern. Ihre
vorhandenen Belege bleiben erhalten; sie werden nicht als neue Korrekturen dieses
Reviews ausgegeben.

Die neuen Ganzdatei-Runden umfassen Remote-Grundlagen, SFTP, FTP/FTPS, WebDAV,
CLI, Datei-/Queue-Frontend, Sync-Frontend, Pane-Shell, Layout/Plattform-Frontend und
Releasepipeline. Tauri-Kommandos, Fensterende und weitere Grenzstellen wurden
gezielt gelesen und korrigiert; das ist kein behaupteter Vollreview ihrer gesamten
übergeordneten Bereiche. `.codeqa/coverage.json` und
`.codeqa/full-review-2026-10-07.json` halten den genauen Bereichs- und Hashstand.

Als nächstes empfiehlt sich ein zusammenhängendes Paket für Hintergrundarbeit,
Datei-Zwischenablage und verständliche Konflikt-/Syncbedienung. SFTP-/WebDAV-
Ausführungsklassen und die aktuelle Linux-Paketabnahme folgen als eigene klar
begrenzte Pakete. Das verhindert, dass ein einzelner Achtstundenauftrag wieder zu
einem vollständigen Dateimanager-Neubau ausufert.

### Produktdeltas nach diesem Review

AP01 und AP02 sind inzwischen umgesetzt und nativ geprüft; ihre Verträge stehen
unter [Hintergrundarbeit](background-file-work.md) und
[Konfliktentscheidungen](conflict-decisions.md). AP03 ergänzt den normalen lokalen
AddOnly-/Update-Start samt eigener Queueabsicht, wirkungsbezogener Bestätigung,
frischer Zustimmung nach Neustart und C2-Staging-Recovery. Geprüft wurden die
geänderten Core-/Queue-, App- und Frontendgrenzen sowie ihre automatisierten und
nativen Abläufe. [Lokaler Sync](local-sync.md) nennt den genauen Umfang und die
verbleibenden Best-Effort-Zielklassen.

Diese Produktprüfungen sind keine erneuten Vollreviews der übergeordneten
CodeQA-Bereiche. Neue Module sind diesen Bereichen in `.codeqa/coverage.json`
zugeordnet und als ausstehende Vollreviewpfade markiert. Historische Hashes und
die damalige Zahl 18/26 bleiben Belege des vorherigen Standes; sie behaupten
keine unverändert aktuelle Abdeckung nach dem Produktausbau.
