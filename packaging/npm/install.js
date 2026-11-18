#!/usr/bin/env node
// Downloads the prebuilt `rusturbo` binary for this platform from GitHub Releases.
//
// Why a downloader instead of bundling: the Rust binary is ~3–6 MB per platform. Shipping
// every platform inside one npm tarball would be wasteful; shipping one tarball per
// platform (the `optionalDependencies` pattern) multiplies the publish surface. A single
// package that fetches the official release asset at install time is what most Rust CLIs
// do, and it keeps one version number across npm, crates.io and GitHub Releases.

const fs = require("fs");
const path = require("path");
const https = require("https");
const { execFileSync } = require("child_process");

const VERSION = require("./package.json").version;
const REPO = "the-golden-army/Rusturbo";

const PLATFORMS = {
  "linux-x64": "x86_64-unknown-linux-gnu",
  "linux-arm64": "aarch64-unknown-linux-gnu",
  "darwin-x64": "x86_64-apple-darwin",
  "darwin-arm64": "aarch64-apple-darwin",
  "win32-x64": "x86_64-pc-windows-msvc",
};

function target() {
  const key = `${process.platform}-${process.arch}`;
  const triple = PLATFORMS[key];
  if (!triple) {
    console.warn(
      `rusturbo: no prebuilt binary for ${key}. Build from source instead:\n` +
        `  cargo install --git https://github.com/${REPO} rusturbo`
    );
    process.exit(0); // Never fail an install because a platform is unsupported.
  }
  return triple;
}

function assetName(triple) {
  return process.platform === "win32"
    ? `rusturbo-${triple}.exe`
    : `rusturbo-${triple}`;
}

function download(url, dest, redirects = 0) {
  return new Promise((resolve, reject) => {
    if (redirects > 5) return reject(new Error("too many redirects"));
    https
      .get(url, { headers: { "User-Agent": "rusturbo-npm-installer" } }, (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          res.resume();
          return download(res.headers.location, dest, redirects + 1).then(resolve, reject);
        }
        if (res.statusCode !== 200) {
          res.resume();
          return reject(new Error(`HTTP ${res.statusCode} for ${url}`));
        }
        const file = fs.createWriteStream(dest);
        res.pipe(file);
        file.on("finish", () => file.close(() => resolve()));
        file.on("error", reject);
      })
      .on("error", reject);
  });
}

// Escape hatches, in order of precedence:
//   RUSTURBO_BINARY_PATH  — use an already-downloaded binary (air-gapped installs, CI)
//   RUSTURBO_BINARY_URL   — fetch from an internal mirror instead of GitHub
//   RUSTURBO_SKIP_DOWNLOAD=1 — install the shim only
async function main() {
  const triple = target();
  const asset = assetName(triple);
  const binDir = path.join(__dirname, "bin");
  const binary = path.join(binDir, process.platform === "win32" ? "rusturbo.exe" : "rusturbo");

  if (fs.existsSync(binary)) {
    console.log("rusturbo: binary already present, skipping download");
    return;
  }
  if (process.env.RUSTURBO_SKIP_DOWNLOAD === "1") {
    console.log("rusturbo: RUSTURBO_SKIP_DOWNLOAD=1, install the binary yourself");
    return;
  }

  fs.mkdirSync(binDir, { recursive: true });

  if (process.env.RUSTURBO_BINARY_PATH) {
    fs.copyFileSync(process.env.RUSTURBO_BINARY_PATH, binary);
    if (process.platform !== "win32") fs.chmodSync(binary, 0o755);
    console.log(`rusturbo: installed from RUSTURBO_BINARY_PATH`);
    return;
  }

  // 1) Asset attached to the versioned GitHub Release (or a mirror).
  const releaseUrl =
    process.env.RUSTURBO_BINARY_URL ||
    `https://github.com/${REPO}/releases/download/v${VERSION}/${asset}`;
  try {
    await download(releaseUrl, binary);
    console.log(`rusturbo: downloaded ${releaseUrl}`);
  } catch (err) {
    // 2) Fall back to building from source, which always works if Rust is installed.
    console.warn(`rusturbo: could not download prebuilt binary (${err.message})`);
    try {
      execFileSync(
        "cargo",
        ["install", "--git", `https://github.com/${REPO}`, "--tag", `v${VERSION}`, "rusturbo"],
        { stdio: "inherit" }
      );
      console.log("rusturbo: installed from source");
      return;
    } catch {
      console.warn(
        "rusturbo: falling back to a source build also failed.\n" +
          `  Install Rust (https://rustup.rs) then run:\n    cargo install --git https://github.com/${REPO} rusturbo`
      );
      return;
    }
  }

  if (process.platform !== "win32") fs.chmodSync(binary, 0o755);
}

main().catch((e) => {
  console.warn("rusturbo: install script failed:", e.message);
  // Exit 0: a failed optional accelerator install must not break `npm install`.
  process.exit(0);
});
