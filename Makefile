FIXTURE := /private/tmp/claude-501/-Users-neelbarmecha-LINKEDIN-PROJECTS/bc69f789-0768-424f-9c98-50960cd19268/scratchpad/sweep-fixture
CARGO_JOBS := -j 4

.PHONY: build release test fmt clippy check fixture demo demo-tui scan install clean-fixture

build:
	cargo build $(CARGO_JOBS)

release:
	cargo build --release $(CARGO_JOBS)

test:
	cargo test $(CARGO_JOBS)

fmt:
	cargo fmt

clippy:
	cargo clippy $(CARGO_JOBS) --all-targets -- -D warnings

check: fmt clippy test

## Build the disposable demo tree of 13 fake projects.
fixture:
	./scripts/make-fixture.sh $(FIXTURE)

## Build, generate the fixture, and show a non-interactive scan of it.
demo: release fixture
	./target/release/sweep scan $(FIXTURE) --all

## The same fixture, in the interactive TUI.
demo-tui: release fixture
	./target/release/sweep $(FIXTURE)

## A read-only scan of your real home directory.
scan: release
	./target/release/sweep scan ~

install:
	cargo install --path .

clean-fixture:
	rm -rf $(FIXTURE)
