# Domain docs

mirvm uses a single-context documentation layout.

## Before development

1. Read `docs/README.md` for document authority, directory layout, and
   replacement rules.
2. Read `docs/current-status.md` for current implementation facts.
3. Read `docs/open-issues.md` for every unresolved debt and refusal boundary.
4. Read `docs/agents/onboarding.md` for environment facts, iron rules, and
   the three-dimension acceptance recipe.
5. Read `docs/decision-history.md`, then the relevant entries under
   `docs/designs/` (active contracts/blueprints) and `docs/history/`
   (construction logs and completed designs, read-only).

Later documents may overturn earlier conclusions. Preserve superseded frame,
vmctx, execution, and ABI models. Record replacement evidence and reopening
conditions instead of deleting old reasoning.

Use terminology from `DESIGN.md` (RAM, P0–P7, C-ledger). If proposed work
conflicts with the decision history or an open-issue entry's stated reopen
condition, surface that conflict explicitly rather than silently overriding it.
