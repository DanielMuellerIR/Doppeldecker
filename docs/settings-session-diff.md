# Einstellungen, Sitzungswiederherstellung und Dateivergleich

Stand: 2026-10-19.

## Globale Einstellungen

Der Dialog „Doppeldecker → Einstellungen …“ verwaltet Theme, eines von sechs
App-Icons, dezimale oder binäre Größen, automatische lokale Ordnergrößen,
LAN-Suche, Sitzungswiederherstellung und das externe Diff-Werkzeug. Theme und
Größeneinheit bleiben zusätzlich im Menü „Darstellung“. Ordnergebundene Spalten,
Sortierung, Dichte und Zeilenbild bleiben im Darstellungsdialog des jeweiligen
Panes. Vorgaben sind automatisches Theme, blaues Icon, dezimale Größen unter
macOS beziehungsweise binäre unter Linux, ausgeschaltete automatische Ordnergrößen
und Sitzungswiederherstellung sowie eingeschaltete LAN-Suche. Das Diff-Werkzeug
beginnt mit Xcode; seine tatsächliche Verfügbarkeit wird getrennt geprüft.

Das Backend speichert globale Einstellungen atomar in `desktop-settings.json` im
App-Konfigurationsordner. Eine beschädigte oder verlinkte Datei wird sichtbar
gemeldet und nicht überschrieben. Änderungen werden feldweise zusammengeführt,
damit ein älterer Dialog in einem zweiten Fenster keine fremde Wahl zurücksetzt.
Der blaue Himmel ist die Vorgabe. Unter macOS ändert die Wahl das laufende Dock-Icon und setzt über
`NSWorkspace` ein dauerhaftes Findericon am App-Bundle. Wegen des Finder-/Dock-Caches
kann nach dem Beenden noch das blaue Standardicon erscheinen. Abhilfe: Im
Kontextmenü des Dockicons „Optionen → Im Finder anzeigen“ wählen; dadurch wird die
Anzeige aktualisiert. Diese Cache-Eigenheit wird als bekannte Einschränkung
akzeptiert und nicht weiter verfolgt. Die signierten Ressourcen
unter `Contents` bleiben unverändert; macOS ergänzt Custom-Icon-Metadaten am Bundle. Die normale
Signaturprüfung und Gatekeeper akzeptieren diese; `codesign --strict` beanstandet
die zusätzlichen Finder-Metadaten. Schreibgeschützte Bundles können nur das
Laufzeiticon übernehmen. Auf Linux erhalten die laufenden Fenster das gewählte
Icon. Ein gescheitertes Icon bricht weder den App-Start noch das Speichern
ab. Beim Start übernimmt das Hauptfenster den einmaligen Queue-Fehler. Ein synchron
zurückgemeldeter Fehler beim Speichern (fehlender Verzeichnis-fsync, Icon setzen)
geht als `settings-error` an das auslösende Fenster; das Frontend zeigt ihn unter
„Einstellungen“ an und lässt den Queue-Zähler unberührt, der nur auf `queue-error`
reagiert. Fehler beim asynchronen macOS-Dock-Icon oder beim Icon eines neu gebauten Fensters werden
nur mit dem Kontext `Fenstericon` und dem Fehlercode in `errors.jsonl` protokolliert.
macOS liest die aktuelle Iconwahl erst im ausführenden Hauptthread-Task. Ein
verspäteter Auftrag aus einem anderen Fenster kann dadurch keine ältere Auswahl
wiederherstellen; die Regression ist im [Review-Abschluss](release-validation.md)
belegt.
Beim ersten Start dieser Version übergibt das
Frontend Theme, Größeneinheit, automatische lokale Ordnergrößen und LAN-Suche aus
den bisherigen lokalen Browser-Einstellungen als gewöhnlichen Patch an das Backend;
danach ist `desktop-settings.json` maßgeblich. Scheitert dieses erste Schreiben,
startet die App trotzdem mit den gelesenen Werten und meldet den Fehler.

Die Datei ist bewusst streng: Ein unbekanntes Feld gilt als Lesefehler, und dann
speichert diese App-Version keine Einstellung mehr (fail-closed, damit eine ältere
Version keine Werte einer neueren still verwirft). Wer ein Feld ergänzt, erhöht
deshalb `version` und schreibt eine Migration für den alten Stand.

### Icon-Persistenz: Prüfung 2026-10-12

174 App-Tests, App-Clippy mit `-D warnings` und die Formatprüfung bestanden.
Der native Regressionstest prüft Wechsel und Rückwechsel des dauerhaften Icons,
unveränderte Ressourcen sowie ein nicht beschreibbares Ziel. Auf einer isolierten
Kopie der notarisierten App wurde nach Ende des setzenden Prozesses das Icon über
`NSWorkspace.icon(forFile:)` in einem neuen Prozess ausgelesen und visuell geprüft.
Normale Signaturprüfung und Gatekeeper bestanden; die strikte Paketprüfung meldet
wie oben beschrieben Custom-Icon-Metadaten.

### Visuelle Dock-Abnahme 2026-10-13: abgeschlossen mit Cache-Einschränkung

Geprüft wurde die Developer-ID-signierte, notarisierte Version 0.1.123 aus
dem damaligen Produktstand auf macOS 26.7.1 (25G241), gestartet über den Sandbox-Launcher
mit isolierter Konfiguration.
Die tatsächliche App lag unter `/Applications/Doppeldecker.app`.

1. Im Einstellungsdialog von „Blauer Himmel“ zu „Gewitter und Goldlicht“ wechseln:
   Auswahl und laufendes Dockicon zeigten das gewählte Motiv.
2. Zu „Polarlichter“ wechseln: Auswahl und laufendes Dockicon zeigten auch dieses
   Motiv korrekt.
3. Die App über „Optionen → Im Dock behalten“ anheften und regulär über ihr
   App-Menü beenden: Nach bestätigtem Prozessende zeigte das Dock erneut
   „Blauer Himmel“. Eine zweite Aufnahme bestätigte den Rückfall.

Die gespeicherte Einstellung blieb `aurora`. Ein neuer Prozess las über
`NSWorkspace.icon(forFile:)` weiterhin das korrekte Polarlichtmotiv am Bundle aus;
dieses wurde ebenfalls visuell geprüft. Normale Signaturprüfung und Gatekeeper
bestanden nach dem Iconwechsel. Der native Persistenztest allein belegt damit
nicht die tatsächliche Dock-Anzeige nach App-Ende.

Ergänzende Nutzerbeobachtung vom 2026-10-13: „Optionen → Im Finder anzeigen“ im
Kontextmenü des Dockicons aktualisiert die Anzeige auf das gespeicherte Motiv.
Der zunächst als offener Fehler erfasste Rückfall wird deshalb als bekannte
Finder-/Dock-Cache-Eigenheit dokumentiert und akzeptiert. Eine weitere Untersuchung
oder Produktänderung ist nicht vorgesehen. Diese Abhilfe stammt aus der
Nutzerbeobachtung; der oben beschriebene Testlauf wurde nicht erneut ausgeführt.

Die temporäre Dock-Anheftung wurde entfernt und die ursprüngliche
Installation unverändert wiederhergestellt; Produktcode wurde bei dieser Abnahme
nicht geändert.

## Sitzungswiederherstellung

Die abschaltbare Wiederherstellung speichert je Fenster beide Pane-Endpunkte,
aktive Seite, Fensterposition, Größe und Maximierungszustand. Format 2 speichert
zusätzlich Navigation, Auswahl und Darstellung mehrerer Tabs je Pane. Der
[Tab-Vertrag](pane-tabs.md) beschreibt Bedienung, Migration aus Format 1 und Grenzen.
Ein Remote-Endpunkt enthält nur Protokoll, Profil-ID, Revision und Pfad; den Profilnamen
löst das Frontend beim Start aus dem Profil auf. Kennwörter, Schlüssel,
Provider-Handles, Queueeinträge, Vorbereitungen und Permits gehören nie in diese
Datei. Eine gespeicherte Position wird nur übernommen, wenn das Fenster damit auf
einem angeschlossenen Monitor sichtbar wäre; sonst gelten nur Größe und
Maximierung. Ein gespeichertes Fenster, das sich nicht bauen lässt, fällt aus der
Sitzung, statt bei jedem Start erneut zu scheitern. War das Hauptfenster einzeln
geschlossen, wird nur ein Nebenfenster ohne offene Queue zum neuen Hauptfenster.

Die validierten Startgeometrien werden vor dem ersten Fensterbau separat im RAM
gebunden und je Fenster einmalig übernommen. Dieser kurze Zugriff wartet nicht am
Mutex des schreibenden Einstellungsstores. Der vollständige Live- und Dateistand
bleibt erhalten; Remember-Reihenfolge, atomarer Write und Beenden-Barriere ändern
sich dadurch nicht. Neue Fenster ohne gespeicherte Geometrie verwenden weiterhin
ihre Standarddarstellung. [Prüfbericht](release-validation.md).

Beim Neustart liest Doppeldecker zuerst die gespeicherten Profile und ersetzt eine
alte Profilrevision durch den aktuellen gespeicherten Stand. Das erste Listing
baut die Providerverbindung über den normalen Identitäts- und Pin-Vertrag neu auf.
Fehlt das Profil oder scheitert der Verbindungsaufbau, bleibt der Fehler in dem
betroffenen Fenster sichtbar. Unfertige Operationen folgen unabhängig davon der
bestehenden Queue-Recovery.

Abgebrochene Einstellungswrites können `.desktop-settings-<Zufallskennung>.tmp`
hinterlassen. Beim Start werden diese Dateien weder als Einstellungen übernommen
noch nach ihrem Namen bereinigt. Dateipräfix und Prozesssperre ersetzen keinen
beim Erzeugen gebundenen Besitzbeleg. Ebenso bleiben frühere Fensterjournale mit
Historie und möglichen Recoverypfaden erhalten. Eine automatische Alterslöschung
ist nicht vorgesehen; [Prüfung der Aufbewahrung](release-validation.md).

## Dateivergleich

„Datei → Dateien vergleichen“ erwartet in jedem Pane genau eine reguläre Datei.
Der Backend-Command validiert beide Pfade gegen ihre Pane-Roots, löst Provider und
Profilrevisionen auf und erstellt unter der Single-Flight-Schranke zwei vollständig
übertragene, BLAKE3-rückgelesene Momentaufnahmen. Erst diese unveränderlichen Kopien
gehen als getrennte Prozessargumente an das externe Werkzeug. Ein dort angebotener
Merge kann deshalb keine Pane-Datei überschreiben.

SFTP verwendet dabei dieselbe begrenzte interaktive Inhaltsklasse wie das Öffnen,
einschließlich der vollständigen Inhalts- und lokalen Hashreadbacks. Vorbereitung
und Ergebnis benennen deren Best-Effort-Pfadrisiko ausdrücklich. Daraus entstehen
weder strikte Plannerbelege noch SFTP-Schreibrechte. Beide Auswahlen und
Profilrevisionen werden vor dem ersten Providerzugriff validiert. Verbindung und
Downloads halten die abbrechbare Ausführungsschranke des verursachenden Fensters;
andere Fenster bleiben unabhängig.

Nach dem letzten Abbruchcheck übernimmt das Werkzeug. Die Oberfläche zeigt diese
Übergabephase gesondert und bietet dort weder über den Knopf noch über Escape eine
Rücknahme an. Ein unterbrochener Helfer-I/O gilt als unbestätigte Übergabe, nicht
als wirkungsloser Vorbereitungsabbruch. Das Terminalereignis wird vor dem nächsten
Queueworker versandt; eine spätere Command-Antwort überschreibt dessen Anzeige
nicht. Vor der Übergabe erzeugt ein Abbruch kein Vergleichsfenster und entfernt
nur die eigene unvollständige Momentaufnahme.

Im normalen Betrieb liegen die Momentaufnahmen im App-Cache und werden vor dem
nächsten Vergleich entfernt, sobald sie älter als sieben Tage sind; bis dahin liegen
dort vollständige Kopien der verglichenen Dateien. Eine über
`tools/run-test-sandbox.sh` markierte GUI-Sandbox legt auch diesen Cache unter ihrer
eigenen Wurzel an; der Test berührt dadurch weder normale Momentaufnahmen noch deren
Aufräumfrist.

Auf macOS unterstützt die App Xcodes `opendiff` sowie BBEdits `bbdiff`, sofern der
jeweilige feste Helfer im App-Bundle vorhanden ist. Xcode FileMerge ist die Vorgabe.
Fastra ab Version 1.119.0 wird verfügbar, wenn sein Bundle-Helfer mit
`--capabilities --json` Protokoll 1 und `fileDiff`, `readOnly`, `focusDiff`,
`labels` sowie `existingInstanceIpc` bestätigt. Die Prüfung läuft im Hintergrund
mit einer Frist von zwei Sekunden und öffnet kein Fenster; sie gehört zum
Einstellungsdialog. Der Vergleich selbst prüft nur, ob der Helfer vorhanden ist,
und verlässt sich auf dessen Exitcode.

```text
/Applications/Fastra.app/Contents/Helpers/fastra-diff \
  --read-only --focus-diff \
  --left-label <name> --right-label <name> -- <linke-datei> <rechte-datei>
```

Der Helfer spricht die laufende Fastra-Instanz über lokale IPC an oder startet sein
App-Bundle. Jeder Aufruf öffnet einen eigenen schreibgeschützten Vergleich ohne
Projektseitenleiste. `--focus-diff` aktiviert dieses Fenster. Normale Fenster und
persistierte Einstellungen bleiben unberührt. Die Namen stammen aus den Panes;
die beiden Pfade zeigen auf die geprüften Momentaufnahmen.

Doppeldecker wartet im Hintergrund auf Exit 0: Erst dann hat Fastra das Fenster
angelegt; die Darstellung des Vergleichs kann noch laufen. Fastras eigene
Übergabefrist beträgt zehn Sekunden, Doppeldecker gibt dem Helfer zwölf Sekunden.
Exit 2–5 melden ungültige Argumente, unlesbare Dateien, ein nicht unterstütztes
Protokoll oder einen fehlgeschlagenen App-Start. Exit 6, Prozessabbruch und Timeout
melden eine unbestätigte Übergabe mit dem Hinweis, dass ein Fenster bereits offen
sein kann. Doppeldecker wiederholt einen solchen Aufruf nicht automatisch.

Die Backend-Tests prüfen Protokoll/Fähigkeiten, defekte und hängende Helfer,
Exitcodes sowie die unveränderte Argumentübergabe mit Unicode, Leerzeichen,
Shell-Sonderzeichen und Labels mit führenden Bindestrichen. Die künstliche
Helferlogik läuft als festes Argument von `/bin/sh`, ohne neue ausführbare
Skriptdateien anzulegen oder eine GUI zu öffnen. Der produktive Bundle-Helfer
wird weiterhin direkt und ohne Shell gestartet.

### Verifikation 2026-09-05

148 App-Tests, die UI-Suite, Formatprüfung und App-Clippy bestanden. Der echte
Fastra-1.119.0-Helfer aus dem Entwicklungs-Bundle bestätigte Protokoll 1 und die
Fehlercodes 2, 3 und 4. Der freigegebene GUI-Test mit zwei Wegwerfdateien bestätigte
Exit 0, genau einen dargestellten Unterschied (`vorher` → `nachher`), unveränderte
Labels mit Unicode und führenden Bindestrichen sowie deaktiviertes „Speichern“
und „Speichern unter“. Beide Eingaben waren nach dem Schließen bytegleich.

Dieser GUI-Test rief den Helfer direkt mit den Integrationsargumenten auf. Der
vollständige Aufruf aus der installierten Doppeldecker-App wurde dabei nicht
geprüft: Der installierten Fastra-App fehlte noch der Bundle-Helfer. Dafür muss
Fastra mindestens 1.119.0 samt Helfer installiert sein.

### Offene Integrationsprüfung

- [ ] Nach Freigabe des GUI-Fokus den Vergleich aus der installierten
  Doppeldecker-App mit zwei Wegwerfdateien an Fastra übergeben, das angezeigte
  Vergleichsfenster und die unveränderten Eingabedateien prüfen und beide Apps
  anschließend ausblenden. Der Helfer ist auf dem M5 inzwischen in der
  installierten Fastra-App 1.126.6 vorhanden; der direkte Helfertest oben
  ersetzt diesen vollständigen Aufruf nicht.
