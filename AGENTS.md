## Agent skills

### Issue tracker

Issues and PRDs live in GitHub Issues for `xuehaonan27/mirvm`.
See `docs/agents/issue-tracker.md`.

**Current maintenance override:** remote repositories and all GitHub issue,
PRD, PR, and `gh` operations are paused. Do not fetch, publish, or mutate
GitHub state until the maintainer explicitly resumes this work. GitHub Issues
remains the selected tracker for that future resumption.

### Triage labels

Use the canonical workflow labels documented in
`docs/agents/triage-labels.md`.

### Domain docs

This is a single-context repository. Read the current-status and
documentation-authority pages before historical designs.
See `docs/agents/domain.md`.

### Infrastructure budget discipline

Benchmark and test harnesses are enabling infrastructure, not product
deliverables. Once a harness can run the current real workload and produce a
trustworthy pass/fail result, freeze it and return to product work.

Do not add schemas, provenance metadata, inventories, generalizations,
refactors, or robustness hardening merely for possible future cases. An
infrastructure change is allowed only when the current product RED cannot be
reproduced or judged correctly without it. Before making such a change, name
that concrete blocker; make the smallest change that removes it; rerun the
exact workload; then immediately return to `src/` or the next real workload.
If the workload already runs and its result is trustworthy, do not modify the
harness.
