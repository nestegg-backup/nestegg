#!/usr/bin/env bash
# Sourced by create.sh. Stop real updates at distinct fsync boundaries; the
# previous active partition and ESP must stay byte-identical even after unmount.
# shellcheck disable=SC2154
original_stick=$stick
for cut in filesystem.squashfs boot.cfg commit.sha256; do
    stick=$work/cut.img
    cp --reflink=auto --sparse=always "$original_stick" "$stick"
    partition_hashes b > "$work/cut-before"
    cat > "$work/tools/sync" <<'WRAPPER'
#!/usr/bin/env bash
set -euo pipefail
for path in "$@"; do
    if [[ $path == */slot_a/live/a/"$NESTEGG_TEST_CUT" ]]; then
        touch "$NESTEGG_TEST_REACHED"
        exec sleep 600
    fi
done
exec /usr/bin/sync "$@"
WRAPPER
    chmod +x "$work/tools/sync"
    rm -f "$work/reached"
    PATH="$work/tools:$PATH" NESTEGG_TEST_CUT="$cut" NESTEGG_TEST_REACHED="$work/reached" \
        "$bin" stick update "$stick" --test-image --image "$iso" > "$work/cut-$cut.log" 2>&1 &
    update_pid=$!
    for ((i=0;i<600;i++)); do [[ -f $work/reached ]] && break; kill -0 "$update_pid"; sleep .1; done
    [[ -f $work/reached ]]
    kill -TERM "$update_pid"
    if wait "$update_pid"; then echo 'ERROR: interrupted update succeeded' >&2; exit 1; fi
    update_pid=
    rm "$work/tools/sync"
    partition_hashes b > "$work/cut-after"
    cmp "$work/cut-before" "$work/cut-after"
    selected=b
    # Cleanup unmount can finish the already complete commit at this cut.
    [[ $cut != commit.sha256 ]] || selected=a
    both "cut-$cut" "$selected"
    rm "$stick"
    echo "PASS interrupted at $cut: whole previous active partition and ESP SHA-256 unchanged"
done
stick=$original_stick
