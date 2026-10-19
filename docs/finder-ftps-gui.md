# Finder→FTPS: nativer GUI-Prüfvertrag

Stand: 2026-10-07. Der integrierte Weg ist in einer markierten macOS-Test-Sandbox
mit tatsächlichen Finder-Mausgesten, nativen Tauri-Dropereignissen, unverändertem
Backend-IPC und lokalem FTPS-Server geprüft. Das Testfenster verwendet die echten
Produktmodule; ein zusätzlicher externer Beobachter erfasst Ereignisse und Anzeige.
Keine synthetischen DOM-Drops oder nachgebildeten Providerantworten tragen diesen Beleg.

## Bedienung und Wirkung

Der Drop kopiert die übergebenen Dateien in den angezeigten Pane-Ordner. Eine
Ordnerzeile unter dem Mauszeiger ändert das Ziel nicht. Die Quelle bleibt erhalten.
Mehrere Quellordner werden getrennt in dieselbe Fensterqueue eingereiht. Scheitert
oder stoppt die Einreihung einer Gruppe, nennt die Anzeige die Anzahl nicht eingereihter Gruppen
und erhält den konkreten Sperrgrund beziehungsweise Providerfehler.
Bereits eingereihte Gruppen bleiben erhalten; noch nicht eingereihte Gruppen
werden dann nicht weiter angefragt.

Bestehende Namen öffnen den üblichen Konfliktdialog. „Überspringen“ erhält das
bestehende Ziel. „Dauerhaft ersetzen“ erklärt die fehlende Quarantäne und den
Best-Effort-Weg, bevor die bestätigte Altversion ersetzt wird. „Abbrechen“ reiht
nichts weiter ein. Der laufende Queueauftrag zeigt Fortschritt und einen eigenen
Abbruchknopf; seine Quelle bleibt auch bei Abbruch unverändert.

## Tatsächlicher Gegenlauf

Der Server bindet ausschließlich Loopback, verlangt TLS für Kontroll- und
Datenkanal und verwendet ein kurzlebiges, gepinntes Testzertifikat. Serverroot,
Quellen, Profil und Queue liegen ausschließlich in der künstlichen Sandbox.
Das wertlose Testpasswort gelangt über den nativen Eingabedialog der App in einen
eigenen temporären Schlüsselbundeintrag; kein Passwort geht durch das Webview.

1. Eine 1-MiB-Datei per Finder auf eine Ordnerzeile der FTPS-Pane ziehen.
   Native Übergabe und Fortschritt sind sichtbar; das bytegleiche Ziel entsteht
   direkt im Pane-Root, der unterlegte Ordner bleibt leer.
2. Einen Namenskonflikt zuerst überspringen, dann erneut ziehen und dauerhaft
   ersetzen. Die erste Variante erhält die Altdatei, die zweite liefert die
   bytegleiche bestätigte Neufassung samt Best-Effort-Bilanz.
3. Eine gedrosselte 32-MiB-Kopie nach 12 MiB über den sichtbaren Abbruchknopf
   stoppen. Der Abschluss meldet Abbruch und gibt die Oberfläche frei. Ein
   konservativer möglicher Temp-Recoverypfad bleibt sichtbar; die unabhängige
   Serverprüfung findet tatsächlich keinen Rest und kein fertiges Ziel.
4. Dem künstlichen Zielordner das Schreibrecht entziehen und erneut per Finder
   ablegen. Es entsteht kein Ziel; die Anzeige erhält den FTP-/FTPS-Fehler und
   „1 von 1 Quellordnern wurden nicht eingereiht“. Nach Wiederherstellung der
   Rechte lässt sich der Konfliktdialog erneut öffnen und ohne Wirkung abbrechen.
5. Alle vier Quellen gegen ihre ursprünglichen Inhalte, Größen, SHA256 und
   Dateisystemidentitäten prüfen. Auswahl-/Profilroot vollständig auf Proben-
   und Temp-Reste prüfen. Eigene App, Finder-Fenster, Server und Testcredential
   nach dem Lauf schließen beziehungsweise entfernen.

Der verweigerte Schreibzugriff reproduzierte vor der Korrektur eine überschreibende
allgemeine Dropmeldung. Zwei Browserregressionen waren ebenfalls zuerst rot:
verlorener Capability-Sperrgrund und verlorener Enqueue-Fehler nach einer bereits
angenommenen Gruppe. Nach der Korrektur bestehen beide und die vollständige
UI-Suite: sechs Modelltests, 234 Browserfälle und die Produktions-CSP-Prüfung.
Gezielter Gegenlauf: `node tools/test-mock-ui.mjs 'Drag-and-drop Finder erhält'`.
Vollständiger Gegenlauf: `pnpm test:ui`.

## Grenzen

Dieser Beleg betrifft macOS und einen lokalen TLS-Testserver. Er behauptet keine
neue Prüfung beliebiger öffentlicher FTP-Server oder der interaktiven Linux-GUI.
Ein geänderter unsignierter Debug-Build kann erneut Schlüsselbundzugriff verlangen;
für den Gegenlauf wird dann ein frischer eigener Testeintrag nativ angelegt.
Der Debug-Testbuild wird ausschließlich über den Sandbox-Launcher gestartet und
anschließend durch den regulären Debug-Build ohne Testbeobachter ersetzt.
