#!/bin/bash
set -u

# M5.3c fib 硬门形状：run --vm-call 'fib(32)' demo/m4/pure.rs
if [ "${2:-}" = --vm-call ] && [ "${3:-}" = 'fib(32)' ]; then
    echo 2178309
    exit 0
fi

if [ "${2:-}" = --engine ] && [ "${4:-}" = --vm-call ]; then
    case "${5:-}" in
        'fib(10)') echo 55 ;;
        'fib(25)') echo 75025 ;;
        'gcd(1071,462)') echo 21 ;;
        'gcd(17,5)') echo 1 ;;
        'sum_to(1000)') echo 500500 ;;
        'collatz_steps(27)') echo 111 ;;
        'popcount_manual(12297829382473034410)') echo 32 ;;
        'mix_signed(3,10)'|'mix_signed(10,3)') echo 13 ;;
        *)
            echo "unexpected gate0 vm-call: ${5:-}" >&2
            exit 99
            ;;
    esac
    exit 0
fi

target=${2:-}
case "$target" in
    demo/ecosystem.rs)
        case "${SCENARIO:-xfail}" in
            false_positive) exit 101 ;;
            xfail)
                echo 'TRAP: synthetic.frontier' >&2
                exit 70
                ;;
            xpass)
                echo 'ecosystem-ok'
                exit 0
                ;;
            stderr_only)
                echo 'ecosystem-ok'
                echo 'unexpected mirvm diagnostic' >&2
                exit 0
                ;;
        esac
        ;;
    demo/ffi_zlib.rs)
        echo 'ffi-ok'
        ;;
    tests/fixtures/real_ripgrep_regex.rs)
        echo 'regex-ok'
        ;;
    tests/fixtures/cargo_warning_return.rs)
        echo 'warning-return-ok'
        ;;
    corpus/c_signal.rs)
        echo 'mirvm: unsupported builtin `signal`' >&2
        exit 70
        ;;
    corpus/c_rayon.rs)
        echo 'par_sort ok = true'
        ;;
    corpus/c_volatile.rs)
        echo 'volatile aligned=0x0123456789abcdef unaligned=0x89abcdef pair=0x90a0b0c0d0e0f000:0x1020304050607080'
        ;;
    demo/jit_unwind_probe.rs)
        echo 'mirvm-jit-stats: c2i=7 c2i_slow=0 i2c=0 alloc=0 tls_ref=0' >&2
        ;;
    */proj)
        echo 'project-ok'
        exit 7
        ;;
esac
