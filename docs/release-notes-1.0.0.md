# Doppeldecker 1.0.0

Doppeldecker ist ein Dateimanager mit zwei Dateibereichen für macOS und Linux.
Version 1.0.0 erweitert die Datei- und Sync-Arbeit über lokale Ordner und
Serververbindungen hinweg.

- **Sync-Aufträge speichern.** Einweg-Aufträge übernehmen beide Ordner,
  Richtung, Modus und Ausschlussmuster. Beim Öffnen prüft der Assistent den
  aktuellen Bestand. Ausgeschlossene Zieleinträge bleiben beim Spiegeln erhalten.
- **Dateikopien fortsetzen.** Lokale Mehrfachauswahlen und kompatible WebDAV-,
  FTPS- und SFTP-Downloads setzen nach Pause und App-Neustart am geprüften
  Zwischenstand an.
- **Wiederherstellung öffnen.** Gehe zu → Wiederherstellung führt zu wartenden
  Aufträgen, Quarantäne und Papierkorb-Journalen. Erfolgreich abgeschlossene
  Archivuploads räumen den Prüf-Tab auf und kehren zum vorherigen Ordner zurück.
- **Archive durchsuchen und entpacken.** Die Archivansicht unterstützt ZIP,
  TAR, TAR.GZ und TGZ. Dateien lassen sich mit F3 ansehen. Entpacken kann
  das ganze Archiv, den geöffneten Ordner oder einzelne Inhalte übernehmen.
- **Serverprofile per Tastatur öffnen.** ⌘K beziehungsweise Strg+K führt zu
  gespeicherten Verbindungen. Nach Enter steht der Fokus in der Dateiliste.
- **macOS-Updates installieren.** Das Updaterarchiv enthält die gestapelte App
  und lässt die beim Entpacken störenden AppleDouble-Einträge weg.

Die Pakete unterstützen macOS 14 und neuer auf Apple Silicon sowie Linux Mint
22.2 und Ubuntu 24.04 auf x86_64. Die
[Paketdateien und Prüfsummen](https://github.com/DanielMuellerIR/Doppeldecker/releases/tag/v1.0.0)
liegen beim Release. Die [Funktionsmatrix](function-audit.md) beschreibt die
unterstützten Operationen und Protokollgrenzen.
