#!/usr/bin/env bash
# generic-templates: structural export, real type/const bindings, eager fallback and cache reuse.
# fields: input
MODE_FIELDS="input"
MODE_REQUIRED="input"

mode_run() {
    case_init
    local shared_home=${MIRVM_CONTRACT_HOME:-${MIRVM_HOME:-$HOME/.mirvm}}
    python3 "$REPO_ROOT/tests/lib/helpers/generic_templates.py" \
        "$MIRVM" "$RUSTC" "$shared_home" "$TMP" "$DATA_DIR/$(field_required input)"
}
