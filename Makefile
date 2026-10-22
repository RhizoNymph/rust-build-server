# Install rbs on this machine, put the cargo shim on PATH, and let Claude
# Code's Bash sandbox reach the build server. See docs/features/install.md.
#
#   make install REMOTE_HOST=node0   # first install on a client
#   make install                     # upgrade (setup reuses existing config)
#   make all REMOTE_HOST=node0       # install + shim PATH + sandbox + doctor

SHELL := bash
.SHELLFLAGS := -euo pipefail -c
.ONESHELL:
.DEFAULT_GOAL := help

ROLE        ?= client
REMOTE_HOST ?=
BIN_DIR     ?= $(HOME)/.local/bin
SHIM_DIR    ?= $(HOME)/.local/share/rbs/shim
RBS         := $(BIN_DIR)/rbs

# Shell rc that Claude Code (and interactive shells) source for PATH.
LOGIN_SHELL := $(notdir $(shell getent passwd "$$(id -un)" | cut -d: -f7))
SHELL_RC ?= $(if $(filter zsh,$(LOGIN_SHELL)),$(HOME)/.zshrc,$(HOME)/.bashrc)

CLAUDE_SETTINGS ?= $(HOME)/.claude/settings.json
CLAUDE_MD       ?= $(HOME)/.claude/CLAUDE.md

# Cargo subcommands the shim ships to a backend. `cargo run` is left out on
# purpose: it executes the built program, which should stay sandboxed.
SANDBOX_CARGO := build check test clippy bench doc nextest
# Read-only helpers agents chain with cargo (`cd x && cargo build | tail`).
# Every part of a compound command must match for the exclusion to apply.
SANDBOX_HELPERS := cd tail head grep

SHIM_BEGIN := \# >>> rbs shim >>>
SHIM_END   := \# <<< rbs shim <<<
GUIDE_BEGIN := <!-- >>> rbs agent guidance >>> -->
GUIDE_END   := <!-- <<< rbs agent guidance <<< -->
GUIDE_FILE  := deploy/claude/agent-guidance.md

.PHONY: help all deps build install restart shim-path sandbox sandbox-show \
        agent-guidance doctor fleet fleet-plan

help: ## Show targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-16s %s\n", $$1, $$2}'

all: install shim-path sandbox doctor ## install + shim PATH + sandbox + doctor

deps: ## Check required tools are installed
	@missing=0
	for t in cargo rustup rsync ssh jq systemctl kache; do
	  if ! command -v $$t >/dev/null; then echo "missing: $$t"; missing=1; fi
	done
	[ $$missing -eq 0 ] && echo "deps ok"

build: ## Build the release binary locally (bypasses the shim)
	RBS_MODE=plain cargo build --release --locked -p rbs

install: deps build ## Install the binary atomically, run setup (relinks shim), restart the server
	install -d "$(BIN_DIR)"
	# Stage then rename: a running rbs server keeps the old inode mapped,
	# so copying over it in place fails with "Text file busy".
	install -m755 target/release/rbs "$(RBS).new"
	mv -f "$(RBS).new" "$(RBS)"
	"$(RBS)" setup --role $(ROLE) $(if $(REMOTE_HOST),--remote-host $(REMOTE_HOST))
	if systemctl --user is-enabled --quiet rbs-server 2>/dev/null; then
	  systemctl --user restart rbs-server && echo "rbs-server restarted"
	else
	  echo "rbs-server unit not enabled; skipping restart"
	fi

restart: ## Restart the local rbs server so it runs the new binary
	@if systemctl --user is-enabled --quiet rbs-server 2>/dev/null; then
	  systemctl --user restart rbs-server && echo "rbs-server restarted"
	else
	  echo "rbs-server unit not enabled; skipping restart"
	fi

shim-path: ## Put the cargo shim first on PATH in your shell rc
	@if grep -qF '.local/share/rbs/shim' "$(SHELL_RC)" 2>/dev/null; then
	  echo "$(SHELL_RC) already puts the shim on PATH"
	else
	  printf '%s\n' '$(SHIM_BEGIN)' 'export PATH="$$HOME/.local/share/rbs/shim:$$PATH"' '$(SHIM_END)' >> "$(SHELL_RC)"
	  echo "added shim PATH block to $(SHELL_RC); open a new shell (or restart Claude Code) to pick it up"
	fi

sandbox: ## Merge rbs's commands into Claude Code's sandbox.excludedCommands
	@f="$(CLAUDE_SETTINGS)"
	[ -f "$$f" ] || { install -d "$$(dirname "$$f")"; echo '{}' > "$$f"; }
	cp "$$f" "$$f.bak"
	# `cargo build` alone and `cargo build <args>` are separate patterns.
	jq --arg cargo '$(SANDBOX_CARGO)' --arg helpers '$(SANDBOX_HELPERS)' '
	  def words: split(" ") | map(select(. != ""));
	  ([$$cargo | words[] | ("cargo " + .), ("cargo " + . + " *")] +
	   [$$helpers | words[] | . + " *"]) as $$new
	  | .sandbox.excludedCommands = ((.sandbox.excludedCommands // []) + $$new | unique)
	' "$$f" > "$$f.tmp"
	mv -f "$$f.tmp" "$$f"
	echo "updated $$f (backup at $$f.bak)"
	jq -r '.sandbox.excludedCommands // [] | .[]' "$$f"

sandbox-show: ## Print the current sandbox.excludedCommands
	@jq -r '.sandbox.excludedCommands // [] | .[]' "$(CLAUDE_SETTINGS)"

agent-guidance: ## Optional: add a cargo-usage note for agents to ~/.claude/CLAUDE.md
	@if grep -qF '$(GUIDE_BEGIN)' "$(CLAUDE_MD)" 2>/dev/null; then
	  echo "$(CLAUDE_MD) already has the rbs guidance block"
	else
	  { echo; echo '$(GUIDE_BEGIN)'; cat "$(GUIDE_FILE)"; echo '$(GUIDE_END)'; } >> "$(CLAUDE_MD)"
	  echo "appended rbs guidance to $(CLAUDE_MD)"
	fi

doctor: ## Verify the local install and the remote server
	"$(RBS)" doctor --remote

fleet-plan: ## Show what `rbs bootstrap` would do to every host
	"$(RBS)" bootstrap --dry-run

fleet: install ## Install here, then upgrade every host in [bootstrap]
	"$(RBS)" bootstrap
