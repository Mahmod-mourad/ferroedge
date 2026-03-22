.PHONY: run-control run-edge build docker-up docker-down test fmt lint

run-control:
	cargo run -p control-plane

run-edge:
	NODE_ID=node-1 cargo run -p edge-node

build:
	cargo build --release

docker-up:
	docker-compose up --build

docker-down:
	docker-compose down

test:
	cargo test --workspace

fmt:
	cargo fmt --all

lint:
	cargo clippy --workspace -- -D warnings
