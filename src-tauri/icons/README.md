# App-Icon

- `icon-source.png` ist die unveränderte, 1254 × 1254 Pixel große Rasterquelle
  des gewählten Entwurfs. Sie enthält die vom Bildanbieter eingebetteten
  C2PA-Content-Credentials mit `gpt-image` als Erzeuger.
- `icon-render.svg` legt die Rasterquelle in einen transparenten SVG-Container,
  damit `rsvg-convert` die von Tauri verlangten RGBA-PNG-Dateien erzeugt.
- `icon.png` ist die daraus abgeleitete 1024er-Quelle für Tauri und das
  Linux-Bundle.
- `icon.icns` enthält die macOS-Größen von 16 bis 1024 Pixel.
- `icon.svg` bewahrt die vorherige Vektorversion als Rückweg, erzeugt aber nicht
  mehr die produktiven Dateien.

`./tools/make-icons.sh` erzeugt `icon.png` und `icon.icns` neu. Die PNG-Quelle
füllt das Quadrat vollständig aus. `icon-render.svg` schneidet die Ecken bereits
mit einer abgerundeten Rechteckmaske aus; derselbe Renderer erzeugt auch die sechs
Laufzeitvarianten. Es gibt keine zusätzliche Unterlage oder einen farbigen Rand.

Die vier ursprünglichen Entwürfe und ihre Gestaltungsparameter liegen unter
`design/icon-concepts/2026-09-02/`.
Die sechs großen Farbvarianten liegen unter `design/icons/`; im ausgelieferten
Frontend bleiben nur die kleinen Vorschauen unter `frontend/icons/preview/`.

## Herkunft und Transparenz

Der gewählte Entwurf wurde am 2026-09-02 mit OpenAI `gpt-image` erzeugt und
anschließend von einem Menschen ausgewählt und technisch eingebunden. Die
unveränderte Rasterquelle bleibt mitsamt ihrer maschinenlesbaren
C2PA-Herkunftsangabe im Repository.

Nach der Projektprüfung vom 2026-09-02 verlangt Artikel 50 Absatz 4 des EU AI Act
für dieses App-Icon keinen sichtbaren KI-Hinweis: Die Vorschrift erfasst Bilder
des Betreibers nur, wenn sie einen Deepfake darstellen. Das frei illustrierte
Flugzeug ähnelt keinem bestimmten bestehenden Objekt und erweckt im Einsatz als
App-Icon keinen falschen Eindruck einer authentischen Aufnahme. Artikel 50
Absatz 2 weist die maschinenlesbare Kennzeichnung generativer Ausgaben dem
Anbieter des Bildsystems zu; die gewählte Originaldatei enthält eine solche
Herkunftsangabe bereits.

Die EU-Primärquellen wurden am 2026-10-07 erneut geprüft. Die Anwendung auf das
konkrete Icon bleibt eine projektspezifische Ableitung aus Motiv und Einsatzkontext.
Eine maschinenlesbare Herkunftsangabe ersetzt keine gegebenenfalls erforderliche
sichtbare Deepfake-Kennzeichnung.

Maßgebliche Quellen:

- <https://ai-act-service-desk.ec.europa.eu/en/ai-act/article-50>
- <https://digital-strategy.ec.europa.eu/en/faqs/transparency-obligations-under-article-50-ai-act>
- <https://developer.apple.com/design/human-interface-guidelines/app-icons>

Diese projektspezifische Einordnung ersetzt keine Rechtsberatung.
