<p align="center">
  <img src="screenshots/app-icon.png" width="128" alt="Doppeldecker app icon">
</p>

# Doppeldecker: A practical, transparent dual-pane file manager

**🌐 Sprache / Language:** [English](README.md) · [Deutsch](README.de.md)

Doppeldecker is a dual-pane file manager for macOS and Linux, built for practical
everyday file work with visible consequences. **You see destructive changes before
they happen, and nothing is deleted behind your back.** Two folders sit side by
side; copy, move and synchronize between local folders and supported remote
endpoints. FTP, FTPS, WebDAV and SMB offer capability-dependent transfers;
SFTP offers best-effort browsing, verified snapshots and additive local downloads
of regular files and bounded directory trees.
Manual SFTP folder sizes use a bounded metadata scan without opening file contents;
the column and information dialog label observed values with “≈” and Best Effort.
Unusable SFTP names and missing file types are reported as omissions in the pane;
independent valid files remain available.

Different servers provide different guarantees. Doppeldecker uses the strongest
available method, labels remaining limitations honestly and offers a guarded
best-effort path where the effect can still be bounded and verified. It refuses an
operation only when its target or potential damage cannot be constrained reliably.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="screenshots/overview-dark.png">
  <img src="screenshots/overview-light.png" alt="Doppeldecker showing two panes and the sidebar with favorites and connections">
</picture>

> The interface is currently German-only.

## Status & downloads

**0.1.124 is the first public preview.** Doppeldecker is pre-1.0; use a second
copy of your data while evaluating it. The interface is currently German-only.

- **macOS 14+ on Apple Silicon:** [Download the signed and notarized DMG](https://github.com/DanielMuellerIR/Doppeldecker/releases/download/v0.1.124/Doppeldecker_0.1.124_aarch64.dmg).
  Open it and drag Doppeldecker to Applications. The app includes
  `doppeldecker-cli`; its optional setup never replaces an existing command.
- **Linux Mint 22.2 / Ubuntu 24.04, x86_64:** [DEB package](https://github.com/DanielMuellerIR/Doppeldecker/releases/download/v0.1.124/Doppeldecker_0.1.124_amd64.deb)
  or [AppImage](https://github.com/DanielMuellerIR/Doppeldecker/releases/download/v0.1.124/Doppeldecker_0.1.124_amd64.AppImage).
  For the AppImage, enable execution with
  `chmod +x Doppeldecker_0.1.124_amd64.AppImage`, then launch it.
  Other distributions and Intel Macs are not yet verified.
- **Updates are manual:** **Doppeldecker → Nach Updates suchen …** checks once.
  Download and installation require a separate confirmation and a valid signature.
  There are no background checks or silent installations.

[Release notes and checksums](https://github.com/DanielMuellerIR/Doppeldecker/releases/tag/v0.1.124)
· [Supported operations and limitations](docs/function-audit.md).

## What it does

- **Preview before anything is written.** Synchronization runs as `plan → execute`:
  the preview lists every copy, update and removal. Planned changes begin only
  after you confirm the displayed plan.
- **Removals go to quarantine, never straight to the trash.** In mirror mode,
  surplus files at the destination are moved to a per-run, restorable quarantine
  outside the target tree — with hard limits and a required second scan.
- **Three sync modes.** *Add only* (default: adds missing entries, never changes
  or removes existing ones), *Update* (add and refresh, never remove) and
  *Mirror* (the strictest, quarantine-backed removal). Available one-way or **bidirectional** against a durable baseline.
- **Remote folders as first-class panes.** SFTP, FTP/FTPS, WebDAV and SMB share the
  same honest capability model as local folders, plus passwords kept only in the
  system keychain — never in a profile file, argument or log. Secure transports pin
  host keys or certificates. Plain FTP and FTPS without a publicly verified
  certificate identity are supported deliberately and remain visibly labelled in
  the sidebar, pane status and bottom status line. New folder, rename, F5 copy, F6
  move, favorites and opening a verified read-only snapshot are remote-pane actions.
  Depending on the server they use a fully verified path, a clearly labelled
  best-effort path, or report the concrete missing prerequisite. SFTP v3 currently
  permits labelled best-effort browsing, verified snapshot opening and additive
  local downloads of up to 100 direct selections, with files up to 8 GiB and
  bounded directory trees. The first OPEN or OPENDIR cannot bind the path atomically
  to the selected root; this risk stays visible. Replacement, synchronization and remote mutations
  remain blocked; see the [content contract](docs/sftp-contents.md). SMB currently
  permits AddOnly uploads, but no target Update/Mirror, replacement or quarantine
  restore; see the [current capability matrix](docs/function-audit.md).
  Ordinary WebDAV folder creation uses one conditional MKCOL and readback without
  a synthetic deletion probe. It shows its best-effort limits before creation;
  an uncertain outcome keeps the possible new folder for inspection and is never
  retried automatically. See the [folder contract](docs/webdav-folder.md).
- **Drag to copy.** Drag files or a selection into the other pane, or drop files
  from Finder into a local or remote pane, including FTPS. The destination is the
  folder shown in that pane, even when dropping over a folder row. Existing names
  use the same conflict dialog as F5; dropping never requests a move.
- **An operations queue that survives crashes.** Copy, move and sync jobs run
  through an atomic, journaled queue you can pause, reorder and stop; after a
  restart it re-checks the world before touching anything. Open multiple windows
  with `⌘N`/`Ctrl+N`: live remote connections are available in all of them, while
  every window has its own queue and can transfer in parallel.
- **Keep browsing during transfers.** Navigate, use tabs, inspect files and enqueue
  more copies or moves while the current job keeps its original endpoints.
- **Native file clipboard.** Copy local selections with `⌘C`/`Ctrl+C` and paste
  with `⌘V`/`Ctrl+V`. On macOS, `⌥⌘V` moves the copied files; on Linux use
  `Ctrl+X`, then `Ctrl+V`. Text fields keep their usual text commands. See the
  [clipboard contract and platform limits](docs/background-file-work.md).
- **Independent tabs in each pane.** Tabs keep their own folder, history,
  selection, sorting and scroll position. Use `⌘T`/`Ctrl+T` to open a tab,
  `Ctrl+Tab` to switch and `⌘⇧W`/`Ctrl+Shift+W` to close it. Optional session
  restoration includes both tab bars. Native macOS checks include keyboard use
  and restoring both panes after restarting the app.
- **A real trash, and honest metadata.** Deletions use a descriptor-relative
  trash with a crash journal. A *Portable* profile carries contents, timestamps
  and normal permissions; opt-in *Native* profiles also transfer owners, ACLs,
  extended attributes, resource forks, sparse files and hard links — and account
  for anything they cannot represent, per file.
- **Favorites & Finder import.** Pin folders to the sidebar, reorder them by drag
  and drop, or import your existing Finder sidebar in one step (macOS).
- **Local & private.** No central account or telemetry. File transfers go only to
  destinations you choose. Native
  Tauri (Rust + system WebView), not Electron. Light and dark mode.

## How synchronization works

Open the sync view from **Commands → Synchronize…** (⇧⌘S). It is a mode you enter
deliberately: the two panes become source and target, the preview becomes the main
view, and a side panel holds direction, effect, comparison and metadata options.

The normal local start executes AddOnly or Update after a concrete preview and
confirmation. Additions need no typed phrase; Update requires content hashes and
prefers quarantine for replaced file versions. Weaker local targets use a visibly
confirmed best-effort path with separate warnings for new-name commits and
durability. Permanent replacements explicitly state their scope and lack of
recovery. A separate simulation button never
changes files, including in a test sandbox. Mirror retains its separate real-mode
assistant. See the [local sync contract and current limits](docs/local-sync.md).

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="screenshots/sync-dark.png">
  <img src="screenshots/sync-light.png" alt="The synchronization view with a previewed plan and options">
</picture>

Comparison is either fast (size and modification time) or exact (BLAKE3 content
digests). In exact mode the digests are part of the confirmed plan, including
before any mirror removal. A running sync shows progress and can be cancelled; a
plan that has drifted since you previewed it requires explicit re-confirmation
before it will run.

> **A sync is not a backup.** It transfers the current state only — no version
> history, no snapshots. The sync view states this next to the metadata profile so
> the boundary is visible exactly where it matters.

## Practicality and safety

Doppeldecker separates non-negotiable boundaries from provider limitations. Local
mutations remain descriptor-relative (`openat`/`renameat`/`unlinkat` with
`O_NOFOLLOW`), source and target may never be equal or nested, secrets never enter
arguments or logs, and an unexpected server or root identity stops the operation.

Within those boundaries, capabilities are graded rather than simply allowed or
blocked. A fully verified operation runs normally. A best-effort operation gets a
fresh preflight, a concrete warning when needed, explicit conflict handling,
post-operation readback and an honest audit result. Only an unbounded or ambiguous
effect is refused. High-impact mirror removals still require complete listings,
visible limits and explicit confirmation. See
[`docs/product-principles.md`](docs/product-principles.md).

## Headless & CLI

The correctness-critical core (sync engine, file operations) is fully testable
**without a GUI**, identically on macOS and Linux — ideal for CI and automation:

```bash
cargo test -p doppeldecker-core      # sync-engine tests (macOS + Linux)
cargo test -p doppeldecker-cli       # the CLI protocol against synthetic temp trees
```

The supported command-line entry point offers a pure dry-run plan, a confirmed
local real-mode run, local/remote `mkdir`, `list`, `sync`, `copy`, `move` and
`rename`, local `trash`, capability-gated remote `delete`, capability
inspection and quarantine management. It resolves stored
profiles and keychain references without accepting secrets as arguments, and uses
the same core plans, confirmations, drift checks and audit contracts as the GUI.
`--json` emits JSON Lines on stdout. See
[`docs/headless-cli.md`](docs/headless-cli.md).
The packaged macOS app offers to make its bundled CLI available as
`doppeldecker-cli` on first launch; Linux packages install the same binary alongside
the GUI.

## Build from source

Prerequisite: a Rust toolchain ([rustup](https://rustup.rs)). For Linux GUI builds
also `libwebkit2gtk-4.1-dev` and `build-essential`.

```bash
cargo run   -p doppeldecker          # start the app
cargo build -p doppeldecker --release
cargo build -p doppeldecker-cli --release
```

The application itself needs no Node. The reproducible browser test suite (against
a controlled Tauri mock) uses Node 20+ and Playwright:

```bash
pnpm install --frozen-lockfile
pnpm exec playwright install chromium
pnpm test:ui
```

Architecture and the product, safety, provider and release contracts:
[`AGENTS.md`](AGENTS.md) · [`docs/INDEX.md`](docs/INDEX.md) ·
[`CHANGELOG.md`](CHANGELOG.md) · [`backlog.md`](backlog.md).

## License

[MIT](LICENSE), © 2026 Daniel Müller. Doppeldecker vendors a small SMB capability
patch and depends on third-party Rust crates; their licenses and notices travel
with their sources.

## Support & privacy

[Issues and support](https://github.com/DanielMuellerIR/Doppeldecker/issues) ·
[Legal notice](https://dm0.de/impressum.html) ·
[Privacy](https://dm0.de/datenschutz.html#doppeldecker).
These links are also available in the app under **Einstellungen**.

The app sends no telemetry or automatic crash reports. Settings, profiles and
error logs are stored locally. File transfers use the destinations you select;
remote connections and update checks contact the selected endpoints. Do not
include passwords or private file paths in public issues.
