#!/bin/sh
# A stand-in for the GitHub CLI used by tests. It records every call in gh.log and keeps
# pull requests and releases as files next to itself, merging into the bare remote named
# in the `remote` file. It never talks to a network.
D="$(cd "$(dirname "$0")" && pwd)"
echo "$*" >> "$D/gh.log"
REMOTE="$(cat "$D/remote")"
mkdir -p "$D/prs" "$D/releases"
arg() { # arg --flag args... -> value after the flag
  want="$1"; shift
  while [ $# -gt 0 ]; do
    if [ "$1" = "$want" ]; then echo "$2"; return; fi
    shift
  done
}
case "$1 $2" in
  "pr list")
    head="$(arg --head "$@")"; base="$(arg --base "$@")"
    out=""
    for f in "$D"/prs/*; do
      [ -f "$f" ] || continue
      read h b s < "$f"
      n="$(basename "$f")"
      if [ "$h" = "$head" ] && [ "$b" = "$base" ] && [ "$s" = open ]; then
        out="{\"number\":$n,\"url\":\"https://github.com/o/r/pull/$n\"}"
      fi
    done
    echo "[$out]" ;;
  "pr create")
    head="$(arg --head "$@")"; base="$(arg --base "$@")"
    n=$(( $(cat "$D/counter" 2>/dev/null || echo 0) + 1 )); echo "$n" > "$D/counter"
    echo "$head $base open" > "$D/prs/$n"
    echo "https://github.com/o/r/pull/$n" ;;
  "pr comment")
    echo "comment on $3: $(arg --body "$@")" >> "$D/comments.log" ;;
  "pr view")
    s=OPEN; m=MERGEABLE
    read h b st < "$D/prs/$3"
    [ "$st" = merged ] && s=MERGED
    [ -f "$D/conflict-$3" ] && m=CONFLICTING
    echo "{\"mergeable\":\"$m\",\"state\":\"$s\"}" ;;
  "pr merge")
    read h b st < "$D/prs/$3"
    msg="$(arg --subject "$@")"
    tmp="$(mktemp -d)"
    git clone --quiet "$REMOTE" "$tmp/c" >/dev/null 2>&1
    cd "$tmp/c" || exit 1
    git checkout --quiet "$b"
    if git -c user.name=gh -c user.email=gh@example.com merge --no-ff -m "$msg" "origin/$h" >/dev/null 2>&1; then
      git push --quiet origin "$b" >/dev/null 2>&1
      echo "$h $b merged" > "$D/prs/$3"
    else
      touch "$D/conflict-$3"
      echo "Pull request is not mergeable: the merge commit cannot be cleanly created" >&2
      exit 1
    fi ;;
  "release view")
    [ -f "$D/releases/$3" ] || { echo "release not found" >&2; exit 1; } ;;
  "release create")
    arg --notes "$@" > "$D/releases/$3"
    echo "https://github.com/o/r/releases/tag/$3" ;;
  "auth status") echo "Logged in to github.com" ;;
  *) echo "fake gh: unsupported: $*" >&2; exit 2 ;;
esac
