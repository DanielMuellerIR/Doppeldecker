# Parser-Fuzzing

Drei Targets füttern beliebige Bytes in die JSON-Deserialisierung und anschließende
Kernvalidierung von Baseline, Laufjournal und Metadatenmanifest. Das vierte Target
`finder_favorites` prüft den Finder-Favoriten-Parser für Plist- und Bookmark-Daten.
Die Targets öffnen keine Dateien, verbinden kein Netzwerk und mutieren keine
Dateisystemdaten.

```bash
rustup toolchain install nightly --profile minimal
cargo install cargo-fuzz --locked
cargo +nightly fuzz run bidirectional_baseline fuzz/seeds/bidirectional_baseline
cargo +nightly fuzz run bidirectional_run_journal fuzz/seeds/bidirectional_run_journal
cargo +nightly fuzz run metadata_manifest fuzz/seeds/metadata_manifest
cargo +nightly fuzz run finder_favorites
```

Für kurze reproduzierbare CI-/Entwicklerläufe kann die Laufzahl begrenzt werden:

```bash
cargo +nightly fuzz run bidirectional_baseline fuzz/seeds/bidirectional_baseline -- -runs=10000
```

Die drei kleinen gültigen Seeds sind versioniert, damit kurze Läufe sofort die
strukturellen Validatoren erreichen. Bei Schemaänderungen erzeugt
`cargo run --manifest-path fuzz/Cargo.toml --example generate_seed_corpus` sie neu.
Wachsendes Corpus und Crash-Artefakte bleiben lokal unter `fuzz/corpus/`
beziehungsweise `fuzz/artifacts/` und werden nicht versioniert. Ein Fund wird erst
nach Minimierung als kleiner normaler Regressionstest in `core/` übernommen.
