GIT_REV = $(shell git rev-parse --short HEAD)

RELEASE =
TARGET = target/debug
VERBOSE =

ifdef CARGO_RELEASE
	RELEASE = --release
	TARGET = target/release
endif

ifdef CARGO_VERBOSE
	VERBOSE = --verbose
endif

TARGET_BIN = $(TARGET)/linkerd2-proxy
PKG_ROOT = $(TARGET)/package
PKG_NAME = linkerd2-proxy-$(GIT_REV)
PKG_BASE = $(PKG_ROOT)/$(PKG_NAME)
PKG = $(PKG_NAME).tar.gz

CARGO = cargo

.PHONY: fetch
fetch: Cargo.lock
	$(CARGO) fetch --locked $(VERBOSE)

.PHONY: test
test: fetch
	$(CARGO) test --frozen --no-default-features $(RELEASE) $(VERBOSE)

.PHONY: test-flakey
test-flakey: fetch
	$(CARGO) test --frozen $(RELEASE) $(VERBOSE)

.PHONY: test-flakey
test-benches: fetch
	$(CARGO) test --frozen --benches --no-default-features $(RELEASE) $(VERBOSE)

$(TARGET_BIN): fetch
	$(CARGO) build -p linkerd2-proxy $(RELEASE) $(VERBOSE)

.PHONY: package
package: $(PKG_ROOT)/$(PKG)

$(PKG_ROOT)/$(PKG): $(TARGET_BIN)
	mkdir -p $(PKG_BASE)/bin
	cp LICENSE $(PKG_BASE)
	cp $(TARGET_BIN) $(PKG_BASE)/bin
	cd $(PKG_ROOT) && tar -czvf $(PKG) $(PKG_NAME)
	rm -rf $(PKG_BASE)
