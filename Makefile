# heinzel quality gates.
# `cq` (code quality) is the gate every change must keep green.

.PHONY: cq fmt fmt-check clippy test boundary

# Full gate: formatting, lints (warnings = errors), tests.
cq: fmt-check clippy test

# Rewrite source to canonical formatting.
fmt:
	cargo fmt --all

# Fail if any file is not canonically formatted.
fmt-check:
	cargo fmt --all --check

# Lint every crate and target; deny every warning.
clippy:
	cargo clippy --workspace --all-targets -- -D warnings

# Run the test suite. It runs a stub in place of every agent CLI.
test:
	cargo test --workspace

# Where the boundary tests put the version of each CLI they passed on.
BOUNDARY_RECORD := target/boundary-record

# Run the real agent CLIs: claude and codex. Each run uses the account's
# usage, so `cq` leaves them out and lists them as ignored. Only when every
# test passes does the version of each CLI become the one its adapter was
# validated against.
boundary:
	rm -rf $(BOUNDARY_RECORD)
	BOUNDARY_RECORD=$(abspath $(BOUNDARY_RECORD)) \
		cargo test --test boundary -- --ignored --test-threads=1
	cp $(BOUNDARY_RECORD)/claude $(BOUNDARY_RECORD)/codex runtime/validated/
