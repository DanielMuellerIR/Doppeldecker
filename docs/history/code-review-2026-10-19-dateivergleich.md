# Interaktive Leseklasse des Dateivergleichs

Gezielter Folgeschnitt des vollständigen lokalen Tauri-Reviews, kein neuer
Vollreview von Frontend, Core oder Prüfwerkzeugen.

## Tatsächlicher Gegenbeleg

Die unveränderte App wählt in einem markierten Testbereich eine reguläre Datei
auf einem eigenen SFTP-Server und eine reguläre lokale Datei aus. Der tatsächliche
Menüaufruf „Dateien vergleichen“ scheitert mit `unsupported`, bevor ein
Inhalts-READ gesendet wird. Elf protokollierte Requests gehören zum Verbindungs-
und Listingweg; keine Mutation wird gesendet. Beide Eingaben bleiben bytegleich,
das eigene App-/Server-/Proxy-Cleanup ist bestätigt. Der native Vorherlauf ist
wegen genau dieser Produktabsage rot.

Der Vergleichsadapter verwendete file_context und damit den bewusst gesperrten
strikten SFTP-Stat-/Leser. Öffnen und additive lokale Kopien besitzen bereits
eine getrennte, begrenzte interaktive Inhaltsklasse. Das Frontend bot den
Vergleich regulärer SFTP-Dateien trotzdem an.

## Korrektur und angrenzende Grenzen

Beide Seiten verwenden jetzt die vorhandene download_interactive_snapshot-Grenze.
Damit bleiben die jeweiligen Providerprüfungen, vollständigen Inhalts- und lokalen
BLAKE3-Readbacks, Größenlimits, privaten Cachedateien und No-Replace-Anlagen erhalten.
SFTP meldet sein Pfadrisiko vor der Übertragung und im Ergebnis als Best Effort;
es entstehen keine Plannerbelege oder zusätzlichen Mutationsfähigkeiten.

Beide Requests werden vor der Queueöffnung und beide Profilrevisionen vor der
ersten Providerauflösung geprüft. Die fenstereigene abbrechbare Reservierung umfasst
Verbindung, beide Downloads, frische Profilprüfung und Werkzeugübergabe. Der
vorherige vom Fenster unabhängige Cancel ist entfernt. Andere Fenster behalten
ihre eigenen Schranken. Das Terminalereignis kommt vor dem nächsten Queueworker;
die Command-Antwort erzeugt keine verspätete Erfolgsmeldung mehr.

Vor dem Absenden prüft der Adapter den Abbruch nochmals. Danach zeigt die
Oberfläche die Werkzeugübergabe ausdrücklich und nimmt Knopf- und Escape-Abbruch
zurück. Ein unterbrochener Helfer-I/O nach dem Absenden wird als outcome_unclear
behandelt, weil ein Fenster bereits geöffnet sein kann. Es gibt keinen Blind-Retry.
Die Dateien in den Panes werden weder verändert noch als Schreibziele an das
Vergleichswerkzeug übergeben.

## Prüfung

Vier gezielte Appfälle bestehen, darunter zwei neue Gegenläufe für tatsächliche
lokale Momentaufnahmen, unveränderte Originale, Abbruch vor Cacheanlage sowie
begrenzte SFTP-Auswahl und eine nicht doppelt gemeldete Risikobilanz. Die gezielten
Browserfälle prüfen beide Themes bei 760 × 480, sichtbare Einschränkungen,
Vorbereitungsabbruch, unklaren Helferausgang und den Erhalt einer folgenden
Queuevorbereitung. Ein weiterer Fall prüft Escape nach Werkzeugübergabe.

Ein früher Browserhelfer wartete auf die absichtlich gehaltene Mock-Promise,
statt das Menüereignis ohne Rückgabewert auszulösen. Nur dieser eigene Prozessbaum
wurde beendet; der korrigierte Aufruf entspricht der nativen Ereigniszustellung.
Dieser Hilfslauf wird nicht als bestandene Produktprüfung gezählt.

Die vollständigen App-Suiten bestehen auf macOS mit 214 und auf Linux mit
204 Tests. Je Plattform bleibt ein isolierter FIFO-Kindtest in der Standardsuite
ignoriert; der bestandene Elternlauf startet ihn ausdrücklich. Zwei zusätzliche
Linux-GTK-Fälle bleiben ohne interaktive Displayabnahme ignoriert. Workspace-Clippy
mit allen Targets und `-D warnings` sowie Formatprüfung sind beidseitig grün.
Der Mac-Build und die vollständige Browser-Suite mit 387 Prüfungen sowie sechs
Tabtests bestehen. Alle 383 Quell- und Werkzeughashes bleiben vor und nach den
Gates identisch.

Der native Nachherblock besteht vollständig. Die App liest vom künstlichen
SFTP-Server und übergibt zwei inhaltsgleiche lokale Momentaufnahmen mit Modus 0600
an den echten Fastra-Helfer. Dessen schreibgeschützte Ansicht zeigt den einen
Unterschied links → rechts. Eingaben und Snapshotbytes wurden zurückgelesen.
Die Best-Effort-Bilanz in der hellen Oberfläche bei 760 × 480 sowie Vorbereitung
und Abbruch in der dunklen Oberfläche wurden visuell geprüft.

Ein eigener verzögerter Inhaltstransfer wird nach dem ersten DATA-Paket abgebrochen.
Die unvollständige eigene Cachekopie wird entfernt, beide zuvor fertigen Snapshots
bleiben erhalten. Kein weiteres Vergleichsfenster entsteht; Originalbytes bleiben
gleich. Die Protokollbilanz enthält 13 Inhaltsreads in 135 Requests und keine Mutation.
Nur das am frischen Lauf und seinen Labels eindeutig erkennbare eigene
Fastra-Vergleichsfenster wird geschlossen. App-, Server- und Proxy-Cleanup sind
bestätigt.

Core, CLI, Fuzz, Manifeste und Lockfiles sind unverändert; frühere Vollgates bleiben
als solche belegt. Der native Test verwendet den eingefrorenen Entwicklungsbuild,
keine neue Installation. Eine Installations- oder menschliche Linux-GUI-Abnahme
wird hier nicht behauptet.
