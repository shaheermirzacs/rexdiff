.PHONY: build install

build:
	cargo build --release

# Links ~/.local/bin/rexdiff to the project binary, so `make build` updates it.
install: build
	mkdir -p $(HOME)/.local/bin
	ln -sf $(CURDIR)/target/release/rexdiff $(HOME)/.local/bin/rexdiff
