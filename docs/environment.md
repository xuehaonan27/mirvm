# Environment

Host, toolchain and network facts that cost real time when rediscovered.

## Hosts and toolchain

- All building and testing happens on the Linux x86_64 container `dev-cpu-hg`. The macOS checkout can edit
  and review but cannot build: its cargo registry cache is not writable.
- The container ships no Rust. Install `nightly-2026-07-02` with the components listed in
  `rust-toolchain.toml` (`rustc-dev`, `rust-src`, `llvm-tools`, `rustfmt`, `clippy`).
- Pinned compiler sources live at
  `~/.rustup/toolchains/nightly-2026-07-02-*/lib/rustlib/rustc-src/rust/compiler` — note `rustc-src/`, not
  `src/`, which holds only the library.
- `rg` is the assertion tool of seven suites and is not preinstalled. A missing `rg` is not a SKIP but a
  wall of false reds. `nasm` is absent, which makes a few corpus candidates unjudgeable.

## Network

- Anything that fetches runs under `withproxy` (a `~/.profile` function). Do not export a proxy globally.
- `harness`-spawned cargo does not inherit `http_proxy`, so it needs `[http] proxy` in
  `~/.cargo/config.toml`; otherwise every cold dependency costs a 30 s timeout and turns red.
- mirvm's own registry does not read the cargo cache, so dependency resolution needs the network even with
  a warm cargo. A whole suite run without the proxy fails with
  `HTTP fetch failed ... Network is unreachable`.
- The local HTTP fixture registries used by `contracts.cargoless-sources` and `contracts.cargoless-git`
  must bypass the proxy: `tests/lib/harness.sh` appends `127.0.0.1,localhost` to `no_proxy`. A
  hand-run suite must do the same or those cases go red.
- `tests/projects/` holds real Cargo projects as submodules. `make projects`
  (`git submodule update --init --recursive`) fetches them and needs the proxy from the macOS
  checkout, e.g. `git -c http.proxy=$PROXY submodule update --init --recursive`; direct access
  times out there. The container's route to GitHub has not been confirmed — when it is absent the two
  `mode=diff` corpus entries record SKIP through their `needs=` path instead of failing.

## Building and pushing

- Build with `cargo build --release --locked`; execute and benchmark only the release binary — a debug
  build distorts every timing gate.
- A commit may be auto-pushed to `origin main` within ~20 s, but it does not always fire: check with
  `git ls-remote origin main` and push manually when it has not. Direct GitHub access from the macOS
  checkout often fails (HTTP/2 framing errors, connect timeouts); the container's `withproxy` works.
- Never rewrite an existing commit (`--amend`, `rebase`, `filter-branch`) — that forks local `main` from
  `origin/main`. Append a new commit instead.
