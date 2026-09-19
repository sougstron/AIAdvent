---
name: push-to-git
description: Push this project's work to GitHub so it is actually visible there. Collects every local branch and every git-worktree branch, fast-forwards `main` onto the newest landed work, pushes all of it to `origin`, and prints GitHub URLs to verify. Triggers on - запушь в гит, push to git, запушь таску N, push the task, залей на гитхаб, push all branches, "агент сказал что запушил, а в гите нет".
---

# Push to git (this project)

## Why this skill exists

This project has a layout that makes "I pushed it" quietly false:

- The folder the human and the agents actually work in —
  `~/.local/share/kanban4ai/projects/homework` — **is not a git repository.**
  It is a plain folder that kanban snapshots.
- The real repository is `~/Github/tests/homework`. Its `main` is frozen far in
  the past (it was left at a TASK-023-era commit while `origin/main` ran 132
  commits ahead), because nothing in the kanban flow ever advances it.
- Each task runs in a worktree on a branch `kanban/TASK-NNN`, cut from a
  *snapshot taken before the task started*. An agent that pushes "its branch"
  can therefore push a tip that predates its own work, and the commit that
  actually lands the work (`kanban: land TASK-NNN`) can end up on **no remote
  ref at all**.
- The human looks at GitHub's default branch. If `main` was not moved, the work
  is invisible there no matter how many branches were pushed.

That is exactly what happened with TASK-067 / `tree/task-11`: the branch
`kanban/TASK-067` was pushed at `f5c4607` (the *before* snapshot), the landing
commit `e8cc321` was on no remote ref, and `origin/main` still pointed at
`kanban: live snapshot before TASK-063` — so `tree/` on GitHub contained only
`task-4 … task-10`.

**Rule: pushing a branch is not done. `main` must carry the work, and you must
verify it by listing the files on the remote ref, not by trusting push output.**

## Procedure

Run every command from the current worktree. Never `cd` to another checkout;
use `git -C <path>` if you must address one.

### 1. Locate every checkout and branch

```sh
git rev-parse --git-common-dir      # the real repo
git worktree list                   # every checkout + the branch it holds
git branch -a                       # local + remote-tracking branches
git remote -v                       # where origin points
```

If the target work lives in a folder that is not a repo (`fatal: not a git
repository`), stop and say so — it cannot be pushed from there. Find the
worktree that holds it instead.

### 2. Commit anything uncommitted on the current branch

```sh
git status --short
```

Nothing may be left in the working tree. Commit it on the current branch
(never `git stash` — the stash stack is shared with other worktrees and other
sessions).

### 3. Find the commit that actually carries the work

Do not assume the branch tip is it. Name a concrete path the work must contain
and search for the commit that introduced it:

```sh
git log --oneline --all -- <path>          # e.g. tree/task-11
git rev-list --count origin/main..HEAD     # how much is unpushed
```

Then check whether any remote ref already contains it:

```sh
for r in $(git for-each-ref --format='%(refname)' refs/remotes/origin); do
  git merge-base --is-ancestor <commit> $r 2>/dev/null && echo "contains: $r"
done
```

No output means the work is nowhere on the remote — regardless of what a
previous agent reported.

### 4. Push every local branch

```sh
git push --set-upstream origin <branch>
```

for the current branch, and for each branch listed by `git worktree list` /
`git branch`. Never force-push: if a push is rejected, fetch and report,
do not rewrite remote history.

### 5. Advance `main` — this is the step that makes it visible

Check the chain first:

```sh
git merge-base --is-ancestor main origin/main   # local main behind remote?
git merge-base --is-ancestor origin/main HEAD   # can main fast-forward to work?
```

If both hold, the history is linear and `main` can be fast-forwarded to the
work commit **without checking out `main`** (its checkout is a different
worktree — do not touch it):

```sh
git fetch origin
git push origin <work-commit>:refs/heads/main   # fast-forward only, no --force
git fetch origin                                # refresh the tracking ref
```

If the fast-forward is rejected, `main` has diverged: report it and ask the
human whether to merge. Do not force.

### 6. Verify against the remote, not against the push output

```sh
git ls-tree origin/main <dir>/       # the work's folder must be listed
git log --oneline -3 origin/main
```

Only after the file listing on `origin/main` shows the work is the push done.

### 7. Report links

Print URLs the human can click, derived from `git remote get-url origin`:

- work folder: `https://github.com/<owner>/<repo>/tree/main/<path>`
- commit:      `https://github.com/<owner>/<repo>/commit/<sha>`
- branches:    `https://github.com/<owner>/<repo>/branches`

## Helper

`push_all.sh` in this folder does steps 1–6 in one go and refuses to claim
success unless the verification in step 6 passes.

```sh
.claude/skills/push-to-git/push_all.sh <path-that-must-appear-on-main>
```

Example: `.claude/skills/push-to-git/push_all.sh tree/task-11`

It never force-pushes and never checks out another worktree's branch.
