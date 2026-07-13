# Issue tracker

Issues and PRDs for this repository live in GitHub Issues for
`xuehaonan27/mirvm`. Use the `gh` CLI for issue operations and infer the
repository from the `origin` remote when running inside this clone.

## Conventions

- Create: `gh issue create --title "..." --body "..."`
- Read: `gh issue view <number> --comments`
- List: `gh issue list --state open`
- Comment: `gh issue comment <number> --body "..."`
- Label: `gh issue edit <number> --add-label "..."`
- Close: `gh issue close <number> --comment "..."`

When a skill says to publish a PRD or issue, create a GitHub issue. When it
asks for the relevant ticket, fetch the issue body, comments, and labels.
