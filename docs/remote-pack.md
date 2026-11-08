# ZIP-Erzeugung zu einem entfernten Ziel

Stand: 2026-11-02. Eine lokale Auswahl lässt sich über **ZIP-Archiv erstellen …**
einmal packen und zur entfernten Gegenpane hochladen. Im Dialog **Andere Pane**
als Ziel wählen. Die Originale bleiben erhalten.

## Ablauf und Wiederanlauf

- Die Bestätigung nennt Auswahl, Archivname und Ziel. Pane-/Profilwechsel machen
  die Vorbereitung ungültig; die Queue gehört dem verursachenden Fenster.
- Ein Queueeintrag führt erst die lokale Archivphase aus. Nach geprüftem Commit
  versiegelt der bekannte private Store den vollständigen Ergebnisbaum mit BLAKE3
  und Objektversionen. Vor Providerkontakt wechselt derselbe Eintrag atomar zum Upload.
- Pending, Konfliktpause und Neustart verwenden diese Bytes. Fehlende, zusätzliche
  oder geänderte Ergebnisinhalte werden vor Providerkontakt abgelehnt. Auch der
  Transfer prüft den erwarteten Inhalt vor seinem endgültigen Commit.
- Zielkonflikte verwenden den gewöhnlichen Kopierdialog: Ersetzen, Überspringen
  oder Beide behalten. Ordner werden zusammengeführt. Eine Konfliktfortsetzung
  erhält Auftrag, Ziel und versiegelte Quelle.
- Die Queue zeigt beide Phasen, Archivhinweise und **Zwischenstand zeigen**.
  Lokaler Abschluss bedeutet noch keinen erfolgreichen Upload. Teilwirkungen und
  unklare Serverantworten bleiben sichtbar; kein blinder mutierender Retry.
- Ein Absturz während der Archivphase verlangt Prüfung des lokalen Zwischenstands.
  Nach der atomaren Übergabe wird das Archiv nicht erneut erzeugt. Ergebnisdaten
  bleiben für offene Aufträge erhalten; es gibt keine automatische Alterslöschung.
- Nach dem dauerhaften erfolgreichen Queueabschluss werden belegte lokale
  Ergebnisdaten freigegeben. Wartende, noch nicht gestartete Aufträge geben ihren
  leeren Arbeitsordner beim Entfernen frei; erneut wartende Uploads ihre versiegelte
  Quelle. Abbruch, Fehler und Konfliktpausen behalten den Zwischenstand.
- Die Bereinigung prüft Fensterjournal, Inhaltsbeleg und aktive Leser. Bei Drift
  oder einem Cleanupfehler bleibt der Prüfpfad sichtbar; ein erfolgreicher Upload
  wird dadurch nicht wieder ausführbar. Beim erneuten Öffnen der Queue wird nur
  die Bereinigung bereits abgeschlossener, belegter Quellen nachgeholt.
- Ältere Zwischenstände ohne gespeicherte Fensterbindung und nicht zugeordnete
  Crashreste bleiben zur manuellen Prüfung erhalten. Ein passender Ordnername
  allein erlaubt keine automatische Löschung.

## Prüfstand

Core-/Queuefälle bestehen auf macOS und Linux: Drift, Abbruch, Journalfehler,
Neustart, Konfliktfortsetzung und verlorene Uploadantwort. Browserfälle prüfen
alle angebotenen Remoteziele und schmale Ansichten. Reale OpenSSH-SFTP-, FTP-,
FTPS-, WebDAV- und Samba-Gegenläufe übertragen ZIPs sowie entpackte Dateien und
Leerordner. Die Bereinigung nach erfolgreichem Upload ist zusätzlich über
OpenSSH-SFTP auf beiden Plattformen und verschlüsseltes Samba auf Linux geprüft;
Dateiinhalte und leere Zielordner bleiben erhalten. Der echte Linux-Fensterlauf belegt Packen, Entpacken, Ersetzen mit
unveränderten Archivbytes und Zwischenstand-Navigation; alle Testprozesse wurden
beendet. Der native macOS-Lauf bestätigt ZIP-Erzeugung und Entpacken zur
SFTP- und HTTPS-WebDAV-Gegenpane, exakte Inhalte, Leerordner und jeweils beide
abgeschlossenen Queueeinträge nach Neustart. Die vollständige native Matrix mit SFTP, FTP, FTPS, HTTPS-WebDAV und
verschlüsseltem SMB3 ist für macOS-App, AppImage und DEB abgeschlossen.
[Abnahme der endgültigen Pakete](release-plan.md).

[Lokales Packen](archive-pack.md) und [Entpacken](archive-extract.md) beschreiben
Formatgrenzen und Auslassungen. Der Upload erbt die Ausführungsklasse des Ziels.
