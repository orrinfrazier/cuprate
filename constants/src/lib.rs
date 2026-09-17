#![doc = include_str!("../README.md")]
#![deny(missing_docs, reason = "all constants should document what they are")]
#![no_std] // This can be removed if we eventually need `std`.

mod macros;

#[cfg(feature = "block")]
pub mod block;
#[cfg(feature = "build")]
pub mod build;
#[cfg(feature = "rpc")]
pub mod rpc;

/// Verifies the workspace's pinned toolchain / MSRV invariants (S06-1), since
/// `cuprate-constants` is the natural home for build/version metadata.
#[cfg(test)]
mod toolchain_pin {
    extern crate std;

    use self::std::{
        format,
        fs::{read_dir, read_to_string},
        string::String,
        vec::Vec,
    };

    const PINNED: &str = "1.94.0";

    /// Workflows that intentionally run on the `nightly` channel (documented in
    /// each file) and are therefore exempt from the pinned-toolchain check.
    const NIGHTLY_WORKFLOWS: &[&str] = &["doc.yml", "fuzz.yml"];

    fn workspace_file(path: &str) -> String {
        format!("{}/../{path}", env!("CARGO_MANIFEST_DIR"))
    }

    /// Parses the `members = [ ... ]` array from the root `Cargo.toml`, returning
    /// each member directory path. Anchored on the exact `members = [` line so it
    /// does not accidentally pick up `default-members`.
    fn workspace_members(root_manifest: &str) -> Vec<String> {
        let mut members = Vec::new();
        let mut in_members = false;
        for line in root_manifest.lines() {
            let trimmed = line.trim();
            if !in_members {
                if trimmed == "members = [" {
                    in_members = true;
                }
                continue;
            }
            if trimmed == "]" {
                break;
            }
            if let Some(path) = trimmed.split('"').nth(1) {
                members.push(String::from(path));
            }
        }
        members
    }

    #[test]
    fn rust_toolchain_toml_is_present_and_pinned() {
        let contents = read_to_string(workspace_file("rust-toolchain.toml"))
            .expect("expected workspace root rust-toolchain.toml to exist");

        assert!(
            contents.contains(&format!("channel = \"{PINNED}\"")),
            "expected rust-toolchain.toml to pin channel = \"{PINNED}\", got:\n{contents}"
        );
        assert!(
            contents.contains("\"rustfmt\""),
            "expected rust-toolchain.toml to include rustfmt in components, got:\n{contents}"
        );
        assert!(
            contents.contains("\"clippy\""),
            "expected rust-toolchain.toml to include clippy in components, got:\n{contents}"
        );
    }

    #[test]
    fn workspace_package_declares_msrv() {
        let contents = read_to_string(workspace_file("Cargo.toml"))
            .expect("expected workspace root Cargo.toml to exist");

        assert!(
            contents.contains("[workspace.package]"),
            "expected root Cargo.toml to contain [workspace.package], got:\n{contents}"
        );
        assert!(
            contents.contains("rust-version = \"1.94\""),
            "expected root Cargo.toml to set rust-version = \"1.94\", got:\n{contents}"
        );
    }

    #[test]
    fn all_members_inherit_workspace_rust_version() {
        let root = read_to_string(workspace_file("Cargo.toml"))
            .expect("expected workspace root Cargo.toml to exist");
        let members = workspace_members(&root);

        assert!(
            members.len() >= 30,
            "expected to parse >= 30 workspace members from root Cargo.toml, parsed {}: {members:?}",
            members.len()
        );

        for member in &members {
            let manifest_path = format!("{member}/Cargo.toml");
            let contents = read_to_string(workspace_file(&manifest_path))
                .unwrap_or_else(|err| panic!("expected {manifest_path} to exist: {err}"));

            assert!(
                contents.contains("rust-version.workspace = true"),
                "{manifest_path} is missing `rust-version.workspace = true` — every workspace member must inherit the MSRV"
            );
        }
    }

    #[test]
    fn all_rust_installing_workflows_are_pinned() {
        let dir = workspace_file(".github/workflows");
        let entries =
            read_dir(&dir).unwrap_or_else(|err| panic!("expected {dir} to be readable: {err}"));

        let mut pinned_count = 0;
        for entry in entries {
            let entry = entry.expect("failed to read a workflow directory entry");
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            if !name.ends_with(".yml") {
                continue;
            }

            let contents = read_to_string(entry.path())
                .unwrap_or_else(|err| panic!("expected to read workflow {name}: {err}"));

            let installs_rust = contents.contains("dtolnay/rust-toolchain")
                || contents.contains("rustup toolchain install");
            if !installs_rust {
                continue;
            }

            assert!(
                !contents.contains("toolchain: stable"),
                "{name} installs Rust on `toolchain: stable` — pin it to {PINNED} or allowlist it as nightly"
            );

            if NIGHTLY_WORKFLOWS.contains(&name.as_ref()) {
                continue;
            }

            assert!(
                contents.contains(&format!("toolchain: {PINNED}")),
                "{name} installs Rust but is not pinned to `toolchain: {PINNED}` (and is not an allowlisted nightly workflow)"
            );
            pinned_count += 1;
        }

        assert!(
            pinned_count >= 3,
            "expected at least 3 pinned Rust-installing workflows (ci/release/deny), found {pinned_count}"
        );
    }
}
