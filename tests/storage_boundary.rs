use std::{
    fs,
    path::{Path, PathBuf},
};

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_sources(root: &Path) -> Vec<(PathBuf, String)> {
    let mut pending = vec![root.to_path_buf()];
    let mut sources = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).expect("source directory must be readable") {
            let path = entry.expect("directory entry must be readable").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let source = fs::read_to_string(&path).expect("Rust source must be readable");
                sources.push((path, source));
            }
        }
    }
    sources
}

#[test]
fn application_sources_do_not_import_concrete_database_adapters() {
    for (path, source) in rust_sources(&repository_root().join("src")) {
        for forbidden in ["sqlx::", "SqliteStorage", "stabbur_storage_sqlite"] {
            assert!(
                !source.contains(forbidden),
                "{} imports adapter detail `{forbidden}`",
                path.display()
            );
        }
    }

    let manifest = fs::read_to_string(repository_root().join("Cargo.toml"))
        .expect("workspace manifest must be readable");
    let package_dependencies = manifest
        .split_once("\n[dependencies]\n")
        .expect("root dependency section must exist")
        .1
        .split_once("\n[dev-dependencies]\n")
        .expect("root dev-dependency section must exist")
        .0;
    assert!(!package_dependencies.contains("sqlx"));
    assert!(!package_dependencies.contains("stabbur-storage-sqlite"));
    assert!(package_dependencies.contains("stabbur-storage-runtime"));
}

#[test]
fn neutral_crates_do_not_depend_on_web_or_database_implementations() {
    let root = repository_root();
    for crate_name in [
        "stabbur-domain",
        "stabbur-auth-core",
        "stabbur-jobs-core",
        "stabbur-builder-core",
        "stabbur-store-core",
        "stabbur-storage-core",
    ] {
        let crate_root = root.join("crates").join(crate_name);
        let manifest = fs::read_to_string(crate_root.join("Cargo.toml"))
            .expect("neutral crate manifest must be readable");
        for forbidden in ["actix", "sqlx", "stabbur-storage-sqlite"] {
            assert!(
                !manifest.contains(forbidden),
                "{crate_name} manifest depends on `{forbidden}`"
            );
        }
        for (path, source) in rust_sources(&crate_root.join("src")) {
            for forbidden in ["actix_", "sqlx::", "SqliteStorage"] {
                assert!(
                    !source.contains(forbidden),
                    "{} imports implementation detail `{forbidden}`",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn complete_contract_is_explicit_and_has_no_unsupported_escape_hatch() {
    let root = repository_root();
    let contract = fs::read_to_string(root.join("crates/stabbur-storage-core/src/lib.rs"))
        .expect("storage contract source must be readable");
    for required in [
        "TransactionalStorage",
        "BootstrapStorage",
        "CredentialStorage",
        "SoftwareStorage",
        "ArtifactStorage",
        "WorkerStorage",
        "RecipeStorage",
        "RunStorage",
        "RunLogStorage",
        "JobStorage",
        "AuditStorage",
        "OperationalStorage",
    ] {
        let aggregate = contract
            .split_once("pub trait Storage:")
            .expect("complete aggregate must exist")
            .1
            .split_once("\n{")
            .expect("complete aggregate must have an explicit body")
            .0;
        assert!(
            aggregate.contains(required),
            "complete contract omits {required}"
        );
    }
    assert!(!contract.contains("UnsupportedOperation"));

    let sqlite = fs::read_to_string(root.join("crates/stabbur-storage-sqlite/src/lib.rs"))
        .expect("SQLite adapter source must be readable");
    assert!(sqlite.contains("impl Storage for SqliteStorage {}"));
}
