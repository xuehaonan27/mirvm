#!/usr/bin/env bash
# base-image: canonical std closure, single-session publication and cross-program reuse.
# fields: none
MODE_FIELDS=""

mode_run() {
    case_init
    local shared_home=${MIRVM_CONTRACT_HOME:-${MIRVM_HOME:-$HOME/.mirvm}}
    python3 "$REPO_ROOT/tests/lib/helpers/base_image.py" "$MIRVM" "$shared_home" "$TMP"
}
