# Doppeldecker 1.0.2

- Andere Fenster bleiben bedienbar, während Doppeldecker einen langsamen lokalen
  Ordner einliest. Lokale Sync-Vorschauen, Simulationen, Quarantäneabfragen,
  Favoriten und Ordnergrößen werden im Hintergrund verarbeitet.
- SFTP-Downloads lassen sich während des Lesens pausieren. FTPS-Downloads
  warten auch bei leeren Dateien auf die Transferbestätigung. Bei
  Verbindungsfehlern zeigt die Queue den bereits gesicherten Abschnitt.
- Die Spiegelvorschau meldet Zielordner mit nicht eindeutig lesbaren Dateinamen.
  Wenn die Vorschau solche Namen auslassen muss, sperrt sie die vorgesehenen
  Entfernungen.
- Das Kontextmenü kann entfernte Archive anhand einer gespeicherten
  Momentaufnahme entpacken. Die Queue zeigt TAR und TAR.GZ bei einer erneuten
  Bestätigung und kennzeichnet bereits entpackte Zwischenstände.

Doppeldecker läuft auf macOS 14 und neuer (Apple Silicon) sowie Linux Mint 22.2
und Ubuntu 24.04 (x86_64). Der Release enthält das notarisierte DMG, DEB,
AppImage und Prüfsummen.

[Pakete und Prüfsummen](https://github.com/DanielMuellerIR/Doppeldecker/releases/tag/v1.0.2) ·
[Funktionsmatrix](function-audit.md)
