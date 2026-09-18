# tests/parked/ — parked test-infrastructure archive

This directory holds **test infrastructure that currently runs in no gate and no CI**, kept for future reuse;
it is not executable documentation, read this note before referencing.

## real_projects.sh + real_projects_regression.sh + project_suite_evidence.py

Heavy "real Cargo project correctness/benchmark" harness built on 2026-07-13
(ripgrep/tokei workload, bwrap isolation + content-addressed evidence chain). Contract document:
[docs/real-projects.md](../../docs/real-projects.md).

**Why parked (confirmed by 2026-07-23 test pipeline cleanup)**:

- Case files and source mirrors live in git-ignored `artifacts/real-projects/`; no cases are in-repo.
- Local artifacts no longer exist, so the harness actually executes zero cases.
- Not in CI (CI only runs gate_truth + gate.sh), yet the harness itself is 118K of self-regression script —
  infrastructure exceeding product violates the AGENTS.md infrastructure budget discipline.
- Its "real-project differential" duty has been taken over by the lighter `corpus/projects/<name>/`
  (manifest mode=diff, native cargo run three-way differential).

**Revival condition**: when real-project evidence chain with pinned provenance + sandboxed execution is needed
(e.g. external compatibility release statement), return here; note that the three scripts assume `tests/` as root,
so either move them back or adjust paths on revival. At that time re-read the infrastructure budget discipline:
once the current real workload runs and produces trustworthy pass/fail results, freeze it.

## History

- `tests/project_suite_rustc_proxy.sh` is not parked — it is an active dependency of diff_cargo.sh,
  moved to `tests/fixtures/rustc_proxy.sh` on 2026-07-23.
