# Workspace-Delta: produktive und plattformgebundene Abhängigkeiten

Das vollständige Delta der Buildkonfiguration seit `fb14df1` betrifft zwei
Cargo-Manifeste. Beide wurden vollständig gelesen und mit ihren heutigen
Aufrufstellen, dem Lockfile und den Targetgrenzen abgeglichen. Es wurde kein
weiterer Fehler gefunden.

Die CLI verwendet `libc` inzwischen produktiv für `O_NOFOLLOW`, `O_NONBLOCK`
und die Fehlerzuordnung beim gehaltenen Lesen von Entscheidungsdateien. Die
Abhängigkeit gehört deshalb in `[dependencies]`. Ihr Wechsel aus den
Testabhängigkeiten ändert weder die gesperrte Version noch den gemeinsamen
Schutz vor echten Nutzerpfaden.

Die App verwendet `url` im gemeinsamen Datei-URI-Codec auch außerhalb des
macOS-Moduls. Die normale Abhängigkeit ist daher richtig. `NSPasteboard`,
`NSPasteboardItem`, `NSArray` und `NSURL` bleiben dagegen ausschließlich im
macOS-Target aktiviert. AppKit und Foundation behalten abgeschaltete
Standardfeatures. Die nativen Tests verwenden private Pasteboards.

`cargo metadata --locked --offline --no-deps --format-version 1` besteht.
Die tatsächlichen Dependency-Arten, Targets und Featuremengen stimmen mit den
Importen überein. Das Lockfile enthält weiterhin `libc 0.2.186`, `url 2.5.8`,
`objc2 0.6.4` sowie AppKit/Foundation `0.3.2`.

Die 385 erfassten Quellen sind zum zuletzt geprüften Stand bytegleich. CLI,
Core, gemeinsame Rust-Konfiguration und Lockfile entsprechen unverändert
dem geprüften CLI-Stand `f279685`; App und Core entsprechen weiterhin dem
geprüften App-Stand `90e8339`. Die passenden früheren Tests, Kompilierungen
und Clippy-Prüfungen werden wiederverwendet. Es wurde kein weiterer Rust-,
Browser- oder nativer GUI-Lauf ausgeführt. Die Diffprüfung ist grün.

Dies ist ein Delta mit aktuellen Grenzen. Die früheren Vollbelege der übrigen
Buildkonfiguration bleiben erhalten. Es gab keine Abhängigkeitsaktualisierung,
Produktänderung, Installation oder Veröffentlichung.
