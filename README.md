# gh-enterprise-repos

Queries a GitHub enterprise over GraphQL and writes one YAML file per
enterprise listing every repository in every organization it contains. Public,
non-archived repositories by default.

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

# GitHub Enterprise Server
gh-enterprise-repos --hostname ghe.example.com -e acme-inc
```

### Options

| Flag | Meaning |
| --- | --- |
| `-e, --enterprise <SLUG>` | Enterprise slug; repeatable, one YAML file each |
| `-d, --output-dir <DIR>` | Where `<enterprise>.yaml` is written (default `.`) |
| `-o, --output <FILE>` | Write one enterprise to this file instead; `-` is stdout |
| `--visibility <V>` | `public` (default), `private`, `internal`, `all` |
| `--archived <A>` | `exclude` (default), `include`, `only` |
| `--forks <F>` | `include` (default), `exclude`, `only` |
| `--topics` | Include topics, which cost a nested lookup per repository |
| `--hostname <HOST>` | GitHub Enterprise Server host, e.g. `ghe.example.com` |
| `--api-url <URL>` | Full GraphQL endpoint, if it is not `https://<host>/api/graphql` |
| `--concurrency <N>` | Organizations queried at once (default 3, max 16) |
| `--max-retries <N>` | Retries per request (default 5) |
| `--batch-size <N>` | Items per cursor fetch (default 100, max 100) |

The token is read from `GITHUB_TOKEN`, falling back to `GH_TOKEN`.

## Output

```yaml
source:
  api_url: https://api.github.com/graphql
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

Organizations with no matching repositories still appear under
`organizations:`. An organization that could not be read at all is listed there
too, plus under `organizations_without_repository_data`, so a permissions gap
does not read as an empty org.

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
fallback, omitted fields) and the backoff/reset arithmetic. They make no
network calls.

`results/` is gitignored and holds YAML captured from real runs, kept so the
output shape can be inspected without re-spending API quota. A `.batch3.yaml`
capture is the same export fetched with `--batch-size 3`; it is identical to the
default-batch export apart from live counters, which is how the cursor paths are
verified.
