#!/usr/bin/env bash

set -Eeuo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
helper="$script_dir/prepare_next_task.sh"
fixtures=$(mktemp -d)
trap 'rm -rf -- "$fixtures"' EXIT

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

assert_line() {
    local file=$1
    local expected=$2
    [[ $(grep -Fxc -- "$expected" "$file" || true) == 1 ]] \
        || fail "expected one line in ${file}: ${expected}"
}

create_fixture() {
    local repo=$1
    local active=$2
    local archive_active=$3
    shift 3

    git init -q "$repo"
    mkdir -p "$repo/docs" "$repo/tree"
    local task
    for task in "$@"; do
        mkdir -p "$repo/tree/task-$task"
        printf 'snapshot %s\n' "$task" > "$repo/tree/task-$task/plain.txt"
    done

    mkdir -p "$repo/tree/task-$active/target/release"
    printf 'source payload\n' > "$repo/tree/task-$active/data.txt"
    printf 'hidden payload\n' > "$repo/tree/task-$active/.hidden"
    printf 'release artifact\n' > "$repo/tree/task-$active/target/release/ask"
    chmod 751 "$repo/tree/task-$active/target/release/ask"
    ln -s data.txt "$repo/tree/task-$active/data-link"

    {
        printf '# CurrentTask — текущая глобальная задача\n\n'
        printf '## Текущая задача\n\n'
        printf '%s\n' "- **Task $active** — путь: \`tree/task-$active/\`"
        printf '%s\n' '- Что это: fixture; ссылки внутри описания не меняем.'
        printf '%s\n' "- Справка: \`tree/task-$active/README.md\`."
        printf '%s\n' "- Все изменения кода вносим только внутри \`tree/task-$active/\`."
        printf '\n## Архив состояний\n\n'
        printf '%s\n' '- `tree/task-1/` — старое состояние. Зафиксировано, не трогать.'
        if [[ $archive_active == yes ]]; then
            printf '%s\n' "- \`tree/task-$active/\` — уже описанное архивное состояние."
        fi
        printf '\n## Правила\n\n'
        printf '%s\n' '1. Fixture rule.'
    } > "$repo/docs/CurrentTask.md"

    {
        printf '# AGENTS.md\n\n'
        printf '%s\n' "**The active snapshot is \`tree/task-$active/\`.** It is the fixture snapshot."
        printf '%s\n' 'Leave this text unchanged.'
    } > "$repo/AGENTS.md"

    {
        printf 'target/\n'
        printf '%s\n' "!tree/task-$active/target/"
        printf '%s\n' "tree/task-$active/target/*"
        printf '%s\n' "!tree/task-$active/target/release/"
        printf '%s\n' "tree/task-$active/target/release/*"
        printf '%s\n' "!tree/task-$active/target/release/ask"
    } > "$repo/.gitignore"
}

manifest() {
    local repo=$1
    (
        cd "$repo"
        find . -path './.git' -prune -o -printf '%y\t%m\t%p\t%l\n' | LC_ALL=C sort
        find . -path './.git' -prune -o -type f -print0 \
            | LC_ALL=C sort -z \
            | xargs -0 -r sha256sum
    )
}

assert_refused_unchanged() {
    local repo=$1
    shift
    local before after
    before=$(manifest "$repo")
    if (cd "$repo" && "$helper" "$@") >/dev/null 2>&1; then
        fail "helper unexpectedly accepted arguments: $*"
    fi
    after=$(manifest "$repo")
    [[ $before == "$after" ]] || fail "refused invocation mutated fixture: $*"
}

assert_success() {
    local repo=$1
    local old=$2
    local new=$3
    local destination="$repo/tree/task-$new"

    [[ -d $destination ]] || fail "tree/task-$new was not created"
    cmp "$repo/tree/task-$old/data.txt" "$destination/data.txt" >/dev/null \
        || fail 'regular file content was not preserved'
    cmp "$repo/tree/task-$old/.hidden" "$destination/.hidden" >/dev/null \
        || fail 'hidden file was not preserved'
    cmp "$repo/tree/task-$old/target/release/ask" "$destination/target/release/ask" >/dev/null \
        || fail 'target artifact was not preserved'
    [[ -L $destination/data-link ]] || fail 'symlink became a regular file'
    [[ $(readlink "$destination/data-link") == data.txt ]] || fail 'symlink target changed'
    [[ $(stat -c '%a' "$destination/target/release/ask") == 751 ]] \
        || fail 'executable mode was not preserved'

    assert_line "$repo/docs/CurrentTask.md" "- **Task $new** — путь: \`tree/task-$new/\`"
    assert_line "$repo/docs/CurrentTask.md" "- Все изменения кода вносим только внутри \`tree/task-$new/\`."
    assert_line "$repo/docs/CurrentTask.md" "- Справка: \`tree/task-$old/README.md\`."
    assert_line "$repo/AGENTS.md" "**The active snapshot is \`tree/task-$new/\`.** It is the fixture snapshot."
    assert_line "$repo/.gitignore" "!tree/task-$new/target/"
    assert_line "$repo/.gitignore" "tree/task-$new/target/*"
    assert_line "$repo/.gitignore" "!tree/task-$new/target/release/"
    assert_line "$repo/.gitignore" "tree/task-$new/target/release/*"
    assert_line "$repo/.gitignore" "!tree/task-$new/target/release/ask"
}

test_default_mode() {
    local repo="$fixtures/default"
    create_fixture "$repo" 3 no 1 2 3
    (cd "$repo" && "$helper") >/dev/null
    assert_success "$repo" 3 4
    assert_line "$repo/docs/CurrentTask.md" '- `tree/task-3/` — предыдущее активное состояние; из его дословной копии вырос `tree/task-4/`. Зафиксировано, не трогать.'
}

test_explicit_gap() {
    local repo="$fixtures/explicit-gap"
    create_fixture "$repo" 3 yes 1 2 3
    (cd "$repo" && "$helper" 7) >/dev/null
    assert_success "$repo" 3 7
    [[ ! -e $repo/tree/task-4 && ! -e $repo/tree/task-5 && ! -e $repo/tree/task-6 ]] \
        || fail 'explicit gap created intermediate snapshots'
    assert_line "$repo/docs/CurrentTask.md" '- `tree/task-3/` — уже описанное архивное состояние.'
}

test_bad_arguments() {
    local repo="$fixtures/bad-arguments"
    create_fixture "$repo" 3 no 1 2 3
    assert_refused_unchanged "$repo" 0
    assert_refused_unchanged "$repo" -1
    assert_refused_unchanged "$repo" abc
    assert_refused_unchanged "$repo" 3
    assert_refused_unchanged "$repo" 4 5
}

test_existing_destination() {
    local repo="$fixtures/existing"
    create_fixture "$repo" 3 no 1 2 3
    printf 'do not overwrite\n' > "$repo/tree/task-4"
    assert_refused_unchanged "$repo" 4
}

test_inconsistent_active() {
    local repo="$fixtures/inconsistent"
    create_fixture "$repo" 2 no 1 2 3
    assert_refused_unchanged "$repo"
}

test_default_mode
test_explicit_gap
test_bad_arguments
test_existing_destination
test_inconsistent_active

printf 'PASS: prepare_next_task.sh fixture tests\n'
