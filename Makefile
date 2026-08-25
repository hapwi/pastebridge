.PHONY: build release test install service uninstall

build:
	cargo build

release:
	cargo build --release --locked

test:
	cargo test --locked

install:
	cargo install --path . --locked --force

service: install
	pastebridge install-service

uninstall:
	-pastebridge uninstall-service
	-cargo uninstall pastebridge
