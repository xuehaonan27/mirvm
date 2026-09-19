#!/usr/bin/env bash
# metering: report the cache and disk picture and enforce the resource budgets. It reports rather
# than judges, so it always passes; the budgets themselves (disk floor, target budget) are the
# harness's environment variables, not this file's constants.
# fields: none

MODE_FIELDS=""

mode_run() {
    case_init
    echo "== Resource metering =="
    cache_snapshot "metering"
    target_budget_check
    local home_dir=${MIRVM_HOME:-$HOME/.mirvm}
    echo "[disk] $(df -h "$home_dir" 2>/dev/null | awk 'NR==2{print "available "$4" / total "$2" ("$5" used)"}')"
    ok "resource metering reported"
}
