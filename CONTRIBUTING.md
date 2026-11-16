# Contributing

This repository follows the shared Git workflow used across our projects
(`~/projects/soft/_docs/git_workflow.md`, summarised in the wiki page
`10-howto/git-workflow.md`). It applies to people, AI assistants and scripts.

## Branches

- `main` holds releases only. No direct commits, no direct pushes, no force
  pushes. It changes only through a `dev -> main` pull request.
- `dev` is the integration branch and the default branch on GitHub.
- Every change goes into a working branch created from an up-to-date `dev`
  and comes back through a pull request into `dev`, preferably squash-merged.
  Branch prefixes: `feat/`, `fix/`, `refactor/`, `docs/`, `test/`, `perf/`,
  `chore/`.

History up to `481bd77` was committed straight to `main` before this workflow
was adopted; that is a one-time exception.

## Commit messages and pull request titles

```text
[TYPE] Short English description
```

Types: `[FEAT]`, `[FIX]`, `[REFACTOR]`, `[DOCS]`, `[TEST]`, `[PERF]`,
`[CHORE]`. `[PR]` is reserved for server-side merge commits such as
`[PR] Release 0.8.7`. Pull request bodies are written in English.

## Local hooks

Enable the versioned hooks once per checkout:

```sh
git config core.hooksPath .githooks
```

They block commits on `main`, pushes to `main` and untyped commit messages.
They are guardrails; branch protection on GitHub is the authoritative layer.

## A typical change

```sh
git switch dev && git pull --ff-only origin dev
git switch -c feat/short-description
make bump            # before a build that will actually be used
make test
git add <paths> && git commit -m "[FEAT] Short English description"
git push -u origin feat/short-description
gh pr create --base dev --title "[FEAT] Short English description"
```

Releases: a `dev -> main` pull request about once a week or after a coherent
milestone, merged with a merge commit titled `[PR] Release <version>`.
