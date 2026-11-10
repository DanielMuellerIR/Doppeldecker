# Preview 0.1.127

Diese Preview korrigiert die Bereinigung erzeugter Uploadquellen und das Beenden
während einer laufenden Bereinigung.

- Nach einem Prozessabbruch bleiben vollständige und teilweise bereinigte
  Zwischenstände an ihre Fensterqueue gebunden. Beim Neustart zeigt die Queue
  den tatsächlichen Prüfpfad; ein unterbrochener Cleanup gilt nicht als erfolgreich.
- Wartende Aufträge werden erst nach erfolgreicher Bereinigung aus dem Journal
  entfernt. Schließen und Beenden warten auf den Cleanupworker und seine
  Ergebnis- oder Recoverymeldung.
- Die Registrierung bleibt auf das verursachende Fenster begrenzt und wird auch
  bei Fehlern freigegeben. Andere gewöhnliche Dateiaktionen bleiben möglich.

Unterbrochene Bereinigungen mit Prüfrest erfordern eine bewusste Prüfung; sie
werden nicht blind wiederholt. Unterstützte Pakete: macOS 14+ auf Apple Silicon
sowie Linux Mint 22.2 / Ubuntu 24.04 auf x86_64.
