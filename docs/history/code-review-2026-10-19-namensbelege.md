# Ersetzungs-Readback mit wirkungsbezogenen Namensbelegen

Zwei tatsächliche FTP-Protokollgegenläufe belegen unterschiedliche Fehler.
Im ersten ersetzt der Produktivprovider `FtpsVfs` das exakt gewählte Ziel neben
einem bereits vorhandenen äquivalenten Namen korrekt. Zielbytes, unveränderter
Nachbar und genau ein finales Rename sind nachgewiesen; dennoch folgt
`outcome_unclear`. Dies betrifft sowohl Großschreibung als auch NFC/NFD.

Im zweiten bestätigt der künstliche Server das finale Rename, erhält den Temp
aber unter großgeschriebener Schreibweise. Das Ziel besitzt bereits die gewünschten
Bytes. Die bisherige Exaktsuche verneint die Temppräsenz, der Hash stimmt und der
Core meldet fälschlich Erfolg mit einer kopierten Datei. Der tatsächliche Temprest
und genau ein finales Rename sind nachgewiesen. Eine erste Fixture traf zu früh
den Upload-Abschluss; erst der ausdrücklich an das finale Ziel gebundene Lauf
zählt als roter Produktbeleg. Ein zuvor von der Ausführungssandbox verhinderter
Loopbackstart zählt ebenfalls nicht als Produktfehler.

## Korrektur

Bei bekannten exakten Providern bleibt der bisherige schnelle Weg erhalten.
Andernfalls bindet der abbrechbare Dateioperationspfad ausschließlich tatsächlich
äquivalente reguläre Nachbarn durch vollständige Listings und Inhaltshashes.
Vor dem Rename bestätigt ein frischer Bestand Ziel, Temp und Nachbarn erneut.
Ein vorhandener bestätigter Zielhash wird nach dieser zusätzlichen Arbeit nochmals
geprüft. Nach dem Rename müssen dieselben Nachbarn mit denselben Metadaten und
Inhalten erhalten sein; ein weiteres Listing bestätigt den aktuellen Zielbeleg.
Erst dann darf der konkrete Readback den exakt gewählten Zielnamen verwenden.
Der Zielinhalt wird weiterhin gegen den übertragenen Hash geprüft.

Die globale Provider-Aussage `has_exact_names` bleibt unverändert. Neue,
veränderte oder nicht belegbare Nachbarn werden nicht durch einen bloßen exakten
Treffer übergangen. Nicht reguläre äquivalente Nachbarn behalten den konservativen
Weg. Die zusätzlichen Reads sind auf die gelistete Größe plus ein Prüfbyte begrenzt
und beachten den bestehenden Abbruchkanal. Bei großen kollidierenden Nachbarn
kostet die zusätzliche Inhaltsverifikation entsprechend Lesezeit; gewöhnliche
Namen ohne diese Kollision und lokale exakte Namen erhalten diese Zusatzarbeit
nicht. Die Belege bleiben ausschließlich im aktuellen Backendaufruf.

Ein nur normalisiert sichtbarer möglicher Tempname ist jetzt weder ein
Abwesenheitsbeleg noch ein Eigentumsbeleg. Der Core meldet `outcome_unclear` und
überträgt auch die konkreten Kandidatenpfade als Prüfhinweise. Er gibt dafür
keinen Temp-Cleanup frei. Diese Pfade erteilen kein Entfernungsrecht.

## Schutzfälle und Prüfung

Fünf gezielte Protokolltests bestehen. Sie prüfen beide korrekten Ersetzungen,
den normalisierten Temprest, einen gleich langen geänderten Nachbarn trotz
unveränderter Listingzeit und einen erst beim Rename hinzugekommenen Nachbarn.
Ein Abbruch bei der zusätzlichen Nachbarprüfung verändert das Ziel nicht;
Quelle und Nachbar bleiben erhalten und mögliche Tempbestände werden bilanziert.

Verlorene Antworten werden vor und nach der tatsächlichen Renamewirkung geprüft.
FTPS hält seinen journalisierten Intent dabei bewusst unklar, auch wenn neue
Zielbytes sichtbar sind. Eine anfänglich zu starke Erfolgserwartung wurde gegen
diesen bestehenden Providervertrag korrigiert; der Produktcode wurde dafür nicht
abgeschwächt. Beide Fälle behalten `outcome_unclear`, einen erhaltenen Intent und
genau ein finales Rename. Die Mutation wird nicht wiederholt.

Die Protokollfälle verwenden konkret den Klartextmodus des Produktivproviders
gegen den vorhandenen künstlichen Loopbackserver mit In-Memory-Bestand. So sind
verschiedene Case-/Unicode-Namen unabhängig vom lokalen Dateisystem darstellbar.
Es wird kein neuer TLS-Namensgegenlauf oder nativer GUI-Lauf behauptet. Der
Fileops-Teillauf mit 81 Tests und Workspace-Clippy mit allen Targets bestehen.
Die vollständigen Rust-Gates bestehen auf beiden Systemen: macOS Core 1141, CLI 76, App 219; Linux Core 1160, CLI 76, App 209. Die Coreläufe haben jeweils 22 ignorierte Fälle, CLI jeweils 1, App macOS 2 und Linux 4. Explizite Kindtests werden durch ihre Eltern ausgeführt; echte Mount-/Providerfälle und zwei Linux-GTK-Fälle behalten ihre Voraussetzungen. Workspace-Clippy mit allen Targets und `-D warnings`, Fuzz-Build mit gelocktem Manifest und Formatprüfung bestehen ebenfalls auf beiden Systemen. Alle 385 Quellen bleiben während der vollständigen Läufe unverändert. Dieser Review betrifft das Namens-/Temp-Delta samt
seinen direkten Aufrufern; frühere vollständige Bereichsreviews bleiben erhalten.
