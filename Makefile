# AlmaLinux / RHEL 9 packaging convenience targets for kmsrdp.
#
# The repository is private, so codeload.github.com archive URLs 404
# without an auth token: both the plain source tarball (Source0) and the
# vendored Rust dependencies (Source1, needed since a COPR/mock build has
# no network access) are generated locally by `make vendor` instead of
# being fetched from a URL.

TOPDIR := $(CURDIR)/.rpmbuild
NAME := kmsrdp
RPMBUILD_OPTS ?=
VERSION := $(shell awk -F '"' '/^version/{print $$2; exit}' $(NAME)/Cargo.toml)
PKGDIR := $(TOPDIR)/build/$(NAME)-$(VERSION)

.PHONY: all help install-build-deps vendor srpm rpm lint clean

all: rpm

help:
	@echo "  make install-build-deps - one-time build dependency setup (needs sudo)"
	@echo "  make vendor             - regenerate the vendored Rust dependency tarball"
	@echo "  make srpm               - build the source RPM"
	@echo "  make rpm                - build the binary RPM"
	@echo "  make lint               - rpmlint the spec"
	@echo "  make clean"

install-build-deps:
	sudo dnf -y install rpm-build rpmdevtools cargo rust gcc libcap rpmlint

vendor:
	rm -rf $(PKGDIR)
	mkdir -p $(PKGDIR) $(TOPDIR)/SOURCES
	tar -c --exclude='./.git' --exclude='./.rpmbuild' --exclude='./target' . \
		| tar -x -C $(PKGDIR)
	tar --transform 's,^\.,$(NAME)-$(VERSION),' -C $(PKGDIR) \
		-czf $(TOPDIR)/SOURCES/$(NAME)-$(VERSION).tar.gz .
	cd $(PKGDIR) && mkdir -p .cargo && cargo vendor vendor > .cargo/config.toml
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

lint:
	@command -v rpmlint >/dev/null 2>&1 || { echo "rpmlint not found; run: sudo dnf install rpmlint"; exit 1; }
	rpmlint $(NAME).spec

clean:
	rm -rf $(TOPDIR)
