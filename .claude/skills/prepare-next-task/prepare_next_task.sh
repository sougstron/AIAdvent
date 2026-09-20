#!/usr/bin/env bash

set -Eeuo pipefail

die() {
    printf 'prepare-next-task: %s\n' "$*" >&2
    exit 1
}

if (( $# > 1 )); then
    die 'expected zero or one positive integer argument'
fi
if (( $# == 1 )) && [[ ! $1 =~ ^[1-9][0-9]*$ ]]; then
    die 'task number must be a positive integer'
fi

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) \
    || die 'run this helper from inside a git repository'
current_task_file="$repo_root/docs/CurrentTask.md"
agents_file="$repo_root/AGENTS.md"
gitignore_file="$repo_root/.gitignore"

for required_file in "$current_task_file" "$agents_file" "$gitignore_file"; do
    [[ -f $required_file ]] || die "required file is missing: ${required_file#"$repo_root"/}"
done

active_pattern='^- \*\*Task ([1-9][0-9]*)\*\* — путь: `tree/task-([1-9][0-9]*)/`$'
mapfile -t active_lines < <(grep -E '^- \*\*Task [1-9][0-9]*\*\* — путь: `tree/task-[1-9][0-9]*/`$' "$current_task_file" || true)
(( ${#active_lines[@]} == 1 )) \
    || die 'docs/CurrentTask.md must contain exactly one active task declaration'
[[ ${active_lines[0]} =~ $active_pattern ]] \
    || die 'could not parse the active task declaration'
active=${BASH_REMATCH[1]}
active_path_number=${BASH_REMATCH[2]}
[[ $active == "$active_path_number" ]] \
    || die 'active task number and path disagree in docs/CurrentTask.md'

highest=0
for candidate_path in "$repo_root"/tree/task-*; do
    [[ -d $candidate_path ]] || continue
    candidate=${candidate_path##*/task-}
    [[ $candidate =~ ^[1-9][0-9]*$ ]] || continue
    if (( candidate > highest )); then
        highest=$candidate
    fi
done
(( highest > 0 )) || die 'no numeric tree/task-N snapshot directories found'
(( active == highest )) \
    || die "active task $active is not the highest snapshot ($highest)"

if (( $# == 0 )); then
    new=$((highest + 1))
else
    new=$1
    (( new > highest )) \
        || die "explicit task number must be greater than the latest snapshot ($highest)"
fi

source_snapshot="$repo_root/tree/task-$active"
destination="$repo_root/tree/task-$new"
[[ -d $source_snapshot ]] || die "active snapshot is missing: tree/task-$active"
[[ ! -e $destination && ! -L $destination ]] \
    || die "destination already exists: tree/task-$new"

old_active_line="- **Task $active** — путь: \`tree/task-$active/\`"
new_active_line="- **Task $new** — путь: \`tree/task-$new/\`"
old_instruction="- Все изменения кода вносим только внутри \`tree/task-$active/\`."
new_instruction="- Все изменения кода вносим только внутри \`tree/task-$new/\`."
old_agent_marker="**The active snapshot is \`tree/task-$active/\`.**"
new_agent_marker="**The active snapshot is \`tree/task-$new/\`.**"
archive_bullet="- \`tree/task-$active/\` — предыдущее активное состояние; из его дословной копии вырос \`tree/task-$new/\`. Зафиксировано, не трогать."

[[ $(grep -Fxc -- "$old_active_line" "$current_task_file") == 1 ]] \
    || die 'the active declaration changed while it was being checked'
[[ $(grep -Fxc -- "$old_instruction" "$current_task_file") == 1 ]] \
    || die 'the current-code instruction is missing or ambiguous'
[[ $(grep -Fxc -- '## Правила' "$current_task_file") == 1 ]] \
    || die 'docs/CurrentTask.md must contain exactly one "## Правила" heading'
[[ $(awk -v marker="$old_agent_marker" 'index($0, marker) { count++ } END { print count + 0 }' "$agents_file") == 1 ]] \
    || die 'AGENTS.md must contain exactly one active snapshot sentence'

archive_present=0
if awk -v prefix="- \`tree/task-$active/\`" '
    /^## Архив состояний$/ { in_archive = 1; next }
    /^## Правила$/ { in_archive = 0 }
    in_archive && index($0, prefix) == 1 { found = 1 }
    END { exit found ? 0 : 1 }
' "$current_task_file"; then
    archive_present=1
fi

allowlist=(
    "!tree/task-$new/target/"
    "tree/task-$new/target/*"
    "!tree/task-$new/target/release/"
    "tree/task-$new/target/release/*"
    "!tree/task-$new/target/release/ask"
)
allowlist_lines=0
for line in "${allowlist[@]}"; do
    count=$(grep -Fxc -- "$line" "$gitignore_file" || true)
    (( count <= 1 )) || die ".gitignore contains a duplicate task-$new allowlist line"
    allowlist_lines=$((allowlist_lines + count))
done
(( allowlist_lines == 0 || allowlist_lines == 5 )) \
    || die ".gitignore contains a partial task-$new allowlist"

work_dir=$(mktemp -d "$repo_root/.prepare-next-task.XXXXXX") \
    || die 'could not create a temporary work directory'
stage="$repo_root/tree/.task-$new.prepare.$$"
committed=0
destination_published=0
current_replaced=0
agents_replaced=0
gitignore_replaced=0

cleanup() {
    status=$?
    set +e
    if (( ! committed )); then
        if (( gitignore_replaced )); then
            mv -T -- "$work_dir/gitignore.old" "$gitignore_file"
        fi
        if (( agents_replaced )); then
            mv -T -- "$work_dir/AGENTS.old" "$agents_file"
        fi
        if (( current_replaced )); then
            mv -T -- "$work_dir/CurrentTask.old" "$current_task_file"
        fi
        if (( destination_published )); then
            rm -rf -- "$destination"
        fi
    fi
    [[ ! -e $stage && ! -L $stage ]] || rm -rf -- "$stage"
    rm -rf -- "$work_dir"
    exit "$status"
}
trap cleanup EXIT HUP INT TERM

cp -a -- "$current_task_file" "$work_dir/CurrentTask.old"
cp -a -- "$agents_file" "$work_dir/AGENTS.old"
cp -a -- "$gitignore_file" "$work_dir/gitignore.old"

awk \
    -v old_active="$old_active_line" \
    -v new_active="$new_active_line" \
    -v old_instruction="$old_instruction" \
    -v new_instruction="$new_instruction" \
    -v archive_bullet="$archive_bullet" \
    -v add_archive="$((1 - archive_present))" '
    $0 == old_active { print new_active; next }
    $0 == old_instruction { print new_instruction; next }
    $0 == "## Правила" && add_archive {
        print archive_bullet
        print ""
    }
    { print }
' "$current_task_file" > "$work_dir/CurrentTask.new"

awk -v old="$old_agent_marker" -v new="$new_agent_marker" '
    {
        position = index($0, old)
        if (position) {
            $0 = substr($0, 1, position - 1) new substr($0, position + length(old))
        }
        print
    }
' "$agents_file" > "$work_dir/AGENTS.new"

cp -a -- "$gitignore_file" "$work_dir/gitignore.new"
if (( allowlist_lines == 0 )); then
    {
        printf '\n'
        printf '%s\n' "${allowlist[@]}"
    } >> "$work_dir/gitignore.new"
fi

chmod --reference="$current_task_file" "$work_dir/CurrentTask.new"
chmod --reference="$agents_file" "$work_dir/AGENTS.new"
chmod --reference="$gitignore_file" "$work_dir/gitignore.new"

[[ ! -e $stage && ! -L $stage ]] || die "temporary staging path already exists: ${stage#"$repo_root"/}"
cp -a -- "$source_snapshot" "$stage"

if mv -T --no-clobber -- "$stage" "$destination" \
    && [[ ! -e $stage && ! -L $stage && -d $destination ]]; then
    destination_published=1
else
    die "could not publish tree/task-$new without overwriting"
fi

current_replaced=1
mv -T -- "$work_dir/CurrentTask.new" "$current_task_file"
agents_replaced=1
mv -T -- "$work_dir/AGENTS.new" "$agents_file"
gitignore_replaced=1
mv -T -- "$work_dir/gitignore.new" "$gitignore_file"

committed=1
printf 'Prepared tree/task-%s from tree/task-%s and made it current.\n' "$new" "$active"
