# Manueller, signierter Updatekanal

Stand: 2026-10-07. „Auto-Update“ bezeichnet die technische Aktualisierung der
installierten App. Vor Version 1.0 prüft und installiert Doppeldecker ausschließlich
auf ausdrücklichen Auftrag über das Menü.

## Bedienvertrag

1. Nur **Doppeldecker → Nach Updates suchen …** startet einen HTTPS-Abruf. Es gibt
   keinen Start-Hook, Timer oder Hintergrundabruf.
2. Ein gefundener Release wird mit Version und Hinweisen gezeigt. „Später“ lädt nichts.
3. Erst **Herunterladen und installieren** startet den Download. `tauri-plugin-updater`
   prüft die Nutzlast zwingend mit dem in der App eingebauten öffentlichen Schlüssel;
   die Signaturprüfung lässt sich nicht abschalten.
4. macOS und Linux verwenden die neue Version beim nächsten Start. Doppeldecker beendet
   oder startet sich nicht ungefragt neu.

Release-Hinweise sind fremder Text aus `latest.json`. Das Frontend maskiert sie deshalb
vor der Anzeige als Text und übernimmt weder HTML noch Links.

## Backend- und Fenstergrenze

`src-tauri/src/update_commands.rs` hält den von Tauri geprüften `Update`-Wert nur im
Speicher und nach Fensterlabel getrennt. Das Frontend erhält Version, Datum und Hinweise,
aber weder Download-URL noch Signatur oder installierbaren Handle. Bei der zweiten Stufe
muss es die sichtbare Version wiederholen; ein fehlender oder abweichender Backendwert
endet als `plan_drift`.

Download und Installation reservieren dieselbe Single-Flight-Schranke wie Datei- und
Sync-Operationen dieses Fensters. Dadurch kann kein Queue-Lauf während des Austauschs
beginnen; ein laufender Queue-Lauf gewinnt umgekehrt zuerst. Andere Doppeldecker-Fenster
behalten ihre eigene Schranke und bleiben unabhängig. Fortschritt wird nur an das
auslösende `WebviewWindow` gesendet. Vorbereitete Werte werden nie persistiert.

Updateprüfung und Installation sind zusätzlich appweit gegeneinander gesperrt,
weil alle Fenster dieselbe installierte Anwendung verwenden. Das gilt nur für
Updateaktionen; die normalen Operationsschranken bleiben fenstereigen. Nach einer
erfolgreichen Installation werden sämtliche vorherigen Updatefunde aller Fenster
entwertet. Eine Generation sperrt auch verspätete Antworten einer älteren Prüfung.
Nach einem unterbrochenen IPC-Aufruf kann die Oberfläche den tatsächlich noch
vorbereiteten Backendstand abfragen, bevor sie eine erneute Installation anbietet.

## Schlüssel und Release-Artefakte

Der öffentliche Minisign-Schlüssel steht als Inhalt, nicht als Pfad, in
`src-tauri/tauri.conf.json`; dort ist auch der spätere HTTPS-Endpunkt für
`latest.json` gebunden. Beides muss bereits in der ersten updaterfähigen App vorhanden
sein: Fehlt der gesamte Konfigurationsblock, verweigert Tauri die Plugin-Initialisierung
und damit den App-Start. Der zugehörige private Schlüssel verlässt den geschützten
Secret-Speicher nur für den Release-Prozess. `tools/build-release.py` verlangt für
öffentliche macOS- sowie Linux-Pakete beide Umgebungswerte:

- `TAURI_SIGNING_PRIVATE_KEY`
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`

Ohne einen Wert bricht der Build vor Tauri ab. Auf macOS erzeugt Tauri zunächst das
Updaterarchiv; der Releasepfad packt und signiert es nach der App-Notarisierung neu.
Unter Linux bleibt `createUpdaterArtifacts` aus: Der Linux-Prüfer entnimmt zuerst aus
DEB und AppImage jeweils das CLI-Binary, führt `--version` aus und vergleicht die
Ausgabe exakt mit der Workspace-Version. Erst danach signiert er AppImage, DEB und die
hashgebundene AppImage-Attestation. Der normale Developer-Build braucht und erzeugt
diese Dateien nicht.

Nachdem beide Plattformartefakte gesammelt wurden, erzeugt
`tools/write-update-manifest.py --pub-date <RFC-3339-UTC>` die statische
`target/release/bundle/latest.json`. Sie enthält ausschließlich die belegten Ziele
`darwin-aarch64-app`, `linux-x86_64-appimage` und `linux-x86_64-deb`; die beiden
Linux-Paketarten können dadurch nicht verwechselt werden. Alle Einträge verweisen auf
Dateien desselben GitHub-Releases. Das Skript akzeptiert je Paketart genau eine
Nutzlast und liest deren Signatur direkt aus der benachbarten `.sig`-Datei.

Jede Nutzlast wird dabei fail-closed an den Releasevertrag gebunden: Die Linux-Pakete
müssen exakt `Doppeldecker_<version>_amd64.AppImage` beziehungsweise `.deb` heißen,
und weil das macOS-Archiv immer denselben Namen trägt, liest das Skript dessen
Kurzversion aus `Info.plist` und die Architektur aus dem Mach-O-Kopf des
Hauptbinaries. Ein liegengebliebenes Paket einer älteren Version kann deshalb nicht
unter der aktuellen Versionsnummer veröffentlicht werden. Die Veröffentlichungszeit
wird vollständig als RFC-3339-UTC-Wert geparst und kanonisch geschrieben.

Der npm-Schritt `release:update-manifest` ist die gemeinsame Finalisierung: Er
schreibt zuerst das Manifest und erzeugt danach `SHA256SUMS` neu. Nur in dieser
Reihenfolge enthält die ausgelieferte Prüfsummendatei das tatsächlich veröffentlichte
`latest.json`; `tools/write-release-checksums.py` nimmt Nutzlast, Signatur und
Manifest gemeinsam auf.

Die Veröffentlichungszeit kommt dabei über die Umgebung, nicht über ein Argument:

```bash
DOPPELDECKER_PUB_DATE=2026-08-06T18:00:00Z pnpm release:update-manifest
```

npm und pnpm hängen zusätzliche Argumente ans **Ende** der Kommandozeichenkette, also
an den zweiten Befehl — ein `--pub-date` würde `write-release-checksums.py` erreichen
und das Manifestskript nie. Beim direkten Aufruf von `tools/write-update-manifest.py`
funktioniert `--pub-date` weiterhin. Fehlt beides, bricht der Lauf ab; die Zeit wird
nie geraten.

Der private Schlüssel ist nicht wiederherstellbar: Sein Verlust verhindert weitere
Updates bereits installierter Versionen. Rotation verlangt deshalb einen gesonderten,
vorher ausgelieferten Vertrauensübergang und gehört nicht in einen normalen Release.

Der Kanal verwendet bewusst Tauri statt Sparkle. Dadurch bleiben Prüfung, Zielauswahl
und Installation für die macOS-App, das Linux-AppImage und das DEB in derselben
Rust-Integration. Sparkle wäre ein zusätzlicher, ausschließlich macOS-spezifischer
Updater und würde die Linux-Verträge nicht abdecken.
