//! The release machinery's contracts with itself.
//!
//! A release crosses files that nothing compiles together: two workflows, the
//! installer and the changelog they feed. Every failure here is silent - a
//! tarball published under one name and fetched under another, a version
//! bumped in a field the workflow no longer finds - so reading the sources and
//! insisting they agree is the test.

const RELEASE_PR: &str = include_str!("../.github/workflows/release-pr.yml");
const PUBLISH: &str = include_str!("../.github/workflows/publish.yml");
const INSTALLER: &str = include_str!("../install.sh");
const CHANGELOG: &str = include_str!("../CHANGELOG.md");
const MANIFEST: &str = include_str!("../Cargo.toml");

#[test]
fn the_linux_tarball_is_published_under_the_name_the_installer_fetches() {
    assert!(PUBLISH.contains("codesnob-v${VERSION}-x86_64-linux.tar.gz"));
    assert!(INSTALLER.contains("Linux/x86_64) SUFFIX=\"x86_64-linux\""));
    assert!(INSTALLER.contains("TARBALL=\"codesnob-$TAG-$SUFFIX.tar.gz\""));
    assert!(INSTALLER.contains("/releases/download/$TAG/"));
}

#[test]
fn the_darwin_tarball_is_published_under_the_name_the_installer_fetches() {
    assert!(PUBLISH.contains("codesnob-v${VERSION}-aarch64-darwin.tar.gz"));
    assert!(
        PUBLISH.contains("gh release upload"),
        "attached to the release the linux job created"
    );
    assert!(INSTALLER.contains("Darwin/arm64) SUFFIX=\"aarch64-darwin\""));
}

#[test]
fn the_tarball_carries_what_the_installer_unpacks() {
    for line in PUBLISH
        .lines()
        .filter(|l| l.contains("tar -C \"$STAGE\" -czf"))
    {
        assert!(
            line.ends_with("codesnob LICENSE"),
            "tarball contents: {line}"
        );
    }
    assert!(INSTALLER.contains("\"$TMP/codesnob\""));
    assert!(INSTALLER.contains("\"$TMP/LICENSE\""));
}

#[test]
fn the_installer_verifies_the_checksum_the_publish_writes() {
    assert!(PUBLISH.contains(".tar.gz.sha256"));
    assert!(INSTALLER.contains("\"$TARBALL.sha256\""));
    assert!(INSTALLER.contains("sha256sum -c"));
    assert!(
        INSTALLER.contains("shasum -a 256 -c"),
        "with the tool a Mac actually ships"
    );
}

#[test]
fn the_installer_can_reach_a_private_repository() {
    assert!(INSTALLER.contains("gh release download \"$TAG\" --repo \"$REPO\""));
    assert!(INSTALLER.contains("REPO=\"mme/codesnob\""));
    assert!(MANIFEST.contains("repository = \"https://github.com/mme/codesnob\""));
}

#[test]
fn the_bump_edits_the_one_version_line_the_manifest_has() {
    let version_lines = MANIFEST
        .lines()
        .filter(|line| line.starts_with("version = \""))
        .count();
    assert_eq!(version_lines, 1, "the version is declared exactly once");
    assert!(
        RELEASE_PR.contains("s/^version = "),
        "the bump anchors to it"
    );
}

#[test]
fn the_changelog_is_written_and_read_by_the_same_marks() {
    assert!(CHANGELOG.contains("<!-- next -->"));
    assert!(RELEASE_PR.contains("<!-- next -->"));
    assert!(PUBLISH.contains("/^## v${VERSION}\\$/"));
}

#[test]
fn the_merge_is_the_publish_trigger() {
    assert!(PUBLISH.contains("paths:\n      - Cargo.toml"));
    assert!(
        PUBLISH.contains("workflow_dispatch"),
        "and can be re-run by hand"
    );
}

#[test]
fn the_tag_points_at_the_commit_that_was_tested() {
    assert!(PUBLISH.contains("--target \"$GITHUB_SHA\""));
}

#[test]
fn a_prerelease_version_never_ships() {
    assert!(PUBLISH.contains("^[0-9]+\\.[0-9]+\\.[0-9]+$"));
}

#[test]
fn an_open_release_pr_is_never_overwritten() {
    assert!(RELEASE_PR.contains("gh pr list --head \"release/v$NEW\""));
}
