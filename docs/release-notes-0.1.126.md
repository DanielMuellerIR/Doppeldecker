# Preview 0.1.126

Stand: 2026-11-08. Diese Fehlerkorrektur verbessert Fensterstart und LAN-Suche
unter Linux sowie das Entfernen wartender Archivuploads.

- Fenstergröße und Maximierung bleiben auch ohne aktivierte Sitzungswiederherstellung
  gespeichert. Unter Wayland verhindert eine nicht verfügbare globale Fensterposition
  das Speichern der Größe nicht mehr.
- Die Linux-LAN-Suche empfängt mDNS-Antworten auf dem gemeinsam mit Avahi verwendeten
  UDP-Port 5353. Bisher konnte sie trotz angekündigter Server ohne Treffer bleiben.
- Wartende Archivuploads lassen sich im Hintergrund entfernen. Die Bereinigung erhält
  Dateien, die währenddessen neu hinzugefügt oder ausgetauscht werden.

Passwörter werden weiterhin im macOS-Schlüsselbund beziehungsweise im Secret Service
unter Linux gespeichert. Profildateien enthalten nur Referenzen auf diese Einträge.
Die LAN-Suche zeigt angekündigte SMB-, SFTP- und WebDAV-Dienste; einzelne SMB-Freigaben
und nicht angekündigte Dienste werden dabei nicht aufgezählt.

[Alle Änderungen](../CHANGELOG.md#01126---2026-11-08) · [Plattformen und Abnahme](release-plan.md).
