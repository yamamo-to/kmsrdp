# AlmaLinux / RHEL 9 packaging for kmsrdp.
#
# The kmsrdp repository is private, so codeload.github.com archive URLs
# 404 without an auth token; both sources are generated locally by `make
# vendor` (`git archive` for Source0, `cargo vendor` for Source1, which a
# mock/COPR build needs since it has no network access) instead of being
# fetched from a URL.

%global forgeurl https://github.com/yamamo-to/kmsrdp
# Cargo-built binaries don't line up with rpm's debugsource expectations
# (paths point into vendor/ and ~/.cargo, not a rebuildable layout), so the
# auto-generated debugsource subpackage ends up empty and fails the build.
%global debug_package %{nil}

Name:           kmsrdp
Version:        0.1.0
Release:        1%{?dist}
Summary:        DRM/KMS-based RDP remote desktop server (pure Rust)

License:        MIT OR Apache-2.0
URL:            %{forgeurl}
Source0:        %{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.xz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
BuildRequires:  systemd-rpm-macros

Requires:       libcap
Requires(post): libcap

%description
kmsrdp is a from-scratch remote desktop server for Linux, inspired by
ReFrame's compositor-bypass architecture (DRM/KMS capture + uinput input
injection) but speaking RDP instead of VNC via its own from-scratch RDP
protocol implementation (no ironrdp or other RDP library dependency). It
supports screen capture, mouse/keyboard input, Japanese/CJK IME text
injection (X11 sessions), bidirectional clipboard sync, audio output and
microphone redirection, TLS + username/password authentication, and
priority-aware scheduling so video traffic can't starve audio.

Known limitations: Linear (non-tiled) framebuffers only, single monitor,
and no printer/drive redirection consumer yet (the MS-RDPDR protocol
itself is implemented and validated, just not wired to a real local
filesystem/printing backend). See the upstream README for details.

%prep
%autosetup -p1 -n %{name}-%{version}
tar -xf %{SOURCE1}
mkdir -p .cargo
cat > .cargo/config.toml <<'EOF'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
EOF

%build
cargo build --release --offline --bin rdp_server

%install
install -D -m755 target/release/rdp_server %{buildroot}%{_libexecdir}/%{name}/%{name}-server
install -D -m644 dist/%{name}.service %{buildroot}%{_userunitdir}/%{name}.service
install -D -m644 dist/%{name}.env.example %{buildroot}%{_docdir}/%{name}/%{name}.env.example
install -D -m644 dist/%{name}-system.service %{buildroot}%{_unitdir}/%{name}.service
install -D -m644 dist/%{name}-system.env.example %{buildroot}%{_docdir}/%{name}/%{name}-system.env.example

%post
setcap cap_sys_admin,cap_dac_override+ep %{_libexecdir}/%{name}/%{name}-server || :
cat <<MSG
kmsrdp installed. Two ways to run it:

Per user (single session, no root):
  mkdir -p ~/.config/kmsrdp
  cp %{_docdir}/%{name}/kmsrdp.env.example ~/.config/kmsrdp/kmsrdp.env
  chmod 600 ~/.config/kmsrdp/kmsrdp.env
  \$EDITOR ~/.config/kmsrdp/kmsrdp.env   # set KMSRDP_USER / KMSRDP_PASSWORD
  systemctl --user enable --now kmsrdp.service

As root (follows whichever login session is active):
  mkdir -p /etc/kmsrdp
  install -m600 %{_docdir}/%{name}/kmsrdp-system.env.example /etc/kmsrdp/kmsrdp.env
  \$EDITOR /etc/kmsrdp/kmsrdp.env   # set KMSRDP_USER / KMSRDP_PASSWORD
  systemctl enable --now kmsrdp.service
MSG

%files
%license LICENSE-MIT LICENSE-APACHE
%doc README.md
%{_libexecdir}/%{name}/%{name}-server
%{_userunitdir}/%{name}.service
%{_unitdir}/%{name}.service
%{_docdir}/%{name}/%{name}.env.example
%{_docdir}/%{name}/%{name}-system.env.example

%changelog
* Wed Jul 15 2026 kmsrdp contributors <noreply@example.com> - 0.1.0-1
- Initial packaging
