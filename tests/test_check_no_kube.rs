//! scripts/check-no-kube.sh against the ways code can reach Kubernetes
//! from the data plane: each must fail the check.

use std::path::Path;
use std::process::Command;

/// A source tree: a clean data plane, a kubernetes module that may use
/// kube, and `extra` (path, content) files on top.
fn check(extra: &[(&str, &str)]) -> std::process::Output {
    let root = tempfile::tempdir().unwrap();
    let src = root.path().join("src");
    let files = [
        (
            "lib.rs",
            "pub mod dataplane;\npub mod kubernetes;\npub mod sync;\n",
        ),
        ("main.rs", "fn main() { portail::kubernetes::run(); }\n"),
        ("sync/mod.rs", "use crate::config::PortailConfig;\n"),
        ("dataplane/mod.rs", "pub mod apply;\n"),
        (
            "dataplane/apply.rs",
            "//! Never `crate::kubernetes`.\nuse crate::sync::Snapshot;\n",
        ),
        ("kubernetes/naming.rs", "use kube::Client;\n"),
    ];
    for (path, content) in files.iter().chain(extra) {
        let path = src.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    run(&src)
}

fn run(src: &Path) -> std::process::Output {
    Command::new("bash")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/check-no-kube.sh"
        ))
        .arg(src)
        .output()
        .unwrap()
}

#[test]
fn a_clean_tree_passes() {
    let out = check(&[(
        "dataplane/strings.rs",
        "const TYPE: &str = \"kubernetes.io/tls\";\nconst FLAG: &str = \"--kubernetes\";\n\
         fn f(path: &str) { let path = \"/\"; }\n",
    )]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn every_way_in_fails() {
    for (path, content) in [
        ("dataplane/a.rs", "use kube::Client;\n"),
        (
            "dataplane/a.rs",
            "use crate::{config, kubernetes::naming};\n",
        ),
        (
            "dataplane/a.rs",
            "use crate::{\n    config,\n    kubernetes::naming,\n};\n",
        ),
        ("dataplane/a.rs", "use kube as k;\n"),
        ("dataplane/a.rs", "extern crate kube as k;\n"),
        (
            "dataplane/a.rs",
            "#[path = \"../kubernetes/naming.rs\"]\nmod naming;\n",
        ),
        (
            "dataplane/a.rs",
            "#[path = \"../elsewhere.rs\"]\nmod elsewhere;\n",
        ),
        (
            "sync/a.rs",
            "type C = k8s_openapi::api::core::v1::Secret;\n",
        ),
        ("sync/a.rs", "use gateway_api::gateways::Gateway;\n"),
        (
            "dataplane/a.rs",
            "fn f() { super::super::kubernetes::naming::NAME; }\n",
        ),
        (
            "dataplane/a.rs",
            "#[ path = \"../elsewhere.rs\"]\nmod elsewhere;\n",
        ),
        (
            "dataplane/a.rs",
            "#[cfg_attr(test, path = \"../elsewhere.rs\")]\nmod elsewhere;\n",
        ),
        (
            "dataplane/a.rs",
            "#[cfg_attr(\n    test,\n    path = \"../elsewhere.rs\"\n)]\nmod elsewhere;\n",
        ),
        // Anywhere in the library, a module file brought in by its name.
        ("config.rs", "#[path = \"kubernetes/mod.rs\"]\nmod k;\n"),
        ("config.rs", "include!(\"kubernetes/naming.rs\");\n"),
        ("config.rs", "include!(\"../elsewhere.rs\");\n"),
        // A module the data plane imports, re-exporting.
        ("config.rs", "pub use crate::{routing, kubernetes};\n"),
        ("config.rs", "pub use crate::kubernetes::naming;\n"),
        (
            "lib.rs",
            "pub mod kubernetes;\npub use kubernetes::naming;\n",
        ),
    ] {
        let out = check(&[(path, content)]);
        assert!(!out.status.success(), "{path}: {content:?} passed");
    }
}

#[test]
fn a_missing_directory_fails() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src/sync")).unwrap();
    let out = run(&root.path().join("src"));
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("dataplane does not exist"));
}
