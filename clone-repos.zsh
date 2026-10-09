#!/usr/bin/env zsh
#
# clone-repos.zsh -- bare-clone every public, non-archived repository named in a
# gh-enterprise-repos report into repos/<org>/<name>, over https.
#
#   ./clone-repos.zsh reports/lf-decentralized-trust.yaml
#   ./clone-repos.zsh reports/*.yaml            # several enterprises at once
#
# The report can be YAML or JSON; it goes through yq either way.
#
# The clones are mirrors kept for counting activity per account over a time
# period, so by default they are fetched with --filter=tree:0: every commit,
# with its author, committer and dates, and no file content. Pass --full when
# something needs the trees or blobs.
#
# Re-running fetches into the clones that are already there, so an interrupted
# run resumes. A repository named .github is cloned as _github, matching how
# dotted repositories are kept elsewhere in the mirror; a dotted directory
# hides itself from globs and from most tooling.

setopt err_exit no_unset pipe_fail
zmodload zsh/datetime

SELF=${0:A}

die() {
  print -ru2 -- "${SELF:t}: $1"
  exit ${2:-1}
}

# Where a repository lands: its own name, with a leading dot turned into an
# underscore so the clone is not a hidden directory.
on_disk() {
  print -r -- ${1/#./_}
}

# One repository, run by xargs in parallel. Reached only through the private
# --clone-one flag, which carries the shared settings in CR_* variables.
clone_one() {
  local idx=$1 org=$2 name=$3 url=$4
  local dest=$CR_ROOT/$org/$(on_disk $name)
  local label="[${(l:${#CR_TOTAL}:)idx}/$CR_TOTAL] $org/$name"
  local started=$EPOCHREALTIME
  local -a filter=(${=CR_FILTER})
  local action rc=0
  local errors=$(mktemp)

  mkdir -p ${dest:h}
  if [[ -d $dest/objects ]]; then
    action=updated
    git -C $dest fetch --prune --quiet 2> $errors || rc=$?
  else
    action=cloned
    # Clone aside and rename, so an interrupted run leaves no half-clone that
    # the next run would mistake for a finished one.
    rm -rf -- $dest.partial
    { git clone --mirror --quiet $filter -- $url $dest.partial 2> $errors \
        && mv -- $dest.partial $dest } || rc=$?
  fi

  local took=$(printf '%.0fs' $(( EPOCHREALTIME - started )))
  if (( rc )); then
    local why=$(grep -v '^[[:space:]]*$' $errors | tail -1 || :)
    print -ru2 -- "$label FAILED ($action, git exit $rc): ${why:-no output}"
    printf '%s\t%s\t%s\t%s\n' "$org/$name" $action $rc "${why:-no output}" >> $CR_FAILURES
    rm -rf -- $dest.partial
  else
    print -r -- "$label $action, $(du -sh $dest | cut -f1 | tr -d ' \t') in $took"
    print -r -- $action >> $CR_TALLY
  fi
  rm -f -- $errors
  return 0
}

if [[ ${1:-} == --clone-one ]]; then
  shift
  (( $# == 4 )) || die "--clone-one takes INDEX ORG NAME URL" 2
  clone_one "$@"
  exit 0
fi

ncpu=$(sysctl -n hw.ncpu 2> /dev/null || print 8)

usage() {
  print -r -- "usage: ${SELF:t} [-d DIR] [-j JOBS] [--full] [-n] REPORT [REPORT...]

  -d DIR     where the clones go (default: ${SELF:h:t}/repos)
  -j JOBS    clones at a time (default: $ncpu)
  --full     clone trees and blobs too, not just commit history
  -n         list what would be cloned and stop
  REPORT     gh-enterprise-repos report, YAML or JSON; several are merged"
}

zparseopts -D -E -F -- d:=opt_dir j:=opt_jobs n=opt_dry -full=opt_full \
  h=opt_help -help=opt_help || { usage >&2; exit 2 }
if (( $#opt_help )); then
  usage
  exit 0
fi
[[ ${1:-} == -- ]] && shift
(( $# >= 1 )) || { usage >&2; exit 2 }

for tool in git jq yq xargs; do
  (( $+commands[$tool] )) || die "$tool is required and was not found" 127
done

reports=($@)
for report in $reports; do
  [[ -r $report ]] || die "cannot read $report"
done

root=${opt_dir[2]:-${SELF:h}/repos}
jobs=${opt_jobs[2]:-$ncpu}
[[ $jobs == <1-> ]] || die "-j takes a positive number, not ${(q)jobs}"

tmp=$(mktemp -d)
trap 'rm -rf -- $tmp' EXIT

# YAML or JSON, through yq either way, one file per report.
for report in $reports; do
  yq -o=json . $report > $tmp/report.json 2>/dev/null \
    || die "$report is not YAML or JSON"
  jq -e '(.repositories | type == "array") and (.source | type == "object")' \
    $tmp/report.json > /dev/null 2>&1 \
    || die "$report is not a gh-enterprise-repos report"
  cat $tmp/report.json >> $tmp/reports.json
done

# Public and not archived, sorted so the numbering is stable between runs. A
# repository two reports both name is cloned once.
jq -s '
  [ .[].repositories[]
    | select(.visibility == "PUBLIC" and .archived != true)
    | { org, name, url: (.url // "https://github.com/\(.org)/\(.name)") } ]
  | unique_by([.org, .name])
  | sort_by([.org, .name])
' $tmp/reports.json > $tmp/selected.json || die "could not read repositories from the report"

total=$(jq length $tmp/selected.json)
(( total )) || die "no public, non-archived repositories in ${(j:, :)${(@)reports:t}}"

if (( $#opt_dry )); then
  print -r -- "$total public, non-archived repositories from ${(j:, :)${(@)reports:t}} would be mirrored into $root:"
  jq -r 'to_entries[] | [.key + 1, .value.org, .value.name, .value.url] | @tsv' \
    $tmp/selected.json |
    while IFS=$'\t' read -r idx org name url; do
      printf '%s\t%s/%s\t%s\n' $idx $org $(on_disk $name) $url
    done
  exit 0
fi

# NUL-separated, so no name could ever split into two arguments.
jq -j -r '
  to_entries[]
  | "\(.key + 1)\u0000\(.value.org)\u0000\(.value.name)\u0000\(.value.url)\u0000"
' $tmp/selected.json > $tmp/queue

export CR_ROOT=$root
export CR_TOTAL=$total
if (( $#opt_full )); then
  export CR_FILTER=''
else
  export CR_FILTER='--filter=tree:0'
fi
export CR_FAILURES=$tmp/failures
export CR_TALLY=$tmp/tally
# A public repository needs no credential; never stop to ask for one, so a
# repository that was renamed or made private fails instead of hanging.
export GIT_TERMINAL_PROMPT=0

mkdir -p $root
: > $CR_FAILURES
: > $CR_TALLY

if (( $#opt_full )); then
  depth='full clones'
else
  depth='commits only'
fi
print -r -- "Mirroring $total public, non-archived repositories from ${(j:, :)${(@)reports:t}} into $root, $jobs at a time ($depth)."
started=$EPOCHREALTIME

xargs -0 -n 4 -P $jobs $SELF --clone-one < $tmp/queue || :

cloned=$(grep -c '^cloned$' $CR_TALLY || :)
updated=$(grep -c '^updated$' $CR_TALLY || :)
failed=$(grep -c . $CR_FAILURES || :)
print -r -- "Done in $(printf '%.0fs' $(( EPOCHREALTIME - started ))): $cloned cloned, $updated updated, $failed failed; $(du -sh $root | cut -f1 | tr -d ' \t') in $root."

if (( failed )); then
  print -ru2 -- ""
  print -ru2 -- "Failed:"
  sort $CR_FAILURES | while IFS=$'\t' read -r slug action rc why; do
    print -ru2 -- "  $slug ($action, git exit $rc): $why"
  done
  exit 1
fi
