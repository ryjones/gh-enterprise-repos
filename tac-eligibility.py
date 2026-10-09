#!/usr/bin/env python3
"""Build the TAC election hashes.js from the mirrors, the CLOWarden configs and
the org-members export.

Two lists go into the page, and they come from different places:

  nominees -- eligible to run: anyone who authored a commit in the window, read
              from the bare mirrors clone-repos.zsh made, with commit emails
              resolved to GitHub logins through the API.
  voters   -- eligible to vote: maintainers, meaning everyone a CLOWarden config
              puts in a team holding write, maintain or admin on some
              repository, plus the organization owners.

Four phases, each resumable, each writing its result beside the mirrors so the
next one can be re-run on its own:

  authors  mirrors            -> repos/authors.json   (no network)
  logins   authors.json       -> repos/logins.json    (GitHub API, cached)
  voters   configs + people   -> repos/voters.json    (no network)
  write    the two lists      -> hashes.js

    ./tac-eligibility.py --report reports/lf-decentralized-trust.yaml
    ./tac-eligibility.py --phase logins          # resume the API half alone

repos/ may hold the mirrors of more than one enterprise, so pass --report (YAML
or JSON, repeatable) to count only the repositories that report names. Without
it, every mirror under repos/ counts.

The email -> login cache lives in repos/cache.json and is keyed by email only,
so it is worth keeping across enterprises: every address it already holds is an
API call the next run does not make.

An address GitHub cannot resolve belongs to no account and is no contribution as
GitHub counts it either, so it is left out and listed in logins.json under
`unresolved`, busiest first. To credit one anyway, put it in repos/aliases.json
as {"email": "login"}: those are never looked up and always win.
"""

import argparse
import collections
import concurrent.futures as futures
import datetime
import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys
import threading
import time

# CLOWarden's config is the same schema under four names (plus one typo-safe
# variant); see the deployment notes in gh-org-members.
CONFIG_NAMES = ("config.yaml", "config.yml", "teams.yml", "teams.yaml", "access-control.yaml")

# "Write access or higher" in CLOWarden's vocabulary. read and triage are not it.
WRITE_PLUS = {"write", "maintain", "admin"}

# Accounts that are automation, not people. A login ending in [bot] is caught
# by pattern; these are the ones that look like users.
BOT_LOGINS = {
    "actions-user", "dependabot", "dependabot-preview", "github-actions",
    "lfdt-bot", "renovate", "renovate-bot", "web-flow",
}
BOT_EMAILS = {
    "actions@github.com", "noreply@github.com", "support@github.com",
    "41898282+github-actions[bot]@users.noreply.github.com",
}

NOREPLY = re.compile(r"^(?:\d+\+)?([A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?)@users\.noreply\.github\.com$")

UNIT = "\x1f"  # between fields of one commit
RECORD = "\x1e"  # between co-authors within a commit

say_lock = threading.Lock()


def say(line, err=False):
    with say_lock:
        print(line, file=sys.stderr if err else sys.stdout, flush=True)


def die(message, code=1):
    say(f"{pathlib.Path(sys.argv[0]).name}: {message}", err=True)
    raise SystemExit(code)


def team_slug(name):
    """GitHub's team slug, near enough: lowercase, punctuation runs to one dash."""
    return re.sub(r"-+", "-", re.sub(r"[^a-z0-9]+", "-", str(name).lower())).strip("-")


def is_bot(login):
    low = login.lower()
    return low.endswith("[bot]") or low in BOT_LOGINS


def read_json(path, default=None):
    try:
        with open(path) as handle:
            return json.load(handle)
    except (FileNotFoundError, json.JSONDecodeError):
        return default


def write_json(path, data):
    """Write through a temporary file, so an interrupt cannot truncate what was
    there before."""
    tmp = pathlib.Path(f"{path}.tmp.{os.getpid()}")
    tmp.parent.mkdir(parents=True, exist_ok=True)
    with open(tmp, "w") as handle:
        json.dump(data, handle, indent=2, sort_keys=True)
        handle.write("\n")
    tmp.replace(path)


def read_yaml(path):
    """YAML through yq: the Macs' python3 has no yaml module."""
    done = subprocess.run(["yq", "-o=json", str(path)], capture_output=True, text=True)
    if done.returncode != 0 or not done.stdout.strip():
        return None
    try:
        return json.loads(done.stdout)
    except json.JSONDecodeError:
        return None


# ---------------------------------------------------------------- phase: authors

def on_disk(name):
    """How clone-repos.zsh spells a repository on disk: a leading dot becomes an
    underscore, so .github is kept as _github."""
    return f"_{name[1:]}" if name.startswith(".") else name


def wanted_from_reports(paths):
    """{(org, on-disk name)} the given reports name, or None for no restriction."""
    if not paths:
        return None
    wanted = set()
    for path in paths:
        report = read_yaml(path)
        if not report or not isinstance(report.get("repositories"), list):
            die(f"{path} is not a gh-enterprise-repos report")
        for entry in report["repositories"]:
            if entry.get("visibility") == "PUBLIC" and not entry.get("archived"):
                wanted.add((entry["org"], on_disk(entry["name"])))
    return wanted


def mirrors(root, wanted=None):
    """Every bare clone under repos/<org>/<name>, as (org, name, path), kept to
    the ones the reports name when there are any."""
    found = []
    for org in sorted(p for p in root.iterdir() if p.is_dir()):
        if not org.is_dir():
            continue
        for repo in sorted(p for p in org.iterdir() if p.is_dir()):
            if repo.name.endswith(".partial") or not (repo / "objects").is_dir():
                continue
            if wanted is not None and (org.name, repo.name) not in wanted:
                continue
            found.append((org.name, repo.name, repo))
    return found


def commits_of(path, since, until, coauthors):
    """One line per commit in the window: author email, author name, and the
    Co-authored-by trailers when they are wanted."""
    fields = ["%H", "%aE", "%aN"]
    if coauthors:
        fields.append(f"%(trailers:key=Co-authored-by,valueonly,separator={RECORD})")
    args = ["git", "-C", str(path), "log", "--all", "--no-notes",
            f"--since={since}", f"--format={UNIT.join(fields)}"]
    if until:
        args.append(f"--until={until}")
    env = dict(os.environ, GIT_NO_LAZY_FETCH="1", GIT_TERMINAL_PROMPT="0")
    done = subprocess.run(args, capture_output=True, text=True, env=env)
    if done.returncode != 0:
        raise RuntimeError((done.stderr or "git log failed").strip().splitlines()[-1])
    return [line.split(UNIT) for line in done.stdout.splitlines() if line]


def phase_authors(args):
    root = args.repos
    if not root.is_dir():
        die(f"no mirrors in {root}; run clone-repos.zsh first")
    wanted = wanted_from_reports(args.report)
    found = mirrors(root, wanted)
    if not found:
        die(f"no bare clones under {root}" + (" for the repositories the reports name"
            if wanted else "") + "; run clone-repos.zsh first")
    if wanted is not None:
        missing = len(wanted) - len(found)
        say(f"{len(found)} of the {len(wanted)} repositories in "
            f"{', '.join(p.name for p in args.report)} are mirrored"
            + (f"; {missing} are not cloned yet" if missing > 0 else ""))

    width = len(str(len(found)))
    emails = {}
    merge_lock = threading.Lock()
    counted = collections.Counter()

    def one(index, org, name, path):
        slug = f"{org}/{name}"
        try:
            rows = commits_of(path, args.since, args.until, args.coauthors)
        except RuntimeError as problem:
            say(f"[{index:>{width}}/{len(found)}] {slug} FAILED: {problem}", err=True)
            counted["failed"] += 1
            return
        seen = {}
        for row in rows:
            sha, email, name_of = row[0], row[1].strip().lower(), row[2].strip()
            people = [(email, name_of, sha)]
            if args.coauthors and len(row) > 3 and row[3]:
                for trailer in row[3].split(RECORD):
                    parsed = re.match(r"\s*(.*?)\s*<([^>]+)>\s*$", trailer)
                    if parsed:
                        people.append((parsed.group(2).strip().lower(), parsed.group(1), None))
            # A co-author has no commit of their own to look up, so no sample sha.
            for who, label, own in people:
                if not who or who in BOT_EMAILS:
                    continue
                entry = seen.setdefault(who, {"commits": 0, "name": label, "sample": None})
                entry["commits"] += 1
                if own and not entry["sample"]:
                    entry["sample"] = f"{slug}@{own}"
        with merge_lock:
            for who, entry in seen.items():
                into = emails.setdefault(who, {"commits": 0, "name": entry["name"],
                                               "repos": [], "sample": None})
                into["commits"] += entry["commits"]
                into["repos"].append(slug)
                if entry["sample"] and not into["sample"]:
                    into["sample"] = entry["sample"]
            counted["commits"] += len(rows)
        say(f"[{index:>{width}}/{len(found)}] {slug} {len(rows)} commits, {len(seen)} addresses")

    with futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        list(pool.map(lambda job: one(*job),
                      ((i, org, name, path) for i, (org, name, path) in enumerate(found, 1))))

    for entry in emails.values():
        entry["repos"] = sorted(set(entry["repos"]))

    out = {
        "window": {"since": args.since, "until": args.until or "now"},
        "reports": [str(path) for path in args.report],
        "mirrors": len(found),
        "coauthors": args.coauthors,
        "commits": counted["commits"],
        "emails": emails,
    }
    write_json(args.authors, out)
    say(f"\n{counted['commits']} commits in the window across {len(found)} mirrors "
        f"({counted['failed']} unreadable), {len(emails)} distinct addresses -> {args.authors}")


# ----------------------------------------------------------------- phase: logins

class Cache:
    """email -> {login, resolved, via, source}, shared across enterprises.

    Re-read before every save: another run of this tool may have added rows in
    the meantime, and no run should cost another one its lookups.
    """

    def __init__(self, path):
        self.path = path
        self.lock = threading.Lock()
        self.added = {}
        self.known = (read_json(path, {}) or {}).get("emails", {})

    def get(self, email):
        return self.known.get(email)

    def put(self, email, entry):
        with self.lock:
            self.known[email] = entry
            self.added[email] = entry
            if len(self.added) % 25 == 0:
                self._save()

    def save(self):
        with self.lock:
            self._save()

    def _save(self):
        on_disk = (read_json(self.path, {}) or {}).get("emails", {})
        on_disk.update(self.added)
        self.known.update(on_disk)
        write_json(self.path, {"emails": on_disk, "updated": datetime.date.today().isoformat()})


def gh_api(route, retries=3):
    """One API call through gh, waiting out a rate limit rather than failing."""
    for attempt in range(1, retries + 1):
        done = subprocess.run(["gh", "api", "-H", "Accept: application/vnd.github+json", route],
                              capture_output=True, text=True)
        if done.returncode == 0:
            try:
                return json.loads(done.stdout)
            except json.JSONDecodeError:
                return None
        problem = (done.stderr or "").strip()
        if "404" in problem or "Not Found" in problem:
            return None
        limited = "rate limit" in problem.lower() or "403" in problem or "429" in problem
        if limited and attempt < retries:
            wait = rate_limit_wait()
            say(f"  rate limited; waiting {wait}s", err=True)
            time.sleep(wait)
            continue
        if attempt == retries:
            raise RuntimeError(problem.splitlines()[-1] if problem else "gh api failed")
        time.sleep(5 * attempt)
    return None


def rate_limit_wait(cap=3900):
    done = subprocess.run(["gh", "api", "rate_limit"], capture_output=True, text=True)
    try:
        core = json.loads(done.stdout)["resources"]["core"]
        return max(5, min(cap, int(core["reset"] - time.time()) + 5))
    except Exception:
        return 60


def phase_logins(args):
    authored = read_json(args.authors)
    if not authored:
        die(f"no {args.authors}; run the authors phase first")
    cache = Cache(args.cache)
    emails = authored["emails"]

    # Hand-written mappings for addresses no account claims; they are never
    # looked up and they override whatever the API said.
    aliases = {str(key).strip().lower(): str(value).strip()
               for key, value in (read_json(args.aliases, {}) or {}).items()}
    if aliases:
        say(f"{len(aliases)} mappings from {args.aliases.name}")

    def pending(email):
        if email in aliases:
            return False
        hit = cache.get(email)
        if hit is None:
            return True
        return args.retry_unresolved and not hit.get("login")

    todo = [email for email in sorted(emails) if pending(email)]
    say(f"{len(emails)} addresses, {len(emails) - len(todo) - len(aliases & emails.keys())} "
        f"already in {args.cache.name}, {len(todo)} to look up")
    if todo and args.offline:
        say(f"--offline: leaving {len(todo)} addresses unresolved")
        todo = []

    width = len(str(max(len(todo), 1)))
    counted = collections.Counter()

    def resolve(index, email):
        sample = emails[email].get("sample")
        login, source = None, None
        if sample:
            slug, sha = sample.rsplit("@", 1)
            try:
                commit = gh_api(f"repos/{slug}/commits/{sha}")
            except RuntimeError as problem:
                say(f"[{index:>{width}}/{len(todo)}] {email} FAILED: {problem}", err=True)
                counted["failed"] += 1
                return
            if commit and isinstance(commit.get("author"), dict):
                login, source = commit["author"].get("login"), "api"
        if not login:
            # No linked account on the commit, or a co-author with no commit of
            # their own: the address itself may still name the account.
            parsed = NOREPLY.match(email)
            if parsed:
                login, source = parsed.group(1), "noreply"
        cache.put(email, {"login": login, "source": source, "via": sample,
                          "resolved": datetime.date.today().isoformat()})
        counted["resolved" if login else "unresolved"] += 1
        say(f"[{index:>{width}}/{len(todo)}] {email} -> {login or 'no account'}"
            f"{'' if not source else f' ({source})'}")

    if todo:
        with futures.ThreadPoolExecutor(max_workers=args.api_jobs) as pool:
            list(pool.map(lambda job: resolve(*job), enumerate(todo, 1)))
        cache.save()

    logins = {}
    unresolved = []
    for email, entry in sorted(emails.items()):
        hit = cache.get(email) or {}
        login = aliases.get(email) or hit.get("login")
        if not login:
            unresolved.append({"email": email, "name": entry.get("name"),
                               "commits": entry["commits"], "repos": entry["repos"][:5]})
            continue
        into = logins.setdefault(login.lower(), {"login": login, "commits": 0,
                                                 "emails": [], "repos": []})
        into["commits"] += entry["commits"]
        into["emails"].append(email)
        into["repos"].extend(entry["repos"])
    for entry in logins.values():
        entry["emails"] = sorted(set(entry["emails"]))
        entry["repos"] = sorted(set(entry["repos"]))

    bots = sorted(login for login in logins if is_bot(login))
    for login in bots:
        del logins[login]

    write_json(args.logins, {
        "window": authored["window"],
        "logins": logins,
        "bots_dropped": bots,
        "unresolved": sorted(unresolved, key=lambda row: -row["commits"]),
    })
    say(f"\n{len(logins)} accounts ({counted['resolved']} newly resolved, "
        f"{len(bots)} bots dropped), {len(unresolved)} addresses with no account "
        f"-> {args.logins}")


# ----------------------------------------------------------------- phase: voters

def clowarden_state(mirror):
    """(teams with write or better, per-org team rosters) from the mirrored configs."""
    write_teams, rosters, orgs, skipped = set(), {}, [], []
    for marker in sorted(mirror.glob("*/*/.git")):
        repo = marker.parent
        org = repo.parent.name
        config = next((repo / name for name in CONFIG_NAMES if (repo / name).exists()), None)
        if config is None:
            skipped.append(org)
            continue
        data = read_yaml(config)
        if data is None:
            say(f"warning: cannot parse {config}", err=True)
            skipped.append(org)
            continue
        orgs.append(org)
        for team in data.get("teams") or []:
            people = [str(who) for who in (team.get("maintainers") or [])]
            people += [str(who) for who in (team.get("members") or [])]
            rosters[(org, team_slug(team.get("name")))] = people
        for entry in data.get("repositories") or []:
            for team, permission in (entry.get("teams") or {}).items():
                if str(permission).lower() in WRITE_PLUS:
                    write_teams.add((org, team_slug(team)))
            # A collaborator named straight on the repository, if any config grows one.
            for who, permission in (entry.get("collaborators") or {}).items():
                if str(permission).lower() in WRITE_PLUS:
                    rosters.setdefault((org, f"collaborator:{entry.get('name')}"), []).append(str(who))
                    write_teams.add((org, f"collaborator:{entry.get('name')}"))
    return write_teams, rosters, orgs, skipped


def phase_voters(args):
    if not args.clowarden.is_dir():
        die(f"no CLOWarden mirror at {args.clowarden}; run gh-org-members' mirror-clowarden.zsh")
    if not args.people.is_file():
        die(f"cannot read {args.people}")

    write_teams, rosters, orgs, skipped = clowarden_state(args.clowarden)
    say(f"{len(orgs)} CLOWarden configs, {len(write_teams)} teams with write or better"
        f"{'' if not skipped else f' ({len(skipped)} orgs without a readable config)'}")

    people = read_yaml(args.people)
    if not people or "people" not in people:
        die(f"{args.people} is not a gh-org-members export")

    voters = {}

    def note(login, source, where):
        entry = voters.setdefault(login.lower(), {"login": login, "sources": set(), "where": set()})
        entry["sources"].add(source)
        entry["where"].add(where)

    for (org, team), roster in rosters.items():
        if (org, team) in write_teams:
            for login in roster:
                note(login, "config", f"{org}/{team}")

    for person in people["people"]:
        login = person.get("login")
        if not login:
            continue
        for membership in person.get("organizations") or []:
            org = membership.get("org")
            if str(membership.get("role", "")).upper() == "ADMIN":
                note(login, "owner", f"{org}/owners")
            for team in membership.get("teams") or []:
                if (org, team_slug(team.get("slug") or team.get("name"))) in write_teams:
                    note(login, "live", f"{org}/{team.get('slug')}")

    bots = sorted(login for login in voters if is_bot(login))
    for login in bots:
        del voters[login]

    by_source = collections.Counter()
    for entry in voters.values():
        by_source["+".join(sorted(entry["sources"]))] += 1
        entry["sources"] = sorted(entry["sources"])
        entry["where"] = sorted(entry["where"])

    write_json(args.voters, {
        "clowarden_orgs": sorted(orgs),
        "orgs_without_config": sorted(set(skipped)),
        "people_export": str(args.people),
        "write_teams": len(write_teams),
        "bots_dropped": bots,
        "voters": voters,
    })
    say(f"{len(voters)} maintainers ({len(bots)} bots dropped) -> {args.voters}")
    for combination, count in sorted(by_source.items(), key=lambda row: -row[1]):
        say(f"  {count:>5}  {combination}")


# ------------------------------------------------------------------ phase: write

TEMPLATE = """'use strict';

class Hashes {{
    static check(hash) {{
        const voter_hashes = [
{voters}
        ];
        const nominee_hashes = [
{nominees}
        ];
        return [(voter_hashes.indexOf(hash) != -1), (nominee_hashes.indexOf(hash) != -1)];
    }}
}}
if (typeof module != 'undefined' && module.exports) module.exports = Hashes;
"""


def hashed(logins):
    """What the page checks: md5 of the login as the form sends it, trimmed and
    lowercased."""
    return sorted({hashlib.md5(login.strip().lower().encode()).hexdigest() for login in logins})


def phase_write(args):
    voters = read_json(args.voters)
    nominees = read_json(args.logins)
    if not voters:
        die(f"no {args.voters}; run the voters phase first")
    if not nominees:
        die(f"no {args.logins}; run the logins phase first")
    if not args.out.parent.is_dir():
        die(f"no directory for {args.out}")

    voter_logins = [entry["login"] for entry in voters["voters"].values()]
    nominee_logins = [entry["login"] for entry in nominees["logins"].values()]
    voter_hashes, nominee_hashes = hashed(voter_logins), hashed(nominee_logins)

    was = {"voter": [], "nominee": []}
    if args.out.exists():
        text = args.out.read_text()
        for kind in was:
            block = text.split(f"{kind}_hashes = [", 1)
            if len(block) == 2:
                was[kind] = re.findall(r"[0-9a-f]{32}", block[1].split("];", 1)[0])

    def rendered(values):
        return "\n".join(f'              "{value}",' for value in values)

    args.out.write_text(TEMPLATE.format(voters=rendered(voter_hashes),
                                       nominees=rendered(nominee_hashes)))

    say(f"\nwrote {args.out}")
    for kind, now, before in (("voter", voter_hashes, was["voter"]),
                              ("nominee", nominee_hashes, was["nominee"])):
        added = len(set(now) - set(before))
        gone = len(set(before) - set(now))
        say(f"  {kind}_hashes: {len(now)} ({len(before)} before, +{added} / -{gone})")
    both = len(set(voter_hashes) & set(nominee_hashes))
    say(f"  in both lists: {both}")
    say(f"\nWindow {nominees['window']['since']} to {nominees['window']['until']}. "
        f"Nothing was committed; the logins behind the hashes are in "
        f"{args.voters.name} and {args.logins.name}, which are gitignored.")


# ------------------------------------------------------------------------- main

def main():
    here = pathlib.Path(__file__).resolve().parent
    neighbor = here.parent / "gh-org-members"
    mirror_root = here.parent.parent / "github-mirror"

    parse = argparse.ArgumentParser(
        description=__doc__.splitlines()[0],
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="Phases: authors, logins, voters, write. All four, in order, by default.")
    parse.add_argument("--phase", action="append", choices=["authors", "logins", "voters", "write"],
                       help="run only this phase (repeatable)")
    parse.add_argument("--report", type=pathlib.Path, action="append", default=[],
                       metavar="FILE",
                       help="count only the repositories this gh-enterprise-repos "
                            "report names, YAML or JSON (repeatable)")
    parse.add_argument("--repos", type=pathlib.Path, default=here / "repos",
                       help="the bare mirrors clone-repos.zsh made (default: repos/)")
    parse.add_argument("--since", default="2025-07-01", help="start of the activity window")
    parse.add_argument("--until", default=None, help="end of the activity window (default: now)")
    parse.add_argument("--coauthors", action="store_true",
                       help="credit Co-authored-by trailers as activity too")
    parse.add_argument("--people", type=pathlib.Path,
                       default=neighbor / "results" / "lf-decentralized-trust.gh.yaml",
                       help="gh-org-members export, for team membership and owners")
    parse.add_argument("--clowarden", type=pathlib.Path, default=neighbor / "mirror",
                       help="gh-org-members' mirror of the CLOWarden config repos")
    parse.add_argument("--out", type=pathlib.Path,
                       default=mirror_root / "LF-Decentralized-Trust" / "tac-eligibility-check" / "hashes.js",
                       help="the hashes.js to write")
    parse.add_argument("--jobs", type=int, default=os.cpu_count() or 8,
                       help="mirrors read at a time")
    parse.add_argument("--api-jobs", type=int, default=4,
                       help="API lookups at a time (GitHub's secondary limit is strict)")
    parse.add_argument("--offline", action="store_true",
                       help="resolve from the cache only, make no API calls")
    parse.add_argument("--retry-unresolved", action="store_true",
                       help="look up again the addresses the API found no account for")
    args = parse.parse_args()

    args.authors = args.repos / "authors.json"
    args.logins = args.repos / "logins.json"
    args.voters = args.repos / "voters.json"
    args.cache = args.repos / "cache.json"
    args.aliases = args.repos / "aliases.json"
    args.repos.mkdir(parents=True, exist_ok=True)

    for tool in ("git", "yq") + (() if args.offline else ("gh",)):
        if subprocess.run(["which", tool], capture_output=True).returncode != 0:
            die(f"{tool} is required and was not found", 127)

    wanted = args.phase or ["authors", "logins", "voters", "write"]
    for phase in ("authors", "logins", "voters", "write"):
        if phase in wanted:
            say(f"== {phase} ==")
            globals()[f"phase_{phase}"](args)


if __name__ == "__main__":
    main()
