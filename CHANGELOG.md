# Änderungsprotokoll

Alle wesentlichen Änderungen an Doppeldecker werden hier dokumentiert. Das Format
folgt Keep a Changelog; Versionen verwenden Semantic Versioning.

## [Unreleased]

## [0.1.124] - 2026-10-19

- Die beim Öffnen geplante Sync-Vorschau entwertet keinen gerade begonnenen
  Bestätigungsdialog mehr. Änderungen an Optionen oder Pane-Zielen verwerfen
  die Vorbereitung weiterhin.

Erste öffentliche Preview für macOS 14+ auf Apple Silicon und Linux Mint 22.2 /
Ubuntu 24.04 auf x86_64. Bekannte Providergrenzen stehen in der Funktionsmatrix.

### Korrigiert

- Linux-Fenster liefern das gewählte App-Icon in einer für GTK/X11 passenden
  Größe an das Dock. Auch das Hauptfenster übernimmt die gespeicherte Icon-Wahl.

- Die Linux-Menüs zeigen die passenden Strg-Kürzel und bieten Beenden,
  Minimieren sowie Fensterschließen als funktionierende native Aktionen.
  Offene Queue-Aufträge behalten ihre Schließ- und Beendenrückfragen.

- Die Linux-Paketprüfung akzeptiert den AppImage-Desktoplink auf die eigene
  installierte Desktopdatei. Fremde Linkziele und verlinkte Zielkomponenten
  bleiben gesperrt; die übrigen Paketdateien müssen weiterhin regulär sein.

- Die Geometriewiederherstellung beim Fensterbau wartet nicht mehr hinter einem
  laufenden Einstellungswrite. Validierte Startgeometrien werden vor dem ersten
  Fensterbau separat gebunden; der gespeicherte Sitzungsstand bleibt erhalten.

- Bidirektionales Vorbereiten, Resume und Rollback scannen außerhalb des
  GUI-Threads. Das Fenster bleibt während der Vorbereitung bedienbar; die eigene
  Reservierung und Profilbindung bleiben erhalten. Ein Abbruch verwirft nur eine
  frische Vorbereitung, während bestehende Recovery-Journale erhalten bleiben.

- Die Queue verwendet gemeinsame private Rahmen für Fortschrittsjournalisierung
  und Dateiabschlüsse. Abbruch nach Journalfehler, terminaler Commit vor dem Audit
  und reine Abschlusswiederholung bleiben für alle betroffenen Wege gleich.

- Additive Ordnerkopien übertragen sichere Kinder auch bei nicht dekodierbaren
  Quellnamen. Ausgelassene Rohnamen erscheinen mit Zahl, Elternpfad und Hexbytes
  in der Quellbilanz; GUI-Queue und CLI erhalten diese Hinweise. Move, Ersetzen
  und vollständige Konfliktkopien behalten ihren strikten Baumvertrag.

- Best-Effort-Ersetzen erkennt korrekt bestätigte exakte Ziele neben bereits
  vorhandenen äquivalenten Dateinamen durch zusätzliche Inhaltsbelege. Neue oder
  veränderte Nachbarn bleiben unklar; normalisierte Temp-Reste werden weder als
  verschwunden noch als sicher eigener Cleanupbestand behandelt.

- Fehlerkontexte erhalten maschinenlesbare Codes und Prüfpfade. Das gilt auch
  für Remote-Löschpläne, Quarantäne-Claims und das Tauri-Fehlerpayload;
  ein Verbindungsverlust vor dem Manifestwechsel bleibt ein Verbindungsfehler.
  Zusammengesetzte DELETE- und Queue-Resetfehler behalten alle Ursprungspfade;
  ein ursprünglicher unklarer Ausgang bleibt auch bei einem Resetfehler führend.

- Remote-Quarantäneproben verwenden denselben Ergebnishelfer wie andere
  Capabilityproben. Unklare Cleanupfehler erhalten ihre strukturierten Prüfpfade;
  kritische Probenfehler werden an den Capabilitygrenzen weitergereicht.

- Die bidirektionale Paarregistry blockiert nicht mehr an FIFOs und folgt keinen
  finalen Symlinks. Lesen und Schreiben sind auf 16 MiB begrenzt; ein zu großer
  Schreibbestand erhält die bisherige Registry ohne temporäre Datei.

- SFTP-Signaltests bereiten ihre unveränderlichen Testprogramme einmal je
  Testklasse vor. Verzögerte macOS-Startprüfungen frischer Attrappen verbrauchen
  dadurch nicht mehr die Ready-Frist eines Signalfalls; Zustände und Roots bleiben
  je Fall getrennt. Die bisherigen Ready- und Cleanupfristen bleiben erhalten.

- Abgebrochene Code-Gates beenden ihre eigene Prüfprozessgruppe einschließlich
  verbleibender Nachkommen. Browserprüfungen und Screenshot-Erzeugung räumen
  ihren Mock-Server auch bei Signalabbruch, wartendem Browserstart oder einem
  Fehler beim Schließen des Browsers auf.

- Der macOS-App-Export im Projekt ersetzt den bisherigen Stand erst nach einer
  vollständigen Kopie durch einen atomaren Tausch. Ein Kopier- oder Übergabefehler
  erhält die vorherige App; für einen Erstexport gilt exklusives Anlegen.

- Verspätete Geräteantworten bringen ausgeworfene Volumes nicht zurück und
  entfernen nach einem alten Lesefehler keine aktuelle Liste. Favoriten werden
  nach Änderungen frisch gelesen; alte Listen oder Mutationsantworten setzen
  spätere Änderungen nicht mehr zurück. Ein Readbackfehler benennt die bereits
  gespeicherte Änderung; auch ein Fehler beim ersten Laden bleibt sichtbar.

- Hintergrundabschlüsse verdrängen keine bereits angeforderte Pane-Navigation
  mehr. Das gilt auch für die noch wartende Home-Pfadauflösung und späte Rename-,
  Ordner- oder Papierkorbabschlüsse. Nach Navigationsfehlern wird wieder aktualisiert.

- Eine verspätete bidirektionale Einreihantwort erhält bereits sichtbare
  Queue-Rückfragen und Abschlüsse. Sie schreibt auch nach einem Wechsel der
  Syncansicht keinen alten Einreihstatus mehr darüber. Wirkungslose Einwegoptionen
  werden in beiden Richtungen ausgeblendet; der feste Inhaltsvergleich und das
  Portable-Profil werden erklärt. Die vorherige Einwegeinstellung bleibt erhalten.

- Eine verspätete Einreihantwort von Dateiaktionen überschreibt neueren
  Fortschritt, Abschluss oder unklaren Ausgang nicht mehr. Die erneute lokale
  Sync-Bestätigung kehrt auch nach einem Mausklick zur sichtbaren Queue und
  ihrem bearbeiteten Eintrag zurück.

- CLI-Sync und Dateiaufträge prüfen beide Profile vor Queueöffnung und erster
  Verbindung. JSONL-Fehler erhalten bekannte Prüfpfade einschließlich getrennter
  Ausführungs- und Journalfehler. Konfliktentscheidungen werden über ein geprüftes
  Handle ohne finalen Symlink und ohne FIFO-Warten bis höchstens 16 MiB gelesen.

- Der externe Dateivergleich liest SFTP-Dateien über die vorhandene geprüfte
  interaktive Inhaltsklasse. Vorbereitung und Downloads sind fenstereigen
  abbrechbar; Best-Effort-Pfadrisiken bleiben sichtbar. Die Werkzeugübergabe
  erhält eine eigene Phase, ohne eine Rücknahme bereits gesendeter Aufrufe
  zu versprechen oder spätere Queueanzeigen zu überschreiben.

- Finder-Import weist nach der Dateiauswahl eingewechselte FIFOs ohne Warten zurück.
  System- und Fastra-Helferausgaben sind schon während des Lesens auf ein MiB
  begrenzt; bei Überlänge bleibt ein möglicher Effekt als unbestätigt sichtbar.

- Die bidirektionale Paarregistry erhält fremde Dateien bei Tempnamenskollisionen.
  Profil- und Terminalfehler des bidirektionalen Syncs behalten ihre Codes.
  FTPS-, WebDAV- und SMB-Verbindungsstatus prüfen den Profilstand vor der Rückgabe erneut.

- Laufwerksdaten benötigen einen exakt passenden Mountpunkt. Das laufende
  macOS-Systemvolume bleibt auch bei einer externen Startplatte nicht auswerfbar.
  Der Favoritenleser weist FIFOs ohne blockierendes Warten zurück; eine
  fehlgeschlagene Ordnergrößen-Neuberechnung reaktiviert keinen alten Vollwert.

- Ein fehlgeschlagener FTP-/FTPS-Upload behält bei weiteren Schreib- und
  Flushaufrufen den unklaren Ausgang samt Prüfpfaden. Eine verlorene Rename-Antwort
  wird nicht mehr nachträglich zu einem einfachen Verbindungsfehler abgeschwächt.

- SFTP erfasst einen beim Schließen des Inhaltsreadbacks gemeldeten
  Verbindungsverlust auch nach einer Inhaltsdrift. Der ursprüngliche Fehler bleibt
  sichtbar; der nächste neue Leseversuch verbindet frisch, selbst wenn der alte
  fehlgeschlagene Leser noch gehalten wird.

- WebDAV-Ordneranlage verwendet eine gemeinsame Vorbereitungsfrist für alle
  Elternprüfungen, auch innerhalb eines Uploads. Eine delegierte Prüfung erneuert
  die Frist nicht mehr; unmittelbar vor MKCOL werden Frist und Sitzung geprüft.

- Gemeinsame Remote-Fehler behalten bekannte Proben- und Cleanup-Pfade sowie
  geerbte Recoveryhinweise nach Rename-Readback und Reconnect strukturiert.
  Providerungültige Rename-Namen und Pfade scheitern vor Profilauflösung,
  Queue-Journal und Fähigkeitsproben über dieselben Parser wie die Provider.

- Das Schließen des letzten sauberen Fensters lässt die macOS-App weiterlaufen.
  Ein neu geöffnetes Fenster kann wieder Dateioperationen starten; explizites
  Beenden bleibt an die vollständige Exitprüfung gebunden.

- SMB prüft den beobachteten GUID-Pin vor der Secretauflösung und verwirft
  unverschlüsselte Antworten in einer verschlüsselten Sitzung. Leere Reads
  erhalten den Stream; nach dem letzten Verbindungsklon werden Transport und
  Empfänger freigegeben. Fehlgeschlagene Proben erhalten kollidierten Bestand
  samt Recoverypfad und stoppen bei unklarem Ausgang. Vollständige SMB-Pfade
  werden vor einem Überlauf des Protokoll-Längenfelds abgelehnt.

- Natives macOS-Beenden einschließlich Cmd-Q durchläuft die Backendprüfung
  für laufende Operationen, Vorbereitungen und offene Queueeinträge. Die direkte
  AppKit-Terminierung konnte eine laufende Kopie unmittelbar beenden.

- Eine gesetzte ungültige Testsandbox bricht mit einem Fehler ab. Leere,
  reine Leerraum- oder nicht als UTF-8 darstellbare Werte aktivieren nicht mehr
  versehentlich den unbeschränkten Normalmodus.

- Queue-Worker starten bei geschlossener Fenster-/Appschranke keine endlose
  Neustartkette. Ein abgewiesenes Schließen öffnet die Schranke und weckt
  bereits bestätigte offene Arbeit wieder, ohne einen zweiten Worker zu starten.

- Lokale Restore-Proben erhalten fremde Dateien, wenn deren Namen beim Anlegen
  oder einem der drei Renames kollidieren. Cleanup folgt nur dem bestätigten
  Probeort; unklare Anlagen und Renames behalten ihre möglichen Recoverypfade.

- Remote-Momentaufnahmen begrenzen Download und Kontrolllesung auf die angekündigte
  Dateigröße plus ein Prüfbyte. Ein zu langer Datenstrom wird früh abgelehnt,
  statt erst nach vollständigem Empfang die Größenabweichung zu melden.

- „Beide behalten“ bindet bei nativen Quellordnern auch die Inhalte des gesamten
  Baums. Nachträglich geänderte Kinder verlangen vor Copy oder Move eine neue
  Planung, auch nach einem Queue-Neustart und bei unveränderten Ordnermetadaten.
  Lokale WebDAV-Uploads bilanzieren Größe und Größenlimits anhand des tatsächlich
  geöffneten Dateihandles statt eines vorangegangenen Pfad-Stats.

- Die Ergebnisbilanz eines begrenzten lokalen Best-Effort-Syncs zählt auch
  tatsächlich abgeschlossene Hardlink-Ersetzungen als dauerhafte Ersetzungen.
  Reine Hardlink-Additionen erhöhen diesen Zähler weiterhin nicht.

- Generischer Provider-Sync behält die unabhängig beobachtete FTP-/FTPS-Zielsitzung,
  wenn zwei Verbindungen trotz gleicher Konfiguration unterschiedliche kanonische
  Profilroots melden. Die gemeinsame Verbindungsauswahl für Sync und Dateioperationen
  verhindert, dass der Zielpfad auf die Quellabbildung umgebunden wird.

- Bidirektionale Konfliktentscheidungen berücksichtigen die lokale Grenze von
  100 Typwechseln bereits bei der nächsten Auswahl, einschließlich „Beide
  behalten“. Mehrere neue Konfliktkopien mit case- oder Unicode-äquivalenten
  Geschwisternamen werden vor der Vorbereitung abgelehnt.

- Die künstlichen Fastra-Helfer für Fähigkeiten und Exitcodes benötigen keine
  frisch erzeugten ausführbaren Skripte mehr. Deren macOS-Ausführungsprüfung
  konnte schon vor der ersten Skriptzeile die Testfrist verbrauchen. Die Tests
  verwenden jetzt den festen Systeminterpreter; produktiver Helferaufruf,
  Prozessstatus, Pipeprüfung und Fristen bleiben erhalten.

- FTP-/FTPS-Skip berücksichtigt jetzt auch kollidierende Kinder zusammengeführter
  Ordner. Vollständig ausgelassene Bäume benötigen weder STOU noch eine
  Quellentfernungsprobe; die Bilanz zählt ihre übersprungenen Blätter. Mischfälle
  bereiten nur neue Inhalte vor und behalten Quellbäume mit ausgelassenen Kindern.
  Passive Konfliktanzeige und Skip benötigen nur Metadaten; Ersetzen und neue
  Namen behalten ihre vollständige Inhaltsbindung und konkrete Bestätigung.

- FTP-/FTPS-Konflikte werden zunächst ohne Schreibprobe angezeigt. Erst die
  gewählte Wirkung wird vorbereitet; Ersetzen und „Beide behalten“ verlangen
  danach die konkrete Bestätigung. Vollständiges Überspringen beendet den Auftrag
  ohne Schreib- oder Löschprobe. Übersprungene Top-Level-Einträge beeinflussen die
  übrigen Inhaltsproben nicht mehr. Ein später verschwundener Konflikt erteilt
  keinen neuen Kopierauftrag. Gesperrtes Ersetzen benötigt keine Quellentfernungsprobe
  und lässt Überspringen erreichbar.

- FTP-/FTPS-Kopien und bestätigte Copy-/Quellentfernungs-Moves vollständig
  inhaltsfreier Ordnerbäume benötigen keine STOU-Dateiproben mehr. Vorhandene
  Zielordner werden ohne zusätzliche Zielmutation zusammengeführt. Neue Ordner
  und frisch leere Quellordner verwenden ausschließlich ihre gebundenen
  MKD-/RMD-Belege und Readbacks als Best Effort. Späterer Dateiinhalt bleibt
  gesperrt; unklare RMD-Ausgänge behalten ihren dauerhaften Intent ohne Retry.

- Escape und Abbruchknopf erreichen jetzt die FTP-/FTPS-Dateivorbereitung des
  verursachenden Fensters. Nach Abbruch beginnen keine weiteren Teilproben und
  keine Nutzeroperation wird eingereiht. Bereits laufende Proben beenden ihren
  geprüften Transport und eigenen Cleanup; unklare Ausgänge bleiben mit ihren
  dauerhaften Intents sichtbar. Andere Fenster arbeiten unabhängig weiter.

- FTP-/FTPS-Moves prüfen No-Replace getrennt für Dateien und Ordner am
  tatsächlichen Zielroot. Eine Ordnerprobe gibt keinen Datei-Commit frei.
  Ohne passende Garantie verwenden GUI, CLI und Queue den ausdrücklich gebundenen
  Copy-/Quellentfernungsweg als Best Effort. Ein bestätigter reiner Rename wechselt
  nach einem Fehler nicht still in diese Wirkung. Gleiche Profil-IDs überspringen
  die Anzeige und Bindung der Quellentfernung nicht mehr; verschiedene beobachtete
  kanonische Roots behalten ihre eigene Zielverbindung.
  F2 bestimmt seine Rename-Garantie ebenfalls anhand des tatsächlichen Eintragstyps.

- Parallele FTP-/FTPS-Vorbereitungen verschiedener Pane-Roots verwerfen ihre
  erfolgreichen Proben nicht mehr gegenseitig. Überlappende Roots und erneute
  Proben derselben Vorbereitung verdrängen weiterhin ältere Antworten; Reconnect
  und widersprochene No-Replace-Garantien bleiben gesperrt. Ein verspätetes
  unabhängiges Ergebnis überschreibt den neueren Verbindungscache nicht.

- FTP-/FTPS-Vorbereitung prüft nur die für die konkrete Wirkung benötigten
  Fähigkeiten. Dauerhafte private Proben-/STOU-Intents binden Fensterjournal,
  Pane-Roots, Server, Konto und Sitzung vor dem ersten Schreibbefehl; bestätigte
  STOU-Namen stehen vor dem ersten Inhaltsbyte im Index. Journalformat 27 zeigt
  unterbrochene Vorbereitungen als manuelle Recovery ohne Retry oder automatische
  Löschung. Parallelproben entziehen laufenden Fenstern keine eigene Freigabe;
  ein bestätigter neuer No-Replace-Gegenbeleg sperrt ältere abgesicherte Wirkungen.
  Queuewrites, Audit und Prozesslock bleiben an den gehaltenen Ordner gebunden.

- FTP-/FTPS-Uploads halten den Datenkanal nach geprüftem Flush, TLS-Abschluss
  und TCP-Schreibende bis zur Kontrollantwort. Ein kontrollierter Linux-Gegenlauf
  reproduziert vorher einen Reset mit leerer STOU-Datei und danach den vollständigen
  Inhalt. Proben-Cleanup erhält Teilinhalt und fremden Bestand, alte betroffene
  Fähigkeiten werden vor Neuprüfung widerrufen, und Probe-/Cleanupfehler behalten
  ihre strukturierten Recoverypfade. Unklare Rename-/Uploadreste werden nicht blind
  gelöscht.

- SFTP-Browsergegenfälle halten verspätete Agent-/Listingantworten jetzt bis zu
  beobachteten Editor- und Navigationsgrenzen statt kurzen Zeitfenstern. Der
  Fastra-Exitcodetest unterscheidet echten Prozessstatus und Fristablauf von der
  Fehlerzuordnung. Der bidirektionale Abgleich prüft Schreibfähigkeiten nur noch
  auf tatsächlich veränderten Zielseiten; leere Erstbaselines und reine Quellen
  starten keine mutierenden Proben mehr. Drift- und Zielgarantien bleiben erhalten.
  Die historische Fastra-Ursache bleibt offen; der STOU-Transportbefund ist im
  nachfolgenden FTPS-Paket untersucht.

- WebDAV-Mutationen binden jetzt genau den unmittelbaren Elternordner mit einer
  `If`-Liste; weitere alternative Listen können eine falsche Elternversion nicht
  mehr überdecken. Der verbleibende Vorfahrenwettlauf bleibt sichtbar Best Effort.
  Allgemeine strikte Schreib-, Lösch-, Sync- und Quarantänewege bleiben konkret
  gesperrt; ihre unsicheren künstlichen Proben und die rekursive Probenbereinigung
  sind entfernt. Additive Uploads, mkdir, Rename und Downloads bleiben verfügbar.
  Die mkdir-/Rename-Vertragstexte sind begrenzt, per Tastatur bedienbar und kopierbar.

- WebDAV-Tragfähigkeitsprüfung: Getaggte `If`-Listen verschiedener Ressourcen
  gelten laut RFC 4918 mit ODER. Der gemeinsame HTTPS-Testserver prüft jetzt diese
  Semantik; frühere Gegenläufe bewiesen keine gemeinsame atomare Elternbindung.
  Upload, mkdir und Rename benennen den verbleibenden Elternwettlauf und erhalten
  mögliche Wirkungen mit unklarem Ausgang. Lokales `copy --replace` scheitert
  konkret vor Profil-, Secret- und Netzwerkzugriff, auch aus persistierter Queue.
  Es gibt keine neue Ersetzungsfreigabe; additive
  Datei-/Baumuploads bleiben verfügbar. Der Konfliktdialog zeigt den konkreten
  Ersetzungsgrund und lange Quellgrenzen in begrenzten, kopierbaren Textfeldern.

### Hinzugefügt

- Begrenzte additive lokale WebDAV-Baumuploads über F5, Dateimenü und CLI mit
  gehaltenen Quellen, Inhalts-/Umfangsdigest und starken Ziel-Elternketten.
  Neue Ordner entstehen mit gewöhnlichem bedingtem MKCOL ohne künstliche
  Collection-Löschprobe; vorhandene Ordner werden exakt additiv zusammengeführt.
  Vor jeder Ordnerwirkung liegt ein privater Queueintent vor; Teilwirkungen bleiben ohne rekursives Cleanup
  erhalten. Journalformat 26 sperrt auch einen Prozessabbruch zwischen fertigen
  Additionen gegen Retry. Pfadgenaue Quell- und Metadatenbilanzen bleiben in
  begrenzten, per Tastatur bedienbaren Textfeldern lesbar und kopierbar.

- Gewöhnliche additive WebDAV-Dateiuploads von lokalen regulären Dateien über F5,
  Dateimenü und CLI ohne künstliche Collection-/Löschprobe. Private Best-Effort-
  Zielsicht, gehaltene und doppelt gehashte Quellen, bedingtes PUT/MOVE und
  vollständiger Readback erhalten bestehende Ziele. Journalformat 25 sichert
  Remote-Tempintents, sperrt unklare Wiederholungen und verlangt nach Neustart eine
  frische Wirkungsbestätigung. Ausgelassene Zielmetadaten bleiben pfadgenau sichtbar
  und kopierbar; die Inhaltsvorbereitung ist in der GUI abbrechbar. Die erneute
  Zustimmung hat einen kurzen Titel und eine per Tastatur scrollbare Erklärung;
  fehlende Zustimmung wird nicht als beobachteter Serverwechsel dargestellt.

- SFTP-Ordnergrößen aus einem begrenzten Metadatenscan ohne Datei-OPEN. Spalte und
  Informationen zeigen beobachtete Werte mit „≈“, Best Effort und konkreten Grenzen.
  Große Dateien sind zählbar; Inhaltsdownloadgrenzen, strikte VFS-Sperren und eigene
  Fenster-/Pane-Läufe bleiben erhalten. Auslassungen und Einzelordnerfehler bleiben sichtbar.

- Gewöhnliche WebDAV-Umbenennung mit vorab sichtbarer Best-Effort-Klasse, starker
  Quellen-/Elternbindung und genau einem bedingten MOVE ohne künstliche Schreib-
  oder Löschprobe. Kollisionen erhalten vorhandene Ziele; ein unklarer Ausgang
  nennt beide Namen und den Elternpfad und führt zu keiner automatischen Mutation.

- Gewöhnliches WebDAV-Ordneranlegen nutzt eine eigene sichtbare Best-Effort-Klasse
  mit Elternbedingungen und Readback, ohne künstliche Schreib- oder Löschprobe.
  Eine fehlende ETag in der MKCOL-Antwort wird ausdrücklich bilanziert;
  unklare Ausgänge erhalten den möglichen neuen Ordner ohne automatischen Retry.
  Vollständige Fehler und Prüfpfade sind im Dock zugänglich aufklappbar.

- Additive SFTP-Downloads unterstützen jetzt begrenzte Verzeichnisbäume mit Unicode-
  Namen, leeren Ordnern, sichtbaren Auslassungen und gebundenen Konfliktkopien.
  Journalformat 24 stoppt bei verändertem Baumumfang vor lokalen Wirkungen und
  fordert eine neue Bestätigung; strikter Sync und SFTP-Mutationen bleiben gesperrt.

- SFTP-Inhalte als eigene begrenzte interaktive Best-Effort-Klasse: geprüfte lokale
  Momentaufnahmen regulärer Dateien und additive lokale Downloads.
  Hostkey, Sitzung, beobachtbare Pfadattribute, gehaltene READ-Handles, Größe und
  vollständiger BLAKE3-Readback werden geprüft; das erste OPEN bleibt ausdrücklich
  pfadbasiert. Vorhandene Ziele bleiben erhalten, „Beide behalten“ verwendet den
  gebundenen lokalen No-Replace-Weg. Queue-Reopen verbindet frisch; private
  Tempintents erhalten Crashreste ohne blinden Retry. Snapshots übernehmen keine
  Ausführungsrechte und öffnen unbekannte/programmartige Endungen als Text.
  Grenzen: 100 direkte Auswahlen, 8 GiB je Datei und die zusätzlichen oben
  beschriebenen Baumgrenzen. Ersetzen, Move, Upload, Sync und Remote-Mutationen
  bleiben für diese Klasse gesperrt.

- Begrenzter Best-Effort-Weg für normalen lokalen AddOnly-/Update-Sync,
  einschließlich macOS-exFAT. Neunamencommit, Datei- und Verzeichnisdauerhaftigkeit
  sowie dauerhafte Ersetzungen sind an den bestätigten Backendplan gebunden und
  in Dialog, Status, Ergebnis und Queue sichtbar. Schwache Ersetzungen bestätigen
  Umfang und fehlende Wiederherstellung ausdrücklich; ContentHash und die Grenze
  von 100 Ersetzungen bleiben erhalten. Gemeinsam mit interaktiven Dateioperationen
  verwendete Commithelfer prüfen Kollisionen und lesen den Inhalt zurück. Private
  Recoveryintents ordnen Temp-/AppleDouble-Reste nach Prozessabbruch ihrer Queue zu.
  Nicht darstellbare Portable-Rechte und native Metadaten bleiben pfadgenau
  bilanziert. Strikte Remote-, Mirror- und bidirektionale Gates bleiben erhalten.
- Normaler lokaler AddOnly-/Update-Start mit vergleichbarer Wirkungsvorschau und
  backendgehaltenen Roots. Reine Additionen benötigen weder Laufphrase noch
  Quarantäneprobe; Update erzwingt ContentHash und bestätigt Ersetzungen mit
  Wiederherstellungsweg. Die eigene Fensterqueue speichert nur die Absicht und
  verlangt nach Neustart eine frische Bestätigung. Ein eigener Simulationsknopf
  verändert auch in der Testsandbox keine Dateien. Mirror behält seinen Assistenten.
  Privates Sync-Staging bleibt nach Prozessabbruch der verursachenden Fensterqueue
  zugeordnet. Der Wirkungsdialog ist auch bei kleinem Fenster per Tastatur scrollbar.
  Eine nach Neustart verlorene wartende lokale Zustimmung wird ausdrücklich als
  lokaler Sync erklärt und verlangt eine neue Prüfung.
- Gemeinsamer Konfliktdialog für Sync und Copy/Move mit beiden Endpunkten, Typ,
  Dateigröße und Änderungszeit; unbekannte Werte bleiben sichtbar unbekannt.
  Erstbaseline-Entscheidungen und kumulierte Mengengrenzen kommen aus dem Backend.
  Beide behalten zeigt den vergebenen Namen vorab und überträgt Copy-/Move-Auswahlen
  über exklusive neue Zielnamen; lange Unicode-Namen erhalten ihre Erweiterung.
  Vorschau, simulierter Testlauf und echte Ausführung sind ausdrücklich beschriftet.
  Ein später eintreffender Startstatus verdrängt keinen bereits sichtbaren Laufabschluss.
- Fensterübergreifendes Paste wartet auf die jüngste noch ausstehende native
  Dateiübergabe. Eine fehlgeschlagene Übergabe fügt dabei keine vorherige Auswahl ein.
  Verspätete Datei-Vorschauen können eine neuere Backendbindung nicht verdrängen.


- Während einer Übertragung bleiben Navigation, Tabs, Auswahl, Informationen und
  weitere Datei-Aufträge erreichbar. Remote-Absichten werden ohne konkurrierende
  Capabilityprobe eingereiht und vor Ausführung frisch geprüft. Fortschritt und
  Abschluss binden Lauf-ID und ursprüngliche Endpunkte; passiver Refresh erhält
  Auswahl und Scrollposition, auch in aktuell geöffneten Unterordnern.
- Native lokale Datei-Zwischenablage mit Mehrfachauswahl, Copy/Paste und
  plattformüblichem Move. Textfelder bleiben unabhängig; Finder-Dateireferenzen und
  GTK-/GNOME-/KDE-Dateiformate werden unterstützt. Ein Move verbraucht nur den
  unveränderten Clipboard-Stand nach vollständigem Erfolg aller Gruppen.
  Große Auswahlen bleiben vollständig; verspätete Copyaufträge erhalten neuere
  Auswahlen und Text. Sofortiges Paste wartet auf die eigene native Übergabe.

- Ein gemeinsamer Papierkorb-Kern hält einen von Anfang an privaten Bereich,
  dauerhaften Anlageintent, direkten Claim, Info-/Daten-Transfer, Recovery und
  nichtrekursives Cleanup bereit. Beide Plattformadapter verwenden ihn; die
  gemeinsame native macOS-GUI-Sandbox-Abnahme ist belegt.

- Pane-Tabs besitzen eigene Leisten und Tastaturbedienung sowie unabhängige
  Navigation, Auswahl, Sortierung und Scrollposition. Wechsel listet frisch und
  verwirft alte Antworten. Sitzungsformat 2 speichert Tab-Stände je Pane und
  migriert Format 1 ohne lesenden Dateiersatz. Die native macOS-Abnahme mit
  Tastaturbedienung und echtem App-Neustart ist belegt.

### Korrigiert

- Ein unbrauchbarer SFTP-Name oder fehlender Dateityp versteckt unabhängige
  gültige Dateien nicht mehr. Die Pane zeigt Anzahl und kopierbare Gründe der
  Auslassungen; CLI-Text und JSON führen dieselbe Bilanz. Der begrenzte
  Lesetransport verwirft nicht benötigte SFTP-Langnamen sofort. Bei ausschließlich
  ausgelassenen Einträgen erklärt die Pane, dass nichts angezeigt werden kann.

- Fehlende SFTP-Dateigrößen bleiben in Pane, Informationen und CLI unbekannt.
  Echte leere Dateien zeigen weiter 0 Byte; gemischte Auswahlen nennen nur die
  bekannte Teilmenge und zählen Dateien ohne Größenangabe ausdrücklich.

- SFTP-Momentaufnahmen öffnen HTML, HTM, SVG und XML als Text. Die übertragenen
  Bytes bleiben unverändert; aktive Webformate gelangen dadurch nicht über ihre
  ursprüngliche Endung an einen Browser.

- Eine bereits vor dem Einfügen fehlgeschlagene Kopierauswahl sperrt die alte
  Datei-/Cut-Auswahl auch fensterübergreifend. Ein neuer externer Clipboardstand
  oder ein später erfolgreicher Copy wird über die native Version wieder freigegeben.
- Lokale Capability-Probe-Roots werden schon vor Providerkontakt gegen eine
  gesetzte Testsandbox geprüft; lesende lokale Copy-Quellen bleiben möglich.
- Tastaturfokus aktiviert das zugehörige Pane ohne Fokusverlust in der Pfadleiste.
- Native Icon-Tasks lesen die zuletzt übernommene Wahl unabhängig von laufenden
  Einstellungswrites; ein langsamer fsync blockiert darüber keine Fenster mehr.
- Während eines verworfenen Sync-Startscans angeforderte Vorschauen werden für die
  weiterhin sichtbare aktuelle Ansicht nachgeholt.
- Ein DMG-Neubau ersetzt das vorherige Paket erst nach erfolgreicher
  Notarisierung, Staple und Validierung des neuen Images.

- Sync-Antworten, Konfliktrollen und Vorbereitungen bleiben an ihre ursprüngliche
  Ansicht und Endpunkte gebunden. Doppelklicks erzeugen nur eine Vorbereitung;
  verspätetes Enqueue schließt keine neu geöffnete Ansicht. Beide Wurzeln erscheinen
  vollständig im bidirektionalen Bestätigungsdialog.
- Teilfehler lesen die sichtbaren Panes frisch. Leerflächen-Kontextmenüs verwenden
  keine unmarkierte Cursordatei; langsame Hidden-Listings erhalten neuere
  Darstellungsoptionen. Informationsdialog und Planvergleich behalten sichtbaren
  Tastaturfokus.
- Fehlgeschlagene Einstellungswrites bleiben wiederholbar. Fensterschluss und
  FTP-/FTPS-, WebDAV- sowie SMB-Providercommands verlagern blockierendes I/O aus
  dem GUI-Thread; konkurrierende Profilwrites blockieren die Exit-Barriere nicht.
  Lesende Einstellungs-Commands warten ebenfalls im Hintergrund auf laufende Writes.
  Auch Konfliktprüfung, Einreihen und direkte Dateiaktionen laufen in Blocking-Workern.
- Die CLI nennt im Textlisting die einzelnen Dateien und verhindert Baselines
  innerhalb eines Rootaliases auch bei noch fehlenden Unterordnern.
- SFTP trennt Identitätsfehler von Verbindungsverlust und erreicht Alternativhosts
  innerhalb seiner bestehenden Frist. Ungültige Unicode-FTP-Zeitangaben liefern
  einen Fehler statt eines Panics.
- WebDAV unterstützt IPv6-Zertifikatsdiscovery, eng gebundene Slashweiterleitungen
  für Metadatenlesen und standardoptionale Antwortbeschreibungen. Unausführbare
  Delete-Tokens stoppen den gesamten ausgewählten Plan vor seiner ersten Mutation;
  verlorene DELETE-Antworten benötigen einen identitätsgebundenen Readback.
- Fehlgeschlagene DMG-Neubauten erhalten das bisherige Paket. Build und Prüfung
  räumen ausschließlich ihre eigenen Mountdevices auf. SFTP-Testtimeouts erlauben
  dem eigenen Runner den regulären Cleanup.

- Verspätete macOS-Iconaufträge aus parallelen Fenstern übernehmen die jüngste
  gespeicherte Auswahl, auch für das dauerhafte Finder-/Dockicon.
- Best-Effort-Kopien und Ersetzungen erhalten ihre Wiederherstellungspfade auch
  bei fehlgeschlagenem Reconnect, mehrdeutigem Listing und Inhaltsfehlern.
  Beim Ersetzen eines Ordners durch eine Datei bleibt der geparkte Altbestand
  nach einem Syncfehler in der Bilanz; ein nachweislich freier Tempname entfällt.

- Die macOS-Icon-Auswahl setzt auch das dauerhafte Findericon der App, damit das
  Dock nach dem Beenden nicht zum bisherigen Icon zurückkehrt.

- Sync-Transfers und Baum-Hashes begrenzen Quellstreams bereits beim Lesen und
  Schreiben auf die bestätigte Größe. Kürzere oder längere Quellen brechen als
  Planänderung ab, bevor ein Ziel committed wird.
- Unklare Best-Effort-Commits erhalten strukturierte Recoverypfade bis in die
  Operationsbilanz; bestätigte Renames melden weiterhin nur vorhandene Ziele.

- Einseitiger Sync bestätigt portable Rechteänderungen erst nach Datei-/Ordner-fsync
  und Rücklesen von Typ und Modus. Ein Fehler nach dem Setzen meldet unklare Wirkung
  mit dem betroffenen Zielpfad. Atomare Sync-, Quarantäne- und Rückwegfehler erhalten
  ihre Recoverypfade außerdem strukturiert über neue Fehlertexte hinweg.

- Bidirektionale Snapshots begrenzen den tatsächlichen Dateistream auf die gebundene
  Größe. Rollback erhält bewusst ausgelassene POSIX-Modi sowie unklare Teilwirkungen
  mit Recoverypfaden; Recovery bestätigt Renames erst nach erneutem fsync.
  Baseline und Laufjournal prüfen die tatsächlich ohne Symlinkfolgen geöffnete Datei.

- Lokale Dateioperationen ermitteln die Commitfähigkeit am konkreten Volume.
  macOS-exFAT kopiert nach einer an Zielroot und Mount gebundenen Bestätigung
  mit frischem Kollisionscheck und Inhalts-Readback. GUI und CLI erklären das
  verbleibende Kollisionsfenster; ein Roottausch stoppt den Commit. Verschieben
  prüft nur darstellbare POSIX-Rechte. Strikte Sync-Commits bleiben exklusiv.

- Die macOS-CLI-Einrichtung verweigert auch einen spät am Linknamen erschienenen
  Ordner oder Ordner-Symlink. Die exklusive native Anlage schreibt nicht hinein
  und lässt vorhandene Dateien und Links unverändert.

- Cargo erzwingt die Sperre gegen echte Nutzerpfade auch bei einem geerbten leeren
  oder auf `0` gesetzten Flag. Tests können sie dadurch nicht versehentlich abschalten.

- Pane-Tabs behalten beim Refresh ihre eigenen Darstellungsoptionen. Ein
  Listingfehler erhält den letzten belegten Auswahl-, Cursor- und Scrollstand;
  ein erneuter Refresh stellt ihn gegen das erfolgreiche Listing wieder her.

- Gemischte Papierkorb-Recovery benötigt die Quellordner abgeschlossener Einträge
  nicht mehr. Ein inzwischen entfernter Ordner blockiert keine unabhängigen
  offenen Claims; deren eigene Identitäts- und Symlinksperren bleiben verbindlich.

- Austausch, Rückweg und Rollback von Dateioperationen führen ihre
  Wiederherstellungspfade strukturiert weiter. Nach einem sichtbaren Rename
  wird das tatsächliche Ziel statt eines freigewordenen Tempnamens bilanziert.

- Dateioperationen erhalten frühere Wiederherstellungspfade auch bei einem
  abgelehnten Temp-Cleanup. Unklare Ordneranlage und Park-Rename führen ihre
  Ziel-/Sicherungsnamen strukturiert bis in die tatsächliche Fehlerbilanz weiter.

- Atomare Zustandsdateien erhalten Wiederherstellungspfade strukturiert über
  Anlage-, Schreib-, Rename- und Syncfehler hinweg. Auch ein unklarer lokaler
  Rename behält beide möglichen Namen und löst keinen voreiligen Temp-Cleanup aus.

- Lokal eingewechselte FIFOs blockieren weder Dateiworker noch Quarantäneaktionen.
  Der geöffnete Deskriptor muss einen regulären Datei- oder Ordnertyp belegen;
  Spezialdateien werden vor Lesen, Schreiben und Metadatenänderungen abgelehnt.

- `NativeBestEffort` bilanziert auch vollständig ausgelassene native Zielmetadaten
  und lässt planbindbare Inhalte zu. Ausgelassene Modusbits werden nicht nachträglich
  über den portablen Rechtepfad gesetzt; exFAT-Manifeste beachten die tatsächlichen
  Dateisystemfähigkeiten.

- Fehlende Ziel-Hardlinks verbergen keine Quellbeziehungen mehr: `NativeStrict`
  lehnt den Verlust vor Mutation ab, `NativeBestEffort` bilanziert ihn pfadgenau.
  Ein externer oder ausgeschlossener zweiter Name löst keine falsche Absage aus.

- Der letzte Queue-Fensterbesitzer gibt den Prozesslock ausdrücklich frei. Ein
  geerbter Dateideskriptor blockiert dadurch keine anschließende eigene Queue;
  ein Kindprozess kann den noch aktiven Elternbesitz weiterhin nicht freigeben.

- Auch das bidirektionale Laufjournal bestätigt einen späten Schreibfehler erst
  nach nachgeholtem Datei-/Eltern-fsync. Ein weiterhin unklarer Commit stoppt die
  Ausführung vor der nächsten Projektion und erhält das Recoveryjournal.

- Späte Queue-Journalfehler werden erst nach nachgeholtem Datei-/Eltern-fsync als
  erfolgreicher Commit gewertet. Pending-Aufträge überholen beim Umordnen keine
  offenen Prüf-/Recoveryeinträge mehr. Frühere Versuche bleiben auch bei alten
  WebDAV-Löschjournalen sichtbar und verlangen die manuelle Zustandsprüfung.

- Quarantäne-Parkordner binden exakte Namen vorrangig und akzeptieren eine andere
  Groß-/Kleinschreibung erst nach der tatsächlichen Anlege-Kollision. Dadurch
  funktionieren getrennte `docs`-/`Docs`-Ordner ebenso wie case-insensitives APFS.

- Nach einem unterbrochenen Quarantäne-Restore synchronisiert die Recovery die
  Ziel- und Quarantäne-Eltern vor dem Manifestabschluss. Ein Syncfehler bleibt
  `outcome_unclear`, statt einen nur sichtbaren Restore als dauerhaft zu bestätigen.

- Die CLI erhält Core-Fehlercodes beim Papierkorb-Journalfehler einschließlich
  `outcome_unclear` und der Wiederherstellungshinweise.

- Fehler beim Öffnen eines Tabs behalten ihren statischen Kontext im sicheren
  Diagnoselog, statt als allgemeiner App-Fehler gespeichert zu werden.

- Dokumentation gegen den aktuellen Produktstand abgeglichen: SFTP-Browsegrenze,
  SMB-/Quarantäne- und Sync-Richtungen, private Papierkorb-v2-Adapter, Tabs,
  Finder-Drop, Defaults sowie Build-/CLI-Verträge konsistent beschrieben. Frühere
  Test- und Implementierungsbelege bleiben ausdrücklich historisch.

- Finder-Drops erhalten den konkreten Sperrgrund oder Providerfehler gemeinsam
  mit der Zahl nicht eingereihter Quellordner. Die allgemeine Dropbilanz
  überschreibt die eigentliche Ursache nicht mehr. Der integrierte native
  Finder→FTPS-Lauf einschließlich Konflikt, Fortschritt und Abbruch ist belegt.

- Linux verwendet den privaten v2-Papierkorb für XDG-Home- und Volume-Layouts.
  Der tatsächliche Datenhome-Mount entscheidet die Zielwahl; Info-Dateien werden
  vollständig vor den Daten veröffentlicht. Vorhandene ACLs, Metadaten und fremde
  Kollisionen bleiben erhalten, unsichere Gruppen beginnen keine Dateiwirkung.
  Verweigerte gemeinsame Trash-Layouts versuchen den sicheren XDG-Fallback; neue
  CLI-Konfigurationsordner werden unabhängig von der Umask mit `0700` angelegt.

- macOS verschiebt Dateien direkt in einen privaten Claim-Bereich und nutzt
  Journal v2 für Home- und Volume-Papierkörbe. Öffentliche Nachfolger können
  nicht mehr als Transferquelle dienen; vorhandene Trash-/Datei-ACLs bleiben
  erhalten. Die Test-Sandbox besitzt einen eigenen künstlichen Papierkorb.

- Papierkorb-Recovery setzt alte öffentliche Claims nicht mehr automatisch zurück.
  V1 verlangt einen heute belegten privaten Quellordner und hält seine Journalwrites
  am geöffneten Deskriptor. V2 bindet Marker, Mounts, Rechte und Sandbox frisch;
  ungeklärte Transfers und fremde Reste bleiben erhalten. FIFO-Journale blockieren
  das Lesen nicht mehr.

- SFTP-Verbindung, SFTP-Listing und Remote-Dateiöffnung laufen im Hintergrund.
  Remote-Dateiöffnung zeigt einen Abbruchweg über den eigenen Fenstertoken;
  ihr Abschluss kann die Anzeige des nächsten Queue-Laufs nicht zurücksetzen.

- Lokale Metadatenfähigkeiten folgen dem tatsächlich gehaltenen Mount.
  exFAT/FAT und unbekannte Dateisysteme erhalten keine falschen nativen Zusagen;
  nur lesbare Mounts melden keine exakten Schreibfähigkeiten. Die Vorschau bleibt
  ohne mutierende Probe; native Kopien wurden auf APFS und ext4 zurückgelesen.

- Baseline-Locks enden ausdrücklich mit ihrem ursprünglichen Besitzer.
  Bei parallelen CLI-Starts verlängert ein vor exec geerbter Dateideskriptor
  den beendeten Lock nicht mehr bis in den anschließenden Recovery-Schritt.

- SMB-Capability-Proben prüfen nach Erfolg und Fehler sowohl den gewählten
  Ordner als auch den gebundenen Profilroot vollständig auf Probenreste.
  Fehlende Listings und Sitzungswechsel verhindern die neue Fähigkeitsvergabe.

- FTP/FTPS-Capability-Proben lesen Auswahl- und Profilroot auch nach Fehlern
  vollständig zurück. Getrennte, identitätsgeprüfte Lesesitzungen erhalten
  den Nachweis auch bei versetztem Kontrollkanal; unbestätigte oder falsch
  platzierte Probenreste sperren die neue Fähigkeitsvergabe.

- Linux-Mountlisten begrenzen die Speicherabfrage zeitlich und warten nach dem
  Abbruch nicht auf hängende Hilfsprozesse. Abfragefehler erhalten den Mount
  sichtbar mit unbekannter Größe; andere Volumes bleiben verfügbar.

- Bidirektionaler Sync erhält die Wiederherstellungspfade früherer Projektionen
  auch nach Abbruch, späteren Prüf- und Journalfehlern. Die Queue speichert alle
  Pfade gemeinsam mit dem Abschlusszustand und bewahrt sie bei Speicherfehlern.

- Sync erhält Wiederherstellungspfade auch nach späteren Fehlern und bei einem
  zusätzlich fehlgeschlagenen Audit. Provider-Queues speichern diese Pfade
  gemeinsam mit dem Erfolgs- oder Abbruchzustand im terminalen Journalcommit.

- Mirror setzt endgültige Ordnerrechte und native Metadaten erst nach den
  Quarantäne-Verschiebungen. Native Update-/Mirror-Pläne berücksichtigen außerdem
  die Zeitstempeländerung durch eigene Kinddateien, auch bei anfangs gleichen
  Ordner-Metadaten; fremde Änderungen bleiben vor Entfernungen gesperrt.
- FTP/FTPS- und SMB-Capability-Proben entfernen keine unbekannten Einträge mehr
  beim Aufräumen. SMB liest außerdem gültige Zeitstempel vor 1970 korrekt zurück.
- Der Papierkorb prüft die Identität nach dem letzten Rename und erhält bei
  Abweichungen ein blockierendes Recovery-Journal. Sehr lange Namen bleiben mit
  Kollisionssuffix und XDG-Infodatei innerhalb der Dateisystemgrenze. Das damalige
  v1-Claim-Rennen ist inzwischen durch den privaten v2-Schreibweg geschlossen;
  der Papierkorbvertrag bewahrt den historischen Befund.
- Die CLI erhält bei einem erneut gespeicherten Dateioperationsfehler auch die
  Liste bekannter Verzeichnissicherungen.
- Ein Queue-Journalschreibfehler erzeugt keine unbegrenzten Worker-Neustarts mehr.
  Fehler beim Zurücksetzen verbrauchter Remote-Vorbereitungen erhalten ihren
  Fehlercode und zeigen den nötigen Neustart mit anschließender Neuprüfung an.
- Gleichzeitige Einstellungsabrufe blockieren den Dialog nicht mehr. Nach einem
  erfolgreichen Auswerfen werden betroffene Panes sofort entwertet; ein verspäteter
  Rückfall überschreibt keine neuere Navigation.
- Finder-Drops zählen bereits eingereihte Kopien auch nach einer Zielnavigation
  korrekt; nur die tatsächlich nicht eingereihten Quellordner erscheinen als offen.
- Vorschau- und Diff-Caches in der Testsandbox lehnen vorhandene Symlinks in ihren
  Zwischenverzeichnissen ab, bevor Momentaufnahmen außerhalb des Testbaums entstehen.
- Private Arbeitsordner werden auch bei frühen Kopier-/Syncfehlern, Abbruch und
  fehlendem Hardlink-Anker abgeschlossen. Unsichere Reste und unklare Commitjournale
  bleiben mit Wiederherstellungspfad erhalten. Die betroffenen Queue-Fehlerhüllen
  und bidirektionalen Abbrüche erhalten diese Pfade strukturiert.
- Dialoge erben keine fremden Breitenklassen oder Eingabehandler mehr. Eine
  verspätete Altwertmigration aus einem zweiten Fenster überschreibt keine neuere
  Einstellung und erzeugt keinen zusätzlichen Commit.
- Der macOS-Testlauncher kodiert Sandbox-Pfade korrekt im Plist und baut für jeden
  Start ein frisches Testbundle, sodass alte Links keine fremden Dateien beschreiben.
- Eine fehlgeschlagene macOS-Installationsprüfung stellt den erhaltenen Altstand
  wieder her beziehungsweise nimmt die Erstinstallation zurück. Die Quell- und
  Staging-Prüfungen finden vor dem atomaren Austausch statt.

- Lokale Übertragungen können eigene Temps in einem nachweislich privaten,
  descriptor-gebundenen Arbeitsbereich sicher bereinigen. Die Ziel-ACL-Vererbung
  bleibt erhalten; Änderungen am Ziel-Elternordner stoppen den vorbereiteten Lauf.
- Nach bestätigter Ersetzung erscheinen erhaltene Verzeichnissicherungen mit Pfad
  neben dem erfolgreichen Ergebnis. Queue, CLI und Abschlussanzeige unterscheiden
  diese Sicherungen von unklaren Übertragungen. Ein Crashjournal erhält lokale
  Zwischenstände für die Prüfung nach einem Neustart und löscht sie nicht blind.

### Hinzugefügt

- Dateien und Mehrfachauswahlen lassen sich zwischen den Panes zum Kopieren ziehen.
  Finder-Drops kopieren in den angezeigten Zielordner, auch bei FTP/FTPS, SFTP,
  WebDAV und SMB. Beide Wege verwenden die vorhandene Konfliktprüfung und Queue;
  eine markierte Pane zeigt das Ziel, eine Navigation entwertet alte Auswahlen.

- Ein globaler Einstellungsdialog bündelt Theme, sechs wählbare Himmels-Icons,
  Größeneinheiten, automatische lokale Ordnergrößen, LAN-Suche,
  Sitzungswiederherstellung und das bevorzugte externe Diff-Werkzeug. Das blaue
  Icon bleibt Vorgabe; alle Varianten werden ohne einfarbige Unterlage direkt aus
  ihrem Himmelbild abgerundet.
- Die optionale Sitzungswiederherstellung öffnet Fenster mit ihrer Position, Größe,
  aktiven Pane und beiden Pane-Zielen erneut. Remote-Ziele verwenden beim Neustart
  die aktuelle Profilrevision und durchlaufen den normalen Verbindungsaufbau.
- Zwei ausgewählte Dateien lassen sich über geprüfte, rückgelesene
  Momentaufnahmen in Xcode FileMerge, BBEdit oder Fastra vergleichen. Fastra
  verlangt einen Helfer mit bestätigtem Protokoll 1; Doppeldecker wertet seine
  Übergabebestätigung und Fehler aus. Prüfung und Übergabe laufen im Hintergrund.
- Die markierte GUI-Testsandbox nimmt auch Vorschau- und Diff-Momentaufnahmen auf,
  statt beim Test den normalen App-Cache zu verwenden.

### Geändert

- Ungenutzte SFTP-Proben-/mkdir-Testmaschinerie und ausschließlich davon abhängige
  Mocktests entfernt. Die bestehenden Schutztests prüfen produktive Funktionen;
  daraus entstehen keine neuen SFTP-Schreibfähigkeiten.

- Der Papierkorbvertrag legt private Claims, Journal v2 und die getrennte
  macOS-/XDG-Umsetzung fest. Der heutige öffentliche Claim bleibt bis zur
  Implementierung und Plattformprüfung als offene Sicherheitsgrenze benannt.

- Vollständig grüne Headless-Gates bereinigen den Debug-Buildcache bereits oberhalb
  von 6 GiB statt 12 GiB. Teilgate-Läufe und Release-Artefakte bleiben erhalten.
- Wartet ein Auftrag auf die Bestätigung einer Best-Effort-Ausführungsklasse, sagt
  die Meldung das jetzt in Alltagssprache: zuerst die Wirkung („Nichts
  überschrieben"), dann den Grund — dieses Dateisystem kann die vorhandene Fassung
  beim Ersetzen nicht sicher aufbewahren —, und das Fachwort steht in Klammern
  dahinter. Der Fehlerbalken bietet zusätzlich einen Knopf „Queue öffnen"; die
  Wartestelle hing bisher allein an einem kleinen Symbol im Seitenleistenkopf. Die
  Oberfläche erkennt den Fall am neuen stabilen Code `queue_confirmation_required`
  und nie am deutschen Text.
- Dieselbe Sprache gilt jetzt für die beiden Providerfälle: „Nichts übertragen"
  nennt zusätzlich, welche Wirkung der Server nicht mehr zusichert — neue Dateien
  anlegen, vorhandene ersetzen oder beides —, und „Nichts verschoben" sagt, dass
  die Quelldatei nach dem Kopieren nicht nachweislich entfernt werden kann. Die
  vom Provider gemeldeten Grenzen bleiben in beiden Fällen wörtlich angehängt; nur
  sie sagen, woran es konkret liegt.

### Behoben

- Nach einem lokalen Typkonflikt-Ersetzen wird eine beim Aufräumen erhaltene
  Sicherung auch in der strukturierten Fehlerbilanz genannt. Beide Richtungen
  behalten das Altziel; der weiterhin unklare Aufräumausgang bleibt sichtbar.
- Temporäre Nachbarnamen enthalten zusätzlich acht Zufallsbytes. Auch bei langen
  UTF-8-Zielnamen bleibt das Namenslimit eingehalten. Die exklusive Anlage bleibt
  die Kollisionsgrenze; das lokale Löschrennen ist weiterhin offen.

- Scheitert das Speichern eines Queue-Fortschritts nach einer möglichen
  Dateiwirkung, meldet Doppeldecker den Ausgang jetzt durchgängig als unklar und
  bietet keinen gefährlichen Wiederholversuch an. Das gilt für lokale und
  entfernte Datei-, Lösch-, Sync- und bidirektionale Läufe.
- Ein beschädigtes Journal eines Nebenfensters wird beim Sitzungsstart nicht mehr
  unter dem Namen des Hauptfensters wiederhergestellt. Einstellungs-, Icon- und
  Fensterbaufehler erscheinen außerdem als App-Fehler, ohne einen Queue-Fehler
  oder ein dauerhaftes Fehler-Badge vorzutäuschen.
- Fortschrittsmeldungen unterscheiden jetzt die Übertragung von Metadaten, das
  reversible Sichern in der Quarantäne und ein endgültiges Löschen.
- Systembefehle halten ihre harte Frist auch dann ein, wenn ein Nachkomme die
  Ausgabepipe offen hält; große oder dauerhafte Ausgabe kann die Fristprüfung
  ebenfalls nicht mehr verdrängen.
- Ein Finder-Drop mit Dateien aus mehreren Ordnern reiht alle Ordner ein, auch
  wenn die Queue die erste Gruppe während eines Konfliktdialogs abschließt;
  Dateien aus dem Zielordner selbst werden übersprungen, statt den Rest still zu
  verwerfen. Nicht eingereihte Quellordner werden sichtbar gemeldet.
- Ein fehlgeschlagenes App-Icon bricht weder den App-Start noch das Speichern von
  Einstellungen ab; eine gespeicherte Einstellung meldet nicht mehr
  „fehlgeschlagen“, wenn nur ein Fenster den Rundruf nicht mehr empfangen konnte.
- Scheitert die einmalige Übernahme der alten Browser-Einstellungen, startet die
  App trotzdem mit Panes und meldet den Fehler, statt leer stehen zu bleiben.
- Eine auf einem inzwischen abgesteckten Monitor gespeicherte Fensterposition wird
  nicht mehr übernommen; das Fenster bleibt sichtbar erreichbar.
- Die gespeicherte aktive Pane geht beim Start nicht mehr verloren, wenn die
  ersten Listings länger dauern.
- Im Einstellungsdialog bleibt der Tastaturfokus nach jeder Änderung erhalten.
- Der Start des Dateivergleichs wartet nicht mehr bis zu zwei Sekunden auf die
  Fastra-Fähigkeitsabfrage, und ein Helfer mit großer Ausgabe läuft nicht mehr in
  den Timeout. Beendete Helferprozesse werden abgeräumt.
- Das Speichern des Sitzungsstands blockiert den Fenster-Thread nicht mehr.

- Fensterbezogene Tauri-Ereignisse verwenden jetzt auf beiden Seiten ausdrücklich
  das aktuelle Webview als Ziel. Die bisherige JavaScript-Brücke registrierte in
  jedem geöffneten Fenster einen prozessweiten Listener; ein Menüklick startete
  den Dateivergleich deshalb in allen Fenstern.
- Unter Linux ließ sich die App überhaupt nicht mehr übersetzen: Die Hilfsfunktion
  mit harter Frist in `src-tauri/src/system_mount_commands.rs` trug
  `#[cfg(target_os = "macos")]`, obwohl der Auswurf sie auf beiden Plattformen
  aufruft — auf Linux für `gio mount -u`. Der Aufruf fand die Funktion dort nicht
  (E0425). Sie ist plattformneutral und gilt jetzt für beide Systeme. Aufgefallen
  beim ersten `cargo check -p doppeldecker` unter Linux; der Gegenlauf im Container
  brauchte dafür die WebKitGTK-4.1-Dev-Bibliotheken.
- Unter Linux brach `cargo test -p doppeldecker` beim Übersetzen ab, bevor ein
  einziger Test lief: `src-tauri/src/browser_commands.rs` definierte zwei
  gleichnamige Testmodule nebeneinander — eines für `test`, eines für
  `all(test, target_os = "linux")`. Auf macOS ist immer nur eines davon aktiv, unter
  Linux beide, und zwei gleichnamige Geschwistermodule sind ein Namenskonflikt
  (E0428). Der plattformabhängige Test hängt jetzt als einzelner Test im gemeinsamen
  Modul. Ein neuer Tooltest hält die Eindeutigkeit für alle Rust-Dateien fest, ohne
  einen Linux-Build zu brauchen; gleichnamige Module mit sich ausschließenden
  `target_os`-Zweigen bleiben erlaubt.
- Die Zusammenfassung von Capability-Probe und ihrem Cleanup warf den
  maschinenlesbaren `ErrorCode` weg: Aus `connection_lost` oder `permission_denied`
  wurde ein rohes `invalid_data`, und bei einem unklaren Ausgang verschwand die
  zweite Ursache ganz. CLI und Oberfläche wählten dadurch Reconnect und
  Nutzerhinweis nach einem Code, der die Lage nicht mehr beschrieb. Der
  zusammengesetzte Fehler übernimmt jetzt Kind und Code des Fehlers, der die
  Reaktion bestimmt, und nennt beide Ursachen im Text. WebDAV nutzt dieselbe
  Funktion wie die anderen Provider statt eines eigenen Zweitabdrucks.
- Die Schranke, die das Auditjournal aus den synchronisierten Roots hält, verglich
  kanonisierte Roots mit einem nur teilweise aufgelösten Auditpfad. Fehlte der
  Unterordner noch — `append_audit` legt ihn später selbst an —, blieb der Pfad
  unaufgelöst, und ein über `/tmp` angegebenes Audit rutschte am Root
  `/private/tmp/…` vorbei in den Zielbaum. Der nächste Mirror-Lauf hätte es als
  überzähligen Eintrag in die Quarantäne verschoben und danach neu angelegt, die
  append-only Historie also auf zwei Dateien verteilt. Die Prüfung löst jetzt den
  vorhandenen Teil des Pfades auf und hängt den fehlenden Rest wieder an.
- `tools/run-smb-e2e.sh` lud die Rustup-Umgebung aus `$HOME/.cargo/env` erst kurz
  vor dem Testlauf, prüfte den erwarteten Testnamen aber schon vorher mit
  `cargo test -- --list`. In einer Umgebung ohne Cargo im PATH scheiterte damit
  genau diese Prüfung und meldete null Treffer, obwohl der Testlauf danach
  funktioniert hätte. Der Import steht jetzt vor der ersten Cargo-Nutzung, und ein
  weiterhin fehlendes Cargo wird als solches gemeldet.
- Auf einem case-sensitiven Dateisystem stufte der Rückleseweg nach einem Ersetzen
  eine vollzogene, korrekte Wirkung als unklaren Ausgang ein, sobald im Zielordner
  ein Name mit anderer Groß-/Kleinschreibung lag. Der lokale Provider sagt jetzt
  ausdrücklich zu, dass ein Name genau ein Objekt bezeichnet; nur dann gilt ein
  byte-exakter Treffer als Beleg. Für Remote-Provider bleibt die konservative
  Schranke unverändert — ein Server kann beim Schreiben normalisieren und trotzdem
  beide Schreibweisen listen. Aufgefallen erst im Linux-Gegenlauf.

### Geändert

- Vier verschachtelte Tauri-Request-Strukturen haben den von den Projektregeln
  verlangten Deserialisierungs-Regressionstest bekommen: Provider-Löschen,
  Provider-Umbenennen, Remote-Snapshot und Provider-Sync. Tauri benennt
  verschachtelte JSON-Felder nicht zuverlässig so um wie Top-Level-Argumente; genau
  daran scheiterte das Remote-Löschen 2026-07-28 erst zur Laufzeit. Mitgeprüft wird
  jetzt auch `mtime_tolerance` als snake_case-Feld in einer camelCase-Hülle.

### Behoben

- Der Quelltext-Guard, der jede pfadbasierte SFTP-Lesewirkung als solche
  ausgewiesen sehen will, prüft `root_identity` wieder wirklich. Sein Endmarker
  existierte im Code gar nicht mehr und fand sich nur in seinem eigenen
  Testliteral; das Schnittfenster war dadurch mehrere hundert Zeilen lang und der
  Assert griff nur zufällig.
- Der SFTP-Verbindungsaufbau hat jetzt dieselbe harte Frist von 15 Sekunden wie
  FTPS, WebDAV und SMB — sie umfasst TCP-Verbindung, Schlüsseltausch, Anmeldung und
  SFTP-Init. Ein vertippter oder gefilterter Host ließ die Oberfläche zuvor bis zum
  TCP-Default des Systems stehen, auf macOS rund 75 Sekunden, und ein Reconnect
  nach einem Netzwechsel konnte das verdoppeln. „Hostkey ermitteln“ läuft
  zusätzlich außerhalb des Fenster-Threads.
- Das Auswerfen eines Volumes blockiert das Fenster nicht mehr. Es lief synchron auf
  dem Fenster-Thread, fragte dabei zweimal alle Volumes ab (je bis zu eine Sekunde
  `diskutil info`) und startete den Aushängebefehl ganz ohne Frist. Beides liegt
  jetzt außerhalb des Fenster-Threads, und der Aushängebefehl bekommt 20 Sekunden;
  läuft er darüber, endet der Vorgang als unklarer Ausgang statt als Stillstand.
- Der Transporthinweis einer FTP-/FTPS-Pane erscheint einmal je Profil und
  Transportstufe, wie schon bei SFTP. Zuvor feuerte er bei jedem Listing und
  ersetzte die gerade gemeldete Dateiaktion im Dock — „Ordner angelegt“ oder die
  Fehlermeldung eines gescheiterten Umbenennens war sofort wieder weg.
- Das Ende eines Laufs hebt keine fachlich gesetzte Sperre mehr auf. Der
  Quarantäne- und der Realmodus-Knopf blieben nach einem Dateioperationsfehler
  klickbar, während Zähler und Titel weiter „gesperrt“ sagten.
- Die Quarantäne-Meldung nennt für FTPS und WebDAV nicht mehr einen
  SFTP-spezifischen Grund und behauptet dort auch nicht mehr, Dateidownload und
  Sync seien gesperrt.
- Die Sync-Ansicht hält ihre beiden Kernbedingungen jetzt in beide Richtungen:
  „Verwaiste in Quarantäne verschieben“ und „Schnell (Größe und Datum)“ führen
  nicht mehr in eine Kombination, die der Kern ablehnt und die jede Vorschau
  scheitern lässt.
- Der FTPS-Editor senkt eine ausdrücklich gewählte Zertifikatsprüfung nicht mehr
  still ab, wenn der Host eine IP-Adresse ist. Die Wahl bleibt stehen; der
  Hinweistext benennt, dass eine öffentliche Namensprüfung so nicht gelingen kann.
- Scheitert nach einem erfolgreichen Auswerfen nur der Pane-Rückfall, meldet die
  Oberfläche das getrennt, statt einen gescheiterten Auswurf zu behaupten.
- Eine verletzte Metadaten-Planbindung meldet wieder `plan_drift` statt eines
  unspezifischen `io`. Derselbe Anlass — geänderte Quellmetadaten zwischen Vorschau
  und Ausführung — trug je nach Zweig zwei verschiedene Maschinencodes; CLI und
  Oberfläche konnten deshalb nicht durchgehend „neue Vorschau erstellen“ anbieten.
- Ein abgelehntes WebDAV-MOVE meldet nur noch dann eine Namenskollision, wenn am
  Zielnamen wirklich etwas liegt. HTTP 412 kann laut RFC 4918 auch heißen, dass die
  Quelle oder ein Elternordner sich seit der Vorschau geändert hat; die Oberfläche
  meldete das bisher als „Der Zielname existiert bereits“, worauf der Nutzer einen
  anderen Namen wählte und die fremde Datei umbenannte. Ein frischer Readback des
  Ziels unterscheidet die Fälle; Plan-Drift kommt jetzt als solche an.
- Die Restore-Probe des lokalen Realmodus räumt ihre Probenamen auch dann
  nachvollziehbar auf, wenn die Nachfrage nach ihrem Verbleib scheitert. `exists()`
  lieferte bei jedem Fehler `false`; ein Probename konnte dadurch unbemerkt im
  echten Zielordner liegen bleiben, obwohl der Kommentar daneben versprach, dass
  jeder Cleanup-Fehler sichtbar bleibt.
- Eine Kopie bricht nicht mehr ab, nur weil im Zielordner ein Name mit anderer
  Groß-/Kleinschreibung oder anderer Unicode-Normalform liegt. Auf einem
  case-sensitiven Dateisystem sind `datei.txt` und `Datei.txt` zwei verschiedene
  Einträge; die konservative Namensäquivalenz ist für normalisierende Server
  gedacht und griff auch lokal. Betroffen war jeweils die gesamte Auswahl, bevor
  irgendetwas passierte. Ein byte-exakter Treffer hat jetzt Vorrang.
- Dateien mit sehr langem Namen lassen sich wieder kopieren. Der interne
  Nachbarname ist rund dreißig Bytes länger als der Zielname und sprengte bei einem
  völlig legalen 225-Byte-Namen das Limit von 255; der Fehler war kein
  Namenskonflikt, also brach die Suche nach einem freien Namen ab und die Datei
  wurde nie kopiert.
- Ein nicht sicher dekodierbarer Tempname beendet den Best-Effort-Commit jetzt
  sauber, statt mit einem leeren Ersatznamen weiterzulaufen. Der Readback prüft
  „Temp verschwunden“ über den Namen; der leere Name kommt in einem sicheren
  Listing nie vor, sodass ein nicht gewirktes Rename als Erfolg gegolten hätte.
- Der Sync plant keine Rechteänderung mehr für ein Ziel, das POSIX-Rechte gar nicht
  schreiben kann. Alle vier Remote-Provider melden genau das, und SMB, WebDAV sowie
  FTPS liefern überhaupt keinen Modus — gegen einen lokalen Quellmodus war das immer
  ein Unterschied. Die Vorschau versprach dann eine Anpassung, die nie stattfand und
  nirgends bilanziert wurde, der Folgeplan wurde nie leer, und ein Mirror-Zweitscan
  brach nach den bereits geschriebenen Kopien ab, weil er die unerledigte Aktion als
  fremde Änderung las: Die Entfernungen liefen gar nicht mehr.
- Die Test-Sandbox lässt sich nicht mehr in einem Git-Repository öffnen, dessen
  `.git` als toter Symlink oder hinter einem nicht durchsuchbaren Ordner liegt.
  `exists()` lieferte dort `false` und damit „kein Repository“. Ein nicht
  auflösbares `HOME` sperrt die Sandbox jetzt ebenfalls, statt die Prüfung
  stillschweigend zu überspringen.
- FTP/FTPS beweist die Abwesenheit eines gelöschten Pfads jetzt über das
  vollständige Listing seines Elternordners statt über einen Statfehler. FTP
  beantwortet mit 550 sowohl „gibt es nicht“ als auch „darf ich nicht“ (RFC 959
  §4.2.1): Ein Konto, das im Elternordner löschen, ihn aber nicht listen darf —
  eine verbreitete ACL für Ablageordner —, bekam die Löschung als geprüft gemeldet,
  ohne dass je ein Listing gesehen wurde. Der Code 450 („Datei belegt“) gilt
  außerdem nicht mehr als Abwesenheit; er ist ausdrücklich vorübergehend.
- Eine späte Namenskollision beim FTP-/FTPS-Umbenennen erreicht wieder den
  Kollisionsdialog. Der Server meldet sie als 550 oder 553, was bisher als
  unspezifische Pfadablehnung ankam; ein frisches Elternlisting unterscheidet die
  Fälle jetzt positiv.
- SMB meldet eine Löschung erst als erfolgreich, wenn ein Readback die Abwesenheit
  des Pfads bestätigt. SMB löscht ein mit `FILE_DELETE_ON_CLOSE` geöffnetes Objekt
  erst beim Schließen des letzten Handles, und Doppeldecker öffnet selbst mit
  `FILE_SHARE_DELETE`: Hält ein anderer Client ein Handle, antwortete CLOSE mit
  Erfolg, während die Datei sichtbar blieb — ein Verschieben meldete sich dann als
  abgeschlossen, obwohl die Quelle noch stand. Ein unbestätigter Readback endet
  jetzt als unklarer Ausgang.
- `SmbVfs::remove_file` entfernt keine echten Verzeichnisse mehr. Der zweite
  Löschversuch mit `FILE_DIRECTORY_FILE` gilt nur noch für einen Symlink auf ein
  Verzeichnis — der ist selbst ein Verzeichniseintrag und lässt sich anders nicht
  öffnen. Zuvor griff er unbedingt: Wurde der Name im Move-Quellabbau zwischen
  Prüfung und Entfernung gegen ein leeres Verzeichnis getauscht, löschte SMB als
  einziger Provider das Verzeichnis, statt mit einem Typfehler zu stoppen.
- Keine Capability-Probe verschweigt mehr ihren Cleanup-Fehler. Vier FTPS- und
  zwei SMB-Proben benutzten `result?; cleanup?;` — scheiterte die Probe selbst,
  kehrte schon das erste `?` zurück und der Cleanup-Fehler fiel weg, samt dem
  einzigen Hinweis darauf, dass unter dem gewählten Ordner ein Probenordner
  liegengeblieben ist. Alle Proben fassen beides jetzt über einen gemeinsamen
  Helfer zusammen, der den Recoverypfad nennt und einem unklaren Ausgang Vorrang
  gibt.
- SMB-, FTPS- und WebDAV-Panes etikettieren ihr Listing nicht mehr mit einer
  fremden Sitzungsgeneration. Listing und Generation stammen jetzt aus demselben
  gehaltenen Providerzustand; zuvor gab der Listingaufruf die Sperre frei, und ein
  Reconnect aus einem anderen Fenster konnte die Generation dazwischen hochzählen.
  Die Einträge der alten Sitzung trugen dann das Etikett der neuen — und genau
  daran hängt die Driftbindung von Pane, Ordnergröße und Folgeoperation. Für SFTP
  war das bereits gelöst; ein Quelltext-Guard hält es jetzt für alle vier Provider
  fest.
- `doppeldecker-cli sync` überholt keine wartende oder pausierte Queue mehr. Es war
  der einzige mutierende CLI-Befehl ohne diese Schranke — `copy`, `move`, `delete`
  und `bidirectional` scheitern über den Queue-Vertrag, `real` über dieselbe Prüfung
  — und veränderte damit genau die Roots und Quarantänestände, die der wartende
  Auftrag beim Claim erwartet.
- Die Auditdatei eines Real- oder Provider-Sync-Laufs darf nicht mehr innerhalb der
  synchronisierten Roots liegen. Lag sie im Ziel, verschob Mirror sie im zweiten
  Lauf als überzähligen Eintrag in die Quarantäne, und der Abschlussdatensatz legte
  sie neu an: Die Audithistorie lag danach zerrissen in zwei Dateien. Die Baseline
  eines bidirektionalen Laufs hatte diese Schranke längst.
- Die Textbilanz von `copy` und `move` nennt übersprungene und ausgelassene
  Einträge. Ein `move` über einen Symlink meldete zuvor „Fertig: 0 Dateien, 0 Bytes“
  mit Exitcode 0, obwohl nichts verschoben wurde; das JSONL war schon vollständig.
- Das Ereignis `file_preview` führt die verlangten Bestätigungsphrasen und beide
  Profil-IDs mit. Ein Controller im JSON-Modus musste sie zuvor aus dem deutschen
  stderr-Text lesen oder aus den eigenen Argumenten rekonstruieren.
- Der Sandbox-Selbsttest der CLI-Tests durchsucht `tests/` und `src/` jetzt
  rekursiv. Ausgerechnet das von seinem eigenen Kommentar empfohlene
  `tests/common/mod.rs` lag im toten Winkel: Ein Verzeichnis hat keine Dateiendung
  und wurde vom Endungsfilter samt Inhalt übersprungen.
- Das Gate `fuzz-check` übersetzt jetzt auch die Tests der Fuzz-Kiste. Ohne
  `--all-targets` wählte `cargo check` nur Lib und Bins, und
  `fuzz/tests/seed_corpus.rs` wurde von keinem Gate übersetzt — eine Schemaänderung
  an Baseline, Laufjournal oder Metadatenmanifest hätte die versionierten Seeds
  ungültig gemacht, ohne dass etwas rot geworden wäre. Nachgestellt und belegt.
- `python3 tools/tests/test_check_gates.py` sammelt wieder alle eigenen Tests ein.
  `unittest.main()` stand in der Mitte der Datei; der Direktaufruf beendete sich
  dort und übersprang drei Tests, die über das Gate ganz normal liefen.
- Drei Release-Gates können ihren Fehlerfall jetzt wirklich melden. Die
  Team-Identifier-Prüfung in `tools/verify-macos-release.sh` lief über `grep -qv`
  und war damit schon wahr, sobald irgendeine Zeile der codesign-Ausgabe die
  Zeichenkette nicht enthielt. `tools/run-smb-e2e.sh` verließ sich auf den Exitcode
  von `cargo test <filter>`, der auch bei null Treffern 0 ist — ein umbenannter Test
  hätte einen grünen SMB-E2E-Lauf gemeldet, ohne dass etwas gegen Samba lief; der
  Runner zählt jetzt vorab genau einen exakten Treffer und fährt mit `--exact`.
  `pnpm release:update-manifest` war überhaupt nicht aufrufbar, weil npm zusätzliche
  Argumente ans Ende der Kette hängt und `--pub-date` damit nie beim Manifestskript
  ankam; die Veröffentlichungszeit kommt jetzt zusätzlich über
  `DOPPELDECKER_PUB_DATE` und wird weiterhin nie geraten.
- `tools/tests/test_fleet_rules.py` belegt die beiden Fleet-Regeln nicht mehr über
  ein beliebiges Textvorkommen: Kommentarzeilen zählen nicht mehr als Beleg, und
  eine Prüfung, die ihren eigenen Fehlschlag mit `|| true` entschärft, lässt den
  Test fehlschlagen.
- `tools/run-smb-e2e.sh` räumt seinen Wegwerfbaum auch dann auf, wenn der Lauf vor
  dem Serverstart abbricht, und beendet beim Aufräumen die ganze Prozessgruppe des
  Testservers statt nur seinen Leiter — zuvor konnten geforkte Verbindungskinder
  weiterarbeiten, während `rm -rf` den Share unter ihnen entfernte.
- Das Quarantäne-Badge zeigt kein Fehlerzeichen mehr, nur weil die Fensterqueue
  gerade belegt ist. Der Zähler behält seinen Stand, wenn das Backend mit dem Code
  `busy` antwortet; zuvor blieb das „!“ stehen, bis zufällig ein Sync endete — der
  Abschluss einer Dateioperation räumte es nicht weg.
- Ereignisse der App-Schicht erreichen nur noch das Fenster, das sie verursacht hat.
  `Emitter::emit` ist in Tauri v2 auch auf einem Fenster ein Rundruf an jeden
  Webview; 73 Sendungen liefen dadurch an alle Fenster. Sichtbar wurde das als
  doppelt ausgeführter Menübefehl (ein „In den Papierkorb“ in Fenster A löschte die
  Auswahl von Fenster B mit), als fremder Fortschrittsbalken samt „Fertig“-Meldung
  und vor allem als vorzeitige Freigabe: Der Abschluss eines Laufs in Fenster A
  entsperrte die Oberfläche eines noch arbeitenden Fensters B und verwarf dort den
  Verweis auf dessen eigene Realmodus-Vorbereitung. Alle Sendungen gehen jetzt über
  `emit_to_window`; einziger bewusster Rundruf bleibt die prozessweite
  Verbindungsänderung. Ein Quelltext-Guard hält das fest.
- Ein bidirektionaler Lauf, dessen Vorbereitung schon vor dem ersten Laufjournal
  scheitert, blockiert die Fensterqueue nicht mehr dauerhaft. Ohne Laufjournal gibt
  es keine mögliche Teilwirkung; der Eintrag bekommt deshalb `interrupted` statt
  `outcome_unclear` und bleibt ganz normal wiederholbar. Zuvor lehnte „Neu prüfen“
  den Code ab, `acknowledge_unclear_outcome` schließt bidirektionale Operationen
  aus, und der nicht-terminale Eintrag hielt jede weitere Queue-Operation dieses
  Fensters an — ohne Ausweg in der Anwendung. Ein bereits so geschriebenes Journal
  wird beim Öffnen repariert.
- Ob ein migrierter WebDAV-Löschaltstand gültig ist, entscheidet jetzt sein
  maschinenlesbarer Fehlercode statt seines deutschen Fehlertexts. Eine bloße
  Umformulierung der Prosakonstante hätte sonst das Öffnen des gesamten
  Queue-Journals scheitern lassen — für jedes Fenster und die CLI, samt aller
  übrigen Einträge.
- Die Papierkorb-Bilanz meldet einen Eintrag nur dann als fehlgeschlagen, wenn das
  auch belegt ist. Lässt sich der Zielname nach einem Fehler nicht abfragen, zählt
  der Eintrag als „Ausgang unklar“ statt als Fehlschlag; `trashed + failed +
  unclear` ergibt die Gesamtzahl. Zuvor meldete ein Stat-Fehler nach erfolgreicher
  Verschiebung „Löschen fehlgeschlagen“.
- Quarantäneläufe aus der Zeit vor der Zeitstempelbindung bleiben automatisch
  wiederherstellbar. Die Zeitstempel wanderten in eine eigene Claim-Digestversion 3;
  Version 2 wird byteweise unverändert weiterberechnet und in Claim-Prüfung und
  Restore getrennt behandelt. Zuvor meldete jeder Restore eines vorhandenen
  Manifests `plan_drift`, obwohl Liste und GUI den Eintrag als automatisch
  wiederherstellbar auswiesen — die Datei blieb in der Quarantäne liegen.
- Schemawechsel einer bidirektionalen FTPS-/WebDAV-Paarbindung bewahren jetzt die
  festen linken und rechten Baseline-Rollen auch dann, wenn sich die kanonische
  Digestreihenfolge umkehrt. WebDAV rekonstruiert dabei auch die tatsächlich
  ausgelieferten Schema-1-Digests für einen fehlenden oder leeren `Server`-Header.
- Queue-Journalversion 18 stuft einen nach Abbruch erneut eingereihten
  Remote-Löschauftrag nicht mehr als sicher ungestarteten Altplan ein: Eine bereits
  mögliche Teilwirkung bleibt als `outcome_unclear` sichtbar. Der
  Quarantäne-Claim bindet außerdem Änderungs- und Erstellungszeit jedes Eintrags.
- CLI, Quarantäne-Badge und Laufansicht unterscheiden physisch vorhandene,
  automatisch wiederherstellbare, nur manuell rettbare und noch abzuschließende
  Einträge. Nach erfolgreichem endgültigem Leeren verschwindet die alte Laufansicht
  vor dem Reload; ein Reloadfehler lässt keine veralteten Restore- oder
  Purge-Schaltflächen zurück. AltGr- und IME-Eingaben in der Pfadleiste lösen keine
  globalen Informations- oder Darstellungsoptionen mehr aus.
- Die Kompatibilitätsprüfung alter bidirektionaler Endpunktbindungen akzeptiert
  ausschließlich die vorgesehene FTPS-/WebDAV-Migration von Schema 1 auf 2 und
  verlangt dabei unveränderten Kontodigest und Root. SFTP- oder FTP/FTPS-Snapshots
  desselben Servers können dadurch nicht mehr eine Paar- oder Recovery-Bindung
  eines anderen Kontos übernehmen.
- Dauerhaftes Remote-Löschen trägt den bestätigten Provider-Token jetzt zwingend bis
  in eine bedingte Löschwirkung und verlangt danach einen Abwesenheits-Readback.
  FTPS bietet diesen irreversiblen Weg nicht mehr an, weil DELE/RMD keine solche
  Objektbedingung kennt; WebDAV nutzt weiterhin `If-Match`. Ein erfolgreicher
  Serverstatus bei sichtbarem oder nicht lesbarem Pfad endet als `outcome_unclear`.
- Quarantänemanifest Version 3 bindet Identität und BLAKE3-Inhalt jedes Eintrags am
  ursprünglichen Claim. Restore lehnt ausgetauschte und in-place veränderte Dateien
  ab. Lokales Restore synchronisiert neu angelegte Elternordner vom Blatt bis zum
  Zielroot und erst danach den entfernten Quarantänenamen. Ein fehlender Lauf gilt
  nach persistiertem Purge-Intent nicht mehr allein aufgrund seiner Pfadabwesenheit
  als erfolgreich geleert; ein leer reservierter lokaler Lauf erhält dafür einen
  identitätsgebundenen Tombstone.
- FTPS-/WebDAV-Endpunktbindungen verwenden Schema 2, Baselines Version 2 und
  bidirektionale Laufjournale Version 8. Alte Digests werden nur nach frischer
  Authentifizierung akzeptiert und anschließend auf das aktuelle Schema angehoben.
  Queue-Journal Version 19 stellt alte Remote-Löschpläne je nach möglicher Vorwirkung
  auf Neuplanung oder manuelle Recovery, statt sie mit neuer Digestbedeutung zu laden.
- Das Quittieren eines unklaren Queue-Ausgangs und eines alten WebDAV-Löschlaufs
  startet den nächsten Pending-Eintrag jetzt über dieselbe fensterlokale
  Exit-Barriere wie Retry und Freigabe. `replan_required` und `quarantine_busy`
  bleiben auch im geheimnisfreien Diagnoselog als konkrete Fehlercodes erhalten.
  Nach einem fehlgeschlagenen Quarantäne-Purge lädt die Oberfläche den Laufstatus
  frisch; ein persistierter Purge-Intent kann dadurch nie über veraltete
  Restore-Schaltflächen umgangen werden.
- Die Queue-Migration bewahrt einen bereits in Journalversion 18 sicher
  stillgelegten WebDAV-Löschaltplan samt eigener Quittierung. Der ungültige
  Root-ETag-Sentinel wird nicht mehr von der allgemeineren Identitätsschema-Migration
  in einen widersprüchlichen Recoveryzustand umgedeutet, der das Journal beim
  nächsten Start unlesbar machte.
- Lokales Quarantäne-Restore synchronisiert die vollständige Ziel-Elternkette auch
  dann, wenn ein anderer Prozess die zuvor fehlenden Ordner genau zwischen
  Abwesenheitsprüfung und descriptor-relativem Öffnen anlegt. Ein Absturz kann den
  wiederhergestellten Eintrag dadurch nicht mehr zusammen mit ungesicherten
  Elternnamen verlieren.

- Ein echter Mirror-Lauf führt unabhängige Ergänzungen und Aktualisierungen jetzt
  auch dann aus, wenn der Plan einen Symlink, eine Spezialdatei oder einen Konflikt
  sichtbar auslässt, aber nichts aus dem Ziel entfernen würde. Sobald derselbe Plan
  mindestens eine Entfernung enthält, sperren ausgelassene Einträge den Lauf
  weiterhin vor Probe, Permit und Wirkung.
- Restore schreibt Zielpfad und stabile Quellidentität jetzt vor dem ersten Rename
  in das Quarantänemanifest und bindet genau diesen Inhalt bis zum Abschluss der
  descriptor-relativen Wirkung. Scheitert nur der anschließende Manifestabschluss,
  gleicht ein erneuter Aufruf Quelle und Ziel ab und schließt den Status ohne zweiten
  Rename. Fremde Zielnachfolger bleiben unangetastet, offene Restore-Abschlüsse
  sperren ein neues endgültiges Leeren und bleiben in GUI sowie CLI bedienbar — auch
  wenn der inzwischen leere Laufordner fehlt. GUI und CLI unterscheiden dabei
  ausdrücklich zwischen einer neuen Wiederherstellung und einer reinen
  Abschlussprüfung; Remoteprovider benötigen für diesen Weg zusätzlich Manifest-CAS.
- Die Provider-Konfliktquarantäne bindet beim Ersetzen jetzt Quellobjekt,
  Quell-Elternordner und Quarantäne-Elternordner in derselben bedingten
  Rename-Wirkung. Ein Austausch des Zielordners zwischen Konfliktprüfung und Parken
  verschiebt dadurch weder den bestätigten Altbestand noch einen gleichnamigen
  Nachfolger; Provider ohne vollständigen Zwei-Eltern-Vertrag lehnen vor der ersten
  Quarantänemutation ab.
- Lokales Restore und endgültiges Leeren binden den manifestierten
  Quarantänelauf und den Restore-Zielroot jetzt bis in die Wirkung an gehaltene
  Deskriptoren sowie an Geräte-, Inode- und Mountidentität. Ein zwischen Prüfung
  und Rename oder rekursivem Leeren eingesetzter gleichnamiger Ordner bleibt
  unangetastet. Purge persistiert sein Intent vor der ersten Löschwirkung, leert
  ausschließlich den gebundenen Lauf und behält Manifest, Intent und leeren
  Laufordner als sichtbaren Status „geleert“. Ein später neu erschienener Inhalt
  macht ihn wieder zum eigenen Purge-Recoveryfall, sperrt Restore und erlaubt nur
  einen erneuten Leerungsversuch. Die gebundene Wirkung hält zusätzlich den
  identitäts- und inhaltsgeprüften Purge-Intent bis zum abschließenden Leerheitsbeleg;
  er kann daher nicht zwischen Journalprüfung und Wirkung unbemerkt verschwinden.
  SMB-Läufe bleiben lesbar, melden Restore und Purge aber erst wieder
  als verfügbar, wenn der Provider dieselbe File-ID-gebundene Wirkung
  implementiert; die Oberfläche übernimmt diese drei Fähigkeiten für jeden
  Provider fail-closed aus dem Backend.
- Quarantänemanifeste der Version 2 binden den zufälligen Laufordner dauerhaft an
  seine stabile lokale Geräte-/Inode- beziehungsweise SMB-File-ID. Restore, Liste
  und Leeren lehnen alte oder ausgetauschte Laufnamen ab; ein als fehlend geprüfter
  Lauf führt nicht mehr zu einem späteren Kindzugriff. Ein Manifest darf nur leer
  auf die exklusive Laufanlage warten. Bricht ein bidirektionaler Typwechsel noch
  vor seiner ersten Datenwirkung ab, entfernt die Recovery dieses leere Sidecar
  unter seiner Prozess-Lease, statt einen nicht existierenden Lauf als abgeschlossen
  auszugeben.
- Ein Commit-Rename des Sync-Kopierwegs, das nach möglicher Wirkung oder erst im
  Verzeichnis-Sync scheitert, meldet den unklaren Ausgang jetzt auch als
  maschinenlesbaren Code `outcome_unclear` und nennt Ziel- und Temp-Pfad. Bisher
  kam dort der rohe Fehler an, also je nach Ursache `connection_lost` oder `io` —
  Oberfläche und CLI konnten eine möglicherweise schon vollzogene Mutation dadurch
  wie einen gewöhnlichen Fehler behandeln und blind wiederholen. GUI-Ereignis und
  Queue-Journal behalten den Code jetzt; „Neu prüfen“ bleibt gesperrt, bis Quelle,
  Ziel und Recoverypfade manuell geprüft und der Eintrag ohne Wiederholung geschlossen
  wurde. Ein erfolgreicher Rollback benennt außerdem ausdrücklich das
  wiederhergestellte alte Ziel statt fälschlich zu behaupten, es habe keinen Rückweg
  gegeben.
- Ein WebDAV-Server ohne `Server`-Header lässt sich wieder verbinden. Der Header
  ist laut HTTP freiwillig; sein Digest wurde über einen leeren Text gebildet, und
  genau den lehnt die Identitätsprüfung seit kurzem als „bindet nichts" ab. Fehlende,
  leere, mehrfache und nicht als UTF-8 dekodierbare Werte werden jetzt mit Länge und
  Rohbytes eindeutig gebunden. Queue-Journale der Version 17 mit dem früheren leeren
  Digest werden geöffnet, stellen aber nur den betroffenen Altplan sichtbar still.
- FTPS bindet den vollständigen FTP-Willkommenstext als begrenzte Rohbytes direkt aus
  der Kontrollverbindung. Latin-1- und andere nicht als UTF-8 dekodierbare Grüße
  erzeugen dadurch keinen leeren Identitätsdigest mehr.
- Die Linux-Releaseprüfung vergleicht das DEB nach der Signatur der
  AppImage-Attestation ein letztes Mal mit seinem gebundenen Hash; die
  Attestationsfunktion prüft ihr AppImage selbst. Ein währenddessen geändertes DEB
  wäre zuvor mit einer Signatur des früheren Inhalts und einer Prüfsumme des späteren
  Inhalts veröffentlicht worden.
- Ein Profilereignis gibt eine gesperrte Verbindungszeile erst frei, wenn der
  jüngste Profilabruf desselben Protokolls fertig ist. Überholte Ereignisse machten
  die alte Zeile samt Favoriten vorzeitig wieder bedienbar; die Aktion lief dann mit
  der alten Revision los und endete erst an der Backend-Prüfung als `plan_drift`.
  Auch ein Fehler beim Sperren, Rendern oder abschließenden Abruf lässt die
  Profilsperre nicht mehr bis zum App-Neustart liegen.
- Der Dialog zum Verwerfen einer Queue-Rückfrage unterscheidet den aktuellen von
  früheren Versuchen, warnt bei einer möglichen früheren Teilwirkung und behauptet
  bei pausierter Queue keinen anschließenden Start. Abbruch und Bestätigung geben den
  Tastaturfokus an denselben Queue-Knopf zurück.
- Der bestätigte SFTP-Realmodus-Lauf nutzt denselben gemeinsamen Ablauf wie FTPS,
  WebDAV, SMB und Providersync. Die letzte protokolleigene Kopie von
  Bindungsprüfung, Fortschrittsdrosselung, Terminalevent und Freigabe der
  Fensterschranke entfällt damit.
- Die Sandbox-Schranke weist ein `..` jetzt schon vor der Pfadauflösung ab. Beide
  Schranken — die der Dateioperationen und die des Syncs — lösten ein Ziel über den
  nächsten **vorhandenen** Vorfahren auf und verwarfen die restlichen Komponenten.
  Ein `..` nach einer noch nicht existierenden Komponente kam dadurch als erlaubt
  durch: Bei `<sandbox>/fehlt/../../draussen.txt` war der nächste vorhandene
  Vorfahre die Sandbox selbst, und sobald `fehlt` entstand, zeigte derselbe Pfad
  aus der Sandbox heraus. Über die normalen Abläufe war das nicht erreichbar, weil
  Auswahlpfade bereits normalisiert werden; die öffentliche Schranke selbst hielt
  ihre Zusage aber nicht.
- Permanente WebDAV-Löschungen reichen die im bestätigten Plan gebundene starke
  ETag bis in dasselbe `If-Match`-DELETE und verlangen nach HTTP 204 einen
  Abwesenheitsbeleg. Ein ausgetauschter Eintrag bleibt unangetastet; SMB-Reconnects
  akzeptieren außerdem keine geänderte Root-File-ID mehr.
- Hardlink-Commitfehler entfernen einen möglicherweise neu belegten Temp-Namen nur
  noch über eine atomare Identitätsbedingung. Dasselbe gilt jetzt für den regulären
  Sync-Kopierweg: Scheitert erst der Verzeichnis-Sync nach dem Rename, ist der
  Temp-Name bereits freigegeben und wird nicht mehr pfadbasiert gelöscht. Vor dem
  Rename wird er unverändert aufgeräumt. Gesperrte bidirektionale Resumes
  starten keine Restore-Probe, zielgefilterte Quarantänelisten validieren keine
  fremden Geschwisterläufe, und ein gescheiterter terminaler Queuecommit wird als
  unklarer Abschluss statt als wiederholbare Dateioperation gemeldet.
- Der Linux-Release-Prüfer bindet DEB und AppImage vor Extraktion und Ausführung an
  private Kopien sowie SHA-256-Hashes und prüft die Originale unmittelbar vor und
  nach jeder Signatur erneut. Ein während der Prüfung ausgetauschtes Artefakt kann
  dadurch nicht als geprüfter Stand signiert werden.
- Alte WebDAV-Queue- und bidirektionale Laufjournale ohne starke Root-ETag
  werden wieder lesbar, aber niemals mit einer erfundenen Identität fortgesetzt:
  sicher ungestartete Intents lassen sich typisiert verwerfen; mögliche
  Teilwirkungen können erst nach ausdrücklich bestätigter manueller Prüfung
  terminal geschlossen werden. Die alten Pläne werden niemals wiederholt.
- Remote-Anfragen prüfen Profilrevision, Pfad, Dateiname und Sync-Optionen vor
  Cachetreffer, Handshake, Secret-Auflösung oder mutierender Capability-Probe.
  Damit kann ein syntaktisch ungültiger oder von einer alten Pane stammender
  Request keinen inzwischen geänderten Endpunkt mehr kontaktieren. Erfolgreich
  entfernte Profile senden in allen vier Providerwegen ein Tombstone-Ereignis an
  jedes Fenster, sodass alte Panes, Profilzeilen und Favoriten sofort ausfallen.
- Rename, Ordneranlage sowie Quarantäne-Restore und -Leeren bilanzieren Erfolg,
  Kollision und Fehler auch dann sichtbar, wenn während der laufenden Mutation
  eine zweite Aktion beginnt oder der Dialog geschlossen wird. Neu geladen werden
  ausschließlich Panes und Badges, die noch denselben Endpunkt und Pfad zeigen.
- WebDAV behandelt Ziel- und Quarantäne-Collection als getrennte ETag-gebundene
  Roots derselben Sitzung. Der PROPFIND-Parser akzeptiert Pflichtmetadaten über
  mehrere erfolgreiche `propstat`-Blöcke, verlangt `resourcetype` aber weiterhin
  genau einmal je Ressource.
- Das CLI nutzt für SFTP-Listen und Verbindungsimporte den ausdrücklich
  interaktiven Best-Effort-Lesepfad. JSON und Hilfe weisen aus, dass daraus kein
  vollständiger Planner- oder Identitätsbeleg entsteht.
- Abbruch und Fehler lassen einen unbelegten Datei-Tempnamen als
  möglichen Prüfpfad stehen, statt ihn als unklaren Operationsausgang zu behandeln
  oder pfadbasiert zu löschen. JSON und Textausgabe nennen den Pfad samt
  Löschwarnung; ein ausgetauschter fremder Name bleibt unangetastet. Der Pfad wird
  auch bei einem zusätzlichen Journalfehler atomar mit Fortschritt und fachlichem
  Fehler finalisiert und bleibt über Neuprüfung und Neustart strukturiert erhalten.
  Lokale und Provider-Dateiworker verwenden dafür denselben detaillierten
  Abschlussweg. Ist die Dateioperation bereits vollständig vollzogen, darf ein
  Journalfehler ausschließlich ihren terminalen Abschluss wiederholen und den
  Eintrag niemals als retryfähigen Ausführungsfehler zurückstufen.
- Remote-Pfade mit `~` bleiben im CLI Providerpfade und werden nicht mehr gegen das
  lokale Home-Verzeichnis aufgelöst. Kopieren, Verschieben, Umbenennen und Löschen
  lehnen solche relativen Remote-Eingaben vor Queue-, Profil- und Netzwerkzugriff ab.
- Scheitert nach einem Provider-Dateifehler zusätzlich das Queue-Journal, weist die
  CLI Ausführungsfehler und einen oder mehrere Journalfehler getrennt aus, ohne
  Teilbilanz oder Recovery-Hinweise zu verlieren.
- Der Linux-Release-Prüfer führt die aus DEB und AppImage entnommenen CLI-Binaries
  aus und vergleicht ihre Versionsausgabe exakt mit der Workspace-Version, bevor er
  AppImage, DEB oder AppImage-Attestation signiert. Tauri erzeugt davor keine frühe
  Linux-Updatersignatur mehr.
- Der Headless-Gate-Runner installiert die gesperrten Node-Abhängigkeiten vor den
  Tooltests; das gilt auch für `--only test-tools`. Der CI-Vertrag wird als
  YAML-Struktur statt über Quelltextfragmente geprüft.
- `macos-public` verwendet jetzt denselben Schlüsselbundprofil-basierten App-,
  Updater- und DMG-Releasepfad wie `macos-release`. Der unerreichbare zweite Weg mit
  Notarisierungs-Secrets in Umgebungsvariablen wurde entfernt.
- Remote-Profile besitzen jetzt dauerhaft monotone Revisionen. Ein langsamer SFTP-,
  FTP/FTPS-, WebDAV- oder SMB-Handshake kann nach gleichzeitigem Speichern oder
  Entfernen keinen alten Cacheeintrag mehr veröffentlichen; Pläne, Konfliktlisten,
  Permits und Queueeinträge stoppen bei einer abweichenden Revision als Plan-Drift.
  Panes, Favoriten, Dateiaktionen, Quarantäne und neue Fenster senden immer den
  sichtbar gebundenen Stand; Profilereignisse entwerten nur betroffene alte Panes.
  Auch Speichern und Entfernen vergleichen die vom Editor geladene Revision unter
  dem prozessübergreifenden Profildatei-Lock. Ein veraltetes Fenster kann damit
  weder neuere Host-/Root-/Pin-/Secret-Änderungen überschreiben noch löschen.
- Profiländerungen invalidieren nur noch die davon abhängigen Vorbereitungen. Ein
  bereits bestätigungsbereiter Remote-Queueeintrag wird zuerst dauerhaft nach
  `Pending` geschrieben und verliert erst danach sein RAM-Permit. Scheitert der
  Journalcommit, bleibt die Vorbereitung retrybar; der weiterhin sichtbare
  Bestätigungsklick wiederholt genau diesen Übergang. Andere Profile und Fenster
  arbeiten unverändert weiter.
- Meldet das Backend nach einem Profil- und Secretwechsel einen unklaren Ausgang,
  schließt der Verbindungseditor und liest alle Profile neu. Die bereits verbrauchte
  Secret-Einmal-ID kann dadurch nicht nochmals den alten Schlüsselbundverweis
  speichern; das gilt für SFTP, FTP/FTPS, WebDAV und SMB.
- Ein veraltetes SFTP-Listing startet nach einer neueren Navigation keinen zweiten
  Verbindungsaufruf mehr. Die Sequenzprüfung liegt jetzt unmittelbar hinter dem
  Listing.
- Nach einem gestarteten lokalen Papierkorbaufruf bleiben Erfolg oder Fehler auch
  bei zwischenzeitlicher Navigation sichtbar. Nur Panes, die aktuell noch denselben
  lokalen Ordner zeigen, werden neu geladen; zwei gleiche Panes ziehen gemeinsam nach.
- Nach einem abgebrochenen Dateilauf bleiben nicht sicher entfernbar gewesene
  Temp-Prüfpfade im terminalen Queuejournal, im append-only Audit, im Abschlussdock
  und in der Queue sichtbar. Sie sind ausdrücklich keine automatische Löschfreigabe.
- Die macOS-Geräteliste blendet Nicht-Root-Mounts mit `MNT_DONTBROWSE` jetzt
  unabhängig von ihrer lokalen oder Netzwerk-Klassifizierung aus. Dadurch erscheint
  der nicht nutzbare `auto_home`-/`autofs`-Platzhalter nicht mehr als Freigabe mit
  unbekannter Speichergröße und Auswerfen-Knopf; sichtbare Volumes und echte
  Netzwerkfreigaben bleiben erhalten.
- Ein bestätigter Typkonflikt wird jetzt zuerst atomar in seinen exklusiv
  reservierten Sicherungsnamen verschoben und **erst dort** gegen den gebundenen
  Nachweis geprüft. Zwischen einer Prüfung am Zielnamen und dem Verschieben konnte
  ein paralleler Lauf das Ziel austauschen; die Sicherung wurde dann nach
  erfolgreichem Commit gelöscht, obwohl ihre Identität nie geprüft war. Bei Drift
  kehrt der geparkte Eintrag unversehrt an seinen Namen zurück.
- Eine lokale Ordnerkopie bindet alle tatsächlich zu ersetzenden Ziele rekursiv —
  wie der Provider-Weg es längst tut. Bisher erfassten Oberfläche und Queue nur die
  oberste Ebene, während der Executor für jede vorhandene Zieldatei einen Nachweis
  verlangt: Der Lauf legte erst neue Untereinträge an und brach dann am ersten
  tieferen vorhandenen Ziel ab. Zwei gleichnamige Ordner bleiben kein Konflikt.
- Die Bereinigung verwaister Queue-Temps läuft nur noch einmal je Prozesssperre.
  Zuvor räumte jede neu geöffnete Fensterqueue alle `.operations-queue-*.tmp` im
  gemeinsamen Konfigurationsordner weg — auch die gerade geschriebene Datei eines
  aktiven Fensters, dessen Rename danach fehlschlug.
- Der Quarantänelaufname eines lokalen Queue-Laufs enthält zusätzlich Fensterlabel
  und Operations-ID. Zwei erlaubterweise parallele Fensteroperationen derselben
  Millisekunde kollidierten sonst beim Anlegen des Manifests.
- Papierkorb-Recovery und Papierkorboperation laufen unter einer prozessweiten
  Sperre. Der Journalordner gehört allen Fenstern gemeinsam; die Single-Flight-
  Schranke eines Fensters schützte ihn nicht.
- Ein beschädigtes Nebenfenster-Journal verhindert nicht mehr den App-Start. Der
  Öffnungsfehler bleibt auf sein Journal beschränkt und wird gemeldet, statt bis ins
  Tauri-Setup durchzuschlagen und damit auch das Hauptfenster zu blockieren.
- Das Aufräumen einer eigenen WebDAV-Temp-Datei sendet den beim PUT gemerkten
  starken ETag als `If-Match` und behandelt HTTP 412 als Drift. Zuvor löschte auch
  der Abbruchweg unbedingt und hätte einen serverseitig untergeschobenen fremden
  Eintrag entfernt.
- Der PROPFIND-Parser akzeptiert `collection` nur noch als direktes Kind von
  `resourcetype`. Ein gleichnamiges Element an anderer Stelle innerhalb von `prop`
  konnte eine Datei als Ordner ausweisen, wodurch die für Dateien verbindlichen
  ETag-, Größen- und Zeitprüfungen übersprungen wurden.
- Die mutierende SFTP-mkdir-Probe hat ihren Aufräumweg schon vor der ersten
  Mutation. Nach unklarem Ausgang wird ausschließlich der eigene, eindeutig benannte
  und lesend als leer belegte Probeordner entfernt.
- Das CLI schreibt sein macOS-Realmodus-Audit in den Anwendungsordner der stabilen
  Kennung `com.doppeldecker.desktop` statt in die nie ausgelieferte
  `com.doppeldecker.app`. CLI und Desktop teilen damit wieder eine Historie.
- Die Updateprüfung stellt sich zurück, solange der gemeinsame `#modal`-Dialog belegt
  ist. Zuvor konnte der native Menüpunkt einen offenen Rename-, Konflikt- oder
  Realmodus-Dialog ersetzen und dessen wartende Dateioperation dauerhaft hängen
  lassen.
- Der Update-Download meldet während des Ladens nur noch den Ladestand; die
  kryptografische Prüfung wird erst nach erfolgreicher Installation bestätigt.
  Scheitert die Registrierung des Fortschritts-Listeners, wird die Aktionssperre
  wieder freigegeben statt bis zum Neuladen des Fensters zu bestehen.
- `tools/write-update-manifest.py` bindet jede Nutzlast fail-closed an Version und
  Architektur — Linux über den Paketnamen, macOS über `Info.plist` und den
  Mach-O-Kopf im Archiv. `--pub-date` wird vollständig als RFC-3339-UTC-Wert geparst
  und kanonisch geschrieben. `release:update-manifest` erzeugt danach `SHA256SUMS`
  neu, damit die Prüfsummendatei das veröffentlichte `latest.json` enthält.
- `tools/run-sftp-e2e.sh` setzt seinen Aufräum-Trap unmittelbar nach `mktemp`. Ein
  Fehler bei Testschlüsseln, Manifest oder Agent hinterließ zuvor private Schlüssel
  und den Serverroot; der Trap beendet außerdem nur einen selbst gestarteten Agenten.
- Die READMEs kennzeichnen Download- und Updateweg bis zum ersten veröffentlichten
  Release ausdrücklich als geplant, statt eine nicht vorhandene Releases-Seite zu
  versprechen.
- FTPS-Listings akzeptieren die case-insensitiven, systemabhängigen
  `OS.name=type`-Werte aus RFC 3659. Unix-Symlinks werden sichtbar als Symlinks,
  andere Systemtypen als Spezialdateien bilanziert und niemals verfolgt; `cdir` und
  `pdir` werden anhand ihres Typs statt nur anhand der Namen `.`/`..` ausgelassen.
  Dadurch lassen sich auch reale Unix-/ProFTPD-Server vollständig prüfen und
  importieren, ohne die strengen Typ-, Größen- und Zeitgates zu lockern.
- Die macOS-Geräteerkennung blockiert den AppKit-/Tauri-Thread nicht mehr. Die nur
  ergänzende `diskutil info`-Abfrage endet pro Volume nach einer Sekunde und lässt
  ein träges Ziel neutral sichtbar; Geräte- und Bonjour-Listen reichern die bereits
  bedienbaren Panes asynchron an. Ein realer Erststart brauchte zuvor wegen eines
  einzelnen Volumes 7,196 Sekunden bis zum ersten sinnvollen Bild.
- Das macOS-Bundle erklärt den tatsächlichen lokalen-Netzwerk-Zweck und deklariert
  exakt die Bonjour-Dienste für SMB, SFTP und WebDAV. Start-, Geräte-, LAN- und
  Ordnergrößenfehler behalten außerdem ihren festen Kontext im geheimnisfreien
  Fehlerlog, statt dort als nichtssagendes `App-Fehler` zu erscheinen.
- Die vollständige Tauri-Updater-Konfiguration mit öffentlichem Signierschlüssel und
  HTTPS-Manifest-Endpunkt ist bereits im Bootstrap-Build eingebettet. Ein Build mit
  registriertem Updater-Plugin, aber fehlendem `plugins.updater`-Block konnte zuvor
  erfolgreich installiert werden und brach dann bei jedem App-Start unmittelbar ab.

### Hinzugefügt

- Datei- und Ordnerauswahlen besitzen jetzt „Informationen“ im Kontext- und
  Datei-Menü sowie `⌘I`/`Ctrl+I`. Der Dialog zeigt alle vom lokalen oder entfernten
  Listing gelieferten Metadaten und berechnet Ordnergröße, Datei- und
  Unterordnerzahl erst nach ausdrücklichem Klick über den bereits fenster-, pane-
  und sitzungsgebundenen Größenlauf. Mehrfachauswahlen summieren ausschließlich
  vollständige Ordnerresultate; partielle Remote-Listings bleiben samt Fehlergrund
  sichtbar. „Darstellungsoptionen …“ ist zusätzlich über das native Menü und
  `⌘J`/`Ctrl+J` erreichbar.
- Manueller, signierter Updatekanal mit `tauri-plugin-updater`: Nur der native
  Menüpunkt startet eine Prüfung; erst ein zweiter ausdrücklicher Klick lädt und
  installiert den im Backend pro Fenster gebundenen Release. Signatur, URL und
  installierbarer Handle bleiben außerhalb des Frontends. Während Download und
  Installation hält das Fenster dieselbe Single-Flight-Schranke wie seine
  Dateioperationen; andere Fenster bleiben unabhängig. Es gibt keinen Start-Timer,
  keine Hintergrundprüfung und keinen stillen Neustart.
- `doppeldecker-cli trash --item <Pfad>` verwendet den descriptor-relativen nativen
  Systempapierkorb samt atomarem Claim, Crash-Journal und fail-closed Recovery aus
  dem Core. Mehrere `--item`-Werte bilden einen Stapel; Symlinks werden selbst statt
  ihres Ziels erfasst. Ein echter Delete- oder Copy-and-delete-Fallback existiert
  weiterhin nicht.
- Öffentlicher macOS-/Linux-CI-Workflow führt denselben `tools/check-gates.py`-Lauf
  auf macOS 14 und Ubuntu 24.04 aus und sichert Fehlerlogs als Artefakt. Der
  Release-Pfad erzeugt passwortgeschützte Tauri-Signaturen für macOS-App, AppImage
  und DEB sowie ein paketartgebundenes `latest.json`.
- Mehrere Dual-Pane-Fenster lassen sich über „Fenster → Neues Fenster“ oder
  `⌘N`/`Ctrl+N` öffnen und übernehmen beide sichtbaren Pane-Endpunkte. Lebende
  SFTP-, FTP/FTPS-, WebDAV- und SMB-Verbindungen erscheinen in allen Fenstern wie
  eingehängte Ziele; jedes Fenster besitzt dagegen eine eigene persistente Queue,
  Single-Flight-Schranke, Realmodus-Vorbereitung, Auditdatei und gezielte
  Fortschrittsereignisse. Fensteroperationen dürfen parallel laufen, während ein
  gemeinsamer externer Lock weiterhin einen zweiten GUI-/CLI-Prozess ausschließt.
  Unfertige Nebenfenster-Queues öffnen nach einem Neustart ein Recovery-Fenster.
- `plan`, `real` und der providerübergreifende CLI-Sync akzeptieren wiederholbare,
  begrenzte `--exclude`-Muster für Datei- und Ordnernamen. `*` und `?` gelten
  innerhalb einer Pfadkomponente, `**` als ganze Komponente über mehrere Ebenen.
  Bewusst ausgeschlossene Einträge sind auf beiden Seiten planfest unsichtbar und
  sperren Mirror nicht; nicht sicher dekodierbare Namen bleiben dagegen sichere
  Skips. Ein Zielordner mit ausgeschlossenen Nachfahren wird nicht als Ganzes
  quarantänisiert, seine übrigen sichtbaren Kinder aber weiterhin einzeln.
- Die Operations-Queue ist aus dem unteren Seitenleistenbereich in den dauerhaft
  sichtbaren Fensterkopf gewandert. Das feste, auch bei schmalen Fenstern
  bedienbare Popover hält den laufenden Eintrag außerhalb der scrollenden
  Restliste angeheftet und bleibt während eines Transfers erreichbar. Ein
  nativer Beenden-Versuch mit offenen, fehlgeschlagenen oder zu prüfenden
  Einträgen zeigt nun ihren Bezug zur persistenten Queue; erst „Trotzdem
  beenden“ ruft den getrennten Exit-Command auf. Abgeschlossene und ausdrücklich
  abgebrochene Einträge lösen den Hinweis nicht aus.
- Die Sync-Vorschau zeigt geänderte Einträge jetzt als zweispaltigen Baum mit
  Quelle und Ziel, Geisterzeilen für fehlende Gegenstücke sowie ＋-, ⟳- und
  Quarantäne-Badges genau auf der betroffenen Seite. Pfadtiefe, Ahnenkontext,
  Metadaten beider Seiten, Wirkung und Kategoriesummen entstehen im Core aus
  demselben Listing-Lauf wie die ausführbaren Aktionen und sind Bestandteil des
  bestätigten Plan-Digests. Große Vergleiche werden virtualisiert; die Ansicht ist
  als `listbox` vollständig per Tastatur bedienbar. Ältere Queue-Pläne ohne das
  neue DTO bleiben über die bisherige Aktionsliste lesbar.
- „Freigaben“ durchsucht das LAN standardmäßig per mDNS/Bonjour nach den drei
  festen DNS-SD-Diensten SMB, SFTP und WebDAV; die aktive Suche lässt sich lokal
  ausschalten und bewusst erneut starten. Der begrenzte Core-Parser akzeptiert
  nur zusammengehörige PTR-/SRV-Antworten mit gültigem `.local`-Host und Port,
  verwirft fehlerhafte Kompression sowie fremde Dienste und verwendet keinen
  NetBIOS-Scan. Ein Klick übernimmt ausschließlich Anzeigename, Host und Port in
  den bestehenden neuen Verbindungseditor. Nutzer, Secret, Root/Freigabe und die
  Hostkey-, Zertifikats- oder Server-GUID-Prüfung bleiben unverändert offen.
- Die Seitenleiste trennt lokale Volumes unter „Geräte“ von eingehängten
  Netzwerkzielen unter „Freigaben“; beide zeigen freien und gesamten Speicher.
  „Favoriten“ enthält sowohl Ordner als auch die gespeicherten Verbindungen,
  danach folgt die Quarantäne. Die Queue sitzt jetzt im Fensterkopf. Die drei
  Hauptkategorien lassen sich am Griff per Drag oder mit Pfeiltasten umsortieren; eine validierte lokale
  Reihenfolge bleibt über Neustarts erhalten. macOS ermittelt Volumes über
  `getfsstat` und bindet auswerfbare Datenträger an frisch gelesene
  `diskutil`-Eigenschaften; Linux filtert `/proc/self/mountinfo`, ergänzt
  `statvfs` und adressiert den gewählten Mountpfad über `gio`. Auswerfen verwendet
  ausschließlich eine opaque, frisch geprüfte Mount-ID, keine Shell und bestätigt
  danach, dass genau dieses Ziel verschwunden ist. Unklare oder nicht belegte
  Strategien bleiben fail-closed; betroffene lokale Panes wechseln nach Erfolg in
  ihren sicheren Standardordner.
- Ordnergrößen sind in der Dateiliste jetzt bedienbar: Für lokale Panes lässt
  sich die automatische Berechnung global einschalten, auf SFTP, FTP/FTPS,
  WebDAV und SMB startet sie ausschließlich pro Ordner per Klick. Laufende,
  vollständige und unvollständige Ergebnisse erscheinen direkt in der
  Größenspalte; ein Klick auf einen fertigen Wert berechnet bewusst neu.
  Navigation bricht den Lauf der betroffenen Pane ab und verwirft verspätete
  Resultate. Backend-Sessions und Abbruchsignale sind bereits nach Fenster und
  Pane getrennt, Ereignisse werden nur an das auslösende Fenster gesendet und
  die Mutations-Queue des Fensters bleibt frei. Die Größensortierung ordnet
  vollständige Ordnerbilanzen nach ihren Byte-Rohwerten live ein; unvollständige,
  fehlgeschlagene oder laufende Werte bleiben als unbekannt am Ende.
- Der Core kann Ordnergrößen jetzt rein lesend, parallel und abbrechbar
  berechnen. Ein begrenzter Worker-Pool verteilt auch Unterordner eines einzelnen
  Baums; jeder angeforderte Ordner liefert ein terminales Ergebnis mit logischen
  Dateibytes, Zählern und maschinenlesbaren Fehlern genau der unvollständigen
  Teilbäume. Symlinks und Spezialdateien werden nie betreten, unsicher dekodierbare
  Ordnernamen nie aus ihrem Anzeigenamen zu einem Pfad zusammengesetzt. Nur
  vollständige Ergebnisse werden endpunktgebunden für die laufende Sitzung
  gecacht; „neu berechnen“ liest bewusst erneut, statt aus unzureichenden
  Ordner-mtime-Werten eine dauerhafte Invalidierung vorzutäuschen.
- Jede Pane besitzt jetzt Darstellungsoptionen mit dem Scope „Dieser Ordner“:
  Größe, Art, Änderungs- und Erstellungsdatum, Eigentümer, Gruppe und
  Berechtigungen lassen sich einzeln einblenden; Ordnerreihenfolge, Fettdruck,
  Zebrazeilen, relative Datumswerte, versteckte Dateien, Dichte und Textgröße
  gelten unabhängig je Provider-/Profil-/Ordnerpfad. „Als Standard verwenden“
  setzt die Vorgabe für Ordner ohne eigene Einstellung, und die bereits
  vorhandene Wahl der Größeneinheit ist auch im Popover erreichbar. Fehlende
  Remote-Metadaten bleiben sichtbar leer; Virtualisierung und Listbox-Bedienung
  bleiben erhalten.
- Alle sichtbaren Pane-Spalten sind jetzt per Maus und Tastatur sortierbar. Jede
  Pane hält Richtung und Spalte unabhängig; Größe, Änderungs- und
  Erstellungszeit sowie Berechtigungen werden nach Rohwerten statt formatiertem
  Text geordnet. Fehlende Providerwerte bleiben auf- wie absteigend am Ende,
  und das Ausblenden der aktiven Spalte setzt nur diese Pane auf Name aufsteigend
  zurück. Native Menübefehle und Kurzbefehle folgen derselben Pane-Grenze.
- Das gemeinsame Listing-DTO liefert jetzt neben Dateityp, Größe und
  Änderungszeit auch eine sichere letzte Dateiendung, Erstellungszeit,
  Eigentümer, Gruppe und Berechtigungen. Lokal stammen die Werte aus
  descriptor-relativen Abfragen mit UID-/GID-Namensauflösung; FTP/FTPS, SFTP,
  WebDAV und SMB reichen nur vorhandene Protokollfelder durch und lassen
  Unbekanntes leer. Damit ist die Core-Grundlage für die konfigurierbaren
  G2-Pane-Spalten gelegt.
- „Ordner anlegen“ verwendet für SFTP, FTP/FTPS, WebDAV und SMB jetzt einen
  einzigen endpunktgebundenen Tauri-Command. Das Frontend übergibt Provider,
  Profil-ID und sichtbaren Pane-Root gemeinsam und setzt keine Commandnamen mehr
  aus Providerstrings zusammen; die mutierende Verzeichnisprobe bleibt innerhalb
  derselben Single-Flight-Schranke wie das anschließende mkdir.
- Die Quarantäneansicht verwendet für alle Remote-Provider dieselben generischen
  Tauri-Kommandos; die Verwaltungsfähigkeit stammt aus `ProviderKind` im Core statt
  aus einer harten FTPS-/WebDAV-Liste im Frontend. Auflisten bleibt ohne mutierende
  Probe, Restore prüft gezielt den AddOnly-/Manifestvertrag und endgültiges Leeren
  nur die getrennte Löschfähigkeit. Provider, Profil-ID und Root bleiben bei jedem
  Aufruf als vollständiger Endpunkt gebunden.
- Ordner aus SFTP-, FTP-/FTPS-, WebDAV- und SMB-Panes lassen sich jetzt wie
  lokale Ordner per Drag-and-drop zu den Favoriten hinzufügen. Der Payload und
  der gespeicherte Favorit binden Provider, Profil-ID und Providerpfad, enthalten
  aber keine Zugangsdaten; die bestehende lokale Umsortierung bleibt unverändert.
- Providerfehler unterscheiden jetzt drei steuerungsrelevante Ursachen:
  `unsupported` bezeichnet eine dauerhafte Protokoll- oder Produktgrenze,
  `capability_probe_required` eine noch fehlende oder nach Reconnect abgelaufene
  sitzungsgebundene Verhaltensprobe und `permission_denied` eine tatsächliche
  Ablehnung durch Server, Schlüsselbund oder Betriebssystem. Die vier
  Remote-Provider, Tauri-Fehler und CLI-JSONL reichen diese Trennung durch;
  belegte fehlende Servergarantien werden nicht mehr als falsches Rechteproblem
  gemeldet.
- Bestätigte Einzeldatei-Ersetzungen funktionieren jetzt auch auf FTPS- und
  WebDAV-Zielen ohne erreichbare Quarantäne. Eine künstliche Probe muss die
  wirkliche Ersetzung samt Inhalts-Readback belegen; der echte Lauf bindet das
  vollständige Elternlisting, erlaubt nur reguläre Dateien, sendet genau eine
  Mutation und prüft Endname, Größe und BLAKE3-Inhalt. GUI und CLI warnen
  ausdrücklich, dass die Altversion dauerhaft verloren geht; die Queue bindet
  diese Best-Effort-Klasse. Ein unklarer Ausgang wird nie automatisch wiederholt,
  Sync, Mirror und Typkonflikte bleiben von diesem irreversiblen Weg ausgeschlossen.
- Interaktive SFTP-Uploads funktionieren jetzt auch auf Servern, die kein
  erfolgreiches `fsync@openssh.com` bieten: Die Sitzung belegt exklusives
  Temp-Anlegen, vollständigen Inhalts-Readback und No-Replace-Rename, Vorschau,
  GUI, CLI und Queue melden die fehlende Dauerhaftigkeitsgarantie als Best Effort.
  Ein bestätigter Upload wird genau einmal committet und am Endnamen rückgelesen;
  AddOnly-Sync, Update und Quarantänemanifeste bleiben ohne echten fsync-Erfolg
  gesperrt. Die Capability folgt nun der tatsächlichen Probe statt bloß der
  Serveransage, weil OpenSSH einen angekündigten Request dennoch verweigern kann.
- SFTP und SMB binden das Entfernen einer Datei und eines leeren Ordners jetzt
  an eine eigene sitzungsgebundene Löschprobe mit vollständigem Listing und
  Readback. `remove_*` und die F6-Quellentfernung hängen dadurch nicht mehr am
  AddOnly-/Datei-fsync-Vertrag; ein Reconnect sperrt sie wieder bis zur neuen
  Probe. Die vollständige Provider-Quarantäne verlangt weiterhin den stärkeren
  AddOnly-/Replace-Vertrag für ihr vor jedem Rename geschriebenes Manifest und
  wird auf einem schwächeren Server deshalb nicht fälschlich angeboten.
- `tools/tests/test_fleet_rules.py` hält die beiden fleetweiten Regeln vom
  2026-08-03 fest: Nach `/Applications` gelangt ausschließlich ein Bundle mit
  angeheftetem Notary-Ticket — `./install` belegt Ticket, Codesignatur und
  Gatekeeper-Urteil, bevor es das erste Mal ins Installationsverzeichnis schreibt,
  und `./build-app` nutzt den notarisierenden Baumodus. Und kein absoluter Pfad des
  Build-Rechners darf im ausgelieferten Bundle landen: Pfadneutralisierung im
  Release-Build, Datenschutzprüfung der Binaries samt CLI-Sidecar, und der
  Terminalbefehl zeigt auf das CLI im eigenen Bundle, aufgelöst über
  `current_exe()`. Der Test liest nur Quelltext — er baut, signiert, notarisiert
  nichts und fasst `/Applications` nicht an, denn ein Test, der zum Beleg den
  echten Installationsweg starten müsste, würde dabei genau den Schaden anrichten,
  vor dem die Regel schützt.
- Neue Dateien auf FTP-/FTPS-Ziele laufen jetzt auch dann, wenn der Server bei
  RNTO vorhandene Ziele ersetzt: Die zweistufige AddOnly-Probe belegt getrennt
  STOU, Inhalts-Readback und gewöhnliches Rename, und der Temp-Commit läuft dann
  als sichtbarer Best-Effort-Weg — frisches vollständiges Elternlisting mit
  konservativer Namensäquivalenz, genau ein Rename, Listing-Readback und
  `outcome_unclear` bei ungeklärtem Ausgang; nach einem Verbindungsabbruch
  klärt höchstens ein identitätsgeprüfter Neuaufbau samt Listing den Zustand.
  GUI und CLI zeigen die Klasse samt konkreter Einschränkung in Vorschau und
  Einreih-Meldung; die Queue bindet die zur Vorschau gezeigte Klasse und
  pausiert sichtbar mit Bestätigungsphrase, falls das Ziel erst zwischen
  Vorschau und Ausführung auf Best Effort degradiert. Unbeaufsichtigte
  Sync-Modi verlangen weiterhin den vollständigen AddOnly-Beweis. Dabei behoben: Die Tauri-Vorschau
  serialisierte verschachtelte Fähigkeitsfelder in einer anderen Schreibweise,
  als das Frontend las — Remote-Kopien waren in der echten App dadurch pauschal
  als gesperrt angezeigt; ein Serialisierungs-Regressionstest hält die
  Schreibweise jetzt fest.
- Umbenennen auf Remote-Panes läuft jetzt in wirkungsspezifischen
  Ausführungsklassen statt an der Sync-Probe zu scheitern: Beweist ein FTP-/
  FTPS-Server nur ein gewöhnliches, potenziell ersetzendes RNTO, bleibt F2 als
  gekennzeichneter Best-Effort-Weg verfügbar — frisches Elternlisting, sichtbare
  Kollision statt stillem Ersetzen, genau eine Mutation, Listing-Readback und
  eindeutige Resultate. Nach einem Verbindungsabbruch klärt höchstens ein
  identitätsgeprüftes Listing den Zustand; ein weiterhin offener Ausgang trägt
  den neuen stabilen Fehlercode `outcome_unclear`. GUI und CLI zeigen dieselbe
  Klasse samt konkreter Einschränkung; `doppeldecker-cli rename` braucht keine
  Bestätigungsphrase mehr, weil ohne Kollision weder Überschreib- noch
  Löschrisiko besteht. Der unbeaufsichtigte Sync-Commit und der
  Move-Schnellweg im selben Namespace verlangen weiterhin den No-Replace-Beweis.
- Neue Funktionsaudit-Matrix [`docs/function-audit.md`](docs/function-audit.md)
  erfasst pro Dateimanager-Wirkung Einstiege, heutige Sperren, Zielklasse und
  Implementierungsstand; sie wird bei jeder Verhaltensänderung mitgepflegt.
- Gespeicherte SFTP-, FTP-/FTPS-, WebDAV- und SMB-Verbindungen lassen sich jetzt
  über den Stiftknopf in der Seitenleiste umbenennen und bearbeiten. Profil-ID,
  Pins, importierte Alternativrouten und vorhandene Schlüsselbundreferenzen bleiben
  bei unverändertem Endpunkt erhalten; ein neues Passwort ist optional. Änderungen
  an Host, Port oder Origin erzwingen erneut die passende öffentliche
  Hostkey-, Zertifikat- oder Server-GUID-Prüfung. Der Rechtsklick auf eine
  Verbindung öffnet statt des WebView-Textmenüs die passenden Datei-Manager-Aktionen:
  in der aktiven oder anderen Pane öffnen, bearbeiten, bei Bedarf trennen und das
  Profil nach Bestätigung entfernen.
- Die FTP-Providerfamilie unterstützt jetzt ausdrücklich `ftp://` ohne TLS sowie
  FTPS mit öffentlich geprüfter, extern geprüfter oder bewusst ungeprüfter
  Zertifikatsidentität. Klartext und ungeprüfte Identität blockieren die Verbindung
  nicht, werden aber in Profil, Pane und Statuszeile sichtbar ausgewiesen. FTPS
  bleibt verschlüsselt und gepinnt; ein explizites `ftps://` fällt nie auf Klartext
  zurück. MLSD-, Inhalts-, Kollisions-, Capability- und Reconnect-Prüfungen gelten
  unabhängig von der Transportstufe weiter.
- Die macOS-App liefert `doppeldecker-cli` im signierten Bundle mit und bietet aus
  einem Programme-Ordner einmalig einen sicheren Verweis unter `/usr/local/bin` an.
  Die Einrichtung verlangt eine ausdrückliche Wahl und ersetzt weder Dateien noch
  fremde oder tote Symlinks. DEB und AppImage enthalten dasselbe CLI neben der GUI;
  Release-Prüfungen kontrollieren Ausführbarkeit, Abhängigkeiten und Pfadneutralität.
- Fehlgeschlagene Verbindungsimporte nennen jetzt die feste SFTP-/FTP-/FTPS-Prüfstufe
  (Erreichbarkeit, Hostkey, Zertifikat, Anmeldung, Fähigkeiten oder Root-Listing),
  ohne Adresse, Nutzer, Passwort oder rohe Providerantwort auszugeben.
- `doppeldecker-cli connections import` übernimmt store-unabhängigen mehrzeiligen
  Text ausschließlich über stdin. Es erkennt gebräuchliche Nutzer-/Passwort-/
  Adressfelder, prüft ohne Schema SFTP vor FTPS und respektiert `ftp://` als
  ausdrücklichen Klartextwunsch. Vor dem Speichern werden Anmeldung und vollständiges
  Root-Listing geprüft; TLS-Zertifikate bleiben auch bei ungeprüfter öffentlicher
  Identität gepinnt. Erst danach landet das Passwort im System-Schlüsselbund.
  IPv6-Alternativen werden transportgerecht übernommen; laufende Apps laden neue
  Profile beim Fensterfokus.
- Sichtbar gewordene App-Fehler werden lokal als begrenztes JSONL mit Zeitpunkt,
  UI-Kontext und stabilem Fehlercode protokolliert. Fehlertext, Providerantwort,
  Profil, Host, Nutzer und Pfad bleiben ausdrücklich außerhalb des Logs.
- Remote-Panes sind für normale Dateimanager-Arbeit vollständig angebunden:
  Ordner anlegen, F2-Umbenennen, rekursive Mehrfachauswahl per F5/F6 zwischen
  Local/SFTP/FTPS/WebDAV/SMB, Remote-Favoriten und das Öffnen einer verifizierten
  schreibgeschützten lokalen Momentaufnahme. Konflikt-`Skip` führt Ordner zusammen;
  bestätigtes Ersetzen parkt Altversionen zuerst manifestiert in der
  Providerquarantäne und bleibt gegen späte Kollisionen No-Replace.
- Remote-Löschen besitzt einen vollständigen, driftgebundenen Plan. SFTP/SMB bieten
  ausschließlich wiederherstellbare Doppeldecker-Quarantäne; FTPS/WebDAV können nach
  echten Löschproben einen ausdrücklich dauerhaft benannten Pfad anbieten. Zwischen
  beiden Wirkungen gibt es keinen stillen Fallback.
- Das Headless-CLI unterstützt nun `mkdir`, `list`, `sync`, `copy`, `move`, `rename`
  und `capabilities` für lokale sowie gespeicherte Remote-Endpunkte und `delete`
  ausschließlich für Remote-Endpunkte (lokale Einträge nutzen weiterhin nur den
  Systempapierkorb). `sync`
  steuert denselben Provider-Realmodus wie die App; Dateiaktionen nutzen dieselbe
  persistente Queue. Schlüsselbundreferenzen, exakte stdin-Phrasen, stabile Fehlercodes
  und das geheimnis-/pfadfreie append-only Audit gelten für beide Wege.
- Reproduzierbare SFTP- und SMB-E2E-Runner starten auf Linux ausschließlich
  kurzlebige, manifestierte Loopback-Server und künstliche `/tmp`-Roots. SFTP nutzt
  einen eigenen Test-Agenten statt persönlicher Schlüssel; erfolgreiche Läufe
  entfernen ihre Testdaten vollständig.
- Favoriten-Seitenleiste verhält sich beim Umsortieren und Entfernen wie im
  Finder: Ein gezogener Favorit zeigt eine dünne Einfügelinie an genau der Kante,
  an der er landet (statt der ganzen Liste als Fläche); ein Rechtsklick öffnet ein
  Kontextmenü mit „Aus Favoriten entfernen“; und ein Favorit, der klar aus der
  Liste heraus losgelassen wird, wird entfernt (ein per Escape abgebrochener Zug
  löscht nichts). Der bestehende Hover-„×“ bleibt als Schnellzugriff. Die
  Menü-Mechanik ist dazu als wiederverwendbares `openMenu`/`hideMenu` aus dem
  Pane-Kontextmenü herausgezogen; das Hineinziehen neuer Ordner aus einer Pane
  hebt weiterhin die ganze Liste hervor.
- macOS-Menüpunkt „Datei → Finder-Favoriten übernehmen“: liest die Ordner der
  Finder-Seitenleiste (`FavoriteItems.sfl2/.sfl3/.sfl4`, eigener fail-closed
  Bookmark-Parser ohne macOS-API) und ergänzt sie additiv als Favoriten —
  Duplikate, Nicht-Ordner, ausgeblendete und unlesbare Einträge werden sichtbar
  bilanziert, entfernt wird nie etwas. Ohne Festplattenvollzugriff erklärt die
  Fehlermeldung den nötigen Schritt in den Systemeinstellungen.
- Root-Skripte `./build-app` und `./install`: Ersteres baut über den neuen
  Bundle-Modus `macos-install` eine Developer-ID-signierte und notarisierte
  `Doppeldecker.app` in den Projekt-Root (stabile Signatur statt Ad-hoc, damit
  macOS-Berechtigungen erhalten bleiben; Zugangsdaten nur über das
  notarytool-Schlüsselbundprofil, Name via `NOTARY_PROFILE` oder clone-lokal aus
  `git config doppeldecker.notaryProfile`). `./install` ersetzt zusätzlich die Kopie
  in `/Applications` und verweigert bei laufender App.

### Geändert

- Das App-Icon verwendet den ausgewählten, detailreichen blauen Doppeldecker auf
  kühlem Hellgrau. Die vollflächige Quadratquelle überlässt macOS seine
  Systemmaske und entfernt damit den bisherigen 100-Pixel-Außenabstand, der das
  Icon im Dock kleiner wirken ließ. Alle vier Entwürfe und die frühere
  SVG-Version bleiben als Rückweg erhalten; die unveränderte gewählte Quelle
  bewahrt außerdem ihre maschinenlesbaren C2PA-Herkunftsdaten.
- `tools/build-release.py` installiert in jedem Bundlelauf zuerst die gesperrten
  Node-Abhängigkeiten mit `pnpm install --frozen-lockfile`. Ein frischer Clone baut
  dadurch reproduzierbar; fehlt `pnpm`, endet der Lauf mit einer konkreten
  Voraussetzung statt eines späteren `pnpm exec`-Fehlers. Signierte öffentliche
  Updater-Artefakte verlangen ihre beiden Secret-Umgebungswerte vor jedem Werkzeuglauf.
- Das pfad- und geheimnisfreie `operations-audit.jsonl` schreibt in Auditversion 2
  zusätzlich Quell- und Zielprovider sowie die nach der frischen Capability-Prüfung
  tatsächlich bestätigte Ausführungsklasse. Abgesicherte Läufe und sichtbar
  bestätigtes Best Effort lassen sich damit unterscheiden, ohne Roots, Profile,
  Dateinamen oder Zugangsdaten zu protokollieren.
- Die Produktidentität unterscheidet jetzt ausdrücklich zwischen vollständig
  abgesicherter Ausführung, sichtbar bestätigtem Best Effort und einer Absage nur
  bei nicht begrenzbarer Wirkung. Strikte Sync-Garantien dürfen normale
  Dateioperationen nicht mehr pauschal sperren; fehlendes No-Replace, `fsync` oder
  Providerquarantäne wird wirkungsbezogen behandelt. Aktive Produkttexte
  beschreiben die eigenständigen Bedienziele ohne Bezug auf ein Konkurrenzprodukt.
- Verschachtelte Remote-Endpunkte aus dem Frontend übernehmen `profileId` jetzt
  zuverlässig bis in die Rust-Commands; Remote-Löschen, F5/F6 und Umbenennen brechen
  dadurch nicht mehr mit einem vermeintlich fehlenden `profile_id` ab. Provider-
  Capability-Proben bleiben außerdem unter dem konkret gebundenen Pane-/CLI-Root.
- FTP-/FTPS-Ordner lassen sich auch auf Servern sicher anlegen, deren `MKD` für einen
  vorhandenen Ordner idempotent Erfolg meldet. Doppeldecker prüft den vollständigen
  Elternbestand vorher, meldet vorhandene Namen als Kollision und bestätigt neue
  Ordner per Readback.
- FTP-/FTPS-Verzeichnislistings handeln angebotene, aber standardmäßig inaktive
  `MLST`-Fakten jetzt vor dem ersten `MLSD` aus. Das bereits per `CWD` gewählte
  Verzeichnis wird standardkonform ohne nichtportable Pfadangabe gelistet. Dadurch
  lassen sich Profile auch gegen strengere Server vollständig prüfen; Typ, Größe,
  Änderungszeit, UTF-8-, Mengen- und Eindeutigkeitsprüfungen bleiben unverändert.
- Nach Server-Leerlauf werden unvollständige FTPS-Kontrollantworten als
  `connection_lost` statt als Syntaxfehler klassifiziert. Die App meldet sich einmal
  neu an, wiederholt das Listing nur bei identischer Serveridentität und zeigt bei
  erneutem Fehlschlag einen verständlichen Verbindungsfehler.
- Bidirektionale Remote-Wirkungen werden nicht mehr durch statische Providerlisten
  freigegeben: Additionen, Dateiersetzungen sowie Verzeichnis-/Löschwirkungen prüfen
  jeweils den benötigten Copy-, Quarantäne-, Restore- und Namespace-
  Durability-Vertrag der konkreten Sitzung. Die Bestätigung zeigt die resultierende
  Capability-Matrix für beide Seiten.
- Queue-Journal v11 bindet generische Provider-Datei- und Remote-Löschoperationen,
  migriert ältere Konfliktpfade und prüft Profile, Listings und Drift vor jedem Claim
  erneut. Remote-Capability-Proben hinterlassen keinen künstlichen Quarantäneordner.
- Remote-Momentaufnahmen werden vor dem Öffnen durch einen zweiten Remote-BLAKE3-
  Readback samt erneutem Stat gebunden. Cache-Symlinks werden vor jeder
  Rechteänderung abgelehnt; die Altersbereinigung folgt weder Datei- noch
  Manifest-Symlinks.
- Der Browser-Gegenlauf für binäre/dezimale Größeneinheiten schaltet auf Linux nun
  tatsächlich zwischen beiden Zuständen, statt den bereits binären Plattformdefault
  nochmals zu setzen. Damit prüft dieselbe Suite macOS und Linux aussagekräftig.
- Der dauerhafte Seitenleisten-Kasten „Noch kein Vollbackup" ist durch eine
  Klartext-Zeile unter der Metadaten-Profilwahl der Sync-Ansicht ersetzt: Sie
  benennt je Profil, was ein Lauf wirklich überträgt, plus den Satz „Ein Sync
  ersetzt kein Backup". Der Hinweis erscheint damit genau im Sync-Kontext statt
  permanent beim Dateibrowsen; die dokumentierten Bedingungen für sein
  Entfallen (NativeStrict vollständig geprüft, Restore-Probe) gelten unverändert.
- Seitenleiste zeigt lebende Remote-Sitzungen: Ein dauerhaft eingeblendeter
  Eject-Knopf rechts am Verbindungsnamen erscheint genau dann, wenn die Sitzung
  des Profils im Backend wirklich offen ist — auch wenn beide Panes längst
  wieder lokal unterwegs sind. Auswerfen trennt die gecachte Sitzung (laufende
  Queue-/Realmodus-Läufe nutzen eigene Verbindungen und bleiben unberührt) und
  holt Panes, die das Profil noch zeigen, auf ihren lokalen Standardordner
  zurück; der nächste Klick auf das Profil verbindet lazy neu. Der
  Entfernen-Knopf einer Verbindungszeile wird jetzt wie bei Favoriten schon
  beim Überfahren der Zeile sichtbar.
- Seitenleiste: Favoriten und Verbindungen bilden eine fortlaufende, gemeinsam
  scrollende Liste; die Abschnittsüberschriften bleiben als Trenner, die
  Trennlinie und die feste Höhenaufteilung entfallen.
- Lokale Pane-Einträge mit nicht eindeutig als UTF-8 darstellbarem Namen bleiben
  sichtbar markiert, können aber nicht mehr geöffnet, gezogen, favorisiert, kopiert,
  verschoben, umbenannt oder in den Papierkorb gelegt werden. Ein echter UTF-8-Name
  mit dem Ersatzzeichen `�` bleibt davon unterscheidbar und normal bedienbar.
- Copy/Move lehnt gleiche Ziele und Ziele innerhalb der Quelle vor dem ersten Scan
  oder Schreibzugriff ab. Ein fehlgeschlagener atomarer Move fällt nur noch bei einer
  echten Dateisystemgrenze (`EXDEV`) auf Copy-and-delete zurück.
- Papierkorb-Recovery hält externe Drift zwischen `planned`-Journal und erstem Claim
  als terminalen Zustand fest, ohne einen nicht existierenden Claim-Pfad zu melden
  oder spätere Queue-Aktionen dauerhaft zu sperren.
- Favoriten werden unter einem Prozess-Lock gelesen und atomar mit Datei- und
  Verzeichnis-Sync ersetzt. Beschädigtes JSON liefert einen sichtbaren Fehler und
  bleibt zusätzlich als Recovery-Kopie erhalten; parallele Änderungen verlieren
  keinen zwischenzeitlich hinzugekommenen Favoriten mehr.
- Die normalen Dateipanes sind nicht mehr dauerhaft als „Quelle“ und „Ziel“
  beschriftet; diese Rollen gehören ausschließlich in die bewusst geöffnete
  Sync-Ansicht. Der frei gewordene Bereich rechts in jedem Pane-Kopf ist jetzt
  eine großzügige Fenster-Ziehfläche, während Navigation und Pfadeingabe
  unverändert bedienbar bleiben.
- Neues App-Icon: Frontalansicht eines Doppeldeckers, dessen zwei Tragflächen
  zugleich die zwei Panes des Dateimanagers andeuten. Das Icon nutzt jetzt den
  macOS-üblichen Squircle-Rand statt der vollen Leinwand und liegt als
  versionierte SVG-Quelle (`src-tauri/icons/icon.svg`) plus daraus gerendertem
  1024er-PNG vor. Für macOS wird zusätzlich ein vollständiges `icon.icns`
  mitgeliefert (`tools/make-icons.sh`), weil der Tauri-Bundler aus einem
  einzelnen 1024er-PNG kein icns erzeugen kann.
- Gewöhnliche Änderungen bleiben bis zu einem ausdrücklich benannten Meilenstein
  oder Releasekandidaten unter `Unreleased`, statt für jeden Commit eine neue
  Produktversion und damit einen vollständig neuen Cargo-Artefaktsatz anzulegen.
- Das Testprofil behält Datei-/Zeileninformationen für Backtraces, verzichtet aber
  auf vollständige Debugsymbole und inkrementelle Testartefakte. Normale Dev-Builds
  behalten ihre vollständige Debugger-Unterstützung und inkrementelle Kompilierung.
- Der vollständige Headless-Gate-Lauf prüft den tatsächlich belegten Debug-Buildcache
  und bereinigt ausschließlich `target/debug`, wenn die gemessene 12-GiB-Grenze
  überschritten ist. Teilgate-Läufe, Gate-Logs und Release-Artefakte bleiben erhalten.
- Die macOS-Mindestversion ist jetzt an genau einer Stelle festgelegt und überall
  gleich: **macOS 14**. Bisher banden `tauri.conf.json`, das Release-Gate und der
  Release-Plan 10.15, während beide READMEs 14+ verlangten. 10.15 war für das
  ausschließlich aarch64 gebaute Artefakt ohnehin gegenstandslos — Apple Silicon
  beginnt bei macOS 11, die Rust-Toolchain klammert 10.15 dort still auf 11.0 hoch,
  und das gebaute Binary trug längst `minos 11.0` gegen ein `Info.plist` mit 10.15.
  Belegt getestet ist ausschließlich 14+; die Herleitung steht in
  [`docs/release-plan.md`](docs/release-plan.md).
- Die Ausführungsklasse „abgesichert" beim Umbenennen verspricht nicht mehr, als
  der Code hält: Sie beschreibt ausschließlich den Schutz des Ziels. Die Quelle ist
  in beiden Klassen über ihren Pfad gebunden — ein zwischen frischem Elternlisting
  und Mutation ausgetauschter gleichnamiger Eintrag würde umbenannt und als Erfolg
  gemeldet. Fähigkeits- und Ergebnisbericht führen diese klassenunabhängige
  Einschränkung jetzt mit, sobald überhaupt eine Klasse verfügbar ist; die
  klassenspezifische Einschränkung steht weiterhin vorn, sodass GUI-Anzeige und
  Vorschau unverändert bleiben. Die CLI nennt sie im Best-Effort-Hinweis und in
  `rename_result`/`capabilities` als JSONL. Verhalten der Umbenennung selbst
  unverändert.
- Die Reichweite der FTPS-Capability-Proben ist jetzt ausdrücklich beschrieben: Der
  Beweis gilt je Server, nicht je Knoten. Probe und spätere Mutation laufen auf
  eigenen frischen Verbindungen, die nur auf dieselbe gemeldete Serveridentität
  geprüft werden — bei mehreren gleich konfigurierten Knoten hinter einem Hostnamen
  kann deshalb ein anderer Knoten den Commit ausführen als den, der die Fähigkeit
  belegt hat. Verhalten unverändert; die Absicherung je Knoten steht als
  Backlog-Punkt für den Fall eines realen Multi-Node-Einsatzes.

### Behoben

- Permanentes WebDAV-Löschen bindet `DELETE` an den unmittelbar davor frisch
  gelesenen starken ETag via `If-Match`. Ein typ- und größengleicher Austausch im
  bisherigen Zeitfenster endet dadurch mit HTTP 412 und bewahrt die neue Datei;
  ein künstlicher echter WebDAV-Gegenlauf hält die Rennbedingung fest.
- `resolve_notary_profile` bricht ohne Eingabe-TTY sofort und mit konkretem Hinweis
  ab, statt in einem unbeaufsichtigten Release-Lauf auf eine unsichtbare Eingabe zu
  warten. Ein Regressionstest belegt zusätzlich, dass `input()` nicht aufgerufen wird.
- Korrektheitsfixes aus dem Code-Review vom 2026-08-06:
  - CLI-Realmodus, Quarantäne und bidirektionale Mutationen halten nun vor ihrer
    ersten Probe denselben Queue-Prozessbesitz wie GUI und Queue-Worker. Das
    unmittelbare CLI-Einreihen schreibt Operation und Claim atomar als
    `Validating`; ein fehlgeschlagener Claim kann keinen später ausführbaren
    `Pending`-Eintrag mehr hinterlassen.
  - Bestätigte Ersetzungen sind in Queue-Journalversion 13 nicht nur an den
    Zielpfad, sondern an Typ, Provider-Version, lokale Inode und bei regulären
    Dateien an den vollständigen BLAKE3-Inhalt gebunden. Ein später neuer oder
    ausgetauschter Eintrag stoppt vor der Mutation. Lokale Ersetzungen halten das
    Altziel dabei per atomarem Namensaustausch bis zur Identitätsprüfung fest;
    Best-Effort-Provider prüfen unmittelbar vor genau einer Mutation erneut.
  - Die erwartete Best-Effort-Klasse wird getrennt für neue Namen und
    Ersetzungen persistiert. Gemischte Auswahlen können dadurch keine erst nach
    der Vorschau schwächer gewordene Copy-New-Wirkung mehr still übernehmen.
  - Die LAN-Suche begrenzt Records und Instanzen über den gesamten Suchlauf,
    dedupliziert früh, verbindet PTR und SRV linear über einen Index und läuft
    außerhalb des Tauri-Runtime-Threads.
  - Ungültige Sync-Ausschlussmuster scheitern jetzt am Anfang der
    Remote-Vorbereitung, bevor Listings, Verbindungen oder mutierende
    Capability-Proben den Zielbestand berühren.
  - WebDAV-Listings akzeptieren eine fehlende optionale `creationdate`, wenn der
    Server sie standardkonform in einem eigenen 404-`propstat` meldet.
    Pflichtfelder und vollständig erfolglose Ressourcen bleiben streng gesperrt.
  - WebDAV-Best-Effort-Ersetzen behandelt auch `201 Created` als erfolgreichen,
    readbackpflichtigen MOVE-Ausgang, wenn das zuvor gelistete Ziel unmittelbar
    vor der Mutation verschwindet. Temp-Verschwinden, Zieltyp, Größe und Inhalt
    werden danach unverändert geprüft.
  - Parallel laufende Ordnergrößenberechnungen binden ihren Cache-Schreibzugriff
    an eine monotone Generation je Pfad. Ein später beendeter alter Lauf kann
    dadurch weder eine erzwungene Neuberechnung überschreiben noch einen bewusst
    geleerten oder entfernten Cache-Eintrag wiederherstellen.
  - Remote-Ordnergrößen prüfen vor jedem Ergebnis neben der Sitzungsgeneration
    auch die konkrete lebende Verbindung im gemeinsamen Pool. Eine ersetzte
    Verbindung mit demselben Zählerstand kann dadurch keine verspäteten Resultate
    mehr an eine Pane senden.
  - Linux-Geräte-IDs enthalten nun die Kernel-Mount-ID aus
    `/proc/self/mountinfo`; ein alter Auswerfen-Klick kann daher kein inzwischen
    neu am selben Pfad eingehängtes Ziel treffen. `gio mount -u` erhält außerdem
    für Blockgeräte und Netzwerkziele immer den konkret gewählten Mountpfad.
  - Ist „Ordner oben anzeigen“ ausgeschaltet, sortiert die gewählte Spalte Ordner
    und Dateien nun über dieselbe transitive Ordnung. Die alphabetische
    Unteroption bleibt gespeichert, ist ohne zusammenhängenden Ordnerblock aber
    sichtbar inaktiv statt einen zyklischen Comparator zu erzeugen.
  - Gefundene SFTP-, SMB- und WebDAV-Dienste im LAN sind jetzt echte Buttons mit
    sichtbarem Fokus und öffnen den bestehenden Verbindungseditor auch per Enter
    oder Leertaste.
  - Nach dem Umsortieren oder Entfernen eines Queue-Eintrags bleibt der
    Tastaturfokus an derselben Operation beziehungsweise am nächsten Eintrag.
    Wiederholte Pfeilaktionen benötigen dadurch keinen erneuten Mausklick.
  - Der echte SFTP-Testlauncher verlangt vor dem Start genau einen Test mit dem
    erwarteten vollständigen Namen und führt ihn anschließend mit `--exact` aus.
    Ein Tippfehler oder umbenannter Test kann das Gate nicht mehr leer bestehen;
    der Root-Identitätstest berücksichtigt außerdem den kanonischen macOS-Pfad
    `/private/tmp`, ohne seine Sicherheitsgrenze unter `/tmp` zu erweitern.
  - Der SFTP-Best-Effort-E2E listet den vollständigen künstlichen Serverroot nach
    erfolgreichen Capability-Proben und nach einem kontrollierten Probe-Fehler.
    Beide Pfade müssen ohne zusätzliche Einträge enden.
  - Pane-Sortierungen halten Cursor und Auswahl an der eindeutigen Zeile statt am
    sichtbaren Namen. Ein echter UTF-8-Ersatzzeichenname wird dadurch nicht mehr
    zusammen mit einem gleich angezeigten, unsicher dekodierten Rohnamen gewählt.
  - SFTP-Capability-Proben reichen Schreibfehler mit ihrer ursprünglichen
    `io::ErrorKind` weiter. Verbindungsabbrüche, Zeitüberschreitungen und
    Speicherfehler werden nicht mehr zum unspezifischen Fehler `Other`.
  - Die kopierfertigen `cargo run`-Beispiele für CLI-Plan und Realmodus nennen
    jetzt den durch die Repo-Sperre verlangten Konfigurationsordner; der Realmodus
    nennt zusätzlich einen getrennten, dauerhaften Auditpfad.
  - Der SFTP-E2E-Launcher validiert seine `fsync`-Umschaltung vor `mktemp`.
    Ein ungültiger Wert beendet den Start, ohne einen temporären Testroot
    anzulegen.
  - Die Linux-Paketprüfung bindet den tatsächlichen 1024×1024-App-Iconpfad des
    Tauri-Bundles. Sie erwartet nicht mehr den veralteten 512er-Pfad und prüft die
    Quelldatei zugleich exakt auf die gebündelte Größe.
- CLI-Tests fassen den echten Konfigurationsordner nicht mehr an. Zehn von 23
  Prozessstarts in `cli/tests/cli_tests.rs` liefen ohne `--config-dir`; die CLI fiel
  damit auf `~/Library/Application Support/com.doppeldecker.desktop` zurück und öffnete
  dort das echte Queue-Journal. Am 2026-08-02 hat ein solcher Lauf das Journal auf eine
  neuere Version migriert, danach blieb `cargo test -p doppeldecker-cli` liegen. Drei
  voneinander unabhängige Sperren beheben das: Jeder Testprozess entsteht jetzt über
  `CliSandbox` und bekommt zwingend einen eigenen Konfigurationsordner sowie ein
  umgeleitetes `HOME`; `DOPPELDECKER_NO_DEFAULT_USER_PATHS` (in `.cargo/config.toml`
  für alle Cargo-Läufe des Repos gesetzt) lässt die CLI ein fehlendes `--config-dir`
  beziehungsweise `--audit` sichtbar mit `invalid_input` abweisen, statt still auf
  Nutzerdaten zu wirken; und ein Testwächter prüft den eigenen Quelltext darauf, dass
  ein CLI-Prozess nur an dieser einen Stelle entsteht. Der Konfigurationsordner wird
  zudem vor jedem Befehl aufgelöst, auch vor denen, die ihn heute nicht brauchen.
- Korrektheits- und Robustheitsfixes aus dem Code-Review vom 2026-08-01:
  - Verschieben mit „Überspringen" verliert keine Daten mehr: Ein bei der
    Ordnerzusammenführung bewusst übersprungenes Kind hält jetzt die gesamte
    Quellkopie zurück, statt dass die Quelle mitsamt dem nie kopierten Eintrag
    gelöscht wird. Zusätzlich führt auch Move (wie Kopieren) zwei gewöhnliche
    Ordner zusammen, statt den ganzen Zielordner zu überspringen — ein
    bestätigtes Provider-Move mit vorab geparkten Konflikten verschiebt damit
    wieder wirklich.
  - Ein fehlgeschlagener Endpunktwechsel einer Pane lässt keine alten Einträge
    des vorherigen Providers mehr stehen; F2 prüft den tatsächlichen
    Cursor-Eintrag statt der Auswahl auf sichere Dekodierbarkeit.
  - Mutierende Capability-Proben (Datei-, Lösch-, Rename-, mkdir-Vorschauen)
    laufen in GUI und CLI jetzt unter derselben Single-Flight-Schranke bzw. dem
    Queue-Prozessbesitz wie die Wirkung selbst; die CLI claimt exakt den eigenen
    neuen Queue-Eintrag und finalisiert Executorfehler, statt fremde Einträge
    als `Interrupted`-Blocker zurückzulassen.
  - Die Bestätigung eines Best-Effort-Downgrades in der Queue bindet die
    Erwartung jetzt wirklich um (eigener Attention-Typ, Journalversion 12) —
    vorher pausierte derselbe Eintrag endlos erneut.
  - Permanentes Remote-Löschen prüft jeden Eintrag unmittelbar vor der Mutation
    erneut gegen Typ und Größe; SFTP-I/O- und WebDAV-Anfrageabbrüche zählen
    konservativ als Verbindungsverlust, damit die Exakt-einmal-Klärung greift
    statt einer blinden Wiederholung.
  - Ein Server, der das optionale `OPTS MLST` ablehnt, aber vollständige
    MLSD-Fakten liefert, ist wieder benutzbar; die Queue-Ansicht zeigt
    wiederherstellbare Quarantäneläufe nicht mehr als „Dauerhaft löschen"
    (snake_case-Vertragstest ergänzt); die Favoriten-Ablage lehnt Bestände
    oberhalb ihrer eigenen Ladegrenze vor dem Schreiben ab; der Finder-Import
    liest die Favoritendatei über genau ein `O_NOFOLLOW`-Handle mit harter
    Größengrenze.
  - Release-/Install-Härtung: `./install` prüft Ticket, Codesignatur und
    Gatekeeper VOR dem Austausch in `/Applications`; `./release` fährt vor
    „RELEASE OK" den vollständigen macOS-Artefaktprüfer (inkl. Ticket und
    Gatekeeper der im DMG montierten App); der DMG-Bau hängt keine fremden
    Volumes mehr per `-force` aus, sondern bindet das eigene Device aus
    `hdiutil attach -plist`, und der Release ist fail-closed an
    `aarch64-apple-darwin` samt `lipo`-Beleg gebunden.
- Korrektheits- und Robustheitsfixes aus dem Code-Review vom 2026-08-05:
  - Sämtliche mutierenden Vorbereitungen halten jetzt vom Öffnen des Queue-
    Prozessbesitzes bis zum Ende der Capability-, Restore- und
    Quarantäne-Proben dieselbe Single-Flight-Schranke wie die spätere Wirkung.
    Das umfasst lokalen Realmodus, SFTP, FTP/FTPS, WebDAV, SMB, generische
    Providertransfers sowie bidirektionale Vorbereitung, Resume und Rollback.
    Die bisherigen bloßen `active_cancel`-Vorabfragen ließen zwischen Prüfung
    und Probe einen parallelen Queue- oder Datei-Lauf zu.
  - Permanentes FTPS-/WebDAV-Löschen behält einen Verbindungsfehler vor dem
    DELETE als `connection_lost`, statt ihn zu `plan_drift` zu vergröbern. Geht
    die Antwort auf den genau einmal gesendeten Löschbefehl verloren, klärt
    höchstens ein identitätsgeprüfter Reconnect den Pfad: fehlend bedeutet
    nachgewiesen gelöscht, unverändert bedeutet nachgewiesen nicht gelöscht,
    jeder andere oder nicht lesbare Zustand `outcome_unclear`. Nach einem
    Reconnect wird kein weiterer Eintrag des Plans gelöscht.
  - Die CLI meldet keinen Fehlschlag mehr für eine Operation, die trotzdem noch
    läuft: Kann sie den soeben selbst eingereihten Queue-Eintrag nicht claimen
    (pausierte Queue oder älterer offener Kopf), entfernt sie ihn wieder. Vorher
    blieb er `Pending` und der GUI-Worker führte ihn später doch aus.
  - Kopieren und Verschieben mit „Überspringen" tasten einen Typkonflikt im
    zusammengeführten Zielordner nicht mehr an: Liegt am Namen eines
    Quellordners eine Datei, wird sie als Blattkonflikt gezählt statt geparkt
    und ersetzt — und das Move behält dadurch seine Quelle.
  - „Neuer Ordner" bricht sichtbar ab, solange eine Pane nach einem
    gescheiterten Listing keinen Pfad hat; zusätzlich weist das Backend einen
    leeren oder relativen Elternpfad mit `invalid_input` ab, statt ihn relativ
    zur Provider-Root aufzulösen.
  - Nach dem Umbenennen zeigt die Oberfläche die Einschränkungen jeder
    Ausführungsklasse an, nicht nur bei Best Effort — die Quelle bleibt auch
    „abgesichert" allein über ihren Pfad gebunden.
  - Der DMG-Bau bindet sein Device innerhalb des geschützten Bereichs und
    behandelt ein gescheitertes Aushängen als Fehler, sodass `hdiutil convert`
    kein noch gemountetes Image komprimiert; `tools/verify-macos-release.sh`
    hängt sein eigenes, per `-plist` gebundenes Device geprüft aus, bevor es
    Erfolg meldet.
  - Dokumentation: `docs/function-audit.md` weist die Provider-Quarantäne für
    SFTP/SMB als an die AddOnly-Probe gebunden aus (bei SFTP inklusive
    `fsync@openssh.com`) und das permanente Löschen nach Providern getrennt;
    `docs/goal-mehrere-fenster.md` nimmt Queue und Queue-Worker ausdrücklich von
    der Schlüsselung je Fensterlabel aus.
- Das Headless-CLI kennzeichnet falsche Provider-Sync-Phrasen und fehlende
  Backup-Bestätigungen jetzt als `confirmation_mismatch`; erfolgreich erneut
  gelesene Abweichungen an Root, Identität, Listing, Plan oder geprüften
  Ziel-Fähigkeiten liefern `plan_drift`. Scheitert die erneute Prüfung selbst,
  bleibt ihr konkreter Fehlercode erhalten. Controller können dadurch zuverlässig
  neu vorbereiten, statt `permission_denied` oder `interrupted` falsch einzuordnen.
- FTPS-Listing scheiterte an verbreiteten ProFTPD-/Plesk-Servern mit
  „FTPS-MLSD ist ungültig: Syntax error: invalid line", sobald ein Eintrag einen
  fünfstelligen Modus wie `UNIX.mode=01777` (Sticky-/Setuid-Bit) trug. Die
  MLSD-Zeilen werden jetzt vollständig aus den eigenen, streng validierten
  Fakten ausgewertet (Typ, Größe, UTC-Änderungszeit mit Kalenderprüfung) statt
  über den nachgiebigeren Fremdparser von suppaftp.

### Verifiziert

- Die interaktive Linux-Abnahme bestand am 2026-08-06 auf Linux Mint 22.2
  (x86_64): AppImage und Debug-App liefen ausschließlich in markierten
  Wegwerfbereichen; F5-Kopie und F6-Verschiebung waren byteidentisch, F2 gab den
  Fokus korrekt zurück, und zwei gleichzeitig geöffnete Fenster zeigten getrennte
  Queue-Zustände. Das DEB wurde als 0.1.122 installiert, im selben Testbereich auf
  0.1.123 aktualisiert und anschließend vollständig deinstalliert. Paket und
  `/usr/bin/doppeldecker` waren danach entfernt, die künstlichen Testdaten
  unverändert und alle Testbäume bereinigt.
- Das neue App-Icon der signierten und notarisierten, unter `/Applications`
  installierten Version 0.1.123 ist am 2026-07-19 im manuellen Test abgenommen.
- Das Fensterziehen an den neuen freien Flächen beider Pane-Köpfe funktioniert im
  regulären macOS-Fenster; der Normalmodus wurde am 2026-07-20 manuell geprüft.

## [0.1.123] - 2026-07-18

### Hinzugefügt

- Größeneinheit ist im Menü „Darstellung" umschaltbar: „Größen: Dezimal (KB, MB,
  GB)" oder „Größen: Binär (KiB, MiB, GiB)". Die Wahl gilt für alle Größenanzeigen
  gleichzeitig — Panes, Plan-Bilanzen, Queue und Quarantäne — und wird lokal
  gemerkt. Voreingestellt ist dezimal unter macOS und binär unter Linux; ohne
  gemerkte Wahl entscheidet also die Plattform.
- Die binäre Einstellung beschriftet mit KiB/MiB/GiB statt KB/MB/GB: „1,00 MB" für
  1.048.576 Byte wäre falsch, und eine falsch beschriftete Zahl ist schlimmer als
  eine ungewohnte. Die Basis ändert ausschließlich die Schreibweise; Vergleich,
  Sortierung, Plan-Digests und Platzprüfungen rechnen unverändert mit rohen Bytes.
- Die Breite der Größenspalte folgt der Einheit, solange sie nicht selbst gezogen
  wurde („999,99 MiB" braucht mehr Platz als „999,99 MB").

## [0.1.122] - 2026-07-18

### Geändert

- Spaltenlayout der Panes (Goal G1): Größe und Änderungsdatum haben feste, je Pane
  gemerkte Breiten mit ziehbaren Trennern im Listenkopf (Maus, Pfeiltasten über die
  separator-Rolle, Doppelklick passt einmalig an den Inhalt an). Beim Verschmälern
  wird nur noch der Name ellipsiert, der Spaltenabstand bleibt konstant.
- Größen wechseln die Einheit, bevor vier Stellen vor dem Komma entstehen
  („999 Byte“, dann zwei Nachkommastellen bis höchstens „999,99 MB“), mit
  Tabellenziffern. Die Basis bleibt wie in den Plan- und Transferbilanzen 1024,
  damit dieselbe Datei überall dieselbe Zahl zeigt.
- Das Änderungsdatum wählt sein Format nach der verfügbaren Spaltenbreite in vier
  Stufen (`17.07.26` → `17.07.26, 12:23` → `17.07.2026, 12:23` → `17. Juli 2026 um
  12:23`) und schaltet beim Ziehen und beim Fensterresize sofort um. Sortiert wird
  weiterhin ausschließlich numerisch über mtime beziehungsweise Bytes.
- Kompaktere Grundabstände in Listen, Kopf- und Fußzeile; Spaltenkopf und Zeilen
  liegen jetzt bündig übereinander, auch wenn ein Listen-Scrollbalken Platz belegt.
- Fehlende Werte (Provider ohne Größe oder Zeitstempel) bleiben sichtbar leer statt
  erfunden; abgeschnittene Werte bekommen Auslassungspunkte, damit „999,99 MB“ nie
  als „999,99“ gelesen werden kann.

## [0.1.121] - 2026-07-18

### Geändert

- Fensterlayout nach Fastra-Vorbild: Programmname und Icon sitzen jetzt links
  über der Seitenleiste (unter den macOS-Ampelknöpfen) und bilden dort die
  Zieh-Fläche des rahmenlosen Fensters; die frühere Kopfleiste entfällt komplett.
- Die Seitenleiste ist flach und platzsparend (kein Kartenrahmen mehr) und läuft
  über die volle Fensterhöhe; rechts nutzen Panes und Sync-Modus die volle Höhe.
- Zwischen Seitenleiste und Arbeitsfläche sitzt ein echter Splitter: per Maus
  ziehbar, per Pfeiltasten bedienbar (separator-Rolle), Doppelklick stellt die
  Standardbreite wieder her; die Breite wird lokal gemerkt.

## [0.1.120] - 2026-07-18

### Geändert

- Sync ist jetzt ein bewusst betretener, eigenständiger Modus: Menü
  „Befehle → Synchronisieren …“ (⇧⌘S, Linux Ctrl+Shift+S) öffnet eine eigene
  Ansicht über den Panes mit Quelle/Ziel-Chips, Plan-Vorschau als Hauptfläche,
  Optionen-Panel (Richtung inkl. „In beide Richtungen“ = Bidirektional,
  Wirkungs-Toggles, Vergleich, Metadatenprofil) und Bilanzleiste. Lokale
  Endpunkte planen bei jeder Optionsänderung sofort neu; Remote-Vorschau bleibt
  wegen Identitäts-/Permit-Bindung ein bewusster Klick. Alle Sicherheits-Gates
  (Dry-Run-Grundsatz, Mirror-Grenzen, Realmodus-Phrasen) sind unverändert.
- Kopfleiste entrümpelt: Theme-Switch und sämtliche Sync-Steuerung entfernt;
  die Darstellung wird nur noch über das Menü „Darstellung“ gewählt. F5/F6/
  Umbenennen liegen jetzt im neuen Menü „Befehle“; der Realmodus-Einstieg lebt
  ausschließlich im Sync-Modus.
- Die Dock-Leiste zeigt weiterhin Fortschritt und Statusmeldungen; die
  Plan-Vorschau erscheint dort nur noch außerhalb des Sync-Modus (z. B. bei
  Queue-Freigaben).

## [0.1.119] - 2026-07-17

### Geändert

- Seitenleiste für schnelle Dateimanager-Navigation überarbeitet: Favoriten und Verbindungen
  zeigen die Art des Eintrags am Symbol. Bekannte Systemordner (Benutzerordner,
  Desktop, Dokumente, Downloads, Bilder, Musik, Filme) erhalten eigene Icons;
  die vier Protokoll-Sektionen sind zu einer „Verbindungen“-Liste mit farbigen
  Protokoll-Icons (SFTP-Schlüssel, FTPS-Schild, WebDAV-Globus, SMB-Server)
  zusammengeführt.
- Ein gemeinsames „+“ öffnet zuerst die Protokollwahl und danach den bekannten
  Editor; die zweistufigen Discovery-/Pinning-Abläufe (Hostkey, Zertifikat,
  Server-GUID) und die Schlüsselbund-Regeln bleiben unverändert.

### Hinzugefügt

- Ordner lassen sich per Drag & Drop aus den Panes in die Favoriten ziehen und
  dort per Drag umsortieren (neues Core-`move_to` samt Command `move_favorite`).
  Nur lokale, nicht-symlinkte Ordner sind Drag-Quellen; die Favoritenliste zeigt
  ein deutliches Drop-Ziel.

## [0.1.118] - 2026-07-17

### Behoben

- Das Fenster lässt sich unter macOS wieder mit der Maus an der Kopfleiste ziehen.
  Zwei Ursachen: `core:window:default` enthält in Tauri 2.11.5 kein
  `allow-start-dragging`, die ACL lehnte das Ziehen also grundsätzlich ab; die
  Capability des Hauptfensters erlaubt es jetzt explizit. Zusätzlich wirkt ein
  bloßes `data-tauri-drag-region` nur auf dem direkt getroffenen Element, und die
  Kopfleiste war fast vollständig von Kind-Containern bedeckt; alle
  nicht-interaktiven Flächen der Leiste tragen das Attribut jetzt selbst.

### Hinzugefügt

- `build-release.py` legt die fertige macOS-App nach dem Build zusätzlich
  doppelklickbar als `Doppeldecker.app` in den Projekt-Root (per `ditto`,
  signaturerhaltend; beim öffentlichen Build erst nach der Notarisierung).
- Neuer Browserfall tastet die Kopfleiste rasterförmig ab und schlägt an, sobald
  nicht-interaktive Punkte ohne Drag-Region entstehen.

## [0.1.117] - 2026-07-16

### Behoben

- Der öffentliche macOS-Buildpfad stapelt jetzt auch das ausgelieferte DMG. Tauri
  notarisiert und stapelt nur die `.app` und signiert das DMG anschließend lediglich,
  sodass `verify-macos-release.sh` an seiner eigenen Staple-Prüfung scheiterte. Der
  Fehler blieb bisher unentdeckt, weil der Pfad nie mit echten Zugangsdaten lief.
  `build-release.py` reicht das DMG nun über ein notarytool-Schlüsselbundprofil ein
  und heftet das Ticket an; ein Preflight belegt das Profil vor dem langen Build.
  Das Passwort erscheint dabei nie als Kommandozeilenargument.

### Verifiziert

- Erster vollständig grüner öffentlicher macOS-Lauf: App und DMG von Apple mit Status
  `Accepted` notarisiert und gestapelt, Developer-ID-Signatur, Datenschutz-/Pfadscan,
  DMG-Baumgleichheit, Gatekeeper-Urteil `Notarized Developer ID` und Prüfsumme.
- Release-Sandbox-Smoke mit genau diesem Bundle: eine synthetische 35-B-Datei lief im
  markierten Wegwerfbereich byteidentisch ans Ziel, ohne Temp- oder Quarantänerest.
- Linux-Pakete 0.1.117 auf dem Linux-Testhost gebaut und paketformatgenau geprüft: DEB amd64 und
  AppImage x86_64, Datenschutz-/Pfadscan und SHA-256-Prüfsummen.
- macOS: 274 Kern-Testfälle und 17 Browserfälle grün.

## [0.1.116] - 2026-07-13

### Dokumentation

- Releaseplan, Roadmap und Projekthandoff unterscheiden wieder konsistent zwischen
  früheren Linux-Buildblockern und dem heute grünen nativen App-/Paketbau.
- Der bereits ausschließlich in der markierten macOS-Sandbox bestandene
  GUI-Papierkorbtest ist auch im zugehörigen Sicherheitsentwurf als abgeschlossen
  dokumentiert.
- Die Linux-Paketgrenze ist festgelegt: unterstützt wird die Distributionbasis
  Linux Mint 22.2/Ubuntu 24.04 mit WebKitGTK 4.1 und GTK 3; weitere Distributionen
  benötigen einen eigenen Gegenlauf.

### Getestet

- Der Ad-hoc-signierte macOS-Kandidat 0.1.115 bestand App-/DMG-Bytevergleich,
  Signatur, DMG-Prüfung, Datenschutzscan und versionsgebundene SHA-256-Prüfsumme.

## [0.1.115] - 2026-07-13

### Release

- Der Linux-Bundlelauf extrahiert DEB und AppImage und prüft fail-closed Version,
  Architektur, Laufzeitabhängigkeiten, Desktop-Integration, ausführbares Binary und
  dessen auflösbare dynamische Bibliotheken.
- Der Datenschutzscan prüft nun neben dem letzten Cargo-Release-Binary auch die
  tatsächlich in DEB und AppImage eingebetteten, von Tauri getrennt gepatchten
  Binaries auf lokale Build-Pfade.
- Datenschutzscan und Prüfsummendatei binden ausschließlich Artefakte der aktuellen
  Workspace-Version; Vorversionspakete können für Upgrade-Tests daneben bestehen.
- Der Sandbox-Launcher startet unter Linux mit `--release` exakt das
  versionspassende AppImage. `--installed` hält auch DEB-Installations-, Upgrade- und
  Recovery-Gegenläufe innerhalb der markierten Wegwerfsandbox.

### Getestet

- Der isolierte Linux-Testhost-Build erzeugte ein gültiges amd64-DEB und x86_64-AppImage;
  Extraktion, Paketmetadaten, WebKitGTK-/GTK-Abhängigkeiten, Desktop-Datei, Icon,
  Binaries, Bibliotheksauflösung, Datenschutzscan und SHA-256-Prüfsummen sind grün.

## [0.1.114] - 2026-07-13

### Behoben

- Bidirektionale Paare mit einem SMB-Endpunkt binden POSIX-Modi nicht mehr, wenn das
  SMB-Profil sie weder lesen noch schreiben kann. Inhalt, Typ und darstellbare
  Metadaten bleiben unverändert digestgebunden.
- Der echte Samba-Gegenlauf prüft nun eine tatsächliche Mirror-Entfernung und trennt
  Löschrichtungen anhand des belegten Verzeichnis-FLUSH-Vertrags.

### Getestet

- Der isolierte Linux-Testhost-E2E bestand SMB 3.1.1 mit aktiver Signatur und Verschlüsselung,
  1.500 gepagte Einträge, Reconnect, Upload/Download, Quarantäne, bidirektionale
  Capability-Matrix und persistente Queue.

## [0.1.113] - 2026-07-13

### Release

- Ein eigener öffentlicher macOS-Buildpfad wählt fail-closed genau eine installierte
  Developer-ID-Application-Identität oder die explizite Tauri-Umgebungsvariable.
- Der Build startet nur mit einem vollständigen offiziellen Notarisierungssatz,
  lehnt Ad-hoc-Signaturen ab und führt danach Binärpfad-, Signatur-, Staple-, DMG-
  und Prüfsummenprüfung aus. Secrets gelangen weder in Argumente noch Projektdateien.
- Der lokale Preflight bestand mit simulierten vollständigen und unvollständigen
  Zugangssätzen. Die echte Apple-Notarisierung bleibt bis zur externen
  Credential-Konfiguration gesperrt.

## [0.1.112] - 2026-07-13

### Release

- Die macOS-Verifikation verlangt nun vollständige Bytegleichheit zwischen dem im
  Sandbox-Smoke-Test ausgeführten `.app`-Bundle und dem tatsächlich im DMG
  enthaltenen Bundle. Abweichende Binär-, Ressourcen-, Plist- oder Signaturdateien
  stoppen den Release fail-closed.
- Der 0.1.112-Kandidat bestand diese Prüfung, Binärscan, Signatur, DMG und Prüfsumme
  sowie einen isolierten byteidentischen 5-B-Queue-Sync im echten Release-Fenster.

## [0.1.111] - 2026-07-13

### Dokumentation

- README, Roadmap und Handoff unterscheiden nun konsistent zwischen abgeschlossenen
  Zwischenstufen, bewusst capability-gesteuert gesperrten Remote-Zielrichtungen und
  den drei externen Release-Gates SMB-E2E, Linux-GUI/-Pakete sowie öffentlicher
  macOS-Signierung/Notarisierung.
- Der vollständig erledigte Qualitäts-Backlog ist nicht länger als verbleibende
  Produktarbeit überschrieben.

## [0.1.110] - 2026-07-13

### Hinzugefügt

- `tools/run-test-sandbox.sh --reuse <Testbereich>` öffnet für Recovery- und
  Upgrade-Gegenläufe ausschließlich einen vorhandenen markierten Bereich mit
  getrennten `quelle`-/`ziel`-Ordnern erneut; alle Backend-Schranken bleiben aktiv.

### Getestet

- Ein absichtlich beschädigtes Queue-v8-Journal führte im wiederverwendeten
  Release-Sandbox-Profil sichtbar zu `Queue !` und einem Startfehler. Das Journal
  wurde nicht repariert, Quelle und Ziel blieben byteidentisch.
- Der pfadneutrale arm64-App-/DMG-Kandidat 0.1.110 bestand Signatur, DMG, Binärscan,
  Prüfsumme und einen isolierten byteidentischen 5-B-Release-Sync. Abschluss:
  macOS 12 App-Tests und App-Clippy; Linux-Testhost 190 Kernfälle und Kern-Clippy.

## [0.1.109] - 2026-07-13

### Behoben

- Markierte GUI-Testläufe verwenden für Queue, Favoriten, Verbindungsprofile,
  Baselines, Audit und Papierkorb-Recovery nun ausschließlich `app-config` innerhalb
  ihrer validierten Sandbox. Die normale Nutzerkonfiguration bleibt unangetastet.
- Die Release-Prüfung verbietet neue direkte Tauri-Zugriffe auf den nativen
  App-Konfigurationsordner außerhalb des zentralen Sandbox-Helfers.
- Der pfadneutrale arm64-App-/DMG-Kandidat bestand Signatur, DMG, Binärscan,
  Prüfsumme und einen isolierten byteidentischen 17-B-Release-Sync. Abschluss:
  macOS 12 App- und 17 Browserfälle sowie App-Clippy; Linux-Testhost 190 Kern- und
  7 CLI-Fälle sowie Kern-/CLI-Clippy.

## [0.1.108] - 2026-07-13

### Behoben

- Release-Builds neutralisieren lokale Workspace-, Cargo-, Rustup- und
  Benutzerpfade über kodierte Rust-Flags. Eine Artefaktprüfung stoppt macOS- und
  Linux-Bundles, falls dennoch ein lokaler Build-Pfad im Binary verbleibt.
- Der Linux-Bundlepfad erzeugt nun wie macOS SHA-256-Prüfsummen.
- Das pfadneutrale arm64-App-/DMG-Paar bestand Binärscan, Signatur-, DMG- und
  Prüfsummenprüfung sowie einen byteidentischen Sync im markierten Release-Sandbox.
- Abschluss: macOS 188 Modul-/24 Dateiops-/57 Sync-/7 CLI-/11 App-Tests,
  FTPS-/WebDAV-E2E, 2 Property- und 17 Browserfälle sowie Workspace-Clippy;
  Linux-Testhost 190/24/60/7, FTPS-/WebDAV-E2E, 2 Property-Fälle und Kern-/CLI-Clippy.

## [0.1.107] - 2026-07-13

### Geändert

- `sync-toolbar.js` kapselt Richtungs-, Modus-, Vergleichs- und Metadatenwahl sowie
  die Toolbar-Ereignisse. `main.js` enthält nur noch Modulkomposition und App-Start.
- Die ausgelagerte native Metadatenwahl lief im echten markierten Sandbox-Fenster
  unter der strikten CSP und aktualisierte die sichtbare Backup-Grenze korrekt.
- Abschluss: 17 Browser- und 11 App-Tests sowie App-Clippy auf macOS; 190
  Kern-Modultests und Kern-Clippy auf dem Linux-Testhost.

## [0.1.106] - 2026-07-13

### Hinzugefügt

- Zwei Remote-Panes können nun denselben generischen, capability-geprüften
  Provider→Provider-Vertrag des Kerns über Direktlauf und persistente Queue nutzen.
- Queue-Journal v8 persistiert für `ProviderSync` nur öffentliche Endpunktbindungen,
  erwarteten Plan und Secret-Referenzen; Sitzungen, Secretwerte und Permits bleiben
  ausschließlich im RAM.

## [0.1.105] - 2026-07-13

### Geändert

- Lokale/Remote-Vorschau sowie Dry-Run-/Sandbox-Planprüfung und sichere lokale
  Queue-Einreihung liegen in `one-way-sync-ui.js`; Mirror-Gates und Planbindung
  bleiben unverändert im Backend.

## [0.1.104] - 2026-07-13

### Geändert

- Der lokale sowie SFTP-/FTPS-/WebDAV-/SMB-Realmodus-Assistent liegt in
  `real-mode-ui.js`; nicht serialisierbare Permits und alle Sicherheitsproofs bleiben
  unverändert im Backend.
- Ein Browser-Gegenfall führt einen SFTP-Direktlauf durch öffentliche
  Identitätsanzeige, Einmalphrase und Eventabschluss aus, ohne Secrets zu verwenden.

## [0.1.103] - 2026-07-13

### Behoben

- Die lokale Mirror-Zielnamenbestätigung nutzt den gemeinsamen definierten
  `baseName`-Helfer; ein Browser-Gegenfall führt die Löschwirkungs- und
  Zielnamenbestätigung bis zur sicheren Queue-Einreihung aus.

## [0.1.102] - 2026-07-13

### Geändert

- Paaridentität, Konfliktentscheidungen, Laufbestätigung und Recovery des
  bidirektionalen Assistenten liegen in `bidirectional-ui.js`.

### Behoben

- Die bidirektionale Paarbenennung besitzt wieder einen definierten, lokal
  gekapselten Basename-Helfer; der bisher ungetestete Vorbereitungsklick ist jetzt
  als Browser-Gegenfall abgedeckt.
- Laufende bidirektionale Queue-Einträge werden bis zum ersten Fortschrittsereignis
  als Sync und nicht mehr fälschlich als Verschiebeoperation angezeigt.

## [0.1.101] - 2026-07-13

### Geändert

- Tauri-Queue-, Sync-, Dateioperations- und Menüevents werden in
  `event-controller.js` auf bereits vorhandene UI-Aktionen abgebildet.

## [0.1.100] - 2026-07-13

### Geändert

- Dock, Bedienelementsperre und gemeinsamer Datei-/Sync-Fortschritt liegen in
  `operation-ui.js`.

## [0.1.99] - 2026-07-13

### Geändert

- Die nativen SFTP-/FTPS-/WebDAV-/SMB-Editoren liegen in
  `connection-editors.js`; öffentliche Identitätsprüfung und anschließende
  Schlüsselbundspeicherung bleiben strikt getrennte Schritte.

## [0.1.98] - 2026-07-13

### Geändert

- Geheimnisfreie SFTP-/FTPS-/WebDAV-/SMB-Profillisten, aktive Markierung,
  Navigation und Entfernung liegen in `connections-ui.js`; die nativen
  Secret-Editoren bleiben getrennt.

## [0.1.97] - 2026-07-13

### Geändert

- Lokale Schnellzugriff-Favoriten liegen mit eigenem Zustand und eigener
  Seitenleistenverdrahtung in `favorites-ui.js`.

## [0.1.96] - 2026-07-13

### Geändert

- Quarantäneläufe, Einträge, Wiederherstellung und explizites Leeren werden im
  eigenen `quarantine-ui.js` dargestellt; die Sicherheitsentscheidungen bleiben im
  Backend.

## [0.1.95] - 2026-07-13

### Geändert

- Die persistente Queue-Oberfläche liegt mit eigenem UI-Zustand in `queue-ui.js`.

### Behoben

- Laufende SMB-Sync-Einträge verwenden nun die Sync-Fortschrittsanzeige statt der
  irreführenden Move-Anzeige.

## [0.1.94] - 2026-07-13

### Geändert

- Globale Tastaturkürzel und native Menüereignisse werden im eigenen
  `input-controller.js` auf injizierte Fachaktionen abgebildet.

## [0.1.93] - 2026-07-13

### Geändert

- Pane-Zustand, Listing-Race-Schutz, Auswahl, Navigation, Sortierung, Type-ahead,
  Virtualisierung und DOM-Verdrahtung liegen im eigenen ES-Modul
  `pane-controller.js`.

## [0.1.92] - 2026-07-13

### Geändert

- Der Tauri-Einstieg enthält nur noch Modulverdrahtung und Tests; strukturierte
  App-Fehler, Sandbox-Grenzen und die native Menüleiste liegen in eigenen Modulen.

## [0.1.91] - 2026-07-13

### Hinzugefügt

- Sichere lokale und providerübergreifende Einweg-Synchronisation für SFTP, FTPS,
  WebDAV und SMB mit vollständigen Listings, Identitätsbindung und Quarantäne.
- Bidirektionaler Drei-Wege-Sync mit Baselines, Konfliktentscheidungen, Tombstones,
  Crash-Journal, Resume und Rollback für freigegebene Zielrichtungen.
- Persistente Operations-Queue, Headless-CLI, Realmodus-Assistent und bedienbare
  Quarantäne.
- Reproduzierbare Playwright-Mock-Suite und Property-/Fuzz-Tests.

### Sicherheit

- Descriptor-relative lokale Pfade, Symlink-/Traversal-Schutz, atomare Temp-/Rename-
  Commits, BLAKE3-Planbindung und fail-closed Providerfähigkeiten.
- Secrets werden ausschließlich über Schlüsselbund-/Agent-Referenzen angebunden und
  nicht in Profilen, Plänen, Queue-Journalen oder Logs persistiert.
- Strikte WebView-CSP ohne Inline-Skripte oder Inline-Styles.

### Geändert

- Korrektheitskritische Logik bleibt vollständig GUI-frei im Core; lange öffentliche
  Positionsargumentlisten wurden durch explizite Ausführungskontexte ersetzt.
- Das Frontend ist schrittweise in native ES-Module zerlegt und virtualisiert große
  Pane-Listen mit barrierearmer Tastatur- und Fokussteuerung.
- Die noch unveröffentlichte Bundle-Kennung lautet `com.doppeldecker.desktop`, damit
  sie auf macOS nicht mit der reservierten `.app`-Endung kollidiert.
