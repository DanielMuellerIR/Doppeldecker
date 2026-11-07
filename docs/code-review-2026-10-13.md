# Review-Korrekturen 2026-10-13

Die drei bestätigten Befunde gegen `4adee46` betreffen die Recoverybilanz von
Dateioperationen und die Reihenfolge nativer Iconänderungen. Dieser Abschluss
betrifft ausschließlich diese Befunde; der offene Umfang des
[Vollreviews](code-review-2026-10-07.md) bleibt unverändert.

## Best-Effort-Commits

Bei Neuanlage und Ersetzen konnten nach einer verlorenen Rename-Antwort die
strukturierten Wiederherstellungspfade fehlen. Dasselbe galt für mehrdeutige
Listings und falsche Zielinhalte. Die Fehlerursprünge ergänzen jetzt Ziel und
gegebenenfalls Tempname, bevor die Best-Effort-Ausführungsklasse weitergereicht
wird. Belegt ein vollständiges, sicher dekodiertes Listing den freigegebenen
Tempnamen, bleibt nur das Ziel. Vorhandene Providerpfade überleben Reconnect-,
Listing- und Inhaltsfehler.

Der Regressionstest durchläuft vollständige Kopien für Neuanlage und Ersetzen
mit sieben Fehlerarten: gescheiterter Reconnect, fehlgeschlagenes, unvollständiges
oder mehrdeutiges Listing, falsche Größe, falscher Hash und Lesefehler. Er prüft
genau eine Mutation, unveränderte Quelldaten, vorhandene Zielbytes und die Pfade
im Fehler sowie in `FileOperationActuals.temp_recovery_paths`.

## Geparkter Altordner

Ohne privaten Stagingbereich kann eine Datei einen zuvor geparkten Zielordner
ersetzen. Scheiterte der Verzeichnis-Sync nach dem bestätigten Rename, nannte
die strukturierte Bilanz bisher Temp und Ziel statt Ziel und geparktem Altordner.
Die Bilanz entsteht nun direkt an dieser Phasengrenze. Bei einer unklaren
Rename-Antwort enthält sie zusätzlich den möglichen Tempbestand.

Der Regressionstest lässt erst den Sync nach dem zweiten Rename scheitern und
prüft die neue Datei, die unveränderten Altdateien in der Sicherung, erhaltene
Providerpfade und das Fehlen des freigegebenen Tempnamens in der Bilanz. Beide
neuen Core-Tests scheiterten auf der vorherigen Produktfassung an den jeweils
gemeldeten Pfadfehlern und bestehen nach der Korrektur.

## Parallele Iconänderungen

Ein Revisionsvergleich vor dem Zustellen eines Hauptthread-Tasks verhinderte
nicht, dass ein älterer Task zuletzt ankam. Der macOS-Task liest die aktuelle
Iconwahl jetzt erst bei seiner Ausführung. Der App-Start nutzt denselben Weg.
Der Regressionstest speichert zwei unterschiedliche Wahlen, lässt beide den
bisherigen Vorabvergleich bestehen und führt ihre Iconaktionen in umgekehrter
Reihenfolge aus. Beide Aktionen übernehmen die jüngste gespeicherte Auswahl.

## Prüfung und Grenzen

Die Tests verwenden künstliche Dateien und Konfigurationen. Die Linux-Gegenprobe
läuft in einer isolierten Kopie; SHA-256 der vier geänderten Rust-Dateien belegt
den gleichen Quellstand. Die vollständigen seriellen Testläufe bestanden:

| Prüfung | macOS | Linux |
|---|---:|---:|
| Core einschließlich Integrationstests | 899 bestanden | 918 bestanden |
| CLI einschließlich Integrationstests | 63 bestanden | 63 bestanden |
| App | 175 bestanden | 171 bestanden |
| Workspace-Clippy, alle Targets, `-D warnings` | bestanden | bestanden |

Je Core-Lauf blieben 15 bereits explizit ignorierte Tests ausgelassen, darunter
die eigens bereitzustellenden Mount-/Providerumgebungen. Die automatisch laufenden
FTP-/FTPS- und WebDAV-Tests bestanden mit künstlichen Serverroots. Die Formatprüfung,
`git diff --check` und der gesperrte Fuzz-Build bestanden auf macOS. Das Frontend
wurde nicht geändert; ein zusätzlicher Browserlauf war nicht Teil dieser Prüfung.

Die bestehende native macOS-Prüfung setzt und liest das Bundleicon an einem
temporären Bundle und prüft unveränderte Ressourcen. Der vollständige manuelle
Einstellungsdialog-/Dock-Ablauf wurde anschließend am 2026-10-13 visuell geprüft:
Die laufende App übernimmt beide gewählten Motive; nach dem Beenden fällt das
angeheftete Dockicon auf die blaue Vorgabe zurück, obwohl Einstellung und natives
Bundleicon weiterhin `aurora` enthalten. Nach ergänzender Nutzerbeobachtung
aktualisiert „Optionen → Im Finder anzeigen“ die Anzeige. Der Punkt ist als
bekannte Finder-/Dock-Cache-Eigenheit dokumentiert und abgeschlossen; eine weitere
Untersuchung ist nicht vorgesehen. Ablauf und Belege stehen in der
[Icon-Persistenzprüfung](settings-session-diff.md).
Die dortige temporäre Testinstallation wurde zurückgenommen. Es wurde keine
Produktversion erhöht oder Veröffentlichung ausgeführt.
