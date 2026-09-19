//! ONE structure: feature modules (docs/design.md section 12).
//!
//! The rule is "a dataset owns everything about itself; infrastructure owns
//! nothing about any dataset". These tests pin the part of it a file
//! listing can see, so the layout cannot rot back into a by-layer
//! `db/models` + `utils` split without someone noticing.
//!
//! What they do NOT check is the part only a human can: that the code in
//! those files is about the right thing.

use std::path::{Path, PathBuf};

/// The repository root: `tests/` lives next to `src/`.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn src(relative: &str) -> PathBuf {
    root().join("src").join(relative)
}

/// Every DATA MODULE, i.e. every module that owns a dataset's tables.
const DATA_MODULES: [&str; 5] =
    ["core", "dex", "predictions", "launchpads", "svm"];

/// The data modules whose ClickHouse suite is their own file.
///
/// `core` is not one of them and must not become one by accident: its
/// server-backed coverage is `db::integration_tests`, which drives the
/// insert path, tombstones, the validity rule, epochs and missing ranges
/// through the only dataset that write path has rows for. Splitting it
/// would mean duplicating one fixture, not separating two suites.
const WITH_INTEGRATION_TESTS: [&str; 4] =
    ["dex", "predictions", "launchpads", "svm"];

#[test]
fn every_data_module_has_the_standard_file_set() {
    for module in DATA_MODULES {
        let directory = src(module);
        assert!(
            directory.is_dir(),
            "src/{module}/ is missing: section 12 lists it as a data module"
        );

        for file in ["mod.rs", "decode.rs", "derived.rs", "events.rs"] {
            assert!(
                directory.join(file).is_file(),
                "src/{module}/{file} is missing"
            );
        }

        // Row structs: one file, or a directory with one file per table.
        assert!(
            directory.join("models.rs").is_file()
                || directory.join("models").is_dir(),
            "src/{module}/ has neither models.rs nor models/"
        );

        assert!(
            directory.join("README.md").is_file(),
            "src/{module}/README.md is missing: a dataset documents its \
             own tables"
        );
    }

    for module in WITH_INTEGRATION_TESTS {
        assert!(
            src(module).join("integration_tests.rs").is_file(),
            "src/{module}/integration_tests.rs is missing"
        );
    }
}

#[test]
fn infrastructure_owns_no_dataset_and_there_is_no_by_layer_bucket() {
    assert!(
        !src("db/models").exists(),
        "src/db/models is back: row structs belong to the dataset that \
         owns the table, not to the database layer"
    );

    assert!(
        !src("utils").exists(),
        "src/utils is back: it is a by-layer bucket, and section 12 allows \
         exactly one structure. A serializer belongs next to the insert \
         path (src/db), a conversion or a signature next to the dataset \
         that needs it"
    );

    // The infrastructure files section 12 names, so a move away from them
    // is deliberate and not a typo.
    for file in [
        "mod.rs",
        "format.rs",
        "migrate.rs",
        "ranges.rs",
        "schema.rs",
        "derived.rs",
    ] {
        assert!(
            src("db").join(file).is_file(),
            "src/db/{file} is missing"
        );
    }
}

#[test]
fn every_source_is_one_file_per_chain_family() {
    for file in ["mod.rs", "evm.rs", "solana.rs"] {
        assert!(
            src("source").join(file).is_file(),
            "src/source/{file} is missing"
        );
    }
}
