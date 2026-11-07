# Review-Nachprüfung 2026-11-01

Basis: `8724e4b5`. Sieben Hinweise wurden am aktuellen Quellstand geprüft.

| Hinweis | Entscheidung / Beleg |
| --- | --- |
| Recoveryeintrag trotz Abbruch vor dem ersten Rename | Korrigiert. Echter FTP-Gegenfall für Drift und Abbruch prüft unveränderte Namen und leere Queue auch nach erneutem Öffnen. |
| SMB-No-Replace-Kollision wird als unklar behandelt | Korrigiert. Garantiert abgelehnte Commits erhalten `Unchanged`; Sparse-Intents bekommen ihren bekannten Temp zurück. Besonders DACL-gebundene Temps können weiterhin zur Prüfung erhalten bleiben. Intent- und Fehlerklassentests sowie der reale Samba-Gegenlauf für reguläre und Sparse-Temps bestehen. |
| Informationsdialog verliert Auswahl, Rechteentwurf und unbekannten Modus | Korrigiert. Pane-Refresh erhält die Ansicht; Größenereignisse ändern ausschließlich Größenwerte. Browsergegenfälle prüfen Auswahl, offenen Editor, Eingabe, Cursor und unbekannte Rechte. |
| CLI-Doppelfehler verliert Neuordnerrechtebilanz | Korrigiert. Gemeinsamer Queue-Fehlerkombinator erhält alle Wirkungsklassen. CLI-Tests prüfen auch die Neuordnerrechte nach zusätzlichem Journalfehler. |
| Veralteter Kommentar zum Umfang der Namenzyklen | Auf Datei- und Ordnerzyklen aktualisiert. |
| Ähnliche Provider-Wrapper zusammenfassen | Kein Funktionsfehler; kein Umbau für den Release. Die kurzen typisierten Fassaden bleiben bestehen. |
| Strengere DACL-Planprüfung in `apply` wiederholen | Verworfen: Der unveränderte Plan wurde bereits beim Erzeugen des Intents vollständig validiert. Kein erreichbarer ungültiger Zwischenzustand belegt. |

Offene Plattform- und Paketabnahme steht ausschließlich im [Releaseplan](release-plan.md).
