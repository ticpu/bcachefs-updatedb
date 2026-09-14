NAME := bcachefs-updatedb
BINARY := bcachefs-updatedb
CARGO_VERSION := $(shell grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
VERSION := v$(CARGO_VERSION)
GIT_DIRTY := $(shell git diff-index --quiet HEAD -- . 2>/dev/null || echo dirty)
GIT_TAG := $(shell git describe --exact-match --tags 2>/dev/null | grep -E '^v')
GIT_VERSION := $(shell git log --oneline . | wc -l)-$(shell git rev-parse --short HEAD)
BASE_VERSION := $(if $(GIT_DIRTY),$(VERSION)+$(GIT_VERSION),$(if $(GIT_TAG),$(VERSION),$(VERSION)+$(GIT_VERSION)))
DEB_VERSION := $(patsubst v%,%,$(BASE_VERSION))
DEB_AMD64 := $(NAME)_$(DEB_VERSION)_amd64.deb
DEB_ARM64 := $(NAME)_$(DEB_VERSION)_arm64.deb
BINARIES := dist/$(BINARY).amd64 dist/$(BINARY).arm64
FLOORS := dist/glibc-floor.amd64 dist/glibc-floor.arm64
PKG := package.tmp

.PHONY: all clean binaries deb deb-amd64 deb-arm64

all: binaries

binaries: $(BINARIES)

deb: deb-amd64 deb-arm64

deb-amd64: $(DEB_AMD64)

deb-arm64: $(DEB_ARM64)

define build-deb
	rm -rf "$(PKG)"
	install -D -m 755 -T "$<" "$(PKG)/usr/bin/$(BINARY)"
	install -D -m 644 -T systemd/$(NAME)@.service "$(PKG)/usr/lib/systemd/system/$(NAME)@.service"
	install -D -m 644 -T systemd/$(NAME)@.timer "$(PKG)/usr/lib/systemd/system/$(NAME)@.timer"
	install -D -m 644 -T profile.d/$(NAME).sh "$(PKG)/etc/profile.d/$(NAME).sh"
	install -D -m 644 -T LICENSE "$(PKG)/usr/share/doc/$(NAME)/copyright"
	install -D -m 644 -T README.md "$(PKG)/usr/share/doc/$(NAME)/README.md"
	install -D -m 644 -T DEBIAN/control "$(PKG)/DEBIAN/control"
	install -D -m 644 -T DEBIAN/conffiles "$(PKG)/DEBIAN/conffiles"
	install -D -m 755 -T DEBIAN/postinst "$(PKG)/DEBIAN/postinst"
	install -D -m 755 -T DEBIAN/prerm "$(PKG)/DEBIAN/prerm"
	install -D -m 755 -T DEBIAN/postrm "$(PKG)/DEBIAN/postrm"
	sed -i -e "s/^Version:.*/Version: $(DEB_VERSION)/" \
		-e "s/^Architecture:.*/Architecture: $(1)/" \
		-e "s/^Depends:.*/Depends: plocate, libc6 (>= $$(cat dist/glibc-floor.$(1))), libgcc-s1/" \
		"$(PKG)/DEBIAN/control"
	@if grep -rq "$$PWD" "$(PKG)"; then echo "ERROR: package contains build path ($$PWD)" >&2; exit 1; fi
	dpkg-deb --build --root-owner-group "$(PKG)" "$@"
	rm -rf "$(PKG)"
endef

$(DEB_AMD64): dist/$(BINARY).amd64 dist/glibc-floor.amd64 DEBIAN/control DEBIAN/conffiles DEBIAN/postinst DEBIAN/prerm DEBIAN/postrm
	$(call build-deb,amd64)

$(DEB_ARM64): dist/$(BINARY).arm64 dist/glibc-floor.arm64 DEBIAN/control DEBIAN/conffiles DEBIAN/postinst DEBIAN/prerm DEBIAN/postrm
	$(call build-deb,arm64)

# One container build cross-compiles both, so they share a single rule
$(BINARIES) $(FLOORS) &: build.sh Containerfile Cargo.toml $(wildcard Cargo.lock) $(wildcard src/*)
	./build.sh

clean:
	rm -rf "$(PKG)" dist *.deb
