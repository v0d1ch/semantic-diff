#!/usr/bin/env bash
# Usage: nix develop --command bash scripts/dev-watch.sh
# Poll for content changes; format saved Rust files, then lint the crate.
set -euo pipefail

if [[ ${1:-} == --help ]]; then
    echo 'Usage: nix develop --command bash scripts/dev-watch.sh'
    echo 'Save to format changed Rust files and run Clippy. Ctrl+C stops.'
    exit 0
fi
if (( BASH_VERSINFO[0] < 4 )); then
    echo 'Bash 4+ required. Run with: nix develop --command bash scripts/dev-watch.sh' >&2
    exit 1
fi
for tool in cargo rustfmt cksum; do
    command -v "$tool" >/dev/null || {
        echo "Missing $tool; run inside nix develop." >&2
        exit 1
    }
done
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
work_dir=$(mktemp -d)
trap 'rm -rf -- "$work_dir"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

declare -A baseline=() current=()
snapshot() {
    local path directory checksum
    current=()
    while IFS= read -r -d '' path; do
        [[ -f $path && ! -L $path ]] || continue
        checksum=$(cksum < "$path") || continue
        current["$path"]=$checksum
    done < <(
        printf '%s\0' Cargo.toml Cargo.lock build.rs rustfmt.toml .rustfmt.toml .cargo/*.toml
        for directory in src tests examples benches; do
            if [[ -d $directory && ! -L $directory ]]; then
                find "$directory" -type f -name '*.rs' -print0
            fi
        done
    )
}

remember_snapshot() {
    local path
    baseline=()
    for path in "${!current[@]}"; do
        baseline["$path"]=${current["$path"]}
    done
}

snapshot
remember_snapshot
echo 'Watching Rust files and Cargo configuration. Save to format and lint; Ctrl+C stops.'
while sleep 1; do
    snapshot
    changed=()
    for path in "${!current[@]}"; do
        if [[ ${current["$path"]} != "${baseline["$path"]:-}" ]]; then
            changed+=("$path")
        fi
    done
    for path in "${!baseline[@]}"; do
        [[ -n ${current["$path"]+exists} ]] || changed+=("$path")
    done
    (( ${#changed[@]} > 0 )) || continue
    # Remember this snapshot, not a new one after Clippy: saves during a check
    # must still be detected on the next iteration.
    remember_snapshot
    for path in "${changed[@]}"; do
        [[ $path == *.rs && -f $path && ! -L $path ]] || continue
        cp -- "$path" "$work_dir/original" || continue
        [[ $(cksum < "$work_dir/original") == "${baseline["$path"]:-}" ]] || continue
        echo "Formatting $path"
        # Edition matches Cargo.toml. Avoid following modules into unsaved files.
        if rustfmt --edition 2021 --emit stdout --config skip_children=true \
            < "$work_dir/original" > "$work_dir/formatted"; then
            # Leave edits made while rustfmt was running for the next pass.
            [[ -f $path && ! -L $path ]] || continue
            cmp -s "$work_dir/original" "$path" || continue
            if ! cmp -s "$work_dir/formatted" "$path"; then
                cat "$work_dir/formatted" > "$path"
            fi
            # Ignore our own formatting write, without ignoring later saves.
            baseline["$path"]=$(cksum < "$work_dir/formatted")
        else
            echo "rustfmt failed for $path; save a fix to retry."
        fi
    done
    echo 'Running cargo clippy --all-targets'
    if cargo clippy --all-targets; then
        echo 'Clippy finished.'
    else
        echo 'Clippy failed; save a fix to retry.'
    fi
done
