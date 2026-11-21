# Änderungsprotokoll

Alle wesentlichen Änderungen an Doppeldecker werden hier dokumentiert. Das Format
folgt Keep a Changelog; Versionen verwenden Semantic Versioning.

## [Unreleased]

## [1.0.2] - 2026-11-21

- Andere Fenster bleiben bedienbar, während Doppeldecker einen langsamen lokalen Ordner einliest.
- Lokale Sync-Vorschauen und Simulationen sowie Abfragen der Sync-Quarantäne laufen im Hintergrund.
- Doppeldecker lädt Favoriten und startet Ordnergrößen im Hintergrund. Auch während der macOS-CLI-Einrichtung bleiben die Fenster bedienbar.
- Fortsetzbare FTPS-Downloads warten auch bei leeren Dateien und wiederaufgenommenen Vollständen auf den bestätigten Transferabschluss. Nach einem Verbindungsfehler zeigt die Queue den gesicherten Abschnitt. Bereits installierte Downloads werden nach einem Neustart auch ohne Serverzugriff abgeschlossen.
- Fortsetzbare SFTP-Downloads lassen sich während des Lesens pausieren. Die Downloadvorbereitung lässt sich abbrechen. Die Mehrfachauswahl prüft kollidierende Namen nach den Regeln des Zielvolumes. FTPS und SFTP sind auch im Datei-Kontextmenü erreichbar.
- Beim Spiegeln werden Zielordner mit nicht eindeutig lesbaren Dateinamen bereits in der Vorschau erkannt. Die Auslassung sperrt die vorgesehenen Entfernungen.
- Entfernte Archive lassen sich aus dem Kontextmenü über eine gebundene Momentaufnahme entpacken. Die Queue nennt TAR und TAR.GZ bei der erneuten Bestätigung und kennzeichnet entpackte Zwischenstände.

## [1.0.1] - 2026-11-18

- Fortsetzbare Kopien speichern bestätigte Byteabschnitte in einem kleinen Wiederanlaufbeleg je Fenster. Die Journalhistorie wird für Zwischenstände nicht neu geschrieben.

- **Dateiinhalt fortsetzbar kopieren** pausiert lokale Dateiauswahlen und kompatible WebDAV-, FTPS- und SFTP-Downloads. Die Queue sichert den Abschnitt der aktuellen Datei und setzt nach einem Neustart dort fort. WebDAV fordert bedingte Bytebereiche an. FTPS und SFTP holen die fehlenden Bytes und prüfen die fertige Datei anhand der Server-SHA-256.

- **Archiv entpacken** übernimmt ZIP, TAR, TAR.GZ und TGZ aus der Archivansicht. TAR unterstützt lange Namen über PAX und GNU sowie die PAX-Größenangabe. Vorschau und Queue weisen ausgelassene Spezialtypen und Teilkataloge aus.

- **Archivinhalt anzeigen** öffnet auch TAR, TAR.GZ und TGZ. Ordner lassen sich durchgehen und reguläre Dateien passiv ansehen. Die Ansicht weist unsichere Namen, Links und Teilkataloge aus.

- **Wiederherstellung** bündelt prüfbare Befunde der Fensterqueue, der Quarantäne am gewählten Ziel und verbliebener Papierkorb-Journale. Von der Übersicht gelangt man zum passenden Auftrag.

- Der Sync-Assistent nimmt Ausschlussmuster für Einweg-Syncs an. Vorschau und Bestätigung zeigen Treffer nach Quelle und Ziel. Ausgeschlossene Zieleinträge bleiben auch beim Spiegeln erhalten.
- Benannte Einweg-Sync-Aufträge speichern beide Ordner, Richtung, Modus und Filter. Beim erneuten Öffnen prüft die App die aktuellen Ordner und erstellt eine frische Vorschau.
- Gespeicherte Serverprofile lassen sich über **Gehe zu → Gespeicherte Verbindungen** oder `⌘K` beziehungsweise `Strg+K` fokussieren. `Enter` öffnet das Profil und setzt den Fokus nach erfolgreichem Laden in die Dateiliste.
- Upload-Recovery gleicht Content-, Workspace- und Cleanup-Pfade ab. Manuell geschlossene Aufträge bleiben nach einem Neustart geschlossen; erhaltene Zwischenstände abgeschlossener Uploads sind direkt erreichbar.
- Das Entfernen wartender Archivaufträge reserviert zuerst einen dauerhaften Entfernungsintent und bereinigt außerhalb des Journal-Mutex. Queue-Abfragen und Fenster-Schließhinweise bleiben währenddessen bedienbar.
- Die Queue öffnet Archiv-Zwischenstände in einem eigenen Tab. Nach erfolgreicher Bereinigung kehrt die Dateiliste zum vorherigen Tab zurück.
- Die Queue hält ungeklärte Zielwirkungen fortsetzbarer Kopien zur Prüfung offen. Einweg-Syncs lassen Teilkopien aus und erhalten sie beim Spiegeln.
- Das macOS-Updaterarchiv enthält das notarielle Ticket im App-Bundle und lässt die beim Tauri-Entpacken störenden AppleDouble-Einträge weg.
- FTP und FTPS verbinden sich auch mit Servern, die UTF-8 über `FEAT` melden und den zusätzlichen Befehl `OPTS UTF8 ON` ablehnen, etwa pyftpdlib 1.5.9.

## [0.1.127] - 2026-11-10

- Unterbrochene Bereinigungen erzeugter Uploadquellen behalten einen dauerhaft an
  Fensterqueue und Quelle gebundenen Claim. Queue-Neustart zeigt den tatsächlichen
  Prüfrest auch bei bereits teilweise gelöschtem Inhalt; kein falscher Cleanup-Erfolg
  und kein erneuter Upload. Wartende Aufträge verschwinden erst nach erfolgreicher
  Bereinigung aus dem Journal. Recoveryreste abgeschlossener Nebenfenster öffnen
  ebenfalls wieder ein Recoveryfenster.

- Das Entfernen wartender Queueaufträge hält Schließen und Beenden bis zum Ende
  des Cleanupworkers samt Ergebnis-/Recoverymeldung auf. Die fenstereigene
  Registrierung wird auch bei Fehlern freigegeben und sperrt keine anderen
  gewöhnlichen Dateiaktionen.

## [0.1.126] - 2026-11-08

- Linux-LAN-Suche empfängt mDNS-Antworten auf Port 5353 neben Avahi; zufällige Antwortports konnten trotz sichtbarer Server leer bleiben.

- Fenstergröße und Maximierung auch ohne Sitzungswiederherstellung merken; fehlende Fensterpositionen unter Wayland verhindern das Speichern nicht mehr.

- Das Entfernen wartender Archivuploads läuft im Hintergrund. Die automatische Bereinigung erhält Dateien, die währenddessen neu hinzugefügt oder ausgetauscht werden.

## Frühere Vorschauversionen

[Änderungsprotokoll bis 0.1.125](docs/history/changelog-preview.md).
