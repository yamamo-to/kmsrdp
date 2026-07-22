# AlmaLinux / RHEL 9 RPM and Debian/Ubuntu .deb packaging targets for kmsrdp.
#
# The repository is private, so codeload.github.com archive URLs 404
# without an auth token: both the plain source tarball (Source0) and the
# vendored Rust dependencies (Source1, needed since a COPR/mock build has
# no network access) are generated locally by `make vendor` instead of
# being fetched from a URL. The .deb path reuses the same vendoring step
# in-tree under `.debbuild/`.

TOPDIR := $(CURDIR)/.rpmbuild
DEBDIR := $(CURDIR)/.debbuild
NAME := kmsrdp
RPMBUILD_OPTS ?=
DPKG_BUILDPACKAGE_OPTS ?= -b -us -uc -d
VERSION := $(shell awk -F '"' '/^version/{print $$2; exit}' $(NAME)/Cargo.toml)
PKGDIR := $(TOPDIR)/build/$(NAME)-$(VERSION)
DEBPKGDIR := $(DEBDIR)/$(NAME)-$(VERSION)

.PHONY: all help install-build-deps install-deb-build-deps vendor prepare-source srpm rpm deb lint clean

all: rpm

help:
	@echo "  make install-build-deps     - RPM build deps (needs sudo, Alma/RHEL)"
	@echo "  make install-deb-build-deps - .deb build deps (needs sudo, Debian/Ubuntu)"
	@echo "  make vendor                 - regenerate the vendored Rust dependency tarball"
	@echo "  make srpm                   - build the source RPM"
	@echo "  make rpm                    - build the binary RPM"
	@echo "  make deb                    - build the binary .deb"
	@echo "  make lint                   - rpmlint the spec"
	@echo "  make clean"

install-build-deps:
	sudo dnf -y install rpm-build rpmdevtools cargo rust gcc gcc-c++ libcap rpmlint fuse3-devel pulseaudio-libs-devel

install-deb-build-deps:
	sudo apt-get update
	sudo apt-get install -y \
		build-essential g++ debhelper devscripts \
		pkg-config libfuse3-dev libcap2-bin libpulse-dev

# Copy the checkout into $(1) and vendor crates there (needs network once).
define prepare_source
	rm -rf $(1)
	mkdir -p $(1)
	tar -c \
		--exclude='./.git' \
		--exclude='./.rpmbuild' \
		--exclude='./.debbuild' \
		--exclude='./target' \
		--exclude='./.claude' \
		--exclude='./.cursor' \
		. | tar -x -C $(1)
	cd $(1) && mkdir -p .cargo && cargo vendor vendor > .cargo/config.toml
endef

vendor:
	$(call prepare_source,$(PKGDIR))
	mkdir -p $(TOPDIR)/SOURCES
	tar --transform 's,^\.,$(NAME)-$(VERSION),' -C $(PKGDIR) \
		-czf $(TOPDIR)/SOURCES/$(NAME)-$(VERSION).tar.gz .
	tar -C $(PKGDIR) -cJf $(TOPDIR)/SOURCES/$(NAME)-$(VERSION)-vendor.tar.xz vendor

srpm: vendor
	mkdir -p $(TOPDIR)/SPECS $(TOPDIR)/SRPMS
	cp $(NAME).spec $(TOPDIR)/SPECS/
	rpmbuild --define "_topdir $(TOPDIR)" $(RPMBUILD_OPTS) -bs $(TOPDIR)/SPECS/$(NAME).spec
	@ls -1 $(TOPDIR)/SRPMS/*.src.rpm

rpm: vendor
	mkdir -p $(TOPDIR)/BUILD $(TOPDIR)/RPMS $(TOPDIR)/SPECS $(TOPDIR)/SRPMS
	cp $(NAME).spec $(TOPDIR)/SPECS/
	rpmbuild --define "_topdir $(TOPDIR)" $(RPMBUILD_OPTS) -bb $(TOPDIR)/SPECS/$(NAME).spec
	@find $(TOPDIR)/RPMS -name '*.rpm'

deb:
	$(call prepare_source,$(DEBPKGDIR))
	cd $(DEBPKGDIR) && dpkg-buildpackage $(DPKG_BUILDPACKAGE_OPTS)
	@ls -1 $(DEBDIR)/$(NAME)_*.deb

lint:
	@command -v rpmlint >/dev/null 2>&1 || { echo "rpmlint not found; run: sudo dnf install rpmlint"; exit 1; }
	rpmlint $(NAME).spec

clean:
	rm -rf $(TOPDIR) $(DEBDIR)
