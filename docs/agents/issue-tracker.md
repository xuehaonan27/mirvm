# Issue tracker

Issues and PRDs for this repository live in GitHub Issues for
`xuehaonan27/mirvm`. Use the `gh` CLI for issue operations and infer the
repository from the `origin` remote when running inside this clone.

> **Current maintenance override (2026-07-13):** remote repositories and all
> GitHub issue, PRD, PR, and `gh` operations are paused. Do not read, fetch,
> publish, comment on, label, close, or otherwise mutate GitHub state until the
> maintainer explicitly resumes this work. The tracker choice below is retained
> for that future resumption; the pause does not replace it.

## Conventions after resumption

- Create: `gh issue create --title "..." --body "..."`
- Read: `gh issue view <number> --comments`
- List: `gh issue list --state open`
- Comment: `gh issue comment <number> --body "..."`
- Label: `gh issue edit <number> --add-label "..."`
- Close: `gh issue close <number> --comment "..."`

After the maintainer resumes GitHub work, when a skill says to publish a PRD
or issue, create a GitHub issue. When it asks for the relevant ticket, fetch
the issue body, comments, and labels. While the override above is active,
stop before any such operation and keep development evidence local.
