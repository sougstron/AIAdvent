#!/usr/bin/env bash
# Push every local/worktree branch to origin and fast-forward main onto the
# current work, then verify against the remote ref. See SKILL.md for why this
# exists: pushing a branch is not enough, the human reads main.
#
# usage: push_all.sh [--prune-merged] <path-that-must-appear-on-main>
#        e.g. push_all.sh tree/task-11
#
# --prune-merged additionally deletes remote kanban/* branches that are fully
# contained in origin/main (they carry no unique commit, so nothing is lost).
# Without it, such branches are only listed.
set -uo pipefail

prune=0
want=""
for a in "$@"; do
  case "$a" in
    --prune-merged) prune=1 ;;
    -*) echo "unknown option: $a" >&2; exit 2 ;;
    *)  want="$a" ;;
  esac
done
if [ -z "$want" ]; then
  echo "usage: $0 [--prune-merged] <path-that-must-appear-on-origin/main>   e.g. tree/task-11" >&2
  exit 2
fi

fail() { echo "FAILED: $*" >&2; exit 1; }

git rev-parse --git-dir >/dev/null 2>&1 || fail "not inside a git repository"

head_branch=$(git rev-parse --abbrev-ref HEAD)
[ "$head_branch" = "HEAD" ] && fail "detached HEAD; check out a branch first"

echo "== repo =="
echo "common dir : $(git rev-parse --git-common-dir)"
echo "branch     : $head_branch"
echo "origin     : $(git remote get-url origin 2>/dev/null || echo '<none>')"
git remote get-url origin >/dev/null 2>&1 || fail "no 'origin' remote"

echo
echo "== worktrees =="
git worktree list

if [ -n "$(git status --porcelain)" ]; then
  echo
  git status --short
  fail "working tree is dirty; commit it on '$head_branch' first (do not stash)"
fi

git fetch origin --quiet || fail "git fetch failed"

work=$(git rev-parse HEAD)
git cat-file -e "$work:$want" 2>/dev/null \
  || fail "'$want' does not exist at HEAD ($work) — wrong worktree or wrong path?"

echo
echo "== remote refs already containing $work =="
found=0
for r in $(git for-each-ref --format='%(refname)' refs/remotes/origin); do
  if git merge-base --is-ancestor "$work" "$r" 2>/dev/null; then
    echo "  contains: $r"; found=1
  fi
done
[ "$found" = 0 ] && echo "  (none — the work is nowhere on the remote yet)"

echo
echo "== pushing branches =="
# every local branch, plus whatever the worktrees hold
branches=$(
  { git for-each-ref --format='%(refname:short)' refs/heads
    git worktree list --porcelain | awk '/^branch /{sub("refs/heads/","",$2); print $2}'
  } | sort -u
)
for b in $branches; do
  # only push branches that are not strictly behind their remote counterpart
  if git rev-parse --verify --quiet "refs/remotes/origin/$b" >/dev/null \
     && git merge-base --is-ancestor "$b" "refs/remotes/origin/$b"; then
    echo "  skip  $b (already on origin, nothing new)"
    continue
  fi
  echo "  push  $b"
  git push --set-upstream origin "refs/heads/$b:refs/heads/$b" || fail "push of '$b' rejected (not forcing)"
done

echo
echo "== advancing main =="
if git merge-base --is-ancestor "$work" refs/remotes/origin/main 2>/dev/null; then
  echo "  origin/main already contains the work"
elif git merge-base --is-ancestor refs/remotes/origin/main "$work" 2>/dev/null; then
  echo "  fast-forwarding origin/main -> $work"
  git push origin "$work:refs/heads/main" || fail "fast-forward of main rejected (not forcing)"
  git fetch origin --quiet
else
  fail "main has diverged from this work; a merge is needed — ask the human, do not force"
fi

echo
echo "== verifying against origin/main =="
git fetch origin --quiet
git ls-tree "refs/remotes/origin/main" "$want" --name-only | grep -q . \
  || fail "'$want' is STILL not on origin/main — do not report success"
git ls-tree "refs/remotes/origin/main" "$want/" --name-only | sed 's/^/  /' | head -20
echo "  ok: '$want' is present on origin/main"
git log --oneline -3 refs/remotes/origin/main | sed 's/^/  /'

echo
echo "== advancing local main (the checkout the human runs things from) =="
# kanban's `land: worktree` copies files into the main checkout but never moves
# its `main`, so tools that read local history (git log -- tree/task-N) see the
# landed folders as untracked. Move local main only when that checkout's files
# are already byte-identical to origin/main: then only the ref and the index
# change, not a single file on disk.
target=$(git rev-parse refs/remotes/origin/main)
local_main=$(git rev-parse --verify --quiet refs/heads/main || true)
main_wt=$(git worktree list --porcelain | awk '/^worktree /{w=substr($0,10)} /^branch refs\/heads\/main$/{print w}')
if [ "$local_main" = "$target" ]; then
  echo "  local main already at origin/main"
elif [ -n "$local_main" ] && ! git merge-base --is-ancestor "$local_main" "$target"; then
  echo "  local main has commits origin/main lacks — not moving it (ask the human)"
elif [ -z "$main_wt" ]; then
  git update-ref -m "push_all: fast-forward to origin/main" refs/heads/main "$target" ${local_main:+"$local_main"} \
    && echo "  local main -> $target (no checkout holds it)"
else
  idx=$(mktemp)
  same=0
  if GIT_INDEX_FILE="$idx" git -C "$main_wt" read-tree "$target" \
     && GIT_INDEX_FILE="$idx" git -C "$main_wt" update-index -q --refresh >/dev/null; then
    GIT_INDEX_FILE="$idx" git -C "$main_wt" diff-files --quiet \
      && [ -z "$(GIT_INDEX_FILE="$idx" git -C "$main_wt" ls-files -o --exclude-standard)" ] \
      && same=1
  fi
  if [ "$same" = 1 ]; then
    git update-ref -m "push_all: fast-forward to origin/main (files already identical)" refs/heads/main "$target" "$local_main" \
      && git -C "$main_wt" reset -q \
      && echo "  local main in $main_wt -> $target (files untouched, status now clean)"
  else
    echo "  local main in $main_wt NOT moved: its files differ from origin/main:"
    GIT_INDEX_FILE="$idx" git -C "$main_wt" diff-files --name-status | head -10 | sed 's/^/    /'
    GIT_INDEX_FILE="$idx" git -C "$main_wt" ls-files -o --exclude-standard | head -10 | sed 's/^/    ?? /'
    echo "  (land or commit those first; nothing was changed)"
  fi
  rm -f "$idx"
fi

echo
echo "== stale remote branches =="
git fetch origin --prune --quiet
stale=""
for r in $(git for-each-ref --format='%(refname:short)' refs/remotes/origin/kanban); do
  b=${r#origin/}
  [ "$b" = "$head_branch" ] && continue
  if git merge-base --is-ancestor "$r" refs/remotes/origin/main 2>/dev/null; then
    echo "  merged into main, carries nothing unique: $b"
    stale="$stale $b"
  fi
done
if [ -z "$stale" ]; then
  echo "  (none)"
elif [ "$prune" = 1 ]; then
  echo "  deleting:$stale"
  # shellcheck disable=SC2086
  git push origin --delete $stale || fail "deleting stale branches failed"
  git fetch origin --prune --quiet
else
  echo "  not deleted (re-run with --prune-merged to delete them)"
fi

url=$(git remote get-url origin | sed -e 's#^git@github.com:#https://github.com/#' -e 's#\.git$##')
echo
echo "== links =="
echo "  work   : $url/tree/main/$want"
echo "  commit : $url/commit/$work"
echo "  branches: $url/branches"
