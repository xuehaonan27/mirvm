# Domain docs

mirvm uses a single-context documentation layout.

## Before development

1. Read `docs/README.md` for document authority and replacement rules.
2. Read `docs/current-status.md` for current implementation facts.
3. Read the root `CONTEXT.md` when it exists.
4. Read relevant ADRs under `docs/adr/` when that directory exists.
5. Read `docs/decision-history.md` and the relevant historical designs.

Later documents may overturn earlier conclusions. Preserve superseded frame,
vmctx, execution, and ABI models. Record replacement evidence and reopening
conditions instead of deleting old reasoning.

Use terminology from `CONTEXT.md` when it exists. If proposed work conflicts
with an ADR or the decision history, surface that conflict explicitly rather
than silently overriding it.
