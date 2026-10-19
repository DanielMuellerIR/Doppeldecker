# Produkt- und Performancechancen

Stand: 2026-08-25

Diese Übersicht sammelt belegte Ausbauchancen aus der CodeQA-Kampagne vom
2026-08-25. Sie ist keine Zusage für Version 1.0 und ersetzt weder
[`roadmap.md`](roadmap.md) noch den aktiven [`backlog.md`](../backlog.md). Ein
Kandidat wird erst dann zur Aufgabe, wenn Nutzen, Sicherheitsvertrag,
Fertigkriterien und Testumgebung konkret beschlossen sind.

## Entscheidungskriterien

Ein Ausbau soll:

1. einen häufigen Dateimanager-Ablauf verkürzen oder einen heutigen Medienbruch
   beseitigen,
2. auf den bestehenden Core-, VFS-, Queue- und Capability-Verträgen aufbauen,
3. Pfade, Plan-Digests, Identitäten, Permits und Secrets weiterhin ausschließlich
   im zuständigen Backend binden und
4. einen messbaren Nutzen besitzen. Eine vermutete Beschleunigung ohne Benchmark
   ist kein Performancebefund.

## Nahe Funktionsausbauten

### 1. Ausschlussmuster im Sync-Assistenten

Der Core validiert `SyncOptions.exclude` bereits in `core/src/sync.rs`; der Planer
wendet die Muster in `core/src/sync/plan.rs` auf beide Bäume an. Das CLI bietet sie
als wiederholbares `--exclude` an. Nur der GUI-Assistent in
`frontend/sync-view.js` hat noch keine Eingabe dafür.

- **Nutzen:** Buildordner, Caches, `.DS_Store` oder temporäre Dateien lassen sich
  ohne Wechsel zum CLI aus einem Lauf nehmen.
- **Umsetzung:** geordnete Musterliste mit Syntaxhilfe und unmittelbarer
  Backendvalidierung; die Vorschau kennzeichnet ausgeschlossene Pfade getrennt von
  Konflikten und technisch ausgelassenen Einträgen.
- **Grenze:** Das Frontend übergibt nur Texte. Ausschlusssemantik, Mengenbilanz und
  Mirror-Entscheidung bleiben im Core. Ein ungültiges Muster stoppt vor jedem
  Providerzugriff.

### 2. Schnellfilter und begrenzte Dateisuche

`frontend/pane-controller.js` besitzt Type-ahead-Navigation, aber keinen dauerhaften
Filter. Ein Schnellfilter kann deshalb zunächst ausschließlich die bereits geladene
Pane-Liste einschränken. Eine rekursive Suche ist ein getrenntes Core-Feature.

- **Schnellfilter:** Name, Endung und Dateityp; Escape leert den Filter, Auswahl und
  aktiver Eintrag bleiben über Objektidentität gebunden.
- **Rekursive Suche:** expliziter Root, Abbruch, Tiefen-/Treffergrenze, vollständige
  oder sichtbar partielle Bilanz und Provider-Paging. Treffer tragen Endpunkt- und
  Profilrevision; eine spätere Mutation plant und bindet den Pfad erneut.
- **Grenze:** Suchtreffer sind nie selbst ein Schreibpermit. Symlinks werden nicht
  verfolgt, unsicher dekodierbare Namen bleiben sichtbar, aber nicht bedienbar.

### 3. Gespeicherte Sync-Aufträge

Häufig verwendete Pane-Paare könnten als benannte Aufträge mit Richtung, Modus,
Metadatenprofil und Ausschlussmustern gespeichert werden.

- **Nutzen:** Wiederkehrende Backups oder Veröffentlichungsabläufe beginnen mit
  einem Klick und trotzdem mit einer frischen Vorschau.
- **Persistiert werden dürfen:** lokale Pfadtexte beziehungsweise Providerart und
  Profil-ID sowie öffentliche Optionen.
- **Nicht persistiert werden dürfen:** Secretwerte, Sitzungen, Handles, Listings,
  Phrasen, Permits oder ein alter Plan. Beim Start liest Doppeldecker die aktuelle
  Profilrevision, verbindet frisch, plant neu und verlangt die heute erforderliche
  Bestätigung.

### 4. Zentrale Recovery-Ansicht

Queue-Recovery, Sync-Quarantäne, Papierkorb-Journale, bidirektionales
Resume/Rollback und unklare Ausgänge besitzen heute getrennte, typisierte Verträge
und Bedienorte. Eine zentrale Ansicht könnte alle offenen Wiederherstellungsfälle
nach Ursprung, Zustand und zulässiger nächster Aktion ordnen.

- **Nutzen:** Nach Absturz oder Verbindungsverlust ist sofort sichtbar, was nur
  geprüft, fortgesetzt, zurückgerollt, wiederhergestellt oder bewusst geschlossen
  werden kann.
- **Umsetzung:** ein rein lesender Core-Index über die bestehenden Journale; jede
  Aktion ruft weiterhin den jeweiligen typisierten Recoveryvertrag auf.
- **Grenze:** Die Ansicht darf Zustände nicht allein aus Pfadexistenz ableiten und
  keine universelle „erneut versuchen“-Aktion anbieten. `outcome_unclear` bleibt
  ohne belegte Zustandsklärung nicht wiederholbar.

### 5. Stapelumbenennung mit Vorschau

Eine Stapelumbenennung kann Präfix/Suffix, Suchen/Ersetzen und fortlaufende Nummern
abdecken.

- **Umsetzung:** Der Core erzeugt aus stabil gebundenen Ausgangseinträgen einen
  reinen Alt→Neu-Plan. Er prüft doppelte Ziele, Groß-/Kleinschreibung,
  Unicode-Normalformen, reservierte Namen und Zyklen. Erst die bestätigte Vorschau
  wird als Queue-Operation ausgeführt.
- **Wirkung:** Jede Stapelumbenennung braucht vor der ersten Mutation einen
  persistenten Gesamt-Intent und je Einzelschritt einen Phasenzustand. So bleiben
  auch azyklische Teilcommits nach Absturz oder Providerfehler fortsetzbar oder
  gezielt rückrollbar. Zyklen benötigen zusätzlich kollisionsfreie Zwischennamen.
- **Grenze:** Kein stilles Ersetzen. Provider ohne atomaren No-Replace-Rename nutzen
  je Schritt nur den vorhandenen sichtbaren Best-Effort-Vertrag mit
  identitätsgebundener Quelle, frischem Listing und Readback. Nach einem unklaren
  Ausgang wird kein weiterer Rename blind gestartet.

## Größere Erweiterungen

### Fortsetzbare große Transfers

Der aktuelle mutierende Reconnect-Vertrag verbietet einen blinden Wiederholversuch
zu Recht. Fortsetzen ist nur möglich, wenn ein Provider einen Teilbereich lesen oder
schreiben, die Temp-Datei eindeutig wiederfinden und deren bisherige Bytes oder
Blockdigests binden kann. Das Laufjournal müsste Quellidentität, Zieltempidentität,
bestätigte Blockgrenze und rollierenden Digest geheimnisfrei persistieren. Ohne
diese Fähigkeiten bleibt der heutige Neustart ab Byte null korrekt.

### Prüfsummenaktion und Gleichheitsprüfung

Der Core verwendet BLAKE3 bereits für Sync, Snapshots und Planbindungen. Eine
explizite Aktion könnte ausgewählte Dateien hashen, beide Panes vergleichen und das
Ergebnis als JSON exportieren. Der Hash wird immer frisch aus einem gebundenen
Stream berechnet; ein Providerwert darf nur als Abkürzung dienen, wenn Algorithmus,
Objektversion und Gültigkeitsbereich eindeutig sind.

### Fenster- und Sitzungsspeicher — inzwischen umgesetzt

Die optionale Sitzungswiederherstellung einschließlich mehrerer Tabs je Pane ist
umgesetzt und unter macOS nach echtem App-Neustart belegt; siehe
[Einstellungen und Sitzung](settings-session-diff.md) sowie [Pane-Tabs](pane-tabs.md).
Persistiert werden nur Providerreferenzen und Darstellungszustand. Offene
Operationen kommen ausschließlich aus ihren Queue-Journalen zurück; vorbereitete
Realmodusläufe, Bestätigungsphrasen und Permits werden niemals rekonstruiert.

### Archive als schreibgeschützter VFS

ZIP-, tar- und komprimierte tar-Dateien könnten zunächst als ausschließlich
lesbarer VFS erscheinen: begrenzte Pfad- und Entpackgrößen, keine absoluten oder
aufsteigenden Komponenten, keine Symlinkverfolgung und sichtbare Spezialdateien.
Kopieren aus dem Archiv kann dann den normalen Dateioperationsweg verwenden.
Schreiben in Archive ist ein eigener späterer Schritt, weil es meist einen
vollständigen Neuaufbau samt atomarem Commit verlangt.

## Performancechancen

### Messgrundlage

Vor jeder Optimierung entsteht ein reproduzierbarer Benchmark mit mindestens:

- flachen und tiefen lokalen Bäumen, einschließlich eines Bestands mit mindestens
  100.000 Einträgen,
- simulierten Providern mit gezählter Anfragezahl sowie niedriger und hoher Latenz,
- kleinen und großen Queue-Journalen und
- einem Frontendlauf mit aufgezeichneten langen Frames während Scrollen, Sortieren
  und gestreamter Ordnergrößen.

Gemessen werden Laufzeit, CPU-Zeit, Spitzen-Speicher, gelesene und geschriebene
Bytes, Provideranfragen, Journalbytes und UI-Frames. Testpfade und Zugangsdaten
gehören nicht in dauerhafte Messprotokolle. Eine Änderung gilt nur dann als Gewinn,
wenn Plan, Digests, Fehlercodes, Provideraufrufe mit Sicherheitswirkung und
Wiederherstellungszustand semantisch gleich bleiben und der Spitzen-Speicher nicht
unbegrenzt mit einem zusätzlich gehaltenen Scan oder Sortierschlüssel wächst.

### 1. Doppelte Baumdurchläufe innerhalb derselben Prüfphase vermeiden

`prepare_local_real_run` in `core/src/real_mode.rs` baut zuerst vollständige
Listing-Proofs und lässt danach `sync::plan` dieselben Bäume erneut traversieren.
`PreparedLocalRealRun::confirm` wiederholt beide Schritte absichtlich frisch; bei
Mirror-Entfernungen folgt ein weiterer Zweitscan.

Die zeitliche Trennung zwischen Vorschau, Bestätigung und Entfernungsphase bleibt
notwendig. Innerhalb **einer** Phase könnte jedoch ein kanonisches Scan-Artefakt
Listing-Proof, Plan und Mengenbilanz gemeinsam speisen. Das Ziel ist weniger
Verzeichnis- und Provideraufrufe, nicht das Wiederverwenden eines alten Scans.
Gegentests müssen Namen-, Inhalts-, Root- und Capability-Drift weiterhin zwischen
allen Phasen erkennen.

### 2. Nur einmal für die Pane-Darstellung sortieren

Lokale und alle vier Remote-Commands sortieren Listings in `src-tauri/src/*_commands.rs`
bereits nach Ordner und kleingeschriebenem Namen. Dabei erzeugt der Rust-Comparator
für viele Vergleiche neue Kleinbuchstaben-Strings. Anschließend sortiert
`frontend/pane-controller.js` dieselben Einträge erneut mit `Intl.Collator` nach der
gewählten Spalte und den Pane-Optionen.

Ein Benchmark soll zwei Varianten vergleichen: ungeordnete Backendantwort plus
alleinige Frontendsortierung oder einmal vorab berechnete Sortierschlüssel. Die
Planer-Sortierungen im Core sind davon unabhängig und bleiben für kanonische
Digests erhalten.

### 3. Fortschritt nicht als vollständiges Queue-Journal neu schreiben

`core/src/queue.rs` serialisiert bei jeder Persistenz den gesamten Snapshot als
formatiertes JSON und schreibt ihn atomar einschließlich Datei- und
Elternverzeichnis-Sync. Dateifortschritt darf bis zu viermal pro Sekunde persistiert
werden. Der Aufwand wächst dadurch mit der Zahl und Größe aller Queueeinträge, nicht
nur mit dem laufenden Eintrag.

Ein möglicher Ausbau trennt strukturelle Queuezustände von einem kleinen,
append-only Fortschrittsjournal und kompaktiert es beim terminalen Übergang. Vorher
müssen Crashpunkte, maximale Dateigröße, monotone Revision, Geheimnisfreiheit und
atomarer Besitzbeleg mindestens dieselben Garantien wie der heutige Snapshot
besitzen. Maßgeblich sind p95-Latenz und geschriebene Bytes bei 1, 100 und 1.000
Einträgen; eine geringere Sync-Zahl ohne gleichwertige Crash-Recovery ist kein
Gewinn.

### 4. Zweiten Remote-Download nur bei starker Objektversion ersetzen

`core/src/remote_snapshot.rs` lädt eine Remote-Datei in den lokalen Cache, liest sie
für einen zweiten Remote-BLAKE3-Beleg erneut und hasht danach auch die lokale Datei.
Das ist korrekt, kostet bei großen Dateien aber ungefähr zwei Netzwerkleseläufe.

Ein Provider darf den zweiten Netzwerkstrom nur dann ersetzen, wenn er eine starke,
unveränderliche Objektversion und einen kryptografischen Inhaltsdigest für genau
diese Version liefert oder einen bedingten Lesezugriff bis zum Streamende bindet.
Der lokale Readback bleibt bestehen. FTPS ohne solche Belege verwendet weiterhin
den heutigen Doppelstream.

### 5. Linux-Listing auf einen Metadatenaufruf je Eintrag bringen

`core/src/local_fs.rs::read_dir_entries` ruft für jeden Namen `statat` auf. Unter
Linux fragt `creation_time_at` anschließend zusätzlich `statx` nur für die
Geburtszeit ab. Ein gemeinsamer `statx`-Aufruf könnte Typ, Größe, Zeiten, Modus,
Gerät und Inode liefern und auf alten Dateisystemen kontrolliert auf den bisherigen
Weg zurückfallen. Tests müssen fehlende `BTIME`, Mountidentität, Symlinks und rohe
Namen unverändert behandeln.

### 6. Gestreamte Ordnergrößen bündeln

Bei aktiver Größensortierung sortiert und rendert
`frontend/pane-controller.js::renderDirectorySizeChange` die ganze Pane nach jedem
einzelnen Ordnerergebnis neu. Die Backendmessung ist bereits parallel, abbrechbar
und gecacht. Das Frontend kann Ergebnisse pro Animationframe oder kurzem festen
Intervall bündeln und danach einmal sortieren. Auswahl, Cursor und die sofort
sichtbare Teilbilanz dürfen dabei nicht verloren gehen.

### 7. Virtualisierte Scrollereignisse pro Frame zusammenfassen

Pane-Listen werden ab 2.000 Einträgen virtualisiert, aber jedes Scrollereignis ruft
direkt `renderRows` auf. Ein einzelner ausstehender `requestAnimationFrame` kann
mehrere Scrollereignisse zusammenfassen. Der letzte Scrollstand muss gewinnen;
ARIA-Aktivnachfahre, Overscan und Kontextmenü-Schließen bleiben synchron.

## Bereits vorhandene Leistungsgrenzen erhalten

- Pane-Listen virtualisieren ab 2.000 Einträgen statt den gesamten Bestand in das
  DOM zu schreiben.
- Ordnergrößen laufen parallel, abbrechbar und sitzungsgebunden mit Cache.
- GUI-Fortschrittsereignisse sind auf 50 ms, persistierter Dateifortschritt auf
  250 ms gedrosselt; terminale Zustände werden gesondert übertragen.
- Vollständige Listings, frische Readbacks, doppelte Inhaltsbelege und
  Verzeichnis-Syncs sind keine zufällige Mehrarbeit. Eine Optimierung darf sie nur
  durch einen nachweislich gleich starken Mechanismus ersetzen.
