# Releasing RustTurbo

Two release surfaces, one version number:

| Surface | Artefact | Published how |
|---|---|---|
| GitHub Releases | `rusturbo-<target>` binaries + `.sha256` | `release.yml` on tag push |
| crates.io | `rusturbo` crate | `cargo publish` |
| npm | `rusturbo` package (downloads the GitHub binary) | `npm publish` from `packaging/npm` |

## 1. Cut the release (GitHub)

```bash
# On the branch that holds the release commit:
cargo set-version 0.2.0        # or edit Cargo.toml + packaging/npm/package.json
git commit -am "release: v0.2.0"
git tag v0.2.0
git push origin <branch> --tags
```

Pushing the tag triggers `.github/workflows/release.yml`, which builds five targets
(linux x64/arm64, macOS x64/arm64, Windows x64), writes SHA-256 sums, and creates the
GitHub Release with generated notes.

Verify:

```bash
gh release view v0.2.0
```

## 2. Publish to crates.io

```bash
cargo login                      # once, stores a token in ~/.cargo/credentials
cargo publish --dry-run          # catches missing metadata/files
cargo publish
```

Notes specific to this crate:

- The workspace includes `demo/actix-hello`; only `rusturbo` itself is published. The demo
  is `publish = false` and excluded from the crate via the default workspace behaviour
  (only the root package is uploaded).
- `rust-version = "1.74"` is enforced by Cargo; keep it in sync with the README.
- crates.io versions are **immutable**. If you need a re-publish, bump the patch version.

## 3. Publish to npm

npm enforces 2FA on publishes, and a plain `npm login` session is **not** enough:

```
npm error 403 403 Forbidden - PUT https://registry.npmjs.org/rusturbo
npm error Two-factor authentication or granular access token with bypass 2fa enabled is required
```

Two supported paths, in order of preference:

### 3a. Trusted publishing from CI (recommended, no secrets)

`.github/workflows/publish-npm.yml` publishes on every `v*` tag using GitHub Actions
OIDC. Nothing is stored in the repo; the job's identity is the credential, which is what
also satisfies npm's 2FA rule.

One-time setup on npmjs.com → package **rusturbo** → *Settings* → *Trusted Publisher* →
GitHub Actions:

| Field | Value |
|---|---|
| Organization or user | `the-golden-army` |
| Repository | `Rusturbo` |
| Workflow filename | `publish-npm.yml` (file name only, no path) |
| Allowed actions | `npm publish` |

Then `git push origin v<version>` publishes to npm automatically.

Constraints npm enforces: GitHub-hosted runner only, npm CLI ≥ 11.5.1, `id-token: write`
on the publish job, and **no** `NODE_AUTH_TOKEN` on the publish step (it would force the
legacy token path). A trusted publisher can be attached only to a package that already
exists, so the *first* publish of a new name still needs one manual upload (3b).
Unvalidated trusted-publisher configs expire 48 hours after creation.

### 3b. Manual publish with 2FA

```bash
cd packaging/npm
npm version <same-version> --no-git-tag-version
npm login
npm profile enable-2fa auth-only      # required for publishes
npm publish --access public --otp=<6-digit code>
```

An automation/granular token must have *bypass 2FA* enabled to publish, and npm is
restricting that capability — which is why 3a is preferred. Note also that npm 12 disables
install scripts by default for consumers: this package's binary fetch cannot be assumed to
run, and the shim prints a clear message when it did not.

Order matters: the npm installer downloads
`https://github.com/the-golden-army/Rusturbo/releases/download/v<version>/rusturbo-<target>`,
so the GitHub Release must exist **before** the npm package is useful. Asset names carry no
version, only the tag does — the installer resolves them from `package.json`.

Then confirm the binary resolution works against the new tag:

```bash
npx rusturbo@<version> --version
```

Offline/mirror testing without waiting for the release:

```bash
RUSTURBO_BINARY_PATH=./rusturbo-x86_64-unknown-linux-gnu node install.js
RUSTURBO_BINARY_URL=https://internal.example/rusturbo node install.js
```

Note on `postinstall`: it always exits 0 so a failed binary fetch can never break a
consumer's `npm install`. The shim reports the problem at run time instead.

If the release assets are not up yet, `postinstall` falls back to a source build, so wait
for the GitHub Release to finish before smoke-testing npm.

## Version checklist

- [ ] `Cargo.toml` version
- [ ] `packaging/npm/package.json` version
- [ ] README "Install" instructions still accurate
- [ ] `docs/BENCHMARKS.md` regenerated on the release machine (numbers are machine-specific)
- [ ] CI green on the release commit
- [ ] `rusturbo --version` matches the tag
- [ ] No binary committed under `packaging/npm/bin/` — only `rusturbo.js` ships; the
      installer downloads the binary, and CI refuses to publish otherwise
