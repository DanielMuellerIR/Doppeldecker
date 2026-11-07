# SFTP-Signaltests: Fixture-Start vor der Signalphase

Der vollständige Werkzeuglauf erreichte bei
`cargo-ready-before-ack` und `SIGHUP` den erwarteten Bereitschaftsmarker nach
15 Sekunden nicht. Ein Signal war zu diesem Zeitpunkt noch nicht gesendet.
Die Test- und Launcherquellen waren gegenüber dem vorherigen Stand bytegleich.
Dieser konkrete Befund löste das bereits vorgemerkte Timing-Thema erneut aus.

## Gegenbeleg und seine Grenze

Zwei kleine Nachläufe verwenden dieselbe Signalphase, ergänzen jedoch einen
Eintrittszeitstempel in den künstlichen Programmen. Beide verfehlen den Marker.
Die erste Python-Zeile der eigenen `ssh-keygen`-Attrappe läuft erst nach etwa
18,8 beziehungsweise 19,5 Sekunden. Nach 15 Sekunden existieren noch keine
Sitzungsmarker; die Shell wartet auf ihren direkten Python-Unterprozess.

Ein Stack des eigenen Prozesses zeigt während dieser Wartezeit ausschließlich
`_dyld_start`, noch vor dem Python-Start. Der gefilterte macOS-Debuglog meldet
den Abschluss der XProtect-Prüfung für exakt diese künstliche `ssh-keygen`-Datei
unmittelbar vor dem Codeeintritt. Die Instrumentierung verändert den Dateiinhalt;
sie ist deshalb kein nachträglicher Stackbeleg für den ursprünglichen Gateprozess.
Sie belegt den konkreten Fehlmechanismus eines noch nicht angelaufenen
Testprogramms vor dem geplanten Signal.

Frische Dateien sind nicht grundsätzlich so langsam: Weitere künstliche
`ssh-keygen`-Varianten starten erstmals in 0,21–0,26 Sekunden. Derselbe Programm-
Inode über neue Test-Symlinks startet in etwa 0,02 Sekunden. Die Korrektur setzt
deshalb nicht auf eine angenommene konstante Startdauer.

## Korrektur

Die Testklasse erzeugt ihre unveränderlichen Programme nun einmal in einem
eigenen temporären Verzeichnis und setzt sie auf lesbar/ausführbar. Eine
ausdrückliche Bereitschaftsoption beendet jedes künstliche Programm sofort,
bevor es Zustände, Dateien, Sockets oder Kindprozesse anlegt. Diese begrenzte
Fixture-Vorbereitung besitzt 30 Sekunden je Programm und erfolgt vor den
getakteten Signalfällen. Ein Fehler bleibt ein sichtbarer Setupfehler.

Jeder Fall bindet dieselben Programme über eigene Symlinks ein. Zustandsdateien,
Sockets, Ports, künstliche Schlüssel und Roots bleiben weiterhin getrennt.
Die Testklasse entfernt anschließend ihr eigenes Programmverzeichnis. Auch
der einzelne künstliche Runner des Timeout-Cleanup-Tests gehört zu dieser
vorbereiteten Fixture; sein TERM-Handler schreibt weiterhin ausschließlich in
den Zustand dieses Falls.

Die Ready-Frist von 15 Sekunden und die Frist von acht Sekunden für den
Signal-/Cleanup-Ausgang bleiben unverändert. Ebenso bleiben alle Signalphasen,
Besitzbindungen, Negativfälle, Schutz fremder Prozesse und Cleanup-Assertions
erhalten. Der SFTP-Launcher und der Produktcode sind unverändert.

## Prüfung

Der zuvor rote instrumentierte Fall erreicht seinen Marker nach der Korrektur
nach etwa 0,6 Sekunden und besteht. Zwei gleichzeitige Fälle mit denselben
Programmen bestehen ebenfalls; ihre gesamte Signalausführung dauert etwa
0,88 beziehungsweise 0,92 Sekunden. Die gemeinsame Fixture-Vorbereitung dauert
dabei etwa 1,53 Sekunden. Dies prüft zugleich die Trennung der Fallzustände.

Die vollständige Signaltestklasse besteht auf Linux: 26 Tests ohne Auslassung.
Sie läuft dort in 70,4 Sekunden. Auf macOS besteht der vollständige
Werkzeuglauf mit allen 146 Tests in 182,3 Sekunden, einschließlich der
26 Signaltests. Die eingefrorene Installation der Node-Abhängigkeiten besteht
ebenfalls; das ausgewählte Gate endet mit Fehlercode 0.

Alle 385 erfassten Quellen bleiben während der Prüfungen unverändert. Gegenüber
dem vorherigen Paket ändert sich ausschließlich eine Testquelldatei. Frühere
Core-, CLI-, App-, Frontend-, Build- und native GUI-Prüfungen werden
weiterverwendet und nicht als erneut ausgeführt gezählt. Es werden ausschließlich
künstliche Programme und Dienste verwendet; echte Providerkonten, Nutzerdaten,
Installation und Release bleiben unberührt.
