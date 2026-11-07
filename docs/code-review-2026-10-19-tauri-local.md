# Vollreview: lokale Tauri-Befehle

Alle elf aktuellen Bereichsdateien mit 7291 Ausgangszeilen wurden vollständig
gelesen. Der Bereich umfasst lokale Navigation und Dateiaktionen, native
Zwischenablage, Favoriten, Ordnergrößen, Einstellungen, lokale Syncadapter,
Netzwerkerkennung und Systemmounts. Die direkten Core-, Fenster-, Queue-,
Profil- und Frontendverträge wurden an den Aufrufgrenzen mitgeprüft.

## Korrigierte Befunde

**P2: Finder-Import konnte bereits beim Öffnen blockieren.** Die Auswahl prüfte
zunächst eine reguläre Finder-Datei; zwischen dieser Auswahl und dem Öffnen
konnte derselbe Pfad durch eine FIFO ersetzt werden. O_NOFOLLOW verhinderte keinen
wartenden FIFO-Open. Die Handle-Typprüfung kam erst danach. O_NONBLOCK ergänzt
jetzt die bestehende No-Follow-Grenze, sodass die Typprüfung den FIFO sofort
ablehnt. Inhalts- und Größenprüfung bleiben an denselben Handle gebunden.

Der echte Dateigegenlauf legt ausschließlich einen künstlichen Finderbaum an,
wählt dessen reguläre sfl4-Datei über den produktiven Chooser aus und erhält die
Originalbytes unter einem anderen Namen. Erst danach ersetzt er den ausgewählten
Pfad durch eine FIFO ohne Schreiber. Der unveränderte Leser blockierte vor der
Korrektur im isolierten Kind; danach endet er mit der erwarteten Absage. Der
Elternprozess prüft exakt einen Kindtest, begrenzt dessen Lauf und erntet nur sein
eigenes Kind. Die Originalbytes bleiben erhalten. Die echte Finder-Konfiguration
wird in keiner Prüfung gelesen oder geändert.

**P2: Helferausgabe war trotz Prozessfrist unbeschränkt gepuffert.** Der gemeinsame
Leser für Systembefehle und den Fastra-Helfer begrenzte Laufzeit und einzelne
Leserunden, aber nicht die gesamte Ausgabe. Ein fest begrenzter echter
head-Prozess lieferte vor der Korrektur zwei MiB erfolgreich zurück. Die bisherige
nachträgliche diskutil-Prüfung kam dafür zu spät; Fastra hatte keine solche Grenze.

Die gemeinsame Grenze von einem MiB wird jetzt vor jeder Puffererweiterung
geprüft. Überlänge beendet und erntet das eigene direkte Kind über den bestehenden
Fehlerpfad. Der Fehler erhält `outcome_unclear`: Ein gestarteter Helfer kann schon
gewirkt haben, auch wenn sein Ergebnis nicht mehr bestätigt werden kann. Der
sichtbare Text verlangt eine Zustandsprüfung vor einem erneuten Aufruf. Genau ein
MiB bleibt erfolgreich lesbar; zwei MiB werden abgewiesen. Die bestehenden
Fastra-Gegenläufe mit 300000 Byte, Exitcodes und Timeout sowie die geerbte offene
stdout-Pipe bleiben grün. Es wurden weder echte Mounts ausgehängt noch echte
Fastra-Fenster durch diese Prozessgegenläufe geöffnet.

## Weitere geprüfte Grenzen und Folgepunkt

Clipboard-Generationen schützen neuere Inhalte und sperren alte Cut-Auswahlen nach
fehlgeschlagenem Copy. Die nativen macOS-Tests verwenden private Pasteboards.
Konflikt- und Aliasentscheidungen bleiben an Fenster, Auswahl, Zeit und Backend-ID
gebunden; Ersetzungen verlangen Inhaltsbelege. Providerdateiaufträge prüfen ihre
Ausführungsklassen frisch. Lokale Syncvorbereitungen, Queueübergänge und
Ordnergrößen-Rückmeldungen behalten ihre Fenster- und Generationsbindung.

Ein gesonderter P2-Folgepunkt ist statisch belegt: Das Frontend bietet den
Dateivergleich auch für reguläre SFTP-Dateien an. Dessen Adapter benutzt noch
file_context und den strikten Snapshotweg, während SFTP-Inhalte ausdrücklich über
die interaktive Leseklasse bereitstehen. Auch Providerauflösung vor Reservierung,
Abbruch und sichtbare Best-Effort-Bilanz gehören an diese konkrete Vergleichsgrenze.
Dieser Befund ist noch nicht mit einem tatsächlichen SFTP-App-Gegenlauf geprüft
oder korrigiert und bleibt als gezielter Folgeschnitt offen. Der Vollreview ist
keine Behauptung, dass dieser bekannte Folgepunkt erledigt sei.

## Verifikation

Die finalen App-Suiten bestehen auf macOS mit 212 und auf Linux mit 202 Tests.
Je Plattform bleibt ein isolierter FIFO-Kindtest in der Standardsuite ignoriert;
der übergeordnete bestandene Test startet genau dieses Kind ausdrücklich.
Zusätzlich bleiben zwei Linux-GTK-Fälle ohne interaktive Displayabnahme ignoriert.
Workspace-Clippy mit allen Targets und `-D warnings` sowie die Formatprüfung sind
beidseitig grün. Alle 383 Quell- und Werkzeughashes bleiben vor und nach den
finalen Gates identisch. Die beiden tatsächlichen Gegenbeispiele waren vor der
Korrektur rot und danach grün.

Core, CLI, Fuzz, Frontend, Manifeste und Lockfiles sind gegenüber den bereits
verifizierten Vorgängerpaketen bytegleich. Deren Vollgates und die Browserprüfung
mit 380 Fällen und sechs Tabtests werden weiterverwendet; sie wurden für diese
beiden Appdateien nicht nochmals ausgeführt. Die Korrektur ändert kein Layout.
Die native bidirektionale Zusatzprobe des Providerpakets hat einen eigenen
unveränderlichen Buildnachweis und wird getrennt bilanziert. Die menschliche
Linux-GUI- und Installationsabnahme bleibt offen.
