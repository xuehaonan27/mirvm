## 工作纪律（2026-07-22 用户钦定）

1. **永远直面问题，而不是绕过问题。** 凡需人工报名、特判开关、把负担
   转嫁给使用者的设计，都是绕行，不是方案——退回重设，直到问题在
   机制内被解决（判例：C4 的 env 名单被退回，改按需救援链）。
2. **讲话不要讲黑话，要讲人能听懂的话。** 解释、文档、评审一律先讲
   清楚是什么、为什么，再讲怎么做；项目内部术语第一次出现必须带
   白话说解，不许成串堆叠。

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
