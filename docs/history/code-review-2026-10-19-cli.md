# CLI-Delta und angrenzende Sicherheitsgrenzen

Geprüft wurden das vollständige CLI-Delta seit dem letzten Vollreview und die
angrenzenden Options-, Root-, Profil-, Verbindungs-, Queue- und JSONL-Grenzen.
Der aktuelle Einstieg und der Headless-Vertrag wurden vollständig gelesen, die
Tests im Delta und in den betroffenen Kontexten. Der unveränderte Importparser
behält seinen früheren Vollbeleg. Das ist kein neuer Vollreview aller CLI-Dateien.

## Belegte Fehler und Korrekturen

Ein tatsächlicher CLI-Sync verwendet ein künstliches gespeichertes SFTP-Quellprofil
und ein fehlendes FTPS-Zielprofil. Ein eigener Loopback-Peer zählt Verbindungen und
beendet den Transport vor Anmeldung. Vorher wird die Quelle einmal kontaktiert und
`connection_lost` gemeldet. Nachher kommt `not_found` vor jedem Quellkontakt; kein
Queuejournal entsteht. Die künstliche Agentreferenz zeigt auf keinen laufenden
Agenten. Nutzerkonten und Schlüsselbundwerte werden nicht verwendet.

Sync und Dateiaufträge lösen jetzt beide geheimnisfreien Konfigurationen vor
Queueöffnung und Verbindung auf. Die bestehende Profilrevisionsbindung für additive
WebDAV-Uploads bleibt erhalten. Der Prozessbesitz schützt die Verbindungen und
Proben; Queue-Claim und VFS-Bindung bleiben vor der Wirkung erhalten. Weitere echte CLI-Fälle
prüfen fehlende Zielprofile beim Kopieren und Verschieben sowie unveränderte
Quelldaten. Für diese Dateibefehle wird kein zusätzlicher Netzwerk-Gegenlauf behauptet.

Zwei rote Gegenproben reichen echte Core-Fehler mit getrennten Prüfpfaden an die
tatsächlichen JSONL-Ereignisadapter weiter. Vorher verschwinden diese Pfade;
nachher stehen sie sowohl allgemein als auch getrennt für Ausführung und sämtliche
Journalfehler bereit. Codepriorität, Wirkungsstatistik und vorhandene Recoverydaten
bleiben erhalten. Diese Gegenproben prüfen den Payloadübergang, keinen zusätzlichen
entfernten Mutationsfehler. Andere App-Fehlerhüllen bleiben ein eigener Querschnitt.

Der Entscheidungsleser prüfte zuerst den Pfad und öffnete ihn danach erneut mit
unbegrenztem read. Die tatsächliche I/O-Grenze wurde zunächst ohne Verhaltensänderung
extrahiert. Eigene Dateien wurden nach der früheren Pfadprüfung ausgetauscht:
Ein Symlink lieferte fremde künstliche Bytes, eine vergrößerte Datei 17 MiB.
Ein FIFO ohne Schreiber blockierte; der private Elternprozess beendete und reepte
ausschließlich sein eigenes markiertes Kind nach der Frist. Die ursprüngliche
Datei blieb erhalten. Das simuliert den Austausch am betroffenen I/O-Übergang;
ein zeitlich gehaltenes Rennen des vollständigen CLI-Aufrufs wird nicht behauptet.

Der Reader öffnet jetzt mit NOFOLLOW und NONBLOCK, prüft das tatsächliche Handle
auf regulären Typ und Größe und begrenzt das Lesen einschließlich Wachstum.
Genau 16 MiB gültige JSON-Daten sind zulässig, ein weiteres Byte wird abgewiesen.
Der bestehende libc-Eintrag wechselt dafür von Test- zu Laufzeitabhängigkeiten;
Version und Lockfiles ändern sich nicht. Die bidirektionale Paarregistry ist von
dieser CLI-Korrektur nicht erfasst und bleibt im FIFO-Querschnitt offen.

## Prüfung

Die vollständigen CLI-Suiten, Workspace-Clippy mit allen Targets und `-D warnings`,
der gesperrte Fuzz-Build und die Formatprüfung laufen auf macOS und Linux gegen
denselben eingefrorenen Quellstand. Der ignorierte FIFO-Kindtest wird zusätzlich
explizit über das von Cargo gemeldete Testartefakt gestartet und verlangt genau
einen gelisteten Treffer. Er weist den FIFO jetzt ohne Schreiber zurück.

Je Plattform bestehen 76 CLI-Tests; ein FIFO-Kindtest bleibt in der Standardsuite
ignoriert und besteht im beschriebenen expliziten Elternlauf. Clippy, Fuzz-Build und
Formatprüfung sind beidseitig grün. Alle 383 Quell- und Werkzeughashes bleiben
vor und nach den Gates identisch. Ein erster Kompilierlauf meldete einen fehlenden
Read-Import; dieser eigene Implementierungsfehler ist korrigiert, der Fehlversuch
bleibt in den Prüfbelegen erhalten.
Core, App und Frontend bleiben unverändert; deren vorherige Vollgates werden
weiterverwendet und hier nicht als neu ausgeführt gezählt. Eine native GUI-,
Installations- oder Releaseabnahme ist nicht Bestandteil dieses CLI-Deltas.
