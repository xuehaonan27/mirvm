# mirvm — the standard entry point for building and testing.
#
# These targets are the interface; tests/run.sh is the implementation, so the case inventory, the
# tiers and the run methods each exist in exactly one place. Every target is a thin forwarder.

SHELL := /bin/bash
RUN := ./tests/run.sh

# C = case id (make list), M = mode (make modes), ARGS = extra arguments for it.
C ?=
M ?=
ARGS ?=

.DEFAULT_GOAL := help
.PHONY: help test fast smoke gate list modes case mode validate inventory projects clean

help:
	@echo 'make test                 daily commit check (the fast tier)'
	@echo 'make smoke                fast + smoke tiers'
	@echo 'make gate                 every tier except manual: the full gate'
	@echo 'make list                 every case, with its mode and tier'
	@echo 'make modes                the available run methods'
	@echo 'make case C=<id> [ARGS="..."]'
	@echo 'make mode M=<mode> [ARGS="..."]'
	@echo 'make validate             manifest: every case names a mode that declares its fields'
	@echo 'make inventory            manifest <-> data/ cross-check'
	@echo 'make projects             fetch the data/projects submodules'
	@echo 'make clean                drop disposable caches'
	@echo
	@echo 'examples:'
	@echo '  make case C=blake3'
	@echo '  make case C=c-unwind'
	@echo '  make mode M=pair'

test fast:
	$(RUN) tier fast

smoke:
	$(RUN) tier smoke

gate:
	$(RUN) tier gate

list:
	$(RUN) list

modes:
	$(RUN) modes

case:
	@test -n "$(C)" || { echo 'make case C=<id>  (make list shows them)' >&2; exit 64; }
	$(RUN) case $(C) $(ARGS)

mode:
	@test -n "$(M)" || { echo 'make mode M=<mode>  (make modes shows them)' >&2; exit 64; }
	$(RUN) mode $(M) $(ARGS)

validate:
	$(RUN) validate

inventory:
	$(RUN) inventory

# Real projects are submodules: a checkout without them SKIPs the project cases.
projects:
	git submodule update --init --recursive

clean:
	@rm -rf target/test-state
	@if [ -x target/release/mirvm ]; then target/release/mirvm cache purge --deps --ir; fi
