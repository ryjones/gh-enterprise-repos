#!/usr/bin/env zsh
#
# drift.zsh -- compare a gh-enterprise-repos export of one organization with
# the CLOWarden config.yaml meant to describe it, and write what differs as a
# markdown report.
#
# The export has to hold everything and carry team access:
#
#   gh-enterprise-repos --org acme --visibility all --archived include --teams -d results
#   ./drift.zsh -c ../acme/governance/config.yaml
#
# Reads two files and writes one; it never talks to GitHub.

setopt err_exit no_unset pipe_fail
zmodload zsh/stat

SELF=${0:A}

usage() {
  print -r -- "usage: ${SELF:t} -c CONFIG [-o OUT] [EXPORT]

  -c CONFIG  CLOWarden config.yaml to compare against (or \$CLOWARDEN_CONFIG)
  -o OUT     report to write (default: DRIFT.md beside the export)
  EXPORT     gh-enterprise-repos YAML for one organization, made with --teams
             (default: the only .yaml in ${SELF:h:t}/results)"
}

die() {
  print -ru2 -- "${SELF:t}: $1"
  exit ${2:-1}
}

for tool in yq jq; do
  (( $+commands[$tool] )) || die "$tool is required and was not found" 127
done

zparseopts -D -E -F -- c:=opt_config o:=opt_out h=opt_help -help=opt_help || {
  usage >&2
  exit 2
}
if (( $#opt_help )); then
  usage
  exit 0
fi
[[ ${1:-} == -- ]] && shift
if (( $# > 1 )); then
  usage >&2
  exit 2
fi

config=${opt_config[2]:-${CLOWARDEN_CONFIG:-}}
[[ -n $config ]] || die "pass -c CONFIG, or set CLOWARDEN_CONFIG"
[[ -r $config ]] || die "cannot read $config"

if (( $# )); then
  export_file=$1
else
  found=(${SELF:h}/results/*.yaml(N))
  case $#found in
    0) die "no export in ${SELF:h}/results; run gh-enterprise-repos first" ;;
    1) export_file=$found[1] ;;
    *) die "several exports in ${SELF:h}/results; name one: ${found:t}" ;;
  esac
fi
[[ -r $export_file ]] || die "cannot read $export_file"

out=${opt_out[2]:-${export_file:h}/DRIFT.md}

tmp=$(mktemp -d)
trap 'rm -rf -- $tmp' EXIT

yq -o=json . $export_file > $tmp/live.json || die "$export_file is not YAML"
yq -o=json . $config > $tmp/config.json || die "$config is not YAML"

jq -e '.repositories | type == "array"' $tmp/live.json > /dev/null \
  || die "$export_file is not a gh-enterprise-repos export"
jq -e '.organizations | length == 1' $tmp/live.json > /dev/null \
  || die "$export_file covers several organizations; export one with --org"
# Without --teams every repository would read as having no team at all.
jq -e '.totals | has("repositories_without_teams")' $tmp/live.json > /dev/null \
  || die "$export_file was made without --teams"
jq -e '(.repositories | type == "array") and (.teams | type == "array")' $tmp/config.json > /dev/null \
  || die "$config has no teams: and repositories: lists"

# Which commit of the config this was, when it is kept in git.
config_rev=$(git -C ${config:h} log -1 --format=%h -- ${config:t} 2> /dev/null || :)
export_day=$(zstat -F %Y-%m-%d +mtime $export_file)

# Paths as the report shows them: relative when under the working directory,
# which may itself have been reached through a symlink.
shown() {
  print -r -- ${${1:A}#${PWD:A}/}
}

program=$(cat <<'JQ'
def lc: ascii_downcase;
def link: "[\(.name)](\(.url // "https://github.com/\(.org)/\(.name)"))";
def day: (. // "")[0:10] | if . == "" then "never" else . end;
def vis: (.visibility // "unknown") | lc;
def yn: if . then "yes" else "no" end;
def grants:
  (.teams // {}) | to_entries
  | if length == 0 then "none" else map("`\(.key)`: \(.value)") | join(", ") end;
def table($head; rows):
  [rows] as $rows
  | if ($rows | length) == 0 then "None."
    else ([$head, ($head | map("---"))] + $rows)[] | "| " + join(" | ") + " |"
    end;
def names: if length == 0 then "None." else map("- `\(.)`") | join("\n") end;
def split_count($all; $some; $label):
  "\($all | length)" + (if ($all | length) > 0 then " (\($some | length) \($label))" else "" end);

$live[0] as $l | $cfg[0] as $c
| $l.organizations[0] as $org
| $l.source.filters as $f
| ($f.visibility == "all" and $f.archived == "include" and $f.forks == "include") as $complete
| ($c.repositories | map({key: (.name | lc), value: .}) | from_entries) as $want
| ($l.repositories | map({key: (.name | lc), value: true}) | from_entries) as $have
| ($l.repositories | sort_by(.name | lc) | map(. + {managed: (.name | lc | in($want))})) as $repos

| ($repos | map(select(.managed | not))) as $unmanaged
| ($repos | map(select(vis != "public"))) as $closed
| ($repos | map(select(.archived))) as $archived
| ($repos | map(select((.teams // {}) | length == 0))) as $teamless

# One row per thing that differs on a repository both sides know about.
| [ $repos[] | select(.managed) | . as $r | $want[.name | lc] as $e
    | ( select($e.visibility != null and ($e.visibility | lc) != ($r | vis))
        | [($r | link), "visibility", ($e.visibility | lc), ($r | vis)] ),
      ( (($e.teams // {}) | with_entries(.key |= lc)) as $asked
        | ($r.teams // {}) as $got
        | ($asked + $got | keys)[] as $team
        | select($asked[$team] != $got[$team])
        | [($r | link), "team `\($team)`", ($asked[$team] // "not granted"), ($got[$team] // "not granted")] )
  ] as $differs
| ([$c.repositories[] | .name | select(lc | in($have) | not)] | sort_by(lc)) as $missing

| ([$c.teams[].name | lc] | unique) as $defined
| ([$c.repositories[] | .teams // {} | keys[] | lc] | unique) as $granted
| ([$repos[] | .teams // {} | keys[]] | unique) as $with_access

| [
  "# Drift: `\($org)` against `\($config | split("/") | last)`",
  "",
  "- Export: `\($export)`, written \($day) as `\($l.source.authenticated_as // "unknown")` (visibility \($f.visibility), archived \($f.archived), forks \($f.forks))",
  "- Config: `\($config)`" + (if $rev != "" then " at `\($rev)`" else "" end),
  "",
  ( select($complete | not)
    | "> **This export was filtered.** Repositories the filter left out are missing from every count below, and a repository in the config can show as not on GitHub only because it was filtered out. Export again with `--visibility all --archived include` for a full comparison.",
      "" ),
  "## Bottom line",
  "",
  table(["Check", "Count"];
    ["Repositories on GitHub", "\($repos | length)"],
    ["Repositories in the config", "\($c.repositories | length)"],
    ["On GitHub, not in the config", split_count($unmanaged; $unmanaged | map(select(.archived | not)); "active")],
    ["Private or internal", split_count($closed; $closed | map(select(.managed | not)); "not in the config")],
    ["Archived", split_count($archived; $archived | map(select(.managed)); "still in the config")],
    ["No team has access", split_count($teamless; $teamless | map(select(.archived | not)); "active")],
    ["In the config, differing from GitHub", "\($differs | map(.[0]) | unique | length)"],
    ["In the config, not on GitHub", "\($missing | length)"],
    ["Teams with access that the config does not define", "\($with_access - $defined | length)"]),
  "",
  "## Unmanaged repositories",
  "",
  "Active repositories on GitHub that the config does not list. Unmanaged archived repositories are under [Archived repositories](#archived-repositories).",
  "",
  table(["Repository", "Visibility", "Last push", "Teams on GitHub"];
    $unmanaged[] | select(.archived | not) | [link, vis, (.pushed_at | day), grants]),
  "",
  "## Private and internal repositories",
  "",
  table(["Repository", "Visibility", "Archived", "In config", "Last push", "Teams on GitHub"];
    $closed[] | [link, vis, (.archived | yn), (.managed | yn), (.pushed_at | day), grants]),
  "",
  "## Archived repositories",
  "",
  table(["Repository", "In config", "Last push", "Teams on GitHub"];
    $archived[] | [link, (.managed | yn), (.pushed_at | day), grants]),
  "",
  "## Repositories without teams",
  "",
  "Active repositories no team has access to. Archived ones show `none` in the table above.",
  "",
  table(["Repository", "In config", "Visibility", "Last push"];
    $teamless[] | select(.archived | not) | [link, (.managed | yn), vis, (.pushed_at | day)]),
  "",
  "## Managed repositories that differ",
  "",
  "Repositories in both places whose visibility or team permissions do not match.",
  "",
  table(["Repository", "What", "config.yaml", "GitHub"]; $differs[]),
  "",
  "## In the config, not on GitHub",
  "",
  ($missing | names),
  "",
  "## Teams",
  "",
  "The export names only teams that reach at least one repository, so a team the config defines but GitHub lacks cannot be told from here.",
  "",
  "### With access on GitHub, not defined in the config",
  "",
  ($with_access - $defined | names),
  "",
  "### Granted a repository in the config, not defined in it",
  "",
  ($granted - $defined | names),
  "",
  "### Defined in the config, granted no repository",
  "",
  ($defined - $granted | names)
] | join("\n")
JQ
)

# Written beside the target and renamed, so a failed run leaves the old report.
jq -rn \
  --slurpfile live $tmp/live.json \
  --slurpfile cfg $tmp/config.json \
  --arg export "$(shown $export_file)" \
  --arg config "$(shown $config)" \
  --arg rev "$config_rev" \
  --arg day $export_day \
  "$program" > $out.$$.tmp || {
  rm -f -- $out.$$.tmp
  die "could not render the report"
}
mv -f -- $out.$$.tmp $out

print -ru2 -- "Wrote $(shown $out)"
