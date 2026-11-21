# Development

Doppeldecker has three Rust crates. The `core` crate owns file operations, provider access, sync planning and recovery. `cli` uses those APIs directly. `src-tauri` connects the same core to the web UI in `frontend`, which uses standard JavaScript modules.

## Local checks

Install Rust with `rustfmt` and Clippy, Node.js 20 or later, and pnpm. Linux GUI builds also need the WebKitGTK 4.1 development libraries and the other system packages listed in the [CI workflow](../.github/workflows/ci.yml).

```sh
pnpm install --frozen-lockfile
pnpm exec playwright install chromium
python3 tools/check-gates.py
```

The CI matrix records the tested Rust version for each platform. Use `--list` to see all checks or `--only NAME` to run a specific check. Browser tests use a local mock server. To exercise native file operations, run `tools/run-test-sandbox.sh` and use the two temporary directories opened by the app.

## Tauri command contracts

Run `pnpm check:ipc` to check command names and argument fields across the UI, including dynamic provider names and nested Serde contracts. The check fails when it cannot establish an argument type or field. Run `node tools/check-ipc-names.mjs --unverified` to locate those calls. Rust tests compare the registered commands with the argument manifest used by the browser mock.

After changing a registered command's response type, regenerate the frontend declaration with `DOPPELDECKER_UPDATE_IPC_TYPES=1 cargo test -p doppeldecker ipc_registry_matches_the_frontend_command_manifest`. For response objects, test the serialized Rust value and the browser mock against the same fixture. When confirmation requires a backend plan or digest, pass the original value from the preview. The frontend type exposes the fields needed for display, and Rust still validates the complete proof.

See the [documentation index](INDEX.md) for file operation, provider, sync and recovery contracts.
