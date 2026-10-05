# gh-enterprise-repos

Queries a GitHub enterprise over GraphQL and writes one YAML file per
enterprise listing every repository in every organization it contains. Public,
non-archived repositories by default. `--org` does the same for a single
organization without going through its enterprise.

Sibling of [`gh-org-members`](../all-users-enterprise), which exports the
people instead.

Works against github.com and GitHub Enterprise Server.

## Build

```sh
cargo build --release
```

## Use

```sh
export GITHUB_TOKEN=…            # needs read:org and read:enterprise

# one enterprise → ./acme-inc.yaml
gh-enterprise-repos -e acme-inc

# several enterprises, one file each, into a directory
gh-enterprise-repos -e acme-inc -e acme-research -d results

# a single file, or stdout
gh-enterprise-repos -e acme-inc -o acme.yaml
gh-enterprise-repos -e acme-inc -o - | yq '.repositories[].full_name'

# everything, including private, internal and archived repositories
gh-enterprise-repos -e acme-inc --visibility all --archived include

# one organization → ./acme-labs.yaml, with the teams that reach each repository
gh-enterprise-repos --org acme-labs --teams

# repositories no team has access to
gh-enterprise-repos --org acme-labs --visibility all --archived include --teams -o - \
  | yq '.repositories[] | select(.teams == null) | .full_name'

# GitHub Enterprise Server
gh-enterprise-repos --hostname ghe.example.com -e acme-inc
```

### Options

| Flag | Meaning |
| --- | --- |
| `-e, --enterprise <SLUG>` | Enterprise slug; repeatable, one YAML file each |
| `--org <LOGIN>` | Organization login, instead of or alongside `-e`; repeatable, one YAML file each |
| `-d, --output-dir <DIR>` | Where `<enterprise>.yaml` or `<org>.yaml` is written (default `.`) |
| `-o, --output <FILE>` | Write one export to this file instead; `-` is stdout |
| `--visibility <V>` | `public` (default), `private`, `internal`, `all` |
| `--archived <A>` | `exclude` (default), `include`, `only` |
| `--forks <F>` | `include` (default), `exclude`, `only` |
| `--topics` | Include topics, which cost a nested lookup per repository |
| `--teams` | Include the teams with access to each repository, which costs a walk of every team |
| `--hostname <HOST>` | GitHub Enterprise Server host, e.g. `ghe.example.com` |
| `--api-url <URL>` | Full GraphQL endpoint, if it is not `https://<host>/api/graphql` |
| `--concurrency <N>` | Organizations queried at once (default 3, max 16) |
| `--max-retries <N>` | Retries per request (default 5) |
| `--batch-size <N>` | Items per cursor fetch (default 100, max 100) |

The token is read from `GITHUB_TOKEN`, falling back to `GH_TOKEN`, and then to
`gh auth token` if the `gh` CLI is logged in. The CLI's credential is worth
preferring: it carries the SSO authorizations and organization grants that a
hand-made PAT has to be given one organization at a time, and an enterprise
listing made with a token that cannot see an organization leaves that
organization out without an error. The startup line names which of the three
the run used.

## Output

```yaml
source:
  api_url: https://api.github.com/graphql
  authenticated_as: alice
  token_scopes: read:org, repo
  enterprise: acme-inc
  filters:
    visibility: public
    archived: exclude
    forks: include
organizations:
  - acme-labs
  - acme-platform
  - acme-tools
totals:
  organizations: 3
  repositories: 42
  organizations_without_repositories: 0
repositories:
  - org: acme-labs
    name: widget-kit
    full_name: acme-labs/widget-kit
    url: https://github.com/acme-labs/widget-kit
    description: A toolkit for building widgets
    visibility: PUBLIC
    archived: false
    fork: false
    default_branch: main
    language: Rust
    license: Apache-2.0
    stars: 120
    forks: 52
    created_at: 2024-09-17T07:53:35Z
    updated_at: 2026-07-25T11:47:32Z
    pushed_at: 2026-06-05T10:19:46Z
```

The file is written the way `yq .` prints it — sequences indented under their
key, quoting only where a scalar needs it — so running `yq` over the output is
a no-op and re-exports diff cleanly against each other.

Ordering is deterministic: repositories by org login then repository name (both
case-insensitive). `template` and `empty` appear only when true; a field the
token could not read, or that the repository does not have, is absent rather
than null. `topics` is only fetched with `--topics`, and is then sorted by name. `license` is the SPDX id, falling back to the
license name when GitHub reports `NOASSERTION`.

With `--org` the file is the same shape, except that `source` names an
`organization` instead of an `enterprise` and `organizations` holds that one
login.

## Teams

`--teams` adds the teams that have access to each repository and the permission
each one holds — `admin`, `maintain`, `write`, `triage` or `read` — in slug
order:

```yaml
    teams:
      widget-admins: admin
      widget-maintainers: maintain
```

A repository no team reaches has no `teams` key, and
`totals.repositories_without_teams` counts those. That total is only present
with `--teams`, which is how a file with no `teams` keys says whether nobody has
access or nobody asked.

GraphQL has no teams field on a repository, so the run walks every team in the
organization and inverts each one's repository list; the cost grows with the
number of teams, not repositories. This is team access only: people added to a
repository directly, and outside collaborators, are not teams and do not appear.
Secret teams are visible to their members and to organization owners, so a
listing made by anyone else can show a repository as teamless when it is not. If
the teams of an organization cannot be read, that organization fails rather
than exporting with every repository looking unowned.

Organizations with no matching repositories still appear under
`organizations:`. An organization that could not be read at all is listed there
too, plus under `organizations_without_repository_data`, so a permissions gap
does not read as an empty org.

## Whose view a listing is

An organization the token cannot see is not an error. `enterprise.organizations`
simply does not return it, the listing covers the organizations that came back,
and nothing in the file says one is missing. An organization it can see but
cannot see *into* is worse: that one answers with an empty repository list, so
it appears in `organizations` holding nothing at all.

`authenticated_as` and `token_scopes` record whose view produced the file —
`token_scopes` is absent for fine-grained PATs and App tokens, which do not
report scopes. `totals.organizations_without_repositories` counts the
organizations that answered and held nothing the filters kept.

Under a filter that count is unremarkable: most organizations have no internal
repositories. When a run filters *nothing* out — `--visibility all --archived
include --forks include` — and at least three organizations, and at least 10% of
them, still come back empty, the run prints a warning and records it under
`notes`:

```yaml
notes:
  - "organizations_without_repositories: 6 of 54 organizations (11.1%) held no
    repository, although this run filtered none out. An organization the token
    cannot see into answers with an empty list rather than an error, so check
    these against the enterprise before reading them as empty."
```

`gh auth token` is usually the credential that reaches everything, since it
carries the SSO authorizations a PAT has to be granted per organization.

## Behavior worth knowing

- **The filter is applied twice.** `visibility`, `isArchived` and `isFork` are
  sent as query arguments so GitHub does the work, and each returned repository
  is re-checked locally, so a server that ignores an argument cannot widen the
  result set. A null `isArchived`/`isFork` reads as false; an unreadable
  visibility keeps the repository rather than silently dropping it.
- **Cursor pagination throughout.** Every connection advances by
  `after: <endCursor>`; there are no page numbers or offsets. Topics, when
  requested, are capped at 20 per repository by GitHub, so a single nested fetch
  is complete.
- **Rate limits.** The client tracks the `x-ratelimit-*` headers and waits for
  the reset before spending the last of the budget, honors `Retry-After`,
  recognizes secondary rate limits and `RATE_LIMITED` responses on an otherwise
  successful request, and backs off exponentially on 5xx and timeouts. A
  hostname that does not resolve fails immediately instead of retrying.
- **Partial results beat no results.** One unreadable organization is reported
  on stderr and the enterprise still exports; one unreadable enterprise does not
  stop the others. The exit status is non-zero only when every organization in
  an enterprise fails, or every enterprise fails.

## Tests

```sh
cargo test
```

Unit tests cover the filter (argument mapping, local re-check, unreadable
fields), report assembly (ordering, unreadable orgs, topic sorting, license
fallback, omitted fields, team attachment) and the backoff/reset arithmetic. They make no
network calls.

`results/` is gitignored and holds YAML captured from real runs, kept so the
output shape can be inspected without re-spending API quota. A `.batch3.yaml`
capture is the same export fetched with `--batch-size 3`; it is identical to the
default-batch export apart from live counters, which is how the cursor paths are
verified.
