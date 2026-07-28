# Praxec workspace tasks.
#
# `cargo install` installs ONE package per invocation, so there is no native
# "install every binary in the workspace" command. `make install` wraps it:
# it discovers every workspace package that produces a binary and installs all
# of them from source at the current version — the praxec MCP gateway, the px
# CLI, the Mission Control cockpit tools, and anything added later. One command
# keeps every praxec binary on PATH in sync.
#
# Run `make install-list` first to see exactly what will be installed.

CARGO ?= cargo
# --force overwrites an older install; --locked builds against the committed
# Cargo.lock. Override on the command line, e.g. `make install INSTALL_FLAGS=--force`.
INSTALL_FLAGS ?= --force --locked

# Emit the manifest directory of every workspace package that has a [[bin]] target.
BIN_PKG_DIRS = $(CARGO) metadata --format-version 1 --no-deps \
	| python3 -c "import sys,json,os; m=json.load(sys.stdin); print('\n'.join(sorted({os.path.dirname(p['manifest_path']) for p in m['packages'] if any('bin' in t['kind'] for t in p['targets'])})))"

.PHONY: install install-list

## install: build and install every workspace binary (praxec, px, cockpit, …).
install:
	@set -e; \
	for dir in $$($(BIN_PKG_DIRS)); do \
		echo ">> installing bins from $$dir"; \
		$(CARGO) install --path "$$dir" --bins $(INSTALL_FLAGS); \
	done; \
	echo "==> all praxec binaries installed at the current source version."

## install-list: show which binaries `make install` would install, without building.
install-list:
	@$(CARGO) metadata --format-version 1 --no-deps \
		| python3 -c "import sys,json; m=json.load(sys.stdin); rows=sorted((t['name'],p['name']) for p in m['packages'] for t in p['targets'] if 'bin' in t['kind']); [print(f'  {b:24} <- {pkg}') for b,pkg in rows]"
