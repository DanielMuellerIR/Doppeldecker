# Prüfung der Konfigurationsreste

Stand: 2026-10-19. Geprüfter Quellstand: `2cc867e`.

Der ältere Bereinigungskandidat `config-dir-leftovers` wird begründet verworfen.
Alte Settings-Temps und Journale bleiben tatsächlich erhalten. Seine Annahme,
eine einmalige Bereinigung unter der gemeinsamen Prozesssperre sei bereits
vertragskonform, ist jedoch durch den aktuellen Besitz- und Recoveryvertrag
widerlegt. Die Prüfung ändert keine Produktlogik.

## Besitz und Aufbewahrung

`OperationQueueProcessLock::acquire` in `core/src/queue.rs` hält ausdrücklich
fest, dass passende Tempnamen auch fremde reguläre Dateien oder Symlinks sein
können. Eine Prozesssperre bindet keinen bei der exklusiven Anlage entstandenen
Besitzbeleg. Auch ein Verschieben in ein Archiv wäre eine Wirkung auf einen
möglicherweise fremden Bestand. Der atomare Core-Writer bereinigt eigene Temps
nur über seinen gehaltenen Erzeugungsbeleg.

`promote_session_main` in `src-tauri/src/window_commands.rs` verschiebt den
Sitzungsstand im RAM nach `main` und erhält das frühere Journal. Die
Startup-Recovery entscheidet anhand des Queuezustands, welche Labels eine offene
Queue tragen. Diese Entscheidung ist keine Prüfung auf entbehrliche Historie:
Auch terminale Einträge können `temp_recovery_paths`, `retained_backup_paths`
und Wiederherstellungsbelege tragen. `next_window_label` schützt vorhandene
Journale mit fehlererhaltendem `symlink_metadata`; unbekannte Fehler zählen nicht
als freies Label.

`desktop_settings::write_document` erzeugt einen exklusiven zufälligen Tempnamen.
Der Reader übernimmt ausschließlich die eigentliche Settings-Datei. Ein alter
Tempname enthält keinen persistierten Nachweis, der nach einem Crash eine
allgemeine Bereinigung autorisieren würde.

## Ausgeführte Prüfung

Vier vorhandene Regressionstests liefen auf macOS jeweils nach genau einem
Treffer in `cargo test -- --list` mit vollständigem Namen und `--exact` grün:

- `queue::tests::tempdatei_ohne_persistierten_besitzbeleg_bleibt_unangetastet`
  erzeugt eine künstliche Altdatei und prüft deren unveränderte Bytes nach Queueöffnung.
- `queue::tests::zweite_fensterqueue_entfernt_keine_aktive_tempdatei`
  öffnet zwei Fensterqueues mit gemeinsamer Sperre und erhält den aktiven Schreibrest.
- `queue::tests::temp_recoverypfade_sind_terminal_in_queue_und_audit_gebunden`
  prüft die dauerhafte Bindung von Recoverypfaden nach terminalem Abschluss.
- `window_commands::tests::nebenfenster_mit_offener_queue_wird_nicht_zum_hauptfenster`
  prüft sichere Beförderung, offene Nebenfensterqueues und ein vorhandenes Hauptfenster.

Die 385 prüfrelevanten Dateien blieben bytegleich zum vorher geprüften Stand.
Es gab keine Änderung an Core, App, Frontend, Dateiformat oder Startverhalten;
deshalb wurden keine neuen vollständigen Plattform- oder GUI-Läufe beansprucht.

## Verbleibende Grenze

Die Aufbewahrung ist absichtlich nicht durch Alter oder Menge begrenzt. Eine
spätere explizite Historienverwaltung müsste Eigentum, Recoverybelege,
Nutzerquittierung und konkurrierende Writes gemeinsam definieren. Das ist keine
zulässige Folge einer bloßen Präfixbereinigung. Diese Prüfung führt daher weder
eine Löschung noch einen neuen Archivierungsmechanismus ein.
