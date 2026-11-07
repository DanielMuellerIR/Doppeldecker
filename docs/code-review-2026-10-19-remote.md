# Deltareview: gemeinsame Remote-Verträge

Der Bereich `core-remote-common` wird seit dem vollständigen Review auf
`fb14df1` nachgeprüft. Gelesen sind alle Änderungen der bisherigen sieben Dateien,
die beiden neu zugeordneten Dateien für Ordneranlage und gewöhnliche Umbenennung
vollständig sowie ihre heutigen Core-, Provider-, Tauri- und CLI-Grenzen.
`connections.rs` und `remote_delete.rs` sind gegenüber der Vollreviewbasis
bytegleich; ihre damaligen Nachweise bleiben erhalten. Dies ist kein erneuter
Vollreview aller neun Dateien.

## Korrigierte Befunde

- **P2: Der bekannte Probenrest fehlte in den strukturierten Recoverypfaden.**
  `combine_probe_outcome` nannte den übergebenen Basisordner nur im Fehlertext,
  wenn Cleanup scheiterte. Ein echter nicht leerer Testordner reproduziert den
  Verlust sowohl nach erfolgreicher als auch nach fehlgeschlagener Probe.
  Der Basisordner ergänzt jetzt die vorhandenen strukturierten Hinweise;
  Fehlerart und führender Fehlercode bleiben erhalten.
- **P2: Ein angehängter Cleanup-Pfad blieb ebenfalls reiner Text.**
  `error_with_path` erhielt innere Recoverypfade, fügte seinen eigenen konkreten
  Pfad aber nicht hinzu. Der Gegenlauf verwendet eine tatsächlich gescheiterte
  Ordnerentfernung und prüft vorhandenen Bestand, Fehlerart, Code und beide
  Pfadhinweise. Ein Hinweis erteilt weiterhin kein Recht zum Entfernen.
- **P2: Providerungültige Rename-Eingaben erreichten Profil und Queue.**
  Der gemeinsame Vorprüfer akzeptierte unter anderem FTP-Steuerzeichen sowie
  SMB-Pfadtrenner und zu lange Komponenten. Die spätere Provideroperation lehnte
  diese Eingaben ab. Vorher erreichte eine echte CLI-Anfrage mit Zeilenumbruch im
  neuen Namen jedoch die Profilauflösung und erzeugte bereits ein Queue-Journal.
  Root, Quelle und vollständiges Ziel durchlaufen jetzt vor diesen Zugriffen
  dieselben reinen Pfadmapper wie FTP/FTPS beziehungsweise SMB. Deren Regeln
  werden wiederverwendet; die beiden vorhandenen Funktionen werden lediglich
  innerhalb der Crate sichtbar. Eingabefehler erhalten hier `invalid_input`.
  Lokale zulässige Namen und wörtliche FTP-Backslashes bleiben erhalten.
- **P2: Eine Rename-Fehlerhülle verlor geerbte Recoveryhinweise.**
  Der gemeinsame Koordinator wandelte einen fehlgeschlagenen Readback oder
  Reconnect korrekt in einen unklaren Ausgang um, verwarf aber vorhandene
  strukturierte Pfade der Ursache. Der Gegenlauf führt im produktiven
  Koordinator über einen Testprovider genau eine echte Dateiumbenennung aus und
  injiziert danach einen Listingfehler mit einem vorhandenen Prüfpfad. Die
  umbenannten Bytes und der Prüfbestand bleiben erhalten. Die Fehlerhüllen
  übernehmen jetzt die bereits belegten Pfade ihrer Ursachen; Fehlercodes,
  Ausführungsklassen und die Grenze von einer Mutation bleiben unverändert.
  Ein echter Netzwerkfehler wird mit diesem Gegenlauf nicht behauptet.

## Verifikation und Grenzen

Die Gegenläufe waren vor der jeweiligen Korrektur rot. Danach bestehen alle
20 gemeinsamen Remote-Tests, beide neuen Rename-Vorprüfungen und alle 18
Rename-Koordinatortests. Die echte CLI verwirft ungültige FTP-/SMB-Eingaben jetzt
mit leer bleibenden künstlichen Konfigurationsordnern. Der gültige Kontrollfall
erreicht weiterhin die Profilauflösung. Nach dem ersten frisch gebauten
CLI-Start wurde die Prüffrist einmal überschritten; der anschließende vollständige
Gegenlauf besteht. Eine Ursache für diese Startverzögerung ist nicht belegt.

Die vollständigen Gates bestehen auf macOS mit 1127 Core-, 69 CLI- und 207
App-Tests sowie auf Linux mit 1146 Core-, 69 CLI- und 197 App-Tests. Je 21
Core-Spezialfälle und zwei Linux-GTK-Fälle bleiben ignoriert. Workspace-Clippy
mit allen Targets und `-D warnings`, Fuzz mit `--locked --all-targets`,
Formatprüfung und die vier SMB-Launcher-Selbsttests bestehen beidseitig. Der
frische verschlüsselte Samba-End-to-End-Lauf und der gesonderte Vendor-Rustfmt-
Check bestehen ebenfalls; 383 Quell-/Werkzeughashes bleiben unverändert.

Im ersten macOS-App-Lauf scheiterte der bestehende Icon-Metadatentest an seinem
künstlichen Bundle. Der gezielte Einzelfall und danach die vollständige App-Suite
bestehen ohne Quelländerung; die erste fehlgeschlagene Runde bleibt als Beleg
erhalten. Eine Ursache ist nicht belegt. Andere noch offene
Provider- und Schnittstellenreviews bleiben getrennt offen. Weitere manuelle
I/O-Kontexthüllen sind als gesonderter Kandidat erfasst; ihre tatsächlichen
Fehlerwirkungen sind noch zu reproduzieren.
