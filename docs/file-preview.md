# Integrierte Dateivorschau

Stand: 2026-10-23.

F3, „Datei → Vorschau“ und das Dateikontextmenü öffnen für genau eine reguläre
Datei einen lesenden Dialog. Escape oder „Schließen“ kehrt zum vorherigen Fokus
zurück. Derselbe Weg gilt lokal und für die verbundenen Remote-Provider. Symlinks,
Spezialdateien und nicht eindeutig dekodierte Namen erhalten keine Inhaltsvorschau.

## Unterstützte Inhalte und Ressourcen

- UTF-8-Text einschließlich optionaler UTF-8-BOM bis 1 MiB; Tabulatoren und
  Zeilenumbrüche sind erlaubt. Andere Steuerzeichen oder ungültige UTF-8-Sequenzen
  führen zu einem sichtbaren Hinweis. HTML, SVG und andere textuelle Dokumente
  erscheinen ausschließlich als unverarbeiteter Text.
- Statische PNG- und JPEG-Bilder bis 8 MiB und 24 Millionen Pixel. Die Signatur und
  die Bildabmessungen kommen aus den Bytes, nicht aus dem Dateinamen. PNG mit
  Animationschunks wird nicht als Bild dargestellt. Der Browserdecoder kann eine
  beschädigte oder nicht unterstützte Variante zusätzlich ablehnen; auch das wird
  sichtbar erklärt.
- Größere Dateien, andere Bildformate, Binärdateien und andere Zeichencodierungen
  erhalten einen konkreten Hinweis samt „Mit System-App öffnen“.

Die 8-MiB-Grenze gilt für den vollständigen Vorschaudownload. Die Snapshotpipeline
prüft sie unmittelbar nach der frischen Quellprüfung, vor Cacheanlage und
Inhaltsöffnung. Ein wachsender Serverstrom bleibt durch denselben Größenbeleg auf
ein zusätzliches Erkennungsbyte begrenzt. Diese Grenze beschränkt ausschließlich
die integrierte Darstellung; normales Öffnen und Kopieren behalten ihren eigenen
Vertrag.

## Bindung und Darstellung

Das Backend validiert Pane-Endpunkt und Auswahl vor dem Providerzugriff. Es nutzt
`interactive_file_read_context` und dieselbe private, manifestierte Snapshotpipeline
wie Remote-Öffnen. Lokal bleibt der Quellzugriff descriptor-relativ unter dem
gehaltenen Pane-Root. Für SFTP gilt weiterhin die ausdrücklich benannte
Best-Effort-Lesesicht einschließlich des serverseitigen Elternpfadrisikos.
Profilrevisionen werden vor Übergabe des Ergebnisses erneut geprüft.

Der Betrachter liest ausschließlich den gerade erzeugten Backend-Snapshot,
begrenzt auch diesen Read und vergleicht Länge sowie BLAKE3 erneut. Cachepfade
werden nicht an das Frontend weitergereicht. Text wird mit `textContent` dargestellt;
Rasterbilder verwenden einen backendseitig festgelegten `data:image/png`- oder
`data:image/jpeg`-Typ im `img`-Element. Frames, Objekt-Plugins und aktive Dokumente
werden nicht verwendet. Die bestehende CSP bleibt unverändert.

Die Operation reserviert nur die Schranke des verursachenden Fensters. Schließen
während der Vorbereitung blendet den Dialog aus und verwirft seine spätere Antwort;
der begrenzte Lesevorgang wird noch abgeschlossen. Es wird dabei kein allgemeiner
Abbruchbefehl gesendet, der nach einem schnellen Abschluss versehentlich den nächsten
Queueauftrag treffen könnte. Dieser Ablauf steht im Ladehinweis. Ein zurückgestelltes
Ergebnis kann keinen später geöffneten Dialog verändern.

Unvollständige Snapshots werden durch den vorhandenen Fehler-/Abbruchpfad entfernt.
Fertig geprüfte Snapshots folgen der vorhandenen manifestgebundenen
Sieben-Tage-Aufbewahrung. Die Vorschau verändert das Original nicht. Bewusstes
Öffnen übergibt lokal das Original und remote eine neu geprüfte Momentaufnahme im
Originalformat an die System-App; deren eigene Rechte und Formatbehandlung gelten
weiterhin.
