.PHONY: help build test clean run-example bench lint fmt check install-tools

help: ## Show this help message
	@echo 'Usage: make [target]'
	@echo ''
	@echo 'Available targets:'
	@awk 'BEGIN {FS = ":.*##"; printf "\n"} /^[a-zA-Z_-]+:.*?##/ { printf "  %-15s %s\n", $$1, $$2 }' $(MAKEFILE_LIST)

install-tools: ## Install required development tools
	cargo install sqlx-cli --no-default-features --features sqlite
	cargo install cargo-watch
	cargo install cargo-audit
	cargo install cargo-tarpaulin

build: ## Build the project
	cargo build --all-features

test: ## Run all tests
	cargo test --all-features -- --nocapture

test-watch: ## Run tests in watch mode
	cargo watch -x "test --all-features"

run-example: ## Run the example Axum server
	cargo run --example axum_server --features axum-integration

bench: ## Run benchmarks
	cargo bench

lint: ## Run clippy linter
	cargo clippy --all-features -- -D warnings

fmt: ## Format code
	cargo fmt

check: fmt lint test ## Run all checks (format, lint, test)

clean: ## Clean build artifacts
	cargo clean
	rm -f test.db idempotency.db

coverage: ## Generate test coverage report
	cargo tarpaulin --all-features --out Html --output-dir coverage

audit: ## Check for security vulnerabilities
	cargo audit

release: check ## Build release version
	cargo build --release --all-features

docker-build: ## Build Docker image
	docker build -t idempotency-rs:latest .

docker-run: ## Run Docker container
	docker run -p 3000:3000 idempotency-rs:latest
