# mirvm — standard entry point for building and testing.
#
# These targets are the interface; tests/run.sh is the implementation, so the tier logic exists in
# exactly one place. Every target is a thin forwarder, and no target needs GNU make extensions.
#
#   make test          daily commit check (same as fast)
#   make smoke         test + small real workloads + runtime semantics
#   make gate          the full final gate: strict corpus, dependency image, performance limits
#   make list          every suite id, discovered from tests/suites/
#   make suite S=<id> [ARGS="..."]
#   make projects      fetch the tests/projects/ submodules
#   make clean         drop disposable caches (keeps the shared dependency store)

SHELL := /bin/bash
RUN := ./tests/run.sh

# S = suite id (make list), ARGS = suite arguments, passed through verbatim.
S ?=
ARGS ?=

.DEFAULT_GOAL := help
.PHONY: help test fast smoke gate list suite projects clean

help:
	@echo 'make test                 daily commit check (same as fast)'
	@echo 'make smoke                test + small real workloads + runtime semantics'
	@echo 'make gate                 the full final gate: corpus, deps image, perf limits'
	@echo 'make list                 list every suite id'
	@echo 'make suite S=<id> [ARGS="..."]   run one suite'
	@echo 'make projects             fetch the tests/projects/ submodules'
	@echo 'make clean                drop disposable caches'
	@echo
	@echo 'examples:'
	@echo '  make suite S=corpus.run ARGS="--tier smoke"'
	@echo '  make suite S=runtime.semantics ARGS=unwind'
	@echo '  make suite S=corpus.contract ARGS=hexyl'

test fast:
	$(RUN) fast

smoke:
	$(RUN) smoke

gate:
	$(RUN) gate

list:
	$(RUN) list

suite:
	@test -n "$(S)" || { echo 'make suite S=<suite-id>  (make list shows them)' >&2; exit 64; }
	$(RUN) suite $(S) $(ARGS)

# Real projects are submodules: a checkout without them SKIPs the mode=diff corpus entries.
projects:
	git submodule update --init --recursive

clean:
	@rm -rf target/test-state
	@if [ -x target/release/mirvm ]; then target/release/mirvm cache purge --deps --ir; fi
