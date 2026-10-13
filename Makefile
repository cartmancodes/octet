.PHONY: help mobile-check mobile-contract mobile-export rust-check rust-build tui tui-demo gate

help:
	@grep -E '^[a-zA-Z_-]+:.*?##' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?##"}{printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}'

rust-check: ## Check formatting, lint, docs and tests for the Rust workspace
	scripts/rust-env.sh cargo fmt --all -- --check
	scripts/rust-env.sh cargo clippy --locked --workspace --all-targets -- -D warnings
	RUSTDOCFLAGS="-D warnings" scripts/rust-env.sh cargo doc --locked --workspace --no-deps
	scripts/rust-env.sh cargo test --locked --workspace

mobile-check: ## Type-check, lint and test the mobile app (needs Node 22)
	cd apps/mobile && npm ci && npm run typecheck && npm run lint && npm test -- --ci

mobile-contract: ## Run the app's hub client against the real octet hub (build it first: cargo build -p octet)
	cd apps/mobile && npm run contract

mobile-export: ## Bundle the mobile app for iOS and Android (no native build)
	cd apps/mobile && npm run export:check

rust-build: ## Build the optimized terminal UI
	scripts/rust-env.sh cargo build --release --locked -p octet

tui: ## Open the terminal UI with Codex
	scripts/rust-env.sh cargo run --locked -p octet -- --engine codex

tui-demo: ## Preview the terminal UI offline
	scripts/rust-env.sh cargo run --locked -p octet -- --engine demo

ENGINE ?= claude
SCENARIO ?= initialize
gate: ## Run one protocol-gate scenario against a real CLI (ENGINE=claude SCENARIO=simple)
	scripts/rust-env.sh cargo run --locked -p octet-gate --bin protocol-gate -- --engine $(ENGINE) --scenario $(SCENARIO)
