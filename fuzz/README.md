# Parser-Fuzzing

Drei Targets füttern beliebige Bytes in die JSON-Deserialisierung und anschließende
Kernvalidierung von Baseline, Laufjournal und Metadatenmanifest. Das vierte Target
`finder_favorites` prüft den Finder-Favoriten-Parser für Plist- und Bookmark-Daten.
Diese vier Targets öffnen keine Dateien und verbinden kein Netzwerk.

Das fünfte Target `archive_read` prüft den lokalen ZIP-Katalog, virtuelle Ordner
und Stored-/Deflate-Inhaltsstreams. Es schreibt ausschließlich maximal 64 KiB
Eingabe in eine reguläre Datei in einem eigenen `tempfile`-Verzeichnis; der Reader
selbst verändert nichts. Das temporäre Verzeichnis wird nach jedem Aufruf
freigegeben. Es gibt keinen Netzwerkzugriff und keinen Entpackvorgang.

```bash
rustup toolchain install nightly --profile minimal
cargo install cargo-fuzz --locked
cargo +nightly fuzz run bidirectional_baseline fuzz/seeds/bidirectional_baseline
cargo +nightly fuzz run bidirectional_run_journal fuzz/seeds/bidirectional_run_journal
cargo +nightly fuzz run metadata_manifest fuzz/seeds/metadata_manifest
cargo +nightly fuzz run finder_favorites
cargo +nightly fuzz run archive_read fuzz/seeds/archive_read -- -max_len=65536
```

Für kurze reproduzierbare CI-/Entwicklerläufe kann die Laufzahl begrenzt werden:

```bash
cargo +nightly fuzz run bidirectional_baseline fuzz/seeds/bidirectional_baseline -- -runs=10000
```

Die drei kleinen gültigen JSON-Seeds sind versioniert, damit kurze Läufe sofort die
strukturellen Validatoren erreichen. Bei Schemaänderungen erzeugt
`cargo run --manifest-path fuzz/Cargo.toml --example generate_seed_corpus` sie neu.
Wachsendes Corpus und Crash-Artefakte bleiben lokal unter `fuzz/corpus/`
beziehungsweise `fuzz/artifacts/` und werden nicht versioniert. Ein Fund wird erst
nach Minimierung als kleiner normaler Regressionstest in `core/` übernommen.

## ZIP-Reader

`archive_read` verwirft Eingaben über 64 KiB. Der Katalog nimmt höchstens 128
Einträge und 64 KiB zentrale Metadaten auf; Namen über 512 Bytes oder 16
Pfadsegmenten bleiben Problemzeilen. Eingabe-I/O ist auf 256 KiB und 512
Leseaufrufe begrenzt. Das Target betrachtet die Wurzel und höchstens acht ihrer
virtuellen Unterordner. Anschließend versucht es höchstens acht vorhandene
Katalog-IDs an `io::sink()` zu lesen, auch bei einem Teilkatalog. Jeder
Inhaltsaufruf erhält höchstens 64 KiB Ausgabe, 256 KiB Eingabe-I/O und 512
Leseaufrufe. Die Grenzen gelten pro Aufruf; das Target besitzt zusätzlich die
feste Obergrenze von acht Inhaltsaufrufen.

Beliebige beschädigte ZIP-Daten dürfen Fehler oder Teilkataloge ergeben. Nur bei
`VerifiedArchiveRead` prüft das Target Erfolgsinvarianten: regulärer problemloser
Eintrag, exakt angekündigte Ausgabemenge und eingehaltene Ausgabe-/I/O-Grenzen.
Vollständige ZIP-Strukturvalidität wird damit nicht behauptet.

Die drei kleinen ZIP-Seeds wurden unabhängig mit Pythons `zipfile` erzeugt:
`stored.zip` enthält Stored-Dateien, einen leeren Ordner und einen UTF-8-Namen;
`deflate.zip` enthält komprimierten Inhalt; `duplicates.zip` enthält zwei
gleichnamige Einträge mit unterschiedlichen Inhalten. Der normale Test
`zip_fuzz_seeds_oeffnen_navigation_und_gepruefte_inhaltsstreams` öffnet alle drei
Seeds über den Core, navigiert und liest die erwarteten Inhalte vollständig ein.

```bash
cargo test --manifest-path fuzz/Cargo.toml --locked --test seed_corpus
cargo check --manifest-path fuzz/Cargo.toml --locked
cargo +nightly fuzz run archive_read fuzz/seeds/archive_read -- -max_len=65536 -runs=1000
```
