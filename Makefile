.PHONY: build
build: yum-deps
	cargo build --workspace --bins --release

.PHONY: dbg-build
dbg-build: yum-deps
	cargo build --workspace --bins

.PHONY: install
install:
	cargo install --locked --path .

.PHONY: clean
clean:
	cargo clean

yum-deps:
	@yum install pixman-devel
	@yum install libcap-ng-devel
	@yum install cyrus-sasl-devel
	@yum install pulseaudio
