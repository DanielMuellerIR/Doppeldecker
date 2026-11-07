# Vollreview: SMB-Verbindung und Dateioperationen

Der Bereich `core-smb` wurde mit allen sieben aktuellen Bereichsdateien vollständig
gelesen: Provider, Samba-E2E, Launcher samt Selbsttests und die gezielt angepasste
Verbindungsimplementierung der vendorten Bibliothek. Die Ausgangsfassung umfasst
6820 Zeilen. Mitgeprüft wurden die direkten Authentifizierungs-, Stream-,
Fähigkeits- und Fehlergrenzen. Andere offene Bereiche bleiben separat offen.

## Korrigierte Befunde

- **P1: ungeschützte Antworten trotz aktiver Sitzungsverschlüsselung.** Der echte
  Paketempfänger akzeptierte Klartext, korrekt signierten Klartext und eine
  ungeschützte Zwischenantwort. Er verwirft diese Frames jetzt vor Dekompression,
  Creditänderung und Routing und beendet die Verbindung. Der Gegenlauf verwendet
  den öffentlichen Transportvertrag und künstliche Protokollpakete; gültiges
  AES-GCM bleibt erfolgreich, ein veränderter Authentifizierungstag wird abgelehnt.
  Ein Angriff über ein echtes Netzwerk wird damit nicht als ausgeführt behauptet.
- **P1: GUID-Pin erst nach Authentifizierung geprüft.** Der bisherige
  High-Level-Verbindungsaufbau löste schon das Secret auf und führte NTLM aus.
  Der Core verhandelt jetzt über die vorhandene Connection-/Session-Schnittstelle,
  prüft Dialekt, Cipher und beobachtete GUID auf derselben Verbindung und löst
  erst danach das Secret auf. Der Samba-Gegenbeleg zählte am alten Stand eine
  Secretanforderung bei falschem Pin. Die öffentliche GUID ersetzt keine
  kryptografische Serverauthentifizierung.
- **P2: leerer Read schloss den Stream.** Ein anschließender nicht leerer Read
  scheiterte am geschlossenen SMB-Handle. Ein leerer Puffer liefert jetzt null,
  ohne den Stream zu schließen. Der Gegenbeleg stammt vom echten Samba-Server.
- **P2: Verbindungszustand und Empfängertask hielten sich gegenseitig.** Der
  Empfänger hält den gemeinsamen Zustand während des Wartens jetzt nur schwach.
  Ein Gegenlauf prüft die tatsächlichen Transportreferenzen nach dem letzten
  Connection-Klon; zuvor blieben drei Referenzen bis zum Runtimeende erhalten.
- **P1: Cleanup nach fehlgeschlagener Probe entfernte fremden Bestand.** Eine
  echte exklusive Anlage gegen einen bereits vorhandenen bekannten Probenamen
  scheiterte; der anschließende Cleanup entfernte trotzdem die fremde Datei.
  Alle vier SMB-Proben erhalten nach einer fehlgeschlagenen Wirkung den Bestand
  und melden den möglichen Probenrest mit strukturiertem Recoverypfad.
  Erfolgreiche Proben verwenden weiter ihren begrenzten Cleanup. Eine atomare
  Prüfung der Objektidentität mit anschließendem Löschen wird nicht behauptet.
- **P1: Fähigkeitsabfrage verlor den unklaren Ausgang.** Die öffentliche
  Dateioperations-Abfrage verwandelte einen unklaren Probenrest in gewöhnlichen
  Blockertext und setzte weitere Proben fort. Sie verwendet jetzt dieselbe
  Fehlererhaltung wie Rename: Verbindungsfehler, unklarer Ausgang, Drift und
  Abbruch behalten ihren Code und Recoverypfad. Ein Gegenlauf mit künstlichen
  SMB-Antworten prüft den Abbruch weiterer Proben.
- **P2: vollständiger Pfad überschritt das Protokoll-Längenfeld.** Der
  CREATE-Codec speichert die UTF-16-Bytelänge in einem 16-Bit-Feld. Der vollständig
  gemappte Pfad einschließlich Profilroot wird jetzt vor dem Providerzugriff auf
  32767 UTF-16-Einheiten begrenzt. Ein Gegenlauf prüft Grenze, Überschreitung und
  den zusätzlichen Profilroot.

## Verifikation

Alle 15 SMB-Unitfälle und der vollständige verschlüsselte Samba-End-to-End-Lauf
bestehen. Samba prüft den falschen GUID-Pin vor jeder Secretauflösung sowie leere
Reads vor, während und nach dem Inhalt. Nach erfolgreicher Fähigkeitsprüfung
verweigert eine frische Probe bei fehlender Berechtigung neue Wirkungen, auch wenn
die Sitzung frühere positive Capabilityflags behält. Eine globale Invalidierung
wurde deshalb nicht ergänzt. Beide geprüften Roots bleiben frei von Probenresten.

Die vollständigen Gates bestehen auf macOS mit 1122 Core-, 69 CLI- und 207
App-Tests sowie auf Linux mit 1141 Core-, 69 CLI- und 197 App-Tests. Je 21
Core-Spezialfälle und unter Linux zwei GTK-Fälle bleiben ignoriert.
Workspace-Clippy mit allen Targets und `-D warnings`, Fuzz mit
`--locked --all-targets` und Formatprüfung bestehen beidseitig. Die vier
Launcher-Selbsttests bestehen ebenfalls auf beiden Plattformen; der gesonderte
Rustfmt-Check der vendorten Verbindungsdatei besteht.

Die Plattformprüfungen beziehen sich auf einen eingefrorenen Arbeitsstand mit
383 Code-/Werkzeughashes. Darin enthaltene, getrennt vorbereitete Fensterkorrekturen
bleiben bis zu ihrer nativen Endabnahme ein eigener offener Arbeitspunkt.
