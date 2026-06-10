# NPM Distribution for portproxy

Date: 2026-06-10
Status: approved

## Goal

Distribute the `portproxy` Rust binary via npm (`npm i -g portproxy`), using the
same mechanism as portless-rs: a single npm package whose `postinstall` script
downloads the prebuilt binary for the current platform from GitHub Releases.

## Decisions

- **Package name**: `portproxy` (verified free on npmjs.org).
- **Mechanism**: portless-rs pattern (user's explicit choice) — single package,
  `postinstall: node install.js`, download `portproxy-<target>.tar.gz` from
  `https://github.com/zzjin/portproxy/releases/download/v<version>/`.
  Not the esbuild-style optionalDependencies pattern.
- **Targets** (4, no Windows — code depends on the `nix` crate, Unix-only):
  - `x86_64-unknown-linux-gnu`
  - `aarch64-unknown-linux-gnu`
  - `x86_64-apple-darwin`
  - `aarch64-apple-darwin`
- **Version sync**: git tag `v<X.Y.Z>` = `Cargo.toml` version = `npm/package.json`
  version. CI fails the release if they disagree.

## Components

### `npm/` directory

- `npm/package.json` — `bin: { portproxy: "bin/portproxy" }`,
  `scripts.postinstall: node install.js`, `files: ["bin/", "install.js"]`,
  `os: ["darwin", "linux"]`, `engines.node >= 14`. No runtime dependencies.
- `npm/install.js` — zero-dependency Node script (CommonJS, stdlib only):
  1. Map `process.platform` + `process.arch` to a Rust target triple; throw a
     helpful error on unsupported platforms.
  2. Read version from its own `package.json`.
  3. Download the release tarball over `https`, following redirects.
  4. Extract with system `tar` into `bin/`, `chmod 755` the binary.
  5. On failure: remove the partial download, print fallback instructions
     (`cargo install` / manual download from the releases page), exit 1.
- `npm/bin/portproxy` — committed shell-script placeholder. npm links the `bin`
  entry to this path at install time; postinstall overwrites it with the real
  binary. If postinstall was skipped, running it prints an error and exits 1.

### CI: `.github/workflows/release.yml`

Triggered on tag push `v*`:

1. **check-version** — assert tag version == Cargo.toml version ==
   npm/package.json version.
2. **build** — 4-way matrix, all native builds (no cross-compilation):
   `ubuntu-latest` (x86_64-linux), `ubuntu-24.04-arm` (aarch64-linux),
   `macos-13` (x86_64-darwin), `macos-latest` (aarch64-darwin).
   Each builds `cargo build --release`, packages
   `portproxy-<target>.tar.gz` containing the single `portproxy` binary,
   uploads it as a workflow artifact.
3. **release** — downloads all artifacts, creates the GitHub Release with the
   4 tarballs attached.
4. **npm-publish** — after the release exists (assets must be downloadable
   before the package goes live), `npm publish` from `npm/` using the
   `NPM_TOKEN` repository secret.

### README

Add an npm install section (`npm i -g portproxy`) before the cargo
instructions, noting supported platforms.

## Testing

`install.js` stays dependency-free and is exercised by `node --check` in CI for
syntax. End-to-end validation happens against the first real release
(`node npm/install.js` downloads the actual asset). No mock-server harness —
matches portless-rs practice; YAGNI.

## Out of scope

- Windows support (blocked on `nix` usage in the proxy core).
- esbuild-style platform sub-packages.
- Publishing to crates.io (separate concern).
