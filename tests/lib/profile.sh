#!/usr/bin/env bash
# profile: one product run under `perf record`, reported as the symbols it spent time in and the
# categories the engine's own work falls into. This is an instrument, not a verdict: nothing here
# compares output or fails anything, and a run whose guest output is wrong still yields a profile.
#
# The report needs the product's own map. `MIRVM_PERF_MAP=1` makes the JIT publish
# `/tmp/perf-<pid>.map` when it starts and write its ranges on the exit path, so `perf report` can
# name compiled guest bodies, the interpreter and the helpers rather than one anonymous blob. A
# report with no `[JIT]` line means that map was missing, empty or unreadable; the usual causes are a
# run with the JIT off, a program that published no code, and a `perf` without permission to read
# another process's map.
#
#   tests/run.sh profile <program.rs|name-under-tests/data/programs> [product opts] [-- guest args]
#
#   tests/run.sh profile c_flate2.rs
#   tests/run.sh profile vmcall_pure.rs --vm-call 'fib(32)'
#
# Environment: MIRVM_PROFILE_DATA and MIRVM_PROFILE_OUT name the raw capture and the text report
# (both default under /tmp and are kept), MIRVM_PROFILE_FREQ the sampling frequency.

profile_main() { # <program> [-- guest args...]
    local program=${1:-}
    [ -n "$program" ] || {
        echo "usage: tests/run.sh profile <program.rs|name> [-- guest args...]" >&2
        return 64
    }
    shift
    [ -e "$program" ] || program=$DATA_DIR/programs/$program
    [ -e "$program" ] || {
        echo "profile: no such program: $program" >&2
        return 64
    }
    command -v perf >/dev/null 2>&1 || {
        echo "profile: no perf on PATH; this instrument is Linux-only" >&2
        return 69
    }
    ensure_product || return 69

    local data=${MIRVM_PROFILE_DATA:-/tmp/mirvm-profile-$$.data}
    local out=${MIRVM_PROFILE_OUT:-/tmp/mirvm-profile-$$.txt}
    local freq=${MIRVM_PROFILE_FREQ:-2999}

    # Arguments before a `--` are the product's own (they precede the program, as the corpus mode
    # passes its `args`), arguments after it belong to the guest.
    local -a pre=() guest=() cmd=()
    local sep=0 arg
    for arg in "$@"; do
        if [ "$sep" = 1 ]; then guest+=("$arg")
        elif [ "$arg" = -- ]; then sep=1
        else pre+=("$arg"); fi
    done
    cmd=("$MIRVM" run ${pre[@]+"${pre[@]}"} "$program")
    [ ${#guest[@]} -gt 0 ] && cmd+=(-- "${guest[@]}")

    echo "== perf record -F $freq -e cycles:u -o $data =="
    MIRVM_PERF_MAP=1 perf record -F "$freq" -e cycles:u -o "$data" -- "${cmd[@]}" ||
        {
            echo "profile: perf record failed" >&2
            return 69
        }
    perf report -i "$data" --stdio --no-children >"$out" 2>/dev/null || {
        echo "profile: perf report failed" >&2
        return 69
    }

    echo
    echo "== the run's heaviest symbols =="
    grep -E '^ +[0-9]+\.[0-9]+%' "$out" | head -15

    echo
    echo "== by category =="
    awk '/^[[:space:]]+[0-9]+\.[0-9]+%/ {
             p = $1; sub(/%/, "", p); line = $0
             if (line ~ /\[JIT\]/) c["compiled guest bodies"] += p
             else if (line ~ /poll_signals/) c["safe point"] += p
             else if (line ~ /stack_guard/) c["stack guard"] += p
             else if (line ~ /call_guest/) c["guest call dispatch"] += p
             else if (line ~ /interp_|runblocks|exec_stmt/) c["interpreter"] += p
             else if (line ~ /rustc|cargo/) c["guest build"] += p
             else if (line ~ /blake3|siphash|Hasher|verify|postcard|memcmp/) c["image load and verify"] += p
             else if (line ~ /memmove|memset|memcpy/) c["memory moves"] += p
             else if (line ~ /malloc|free|calloc|realloc/) c["allocator"] += p
             else c["everything else"] += p
         }
         END { for (k in c) printf "%6.2f%%  %s\n", c[k], k }' "$out" | sort -rn

    if ! grep -q '\[JIT\]' "$out"; then
        echo
        echo "note: no compiled guest body was named. MIRVM_PERF_MAP=1 installs /tmp/perf-<pid>.map"
        echo "      when the JIT starts and writes it on the exit path; a JIT-off run, a program"
        echo "      that compiled nothing, or an unreadable map all look like this."
    fi

    echo
    echo "full report: $out"
    echo "raw capture: $data"
}
