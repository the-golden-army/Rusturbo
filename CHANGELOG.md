# Changelog

## v0.1.2

Fixes found by running the tool against a real Actix project on Windows.

### CLI
- **`--` no longer swallows Cargo arguments.** `passthrough` was declared as a `last`
  positional, so clap treated the first `--` as end-of-flags and any `--cargo-arg` after it
  was forwarded to the program instead of Cargo. Cargo arguments must now precede `--`:
  `rusturbo run --cargo-arg=--release -- --port 9000`.
- With program arguments, RustTurbo builds and then **execs the binary itself**, propagating
  its exit code, instead of relying on `cargo run --` forwarding.
- **Echoed command lines are quoted.** `--config profile.dev.package."*".debug=0` is one
  argument, but printed unquoted it read like two. Execution was always argv-based; only the
  display changed.
- Hidden `host` proxy subcommand so program arguments can never be confused with RustTurbo's.

### Packaging
- **`cargo install --git <repo>` is unambiguous.** `demo/actix-hello` is no longer a
  workspace member, so the bare command resolves to a single binary package instead of
  failing with `multiple packages with binaries found`. CI now guards this.
- **npm 12 install scripts**: documented `--allow-scripts=rusturbo` and added a
  `rusturbo:install` recovery script, since `postinstall` is blocked by default.
- `publishConfig.provenance` removed (it made local `npm publish` fail with `EUSAGE`).
- `name` is no longer a hard requirement in the install hints.

### CI
- `--workspace` on check/test/clippy; an install guard that runs the documented
  `cargo install` command and asserts it stays unambiguous.
- Five new CLI parsing tests covering `--`, `--cargo-arg`, and the `host` proxy.

## v0.1.1
- npm installer overrides (`RUSTURBO_BINARY_PATH`, `RUSTURBO_BINARY_URL`,
  `RUSTURBO_SKIP_DOWNLOAD`), `postinstall` never fails a consumer install.
- `Cargo.lock` committed for reproducible `--locked` release builds.

## v0.1.0
- First release: dependency-graph analysis, fingerprinted artifact cache (shared target
  dirs + portable entries), verified fast-linker engine, Actix preset, benchmark harness,
  binaries for linux/macos/windows (x64 + arm64).
