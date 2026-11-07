# Linux-Paketprüfung: interner AppImage-Desktoplink

Der aktuelle reguläre Linux-Paketbau erzeugt DEB und AppImage, scheitert danach
aber an der eigenen Prüfung: `Doppeldecker.desktop` an der AppDir-Wurzel ist ein
relativer Link auf `usr/share/applications/Doppeldecker.desktop`. Die bisherige
Prüfung verlangt dort eine reguläre Datei und lehnt damit das tatsächlich
erzeugte Paket ab.

Die [AppDir-Spezifikation](https://docs.appimage.org/reference/appdir.html)
erlaubt diesen internen Desktoplink ausdrücklich. Der Prüfer akzeptiert jetzt
genau das genannte relative Ziel. Jede Zielkomponente wird einzeln geprüft:
Eltern müssen echte Verzeichnisse und das Ziel eine reguläre Datei sein.
Absolute, fremde, fehlende und über weitere Symlinks erreichbare Ziele bleiben
gesperrt. Eine reguläre Desktopdatei an der Wurzel bleibt zulässig. Die allgemeine
Prüfung anderer Paketdateien wird nicht gelockert.

## Ausgeführte Prüfungen

Der Fehler ist am regulär gebauten AppImage reproduziert. Die zwölf gezielten
Werkzeugtests bestehen auf macOS; drei neue Fälle prüfen die tatsächlichen
Dateisystemgrenzen einschließlich unveränderter fremder Dateien. Der gesamte
Werkzeuglauf besteht auf macOS mit 149 Tests und auf Linux mit 130 bestandenen
sowie 19 plattformbedingt übersprungenen Fällen. Die 385 erfassten
prüfrelevanten Dateien bleiben während der Läufe unverändert.

Der erste Linux-Werkzeuglauf hatte in der isolierten Quellkopie keine
Git-Dateiliste für den Modulscan. Nach dem Anlegen eines ausschließlich lokalen
Testindexes mit den ursprünglichen 487 verfolgten Pfaden besteht derselbe Lauf.
Die Scannerprüfung wurde dafür nicht geändert. Zwei anfängliche Einzelaufrufe
mit dem alten System-Python sind als ungeeignete Interpreterläufe ausgeschlossen;
die gültigen macOS-Prüfungen verwenden den Projektinterpreter.

Der korrigierte Prüfer passiert am echten DEB und AppImage die Layout-, ELF-,
Abhängigkeits- und CLI-Versionsprüfungen. Alle vier verbleibenden regulären Schritte
einschließlich Datenschutzscan, Signierung und Prüfsummen bestehen. AppImage, DEB
und AppImage-Attestation werden unabhängig gegen den eingebauten öffentlichen
Schlüssel verifiziert; eine vertauschte Payloadsignatur wird abgelehnt.
Die unveränderten Produktbinaries stammen aus `45e21af`;
die Änderungen betreffen nur Paketprüfer und Werkzeugtests. Frühere Core-, CLI-,
App-, Fuzz- und Browsergates sind deshalb weiterverwendete Belege, keine neuen
Läufe dieser Prüfung.

## Getrennter nativer Gegenlauf

Das kompilierte Linux-Binary startet in einer eigenen X11-Anzeige mit künstlichen
Panes über den vorgesehenen Test-Sandbox-Launcher. F2 benennt eine Datei um,
F5 kopiert sie und F6 verschiebt eine zweite Datei; die nachgelesenen Inhalte
stimmen bytegenau. Ein weiteres echtes Fenster besitzt sein eigenes leeres
Journal, während das erste zwei abgeschlossene Copy-/Move-Aufträge enthält.
Die betroffenen Ansichten wurden anhand der Screenshots visuell geprüft.

Auch das tatsächliche AppImage startet über denselben Sandbox-Launcher und besteht
F2, F5 und F6 mit identischen Inhaltsprüfungen. Der Lauf bindet den Prozess an
den geprüften AppImage-Pfad. Ein zweites Fenster und getrennte Journale sind
nachgewiesen; dessen Screenshot wurde während des Ladens aufgenommen und belegt
noch nicht die fertige zweite Ansicht. Beim ersten Versuch war F5 bereits vor dem
sichtbaren Rename-Abschluss gesendet worden. Der gültige Kopierlauf startet erst
nach beobachteter abgeschlossener Oberfläche mit ausgewählter umbenannter Datei.

Das DEB wird außerdem in einem eigenen Ubuntu-24.04-Container installiert,
versionsgeprüft, durch dieselbe Version ersetzt und wieder entfernt. Die
installierten GUI-/CLI-Hashes stimmen mit dem geprüften Paketinhalt überein.
Die künstliche Konfiguration bleibt erhalten, die Programm- und Desktopdateien
verschwinden. Der Container wird anschließend entfernt. Eine Wiederinstallation
derselben Version ersetzt keinen echten Upgradegegenlauf.

Dabei sind zusätzliche Linux-Menübefunde sichtbar: Die Beschriftung für ein neues
Fenster zeigt ein macOS-Kürzel. Der Versuch mit Strg+Q beendet die App nicht.
Diese Befunde sind noch nicht korrigiert. Der eigene Launcher beendet seine
Testprozesse anschließend über den vorgesehenen Cleanup.

Die geprüften Payloads bleiben während der Paketgates unverändert:

| Paket | Bytes | SHA-256 |
| --- | ---: | --- |
| DEB 0.1.123 amd64 | 19.835.764 | `01105a735eaef71d1d8848fd2fd7c1b4f43f67746614c7a9adf9af34c9b62058` |
| AppImage 0.1.123 amd64 | 94.783.992 | `5a43145c3cd6bc80c842873d4113034d8f036cffa50c8a06d68a1b3ed7d5389e` |

Der tatsächliche Versionswechsel und die menschliche Abnahme am Gerät bleiben
offen. Es fand keine Veröffentlichung statt. Die später korrigierten Menübefunde
erfordern neue Produktbinaries und einen neuen gebundenen Paketgegenlauf.

Der anschließende [aktuelle Kandidatengegenlauf](linux-kandidat-2026-10-19.md)
korrigiert die Menübefunde und belegt den echten Versionswechsel, den erneuten
nativen Lauf beider Paketarten sowie die Deinstallation. Die obigen Hashes
bezeichnen weiterhin ausdrücklich den vorherigen Prüfergegenlauf.
