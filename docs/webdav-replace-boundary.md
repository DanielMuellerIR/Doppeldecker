# Grenze des bestätigten WebDAV-Dateiersetzens

Stand: 2026-10-18. Die Tragfähigkeitsprüfung für gewöhnliches Ersetzen direkter
lokaler regulärer Dateien ist negativ. Es gibt keine neue Ersetzungsfreigabe ohne
Collection-/Löschprobe. Additive Datei- und Baumuploads bleiben verfügbar.

## Warum ein einzelnes MOVE nicht genügt

Der erforderliche Commit müsste gleichzeitig die eigene vollständig geprüfte
Temp-Version, die ausdrücklich bestätigte Altzielversion und jede unveränderte
Elternversion vom Profilroot bis zum Pane-Ordner voraussetzen. `If-Match` bezieht
sich auf die Request-URI, beim MOVE also auf die Temp-Datei. `Overwrite: T` erlaubt
das Verwerfen des vorhandenen Ziels, bindet aber dessen Version nicht.

Ein getaggter `If`-Header kann ETags verschiedener Ressourcen ausdrücken. Nach
[RFC 4918 §10.4.3](https://www.rfc-editor.org/rfc/rfc4918.html#section-10.4.3)
gelten Bedingungen innerhalb einer Liste gemeinsam, mehrere Tagged-list-
Produktionen jedoch alternativ. Deshalb bedeutet

```http
If-Match: "temp-version"
If: <https://example.invalid/pane/file> (["old-version"])
    <https://example.invalid/pane> (["parent-version"])
Overwrite: T
```

keine gemeinsame Bindung von Altziel und Eltern. Ein passender Elterntoken reicht
für den `If`-Header, obwohl ein anderer Prozess die bestätigte Zieldatei bereits
ausgetauscht hat. Das ist zulässige Protokollsemantik, keine missachtete Bedingung.
Eine einzelne getaggte Altzielliste würde zusammen mit `If-Match` die beiden
Dateiversionen prüfen, aber keine vollständige Elternkette. Ein vorheriger GET
oder PROPFIND schließt den späteren Wettlauf nicht. Mehrere Bedingungen innerhalb
einer Liste beziehen sich auf dieselbe Ressource, nicht auf verschiedene Eltern.

Auch die [Protokollklärung vom 2026-06-09](https://lists.w3.org/Archives/Public/ietf-http-wg/2026AprJun/0074.html)
bestätigt die alternative Auswertung. Das zusätzliche Einreichen von Locktokens
hat eine getrennte Bedeutung und erzeugt keine UND-Verknüpfung von ETags.
Die [veröffentlichten Errata](https://www.rfc-editor.org/errata_search.php?rfc=4918)
ändern diese Regel zum Prüfdatum nicht.

LOCK, ein anderer serverseitiger Transaktionsvertrag oder eine ausdrücklich
schwächere Pfadwirkung wären andere Schnitte. Der hier geprüfte Server bietet
diese zusätzliche Bindung nicht. Eine Copy-/Delete-Sequenz, ein vorheriger Rename
oder ein nachträglicher Rücktausch stellen weder die geforderte gemeinsame
Bindung noch eine bewiesene Quarantäne her. Ohne Quarantäne wäre die Altversion
nach erfolgreichem Ersetzen unwiederbringlich verloren.

## Gemeinsamer enger Schnitt

Die private F5-/Menü-Zielsicht liefert weiterhin nur additive Varianten. Ihr
Core-Blocker erklärt die konkrete Protokollgrenze; das Frontend erfindet keine
Freigabe. Lokales WebDAV-CLI-`copy --replace` und der Queue-Executor lehnen diese
Ersetzungsabsicht mit `unsupported` vor Profil-, Secret- und Netzwerkzugriff ab.
Die reine Core-Eingabegrenze gilt auch für direkt verwendete interaktive
Endpunkte. Ein persistiertes `overwrite=true`, alte Best-Effort-Erwartungen und
Retry nach Queue-Reopen erteilen keine Ausnahme. Neue Dateien können separat
additiv kopiert werden. Move, Remote-Quellen und strikter Sync sind damit weder
neu freigegeben noch vollständig überprüft.

## Korrektur der bisherigen Elternzusage

Der gemeinsame HTTPS-Testserver prüfte bisher alle getaggten Elternlisten mit
UND. Er prüft jetzt die vom Produkt verwendete Teilsyntax standardgemäß mit
ODER. Damit waren frühere Tests kein Beleg für eine atomare gemeinsame
Elternbindung bei AP05/AP07/AP11/AP12.

Die Anwendung hält weiterhin starke Elternbelege, prüft die vollständige Kette
vor und nach jeder Wirkung und übernimmt keine neuen Collectiontokens nach
eigener Mutation. Trotzdem kann ein ausgetauschter Elternordner bereits eine
Wirkung erhalten, während eine andere Liste passt. Der Readback meldet dann
`outcome_unclear`; er verhindert die vorherige Wirkung nicht. GUI-/CLI-Hinweise
für Upload, mkdir und Rename benennen dieses konkrete Risiko jetzt ausdrücklich.
Ein möglicher neuer Ordner oder Tempbestand bleibt erhalten, ohne rekursives
Cleanup oder Wiederholung. AP13R sendet inzwischen ausschließlich die unmittelbare Elternbedingung und sperrt
strikte Proben vor jeder Wirkung. Die übrige Vorfahrenbindung bleibt Best Effort.
[Aktueller Mutationsvertrag](webdav-mutation-boundary.md).

## Ausführbare Gegenbelege

`core/tests/webdav_replace_boundary.rs` verwendet echtes HTTPS mit einem
validierten lokalen v3-Testzertifikat, vollständigen Listings, starken Tokens,
exklusivem PUT und genau einem MOVE pro Fall. Collection-Proben und Collection-
DELETE sind gesperrt. Sechs Fälle unterscheiden frische Versionen, ausgetauschtes
Altziel, veraltete einzelne Eltern, insgesamt falsche Listen und falsches
Temp-`If-Match`. Vor MOVE bleibt der Altinhalt erhalten; der Temp wird vollständig
mit BLAKE3 zurückgelesen. Der Gegenlauf belegt die unerlaubt gedachte Ersetzung
trotz falschem Altzieltoken sowie die getrennte Wirksamkeit von Temp-`If-Match`.
Quellen-/Zielpfade und Sentinels liegen ausschließlich im künstlichen Root.

Ein Queue-Gegenfall belegt vor und nach Reopen fehlenden Secret-/Netzwerkzugriff
und unverändertes Altziel trotz persistierter Ersetzungsflags. Ein echter
CLI-Prozess belegt denselben maschinenlesbaren Fehler vor Profilauflösung und
Queueanlage. Die korrigierten Upload-/mkdir-/Rename-Gegenfälle prüfen tatsächliche
Wirkungen nach Elternwechsel und deren ehrliche Recoverybilanz. Die additiven
AP11-/AP12-Fälle bleiben eigenständige Regressionen.

## Historischer AP13-Prüfstand

Am 2026-10-18 sind die vollständigen Headless-Gates grün: macOS 1056 Core-,
68 CLI- und 203 App-Tests; Linux 1075 Core-, 68 CLI- und 193 App-Tests. Je Plattform
bleiben 21 opt-in Core-Spezialtests ignoriert, unter Linux zusätzlich zwei native
GTK-Clipboard-Fälle. Formatierung, Workspace-Clippy mit Warnungen als Fehler und
Fuzz-Check mit `--locked --all-targets` bestehen auf beiden Plattformen.
339 Browserfälle, sechs Pane-Tests, CSP und 135 Tooltests bestehen ebenfalls.
Ersetzungsgrund und Quellgrenzen sind im Browser in beiden Themes bei 800×600
visuell geprüft, per Tab erreichbar, mit PageDown scrollbar und vollständig
auswählbar. Die Quellenbilanz bleibt zunächst sichtbar, ohne den eigentlichen
Konfliktvergleich durch eine unbeschränkte Meldungsliste zu verdrängen.

Der native macOS-Gegenlauf prüft den endgültigen App-Build in beiden Themes:
echten F5-Konflikt ohne Ersetzungsangebot, unveränderten Altbestand, 800×600,
lange Bilanzen, Tastatur, frische Zustimmung nach Neustart, Abbruch nach
Teilwirkung, Antwortverlust, SIGKILL-Recovery und zwei parallele Fensterqueues
samt Navigation. Er zählt 12 MKCOL, 17 PUT, 14 MOVE und zwei bedingte DELETE
ausschließlich eigener regulärer Temps. Keine Collection-Probe und keine
rekursive Bereinigung; Quellen und fremde Sentinels bleiben erhalten. Cleanup
bestätigt beendete eigene Prozesse, geschlossene Listener und entfernte künstliche
Schlüsselbundeinträge.

Es gibt keinen nativen Ersetzungslauf und keine neue Ersetzungsfreigabe. Die übrigen WebDAV-Mutationsverträge wurden anschließend im
[AP13R-Schnitt](webdav-mutation-boundary.md) begrenzt. Acht offene CodeQA-Ganzbereiche,
der Querschnitt und die menschliche interaktive Linux-Kandidatenabnahme bleiben offen.
