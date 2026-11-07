# Test- und Gate-Harness: Prozessende und Isolation

Alle elf bisherigen Bereichsdateien wurden vollständig gelesen, einschließlich
der gesamten Browser-Suite und ihrer Mockbrücke. Die zwei neuen Cleanup- und
Testdateien wurden ebenfalls vollständig geprüft. Abgeglichen wurden
Sandboxmarker, Wiederanlauf, vorgebaute Programme, Produktions-CSP, unbekannte
IPC-Aufrufe, Profilrevisionen, getrennte Browserkontexte, Fehlererkennung,
leere Testfilter, Prozessbesitz und Aufräumpfade.

## Zwei belegte Abbruchfehler

Ein tatsächlicher `run_gate` startet einen eigenen künstlichen Hilfsprozess und
wartet auf dessen Bereitschaft. Ein ausschließlich an den Runner gesendetes
`SIGTERM` beendet den Runner, lässt den Hilfsprozess aber weiterlaufen. Der
Gate-Runner startet Kommandos jetzt in einer eigenen Sitzung, merkt den Abbruch
und beendet genau diese Prozessgruppe. Nach höchstens fünf Sekunden folgt für
verbleibende Nachkommen `SIGKILL`; das direkte Kind wird abgeholt. Der Runner
endet mit dem zum Signal gehörenden Fehlercode und startet kein weiteres Gate.
Dasselbe gilt für die Bereinigung des Debug-Buildcaches.

Auch die echte Browser-Suite ließ bei einem ausschließlich an sie gesendeten
`SIGTERM` ihren Mock-Server zurück. Der gültige Gegenbeleg wartet auf eine echte
HTTP-Antwort des Servers. Ein früherer Versuch ohne diesen Bereitschaftsbeleg
wird nicht als Fehlernachweis gezählt.

Browser-Suite und Screenshot-Erzeugung verwenden nun denselben Cleanup. Er
beendet Server und Browser unabhängig voneinander, erhält einen Schließfehler
und eskaliert einen nicht reagierenden eigenen Server nach zwei Sekunden.
Signalabbruch besitzt zusätzlich eine Gesamtfrist von fünf Sekunden. Die
im Lockfile festgelegte Playwright-Version registriert auch bei abgeschalteten eigenen
Signalhandlern ihren Exit-Cleanup für die getrennte Browser-Prozessgruppe.
Diese Grenze wurde im installierten Abhängigkeitscode geprüft und mit echtem
headless Chromium gegengeprüft: Nach `SIGTERM` fehlen sowohl der eigene
Serverprozess als auch der Browserprozess.

## Prüfungen und Grenzen

Die 16 gezielten Gate-Tests und vier Browser-Cleanup-Tests bestehen auf macOS.
Die neuen Prozessfälle senden `SIGINT`, `SIGTERM` und `SIGHUP` und prüfen auch
signalignorierende Nachkommen, wartenden Browserstart sowie den Schließfehler.
Der ursprüngliche Gate-Gegenversuch und der HTTP-bereite Browser-Gegenversuch
sind nach der Korrektur grün; beide Runner enden mit 143 ohne laufenden Server
beziehungsweise Hilfsprozess.

Im vollständigen macOS-Werkzeuglauf bestehen 145 von 146 Tests. Der unveränderte
SFTP-Signaltest erreicht bei `cargo-ready-before-ack` und `SIGHUP` seinen
15-Sekunden-Bereitschaftsmarker nicht. Der Fehler wird getrennt im bereits offenen
Timing-Thema untersucht; dieser Werkzeuglauf ist ausdrücklich rot. Die Quelle
des SFTP-Tests und dessen Launcher sind gegenüber dem vorherigen Stand bytegleich.

Der vollständige Browserlauf besteht: 408 Prüfungen einschließlich der
Produktions-CSP sowie sechs Pane-Tab-Tests. Die eingefrorene Installation der
Node-Abhängigkeiten besteht ebenfalls. Der kombinierte Gatelauf endet wegen
des getrennt benannten SFTP-Tests insgesamt mit Fehlercode 1.

In einer frischen Linux-Kopie bestehen 19 der 27 ausgewählten Gate-/Cleanup-/
Launcher-Tests. Vier Darwin-Launcherfälle und vier Browser-Cleanup-Fälle ohne
vorhandene Node-Laufzeit sind ausdrücklich übersprungen. Der erste
Ergebnis-Collector scheiterte erst nach dem Testabschluss an seiner unbedingten
Node-Versionsabfrage; der erhaltene Testlog und die Quellhashes wurden danach
ohne erneuten Testlauf gesichert. Eine Linux-Ausführung der JavaScript-
Prozessregressionen wird damit nicht behauptet.

Die 385 erfassten Quellen bleiben während der Prüfungen unverändert. Core, CLI,
App, Frontend und Lockfiles bleiben bytegleich zum vorherigen Paket; ihre
vorherigen Rust-/Build-/nativen Prüfungen werden weiterverwendet. Alle gestarteten
Gegenprozesse und Dateien sind künstlich. Es gab keine native App-Bedienung,
Installation, Notarisierung oder Veröffentlichung.
