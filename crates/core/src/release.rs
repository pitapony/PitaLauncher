//! The release-packaging contract (`docs/m4-release.md`).
//!
//! Two PKGBUILDs ship in `packaging/`: `PKGBUILD` (`pitagames-git`) tracks
//! `main` and is the development package, and `PKGBUILD.release` (`pitagames`)
//! builds a signed release tag. The release package must not drift from the dev
//! package in anything the user installs, so this module names the invariants
//! both files share and the tests below assert the checked-in files carry them.
//!
//! The release package is a template until a tag exists: its `sha256sums`
//! carries [`SHA256_PLACEHOLDER`], which `packaging/release.sh` replaces with
//! the released tarball's digest at tag time.

/// The release package's name. The dev package declares it in
/// `provides`/`conflicts`, so pacman sees one replacement, not two packages.
pub const PACKAGE: &str = "pitagames";

/// The development package's name, kept until the first release tag exists.
pub const DEV_PACKAGE: &str = "pitagames-git";

/// Release tags are `<TAG_PREFIX><version>` (`TAG_PREFIX` is `v`, so `v0.1.0`).
pub const TAG_PREFIX: &str = "v";

/// The tag for a given `pkgver` (`tag("0.1.0") == "v0.1.0"`).
pub fn tag(version: &str) -> String {
    format!("{TAG_PREFIX}{version}")
}

/// The token `packaging/release.sh` replaces in `PKGBUILD.release`'s
/// `sha256sums` with the released tarball's digest.
pub const SHA256_PLACEHOLDER: &str = "SHA256_PLACEHOLDER";

/// Paths (from the repository root) that the contract tests read.
pub const DEV_PKGBUILD: &str = "packaging/PKGBUILD";
pub const RELEASE_PKGBUILD: &str = "packaging/PKGBUILD.release";
pub const RELEASE_SCRIPT: &str = "packaging/release.sh";
pub const CHECKLIST: &str = "docs/m4-release.md";

/// The absolute paths the package owns. Both PKGBUILDs install exactly these,
/// so an install layout change has to be made twice or the tests fail.
pub const INSTALL_PATHS: &[&str] = &[
    "/usr/bin/pitagames",
    "/usr/bin/pitagames-cli",
    "/usr/bin/pitagames-shim",
    "/usr/lib/pitagames/pitagames-shim",
    "/usr/share/applications/pitagames.desktop",
    "/usr/share/icons/hicolor/scalable/apps/pitagames.svg",
    "/usr/share/metainfo/io.github.pitapony.PitaGames.metainfo.xml",
    "/usr/share/pitagames/compat",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
    }

    fn read(relative: &str) -> String {
        let path = repo_root().join(relative);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
    }

    /// Read one bash array literal (`name=( … )`) from a PKGBUILD, returning its
    /// entries with comments and quoting stripped. Quoted entries keep their
    /// inner spaces (`'proton-cachyos: recommended …'` stays one entry).
    fn array(text: &str, name: &str) -> Vec<String> {
        let needle = format!("\n{name}=(");
        let start = text
            .find(&needle)
            .unwrap_or_else(|| panic!("no `{name}=(` array"));
        let rest = &text[start + needle.len()..];
        let mut entries = Vec::new();
        let mut current = String::new();
        let mut quote: Option<char> = None;
        let mut comment = false;
        for ch in rest.chars() {
            if comment {
                if ch == '\n' {
                    comment = false;
                }
                continue;
            }
            if let Some(q) = quote {
                if ch == q {
                    quote = None;
                } else {
                    current.push(ch);
                }
                continue;
            }
            match ch {
                '\'' | '"' => quote = Some(ch),
                '#' => comment = true,
                ')' => {
                    if !current.trim().is_empty() {
                        entries.push(current.trim().to_string());
                    }
                    return entries;
                }
                c if c.is_whitespace() => {
                    if !current.trim().is_empty() {
                        entries.push(current.trim().to_string());
                        current.clear();
                    }
                }
                c => current.push(c),
            }
        }
        panic!("unterminated `{name}` array");
    }

    fn scalar(text: &str, name: &str) -> String {
        let needle = format!("\n{name}=");
        let start = text
            .find(&needle)
            .unwrap_or_else(|| panic!("no `{name}=` assignment"));
        let rest = &text[start + needle.len()..];
        let line = rest.lines().next().unwrap_or_default();
        line.trim().trim_matches('\'').trim_matches('"').to_string()
    }

    // -- The two packages cannot drift --------------------------------------

    #[test]
    fn the_release_package_matches_the_dev_package() {
        let dev = read(DEV_PKGBUILD);
        let release = read(RELEASE_PKGBUILD);

        // Everything a user gets installed must be identical; only the source
        // (a tag instead of `main`) and the version scheme differ.
        for field in ["depends", "makedepends", "optdepends"] {
            assert_eq!(
                array(&dev, field),
                array(&release, field),
                "`{field}` differs between the dev and release PKGBUILDs"
            );
        }
        for field in ["arch", "url", "license", "pkgdesc"] {
            assert_eq!(
                scalar(&dev, field),
                scalar(&release, field),
                "`{field}` differs between the dev and release PKGBUILDs"
            );
        }
        for path in INSTALL_PATHS {
            for (label, text) in [("dev", &dev), ("release", &release)] {
                assert!(
                    text.contains(path),
                    "the {label} PKGBUILD does not install {path}"
                );
            }
        }
    }

    #[test]
    fn the_dev_package_tracks_main() {
        let dev = read(DEV_PKGBUILD);
        assert!(dev.contains(&format!("pkgname={DEV_PACKAGE}\n")));
        assert!(
            dev.contains("git+$url.git"),
            "the dev package clones a repo"
        );
        assert!(
            dev.contains("\npkgver() {"),
            "the dev package versions per commit"
        );
        assert!(dev.contains(&format!("provides=('{PACKAGE}')")));
        assert!(dev.contains(&format!("conflicts=('{PACKAGE}')")));
    }

    // -- The release package is tag-driven and reproducible -----------------

    #[test]
    fn the_release_package_builds_a_signed_tag_tarball() {
        let release = read(RELEASE_PKGBUILD);
        assert!(release.contains(&format!("pkgname={PACKAGE}\n")));
        assert_eq!(scalar(&release, "pkgver"), env!("CARGO_PKG_VERSION"));

        // The source is the immutable tag tarball, never a moving branch, and
        // its digest is pinned rather than skipped.
        let source = array(&release, "source");
        assert_eq!(source.len(), 1, "one source: {source:?}");
        assert!(
            source[0].contains("archive/refs/tags/") && source[0].contains("$pkgver"),
            "the source is the tag tarball: {:?}",
            source[0]
        );
        assert!(
            !release.contains("git+"),
            "the release source is not a git clone"
        );

        let sums = array(&release, "sha256sums");
        assert_eq!(sums, vec![SHA256_PLACEHOLDER.to_string()]);
        assert!(release.contains("--locked"), "the build is `--locked`");
    }

    #[test]
    fn the_dev_and_release_names_replace_each_other() {
        let release = read(RELEASE_PKGBUILD);
        assert!(
            release.contains(&format!("conflicts=('{DEV_PACKAGE}')")),
            "installing the release must remove the dev package"
        );
    }

    // -- The checklist and the helper exist and agree ------------------------

    #[test]
    fn the_release_script_fills_the_digest_and_writes_srcinfo() {
        let script = read(RELEASE_SCRIPT);
        assert!(
            script.contains(SHA256_PLACEHOLDER),
            "the script must know the placeholder it replaces"
        );
        assert!(
            script.contains("sha256sum"),
            "the script pins a sha256 digest"
        );
        assert!(
            script.contains("makepkg") && script.contains("--printsrcinfo"),
            "the script generates `.SRCINFO`"
        );
        assert!(script.contains(RELEASE_PKGBUILD) || script.contains("PKGBUILD.release"));
    }

    #[test]
    fn the_checklist_documents_every_release_step() {
        let doc = read(CHECKLIST);
        for needle in [
            format!("git tag -s {}", TAG_PREFIX),
            "git tag -v".to_string(),
            "sha256sums".to_string(),
            "--locked".to_string(),
            "SOURCE_DATE_EPOCH".to_string(),
            "makepkg".to_string(),
            "--printsrcinfo".to_string(),
            ".SRCINFO".to_string(),
            RELEASE_SCRIPT.to_string(),
            PACKAGE.to_string(),
            DEV_PACKAGE.to_string(),
        ] {
            assert!(
                doc.contains(&needle),
                "docs/m4-release.md is missing `{needle}`"
            );
        }
    }
}
