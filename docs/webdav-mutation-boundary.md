# WebDAV-Mutationsbedingungen und strikte Grenze

Stand: 2026-10-18. AP13R korrigiert die übrigen Aufrufer des bisherigen getaggten
`If`-Helfers. Gewöhnliche Additionen, mkdir und Rename bleiben ausdrücklich
Best Effort. Allgemeine VFS-Schreibwirkungen, Quellentfernung, strikter Ziel-Sync,
Dauerlöschen und mutierende Quarantäneproben erhalten keine Freigabe.

## Standardbedingung und verbleibender Wettlauf

[RFC 4918 §10.4.3](https://www.rfc-editor.org/rfc/rfc4918.html#section-10.4.3)
verknüpft verschiedene getaggte Listen mit ODER. Mehrere Elternlisten verschärfen
eine Mutation deshalb nicht: Eine passende Vorfahrenliste kann eine falsche
unmittelbare Elternversion überdecken. `If-Match` bindet unabhängig davon die
Request-URI. `If-None-Match: *` beziehungsweise `Overwrite: F` schützt vorhandene
Namen, ohne die gesamte Elternkette zu binden.

Der gemeinsame Helfer `with_parent_condition` wählt jetzt ausschließlich die
bereits gebundene Version des unmittelbaren Elternordners. Fehlender oder
widersprüchlicher Beleg verhindert den Request. Die gesamte starke Kette bleibt
für die frischen Prüfungen vor und nach jeder Wirkung erhalten. Kein neuer
Collectiontoken ersetzt nach eigenen Mutationen den ursprünglichen Beleg.

Damit kann eine wahre Rootliste keine falsche unmittelbare Elternversion mehr
überdecken. Ein später Wechsel eines anderen Vorfahren bleibt trotzdem möglich,
während der unmittelbare Elternbeleg weiterhin passt. Die Wirkung kann vor der
Drifterkennung eintreten. Das ist keine gemeinsame atomare Rootbindung und keine
lokale Deskriptorbindung. Die Core-Verträge zeigen diesen verbleibenden Wettlauf
in GUI und CLI vor der Wirkung; maschinenlesbare Ergebnisse erhalten dieselben
Einschränkungen, Teilbilanzen und konkreten Recoverypfade. Die mkdir-/Rename-
Vertragstexte sind begrenzte schreibgeschützte Textfelder, per Tab/PageDown und
vollständiger Textauswahl bedienbar; die Namenseingabe bleibt davon getrennt.

## Inventar und tatsächlich verlangte Belege

| Wirkung / Aufrufer | Atomare Standardbedingung | Zusätzliche Prüfung und Grenze |
| --- | --- | --- |
| Gewöhnliches MKCOL, `create_bound_directory` | Genau eine unmittelbare Elternliste und `If-None-Match: *` | Vollständiges Kollisionslisting, frische Kette/Sitzung, exakter leerer Collection-Readback; kein Cleanup des Ordners. |
| Additiver Temp-PUT, `put_stream_and_readback` | Genau eine unmittelbare Elternliste und `If-None-Match: *` | Gehaltene lokale Quelle, vollständiger BLAKE3-Hash, starker PUT-Token, bedingter vollständiger Inhaltsreadback, privater Intent vor PUT. |
| Uploadcommit, `Upload::rename_plain_once` | Temp-`If-Match`, genau eine unmittelbare Elternliste, `Overwrite: F` | Frische Quelle, Temp, Kette, Profilrevision und Zielbestand; exakter vollständiger Zielreadback. Nur ein Versuch. |
| Gewöhnliches Rename, `InteractiveRename::mutate` | Quell-`If-Match`, genau eine gemeinsame unmittelbare Elternliste, `Overwrite: F` | Quelle und Ziel bleiben im selben Elternordner; vollständiger Bestand und Versionstokens vor/nach MOVE. Keine Quellentfernung über Copy/Delete. |
| Eigenes reguläres Temp-Cleanup, `Upload::cleanup_owned_temp` / `delete_owned` | Eigener starker Datei-`If-Match` und genau eine unmittelbare Elternliste | Frischer regulärer Dateibeleg und ursprüngliche Kette; nur vor unklarem Commit, ein DELETE und echter NotFound-Readback. Keine Collection-Depth, kein rekursives Cleanup. |
| Allgemeine VFS-Schreib-/Rename-/DELETE-Wirkungen | Vollständige gemeinsame Root-/Elternbindung wäre nötig | Passive konkrete Absage vor Providerzugriff. Kein Ersetzen, kein Quellentfernen, kein Sync-Ziel-Fallback. |
| Vier strikte Capability-Proben | Eine Primitiveprobe beweist keine vollständige Namespacebindung | Eingabevalidierung und konkrete Absage ohne MKCOL/PUT/MOVE/DELETE oder Listing. Alte Probenkörper samt rekursiver Bereinigung entfernt. |
| Datei-/Lösch-/Quarantäne-Capability-Koordinator | Keine aus Standardproben ableitbare Freigabe | Fähigkeiten bleiben falsch und der konkrete Blocker sichtbar. Kein Anlegen oder Bereinigen eines Quarantäneroots. |

Der öffentliche VFS führt keine versteckte Drop-Bereinigung mehr aus. Nur die
private additive Uploadsicht kann nach ihrem frischen regulären Eigentumsbeleg
das begrenzte Temp-DELETE auslösen. Der gemeinsame MOVE-Request bietet ausschließlich
`Overwrite: F`; die alte allgemeine Ersetzungsprimitive ist gesperrt.

## Recovery und unabhängige Funktionen

Intents entstehen vor PUT, MKCOL und möglichem Commit in privaten fenstereigenen
Queueordnern. Antwortverlust, widersprüchlicher Readback, Abbruch nach Teilwirkung
und echter Prozessabbruch erhalten die Prüfpfade. Ein unklarer Commit sperrt sowohl
automatischen als auch manuellen Retry, auch nach Queue-Reopen. Quittieren schließt
den Eintrag ohne neue Wirkung. Es gibt keinen Rücktausch, kein rekursives Cleanup
und keine vermeintliche Sicherung aus ungebundenem Copy/Rename/Delete.

Gehaltene Quellen, native Metadatenauslassungen, starke Tokens, vollständige
Listings, Profil-/Sitzungsrevisionen und lokale descriptor-relative Grenzen bleiben
bestehen. Lesen, WebDAV-Downloads in lokale Ziele und gewöhnliche bestätigte
Additionen, Ordneranlage und Umbenennung übernehmen keine strikte Sperre. Die
[Ersetzungsabsage](webdav-replace-boundary.md) bleibt erhalten.

## Gegenbelege und Prüfgrenze

Der echte HTTPS-Gegenlauf verwendet ein gültiges lokales v3-Testzertifikat und
eine RFC-ODER-Auswertung. Die AP13-Matrix trennt falsches Altziel, Temp,
unmittelbare Eltern und Root bei ansonsten wahren Listen. Die zusätzliche Matrix
prüft für PUT, MKCOL, MOVE und Datei-DELETE die einzelne Elternbedingung getrennt
von der Request-Version beziehungsweise Existenzbedingung. Je Fall gibt es genau
einen Mutationsversuch. Weitere Produktgegenfälle tauschen Eltern oder Profilroot
unmittelbar vor der Wirkung und prüfen Originalbestand, neuen Bestand, Quellen und
Sentinels. Strikte Abfragen und direkte VFS-Wirkungen müssen ohne Providerrequest
enden. Recovery-, Crash-, Queue-, Inhalts- und Baumtests bleiben eigenständige
Regressionen.

Die strikte VFS-Schreibgrenze und der verbleibende Vorfahrenwettlauf gelten
unabhängig von den übrigen freigegebenen Dateioperationen. Zusätzliche allgemeine
Datei-/Löschklassen benötigen einen eigenen begrenzten Ausführungsvertrag.

## Abnahme des AP13R-Deltas

macOS: 1053 Core-, 68 CLI- und 203 App-Tests bestanden; die App-Suite bestand nach
dem genannten Fastra-Befund vollständig seriell. Linux: 1071 übrige Core-Tests
bestanden mit ausdrücklich herausgefiltertem FTPS-Gegenfall; dieser bestand separat
mit `--exact`. 68 CLI- und 193 App-Tests bestanden. Je Plattform bleiben 21 opt-in
Core-Spezialtests ignoriert, unter Linux zusätzlich zwei native GTK-Clipboard-Fälle.
Die letzte Temp-DELETE-Diagnosekorrektur bestand danach zusätzlich alle 33 betroffenen
Upload-/Bedingungsfälle auf beiden Plattformen. HTTP 412 wird dabei als Datei- oder
Elterndrift erklärt, ohne einen bewiesenen Dateiaustausch zu behaupten.

Formatierung, Workspace-Clippy mit Warnungen als Fehler und Fuzz-Check mit
`--locked --all-targets` bestehen beidseitig. Der vollständige Browserlauf hatte
337 erfolgreiche Fälle und die beiden oben genannten Fehler; beide Fehlerfälle
bestanden einzeln. Dazu bestehen CSP, sechs Pane-Tests, acht gezielte Ordnerdialogfälle
und 135 Tooltests. Dies ist ausdrücklich keine pauschal grüne Gesamt-Gateabnahme.

Der endgültige Test-App-Build bestand die native macOS-Abnahme in beiden Themes
bei 800×600: gewöhnliches mkdir und Rename, Tab/PageDown und tatsächliches
vollständiges Textkopieren, F5-Konflikt ohne Ersetzung, frische Zustimmung nach
Neustart, Abbruch nach Teilwirkung, Antwortverlust, SIGKILL-Recovery und parallele
Fensterqueues samt Navigation. 14 MKCOL, 17 PUT, 16 MOVE und zwei bedingte DELETE
betrafen ausschließlich künstliche Daten beziehungsweise eigene reguläre Temps.
Jede Mutation trug genau eine unmittelbare Elternliste; Collection-Proben,
rekursives Cleanup und Blind-Retry fehlten. Quellen und Sentinels blieben erhalten.
Der eigenständige Cleanup bestätigte beendete eigene Prozesse, geschlossene Listener,
entfernte künstliche Schlüsselbundeinträge und wiederhergestellte Zwischenablage.
Keine Installation oder Veröffentlichung; die interaktive Linux-Abnahme bleibt offen.
