# Dateiaktionen, Zwischenablage und Queueabschluss

Das vollständige Delta seit dem letzten Frontend-Datei-/Queue-Vollreview wurde
einschließlich angrenzender aktueller Abläufe gelesen. Die neuen Dateien für
Zwischenablage und Konfliktdialog sowie Dateiaktionen, Fortschrittsanzeige, Queue,
Modalsteuerung und Drops wurden vollständig gelesen. Die unveränderte Quarantäne
behält ihren früheren Vollbeleg. Die gemeinsame Clipboard-Versionsbindung und die
Event-/IPC-Verdrahtung wurden an ihren Backend- beziehungsweise Aufrufergrenzen
abgeglichen. Das ist kein neuer Vollreview von Core oder Prüfwerkzeugen.

## Zwei belegte Abschlussfehler

Der lokale Queue-Sync besitzt nach erneutem Öffnen eine eigene Wirkungsbestätigung.
Ein Mausklick in diesem Dialog zählt zugleich als Außenklick des Queue-Popovers.
Der neue lokale Rückweg stellte es anschließend nicht wieder her. Zwei rote
Browsergegenproben erreichen sowohl Abbruch als auch Bestätigung über die echte
Oberfläche. In beiden Fällen verschwindet die Queue. Die bestehende Rückkehrfunktion
wird jetzt auch für diesen Pfad abschließend ausgeführt: Die Queue wird wieder
sichtbar und ihr semantisch gewählter Eintrag erhält den Fokus beziehungsweise
eine passende verbleibende Queueaktion.

Vier weitere rote Browsergegenproben halten die tatsächliche Einreihantwort an
der Tauri-Brücke zurück. Ein vorher zugestelltes Dateiabschluss- oder Fehlerereignis
zeigt bereits „Fertig“ beziehungsweise „Ausgang unklar“. Danach ersetzt die
Einreihantwort beide Stände fälschlich durch „Eingereiht“, lokal und beim WebDAV-
Dateiauftrag. Das belegt die Event-/Antwortreihenfolge in der echten Oberfläche;
ein zusätzlicher entfernter Mutationsfehler wird damit nicht behauptet.

Dateiaufträge verwenden jetzt die bereits vorhandene Anzeigenrevision. Sobald der
Worker oder eine andere aktuelle Anzeige den Stand geändert hat, bleibt dieser
erhalten. Der erfolgreiche Queuecommit und seine ID werden trotzdem für die
Zwischenablage und die Bilanz mehrerer Quellgruppen weitergereicht. Ein späterer
Navigationstoken wird weiterhin geprüft. Neue Sicherheitsentscheidungen oder
Providerfähigkeiten entstehen dadurch nicht.

## Prüfung und Grenzen

Alle sechs neuen Browsergegenproben bestehen nach der Korrektur. Die vollständige
Suite besteht mit 393 Browserprüfungen einschließlich CSP und sechs Tabtests.
Der echte Mac-App-Build ist grün. Alle 383 Quell- und Werkzeughashes bleiben vor
und nach den Gates identisch.

Der erste native Hilfslauf setzte die Fenstergröße vor der Fensterbereitschaft.
Er erreichte den Produktfall nicht; sein eigener App-Cleanup ist bestätigt.
Die Wiederholung wartet ausdrücklich auf die sichtbare Dateiliste. Eine erste
Aktivierung über System Events erreicht Abbruch, Bestätigung und eine tatsächliche
F5-Kopie erfolgreich, bildet aber den Pointerpfad nicht verlässlich ab. Deshalb
verwendet der maßgebliche Gegenlauf die vorhandene CGEvent-Mausfunktion.
Die nativen Ergebnisse werden getrennt von diesen Hilfsgrenzen bewertet.
Ein weiterer Hilfslauf mit direkt aufeinanderfolgenden Mausereignissen schloss den
Dialog nicht und zählt ebenfalls nicht als Produktgegenbeleg.

Der maßgebliche native Vorherlauf sendet Maus-Down und -Up über die vorhandene
CGEvent-Funktion mit 50 ms Abstand. Nach dem tatsächlich abgeschlossenen Abbruch
fehlen Queue, Bestätigungsknopf und Fokus. Der Queueeintrag bleibt korrekt wartend;
die Quelle ist bytegleich und das Ziel nicht angelegt. Der eigene Cleanup besteht.
Eine im Helfer vorgesehene AX-Ergänzung wurde in diesem Gegenlauf nicht benötigt.

Der native Nachherblock mit demselben Mausereignisweg besteht vollständig.
Nach Abbruch sind Queue und „Plan bestätigen“ wieder sichtbar und der Knopf
besitzt den Fokus. Quelle und fehlendes Ziel bleiben unverändert. Nach Bestätigung
ist der Lauf Completed und der Zielinhalt exakt; eine verbleibende Queueaktion
behält den Fokus. Beide Mausaktionen benötigen keine AX-Ergänzung.

Eine zusätzliche echte F5-Kopie zwischen frischen künstlichen Ordnern besteht mit
unveränderter Quelle, exaktem Ziel und sichtbarem „Fertig“. Sie prüft den nativen
Dateiaktionsabschluss; die gezielt gehaltene Antwortreihenfolge wird durch die
vier Browsergegenproben belegt. Die tatsächlichen dunklen Queue-/Abschlussansichten
wurden visuell geprüft. Dateinamen einzelner Helferscreenshots sind keine Belege
für einen Themewechsel. Eigene App- und Prozessressourcen sind nachweislich frei.

Rust, CLI, Core und Provider bleiben unverändert. Ihre vorherigen beidseitigen
Vollgates werden weiterverwendet und hier nicht als neu ausgeführt gezählt.
Die unabhängige menschliche Linux-GUI-/Installationsabnahme bleibt offen.
