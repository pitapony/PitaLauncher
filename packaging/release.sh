#!/bin/sh
# packaging/release.sh — cut a signed release tag and prepare the release
# package. The full checklist, including what is run where, is
# docs/m4-release.md.
#
# It runs on Arch/CachyOS from the repository root:
#
#   packaging/release.sh tag 0.1.0        # create the signed tag v0.1.0
#   packaging/release.sh srcinfo 0.1.0    # pin the tarball's sha256, write .SRCINFO
set -eu

URL="https://github.com/pitapony/PitaLauncher"
PKGBUILD_NAME="PKGBUILD.release"
PLACEHOLDER="SHA256_PLACEHOLDER"

usage() {
  cat <<'EOF'
Usage:
  packaging/release.sh tag <version>      create the signed git tag v<version>
  packaging/release.sh srcinfo <version>  pin the released tarball's sha256 into
                                          PKGBUILD.release and write .SRCINFO

Run from the repository root on Arch/CachyOS. See docs/m4-release.md.
EOF
}

die() {
  echo "release.sh: $*" >&2
  exit 1
}

# Set `version` and `tag`, and refuse when the three copies of the version —
# Cargo.toml, PKGBUILD.release's pkgver, and the argument — disagree.
check_version() {
  version="${1:-}"
  [ -n "$version" ] || die "a version is required (e.g. 0.1.0)"
  [ -f Cargo.toml ] || die "run from the repository root (no Cargo.toml)"

  crate_version=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -n1)
  [ "$version" = "$crate_version" ] \
    || die "Cargo.toml says $crate_version, not $version; bump it first"

  pkg_version=$(sed -n 's/^pkgver=//p' "packaging/$PKGBUILD_NAME")
  [ "$version" = "$pkg_version" ] \
    || die "packaging/$PKGBUILD_NAME pkgver is $pkg_version, not $version"

  tag="v$version"
}

case "${1:-}" in
  tag)
    check_version "${2:-}"
    [ -z "$(git status --porcelain)" ] || die "the working tree is dirty; commit before tagging"
    if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
      die "tag $tag already exists"
    fi
    # A signed, annotated tag: `git tag -v` must verify it before a release.
    git tag -s "$tag" -m "PitaGames $version"
    echo "created signed tag $tag"
    echo "next: git push origin $tag, then packaging/release.sh srcinfo $version"
    ;;
  srcinfo)
    check_version "${2:-}"
    command -v makepkg >/dev/null 2>&1 || die "makepkg is not on PATH (run on Arch/CachyOS)"
    command -v sha256sum >/dev/null 2>&1 || die "sha256sum is not on PATH"

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT INT TERM
    tarball="$tmp/$tag.tar.gz"
    url="$URL/archive/refs/tags/$tag.tar.gz"
    echo "downloading $url"
    # GitHub serves a tag archive only once the tag is pushed.
    curl -fL "$url" -o "$tarball" \
      || die "cannot download $tag; push the tag first"

    sum=$(sha256sum "$tarball" | cut -d' ' -f1)
    sed -i "s|sha256sums=('$PLACEHOLDER')|sha256sums=('$sum')|" \
      "packaging/$PKGBUILD_NAME"
    grep -q "sha256sums=('$sum')" "packaging/$PKGBUILD_NAME" \
      || die "could not pin the digest in packaging/$PKGBUILD_NAME"

    # `makepkg` only reads a file named PKGBUILD by default; -p names ours.
    ( cd packaging && makepkg -p "$PKGBUILD_NAME" --printsrcinfo > .SRCINFO )
    echo "pinned sha256 $sum and wrote packaging/.SRCINFO"
    echo "commit both files (or attach them to the release) before submitting"
    ;;
  *)
    usage
    exit 2
    ;;
esac
