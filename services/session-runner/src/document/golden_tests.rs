//! The shipped first-party documents (`workflows/*.json`) stay valid.
//!
//! Walks the package's `workflows/` directory and runs every document
//! through both the validation walk (`Document::parse`) and the
//! published schema, so a format change that breaks a shipped document
//! fails the build (design doc § Golden documents).

use std::path::PathBuf;

use serde_json::Value;

use super::Document;

static SCHEMA: &str = include_str!("../../schema/workflow-v1.schema.json");

/// Resolve this package's directory at runtime for both Cargo and Bazel.
/// Cargo: `CARGO_MANIFEST_DIR` is the package source dir. Bazel: `rules_rust`
/// bakes a compile-time `CARGO_MANIFEST_DIR` that no longer exists at test
/// runtime, so fall back to the runfiles tree via `TEST_SRCDIR`/`TEST_WORKSPACE`.
fn workflows_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if manifest.join("workflows").is_dir() {
        return manifest.join("workflows");
    }
    if let Ok(srcdir) = std::env::var("TEST_SRCDIR") {
        let workspace = std::env::var("TEST_WORKSPACE").unwrap_or_else(|_| "_main".into());
        return PathBuf::from(srcdir)
            .join(workspace)
            .join("services/session-runner/workflows");
    }
    manifest.join("workflows")
}

#[test]
fn test_shipped_workflow_documents_pass_validation_and_the_published_schema() {
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    let schema = jsonschema::validator_for(&schema).unwrap();

    let dir = workflows_dir();
    let mut documents: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    documents.sort();
    assert!(
        !documents.is_empty(),
        "no shipped documents found in {} — the golden suite would be vacuous",
        dir.display()
    );

    for path in documents {
        let src = std::fs::read_to_string(&path).unwrap();
        let document = Document::parse(&src).unwrap_or_else(|issues| {
            panic!(
                "{} fails validation:\n{}",
                path.display(),
                issues
                    .iter()
                    .map(|i| format!("  {}: {}", i.pointer, i.message))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        });
        // The identifying `name` should match the file it ships in
        // (underscores in file names, hyphens in document names aside).
        let stem = path
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .replace('_', "-");
        assert_eq!(document.name, stem, "{}", path.display());

        let value: Value = serde_json::from_str(&src).unwrap();
        let errors: Vec<String> = schema
            .iter_errors(&value)
            .map(|e| format!("  {} at {}", e, e.instance_path()))
            .collect();
        assert!(
            errors.is_empty(),
            "{} is rejected by the published schema:\n{}",
            path.display(),
            errors.join("\n")
        );
    }
}
