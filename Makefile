.PHONY: build install run clean

build:
	cargo build --release

install: build
	mkdir -p ~/.local/bin
	install -m 755 target/release/clear-email-interface ~/.local/bin/clear-email-interface

run:
	cargo run

clean:
	cargo clean
