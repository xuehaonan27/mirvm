# tests/projects/ — real Cargo projects under differential

This directory holds **real-world Cargo projects** (the kind that produce a binary), registered in
`tests/suites/corpus/cases.manifest` with `mode=diff`. `make gate` compares `mirvm run <project>`
against native `cargo run` byte-for-byte on stdout, stderr and exit code, plus an L2 warm rerun that
guards against a cache replay faking a green. The verdict rules and the `{ROOT}` path placeholder
are documented in the manifest header and in `tests/support/harness.sh`.

## These are submodules, not vendored sources

Each project keeps its own Git tree upstream, so the mirror of it here is a **submodule** pinned to
one commit. Nothing from these projects is committed into mirvm's tree.

```bash
make projects          # git submodule update --init --recursive
```

An uninitialized checkout is not a failure: the manifest entries carry
`needs=tests/projects/<name>/Cargo.toml`, so the corpus batch records them as SKIP until the
submodules are fetched. That is also why the pins must stay in sync with `needs=`.

## Pins

| Project | Version | Upstream | Pinned commit |
|---|---|---|---|
| hexyl | 0.17.0 | https://github.com/sharkdp/hexyl | `8eb6d4771ce1ec7af65d06bd335457783b77d557` |
| tokei | 14.0.0 | https://github.com/XAMPPRocky/tokei | `8cdd6fa3a54f8cd69442d2f00effb29aa3110353` |

The pins are the commits the published crates were built from, taken from each package's own
`.cargo_vcs_info.json` (`sha1`). They are the same revisions that were previously vendored here, so
the differential keeps comparing the same code; only the storage changed.

## Why these two

- hexyl (small): a pure-Rust hex viewer over anyhow/clap/termcolor — the smallest real project
  shape, which got the differential pipeline working.
- tokei (medium): a real `build.rs` (tera templates generating `language_type.rs`) plus a real
  dependency tree (grep-searcher/ignore/dashmap/crossbeam) — it exercises cargo_shim's build-script
  scheduling and the medium-project load path.

## Adding or bumping a project

1. Add it as a submodule under `tests/projects/<name>`, pinned to a commit whose output for a fixed
   input is byte-deterministic. A project that is nondeterministic, needs the network at run time,
   or depends on an absolute machine-local prefix is not accepted; text normalization must never
   hide nondeterminism.
2. Register it in `cases.manifest` with `mode=diff`, `full` tier first, plus
   `needs=tests/projects/<name>/Cargo.toml`.
3. Add its row to the table above.
4. Verify with `make suite S=corpus.contract ARGS=<name>`.
