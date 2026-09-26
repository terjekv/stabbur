//! Read-only local discovery. Paths and recipe inputs never leave this adapter.

use super::{
    AutoPkgError, MAX_RECIPE_FILE_BYTES, MAX_RECIPE_TREE_DEPTH, MAX_RECIPE_TREE_ENTRIES,
    PinnedSource, catalog_entry_from_document, is_recipe_path, parse_recipe_document,
    read_catalog_recipe,
};
use stabbur_builder_core::RecipeImportSources;
use stabbur_builder_core::{
    RecipeCatalogDiagnostic, RecipeCatalogDiagnosticSeverity, RecipeCatalogEntry,
    RecipeCatalogManifest, RecipeCatalogSource,
};
use std::collections::BTreeSet;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{io::AsyncReadExt, process::Command};

struct ObservedRecipe {
    entry: RecipeCatalogEntry,
    source: Option<RecipeCatalogSource>,
    custom_processors: bool,
    parent_trust: bool,
}

/// Inspects the locally configured AutoPkg profile without updating repositories or running recipes.
/// The caller supplies a stable inventory identity, never a filesystem path.
pub async fn discover_autopkg_catalog(
    program: &Path,
    prefs: Option<&Path>,
    inventory_identity: &str,
) -> Result<RecipeCatalogManifest, AutoPkgError> {
    let mut command = Command::new(program);
    command.args(["list-recipes", "--plist", "--show-all"]);
    if let Some(prefs) = prefs {
        command.arg("--prefs").arg(prefs);
    }
    // AutoPkg needs the worker account's HOME to resolve its configured profile.
    // No server-supplied arguments, environment overrides, or recipe execution are involved.
    let bytes = bounded_output(&mut command, 8 * 1024 * 1024).await?;
    let value = plist::Value::from_reader(std::io::Cursor::new(bytes))
        .map_err(|_| AutoPkgError::CatalogGenerationFailed)?;
    let rows = value
        .as_array()
        .ok_or(AutoPkgError::CatalogGenerationFailed)?;
    if rows.len() > MAX_RECIPE_TREE_ENTRIES {
        return Err(AutoPkgError::CatalogGenerationFailed);
    }
    let mut observations = BTreeMap::new();
    let mut duplicates = BTreeSet::new();
    let mut repositories = BTreeMap::<PathBuf, Option<RecipeCatalogSource>>::new();
    for row in rows {
        let Some(path) = row
            .as_dictionary()
            .and_then(|row| row.get("Path"))
            .and_then(plist::Value::as_string)
        else {
            continue;
        };
        let path = Path::new(path);
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > MAX_RECIPE_FILE_BYTES || !is_recipe_path(path) {
            continue;
        }
        let bytes = read_catalog_recipe(path)?;
        let Some(document) = parse_recipe_document(path, &bytes) else {
            continue;
        };
        let Some(entry) = catalog_entry_from_document(path, &document) else {
            continue;
        };
        let source = pinned_local_source(path, &bytes, &mut repositories).await;
        let observed = ObservedRecipe {
            entry,
            source,
            custom_processors: has_external_processors(&document),
            parent_trust: document
                .get("ParentRecipeTrustInfo")
                .is_some_and(serde_json::Value::is_object),
        };
        let identifier = observed.entry.identifier.clone();
        if observations.insert(identifier.clone(), observed).is_some() {
            duplicates.insert(identifier);
        }
    }
    build_manifest(&observations, &duplicates, inventory_identity)
}

fn has_external_processors(document: &serde_json::Value) -> bool {
    document
        .get("Process")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|steps| {
            steps.iter().any(|step| {
                step.get("Processor")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|processor| processor.contains('/'))
            })
        })
}

fn build_manifest(
    observations: &BTreeMap<String, ObservedRecipe>,
    duplicates: &BTreeSet<String>,
    identity: &str,
) -> Result<RecipeCatalogManifest, AutoPkgError> {
    let mut recipes = Vec::new();
    let mut diagnostics = Vec::new();
    for observed in observations.values() {
        let mut entry = observed.entry.clone();
        match source_closure(&entry.identifier, observations, duplicates) {
            Ok(sources) => entry.import_sources = Some(sources),
            Err((code, detail)) => diagnostics.push(diagnostic(
                &entry.identifier,
                code,
                detail,
                RecipeCatalogDiagnosticSeverity::Error,
            )),
        }
        if !entry.parents.is_empty() && !observed.parent_trust {
            diagnostics.push(diagnostic(&entry.identifier, "parent_trust_required", "This recipe has no parent trust information. Create and review a trusted override before building; import does not accept trust automatically.", RecipeCatalogDiagnosticSeverity::Warning));
        }
        recipes.push(entry);
    }
    diagnostics.sort_by(|a, b| (&a.identifier, &a.code).cmp(&(&b.identifier, &b.code)));
    let mut manifest = RecipeCatalogManifest {
        schema_version: RecipeCatalogManifest::SCHEMA_VERSION,
        producer: "autopkg".into(),
        source: RecipeCatalogSource {
            locator: identity.into(),
            revision: "pending".into(),
        },
        recipes,
        diagnostics,
    };
    // Content identity is stable across repeated observations; publication stays idempotent.
    manifest.source.revision = manifest
        .canonical_digest()
        .map_err(|_| AutoPkgError::CatalogGenerationFailed)?
        .to_string();
    manifest
        .canonical_digest()
        .map_err(|_| AutoPkgError::CatalogGenerationFailed)?;
    Ok(manifest)
}

fn diagnostic(
    identifier: &str,
    code: &str,
    detail: &str,
    severity: RecipeCatalogDiagnosticSeverity,
) -> RecipeCatalogDiagnostic {
    RecipeCatalogDiagnostic {
        identifier: Some(identifier.into()),
        code: code.into(),
        severity,
        detail: detail.into(),
    }
}

type Blocker = (&'static str, &'static str);
fn source_closure(
    identifier: &str,
    observations: &BTreeMap<String, ObservedRecipe>,
    duplicates: &BTreeSet<String>,
) -> Result<RecipeImportSources, Blocker> {
    let mut visited = BTreeSet::new();
    let mut sources = BTreeMap::<String, RecipeCatalogSource>::new();
    let mut current = identifier;
    loop {
        if visited.len() >= MAX_RECIPE_TREE_DEPTH || !visited.insert(current) {
            return Err((
                "parent_cycle",
                "Parent recipes contain a cycle or exceed the supported depth.",
            ));
        }
        if duplicates.contains(current) {
            return Err((
                "ambiguous_identifier",
                "Multiple recipes share an identifier. Give overrides unique identifiers before importing.",
            ));
        }
        let observed = observations.get(current).ok_or((
            "missing_parent",
            "A parent recipe is unavailable. Add its repository to AutoPkg and refresh discovery.",
        ))?;
        if observed.custom_processors {
            return Err((
                "processor_dependency",
                "This recipe uses external processors. Their source closure requires manual review; import a reviewed catalog manifest instead.",
            ));
        }
        let source = observed.source.as_ref().ok_or(("unpinned_source", "A recipe or parent is outside a clean Git checkout with a credential-free HTTPS origin. Commit overrides and local changes, and publish the exact revision before importing."))?;
        if sources
            .get(&source.locator)
            .is_some_and(|previous| previous.revision != source.revision)
        {
            return Err((
                "conflicting_sources",
                "The parent chain requires conflicting revisions of one repository.",
            ));
        }
        sources.insert(source.locator.clone(), source.clone());
        let Some(parent) = observed.entry.parents.first() else {
            break;
        };
        current = parent;
    }
    RecipeImportSources::new(sources.into_values().collect()).map_err(|_| {
        (
            "source_limit",
            "The recipe needs more source repositories than an isolated build supports.",
        )
    })
}

async fn pinned_local_source(
    path: &Path,
    bytes: &[u8],
    repositories: &mut BTreeMap<PathBuf, Option<RecipeCatalogSource>>,
) -> Option<RecipeCatalogSource> {
    let directory = path.parent()?;
    let root = PathBuf::from(git_text(directory, &["rev-parse", "--show-toplevel"]).await?);
    let canonical = std::fs::canonicalize(path).ok()?;
    let relative = canonical.strip_prefix(&root).ok()?.to_str()?;
    if !repositories.contains_key(&root) {
        let source = async {
            if !git_text(&root, &["status", "--porcelain", "--untracked-files=all"])
                .await?
                .is_empty()
            {
                return None;
            }
            let pin = PinnedSource {
                url: git_text(&root, &["remote", "get-url", "origin"]).await?,
                commit: git_text(&root, &["rev-parse", "HEAD"]).await?,
            };
            pin.validate().ok()?;
            Some(RecipeCatalogSource {
                locator: pin.url,
                revision: pin.commit,
            })
        }
        .await;
        repositories.insert(root.clone(), source);
    }
    let source = repositories.get(&root)?.clone()?;
    let object = format!("{}:{relative}", source.revision);
    // Compare committed bytes, including ignored/untracked and concurrently edited files.
    let committed = git_bytes(
        &root,
        &["show", &object],
        usize::try_from(MAX_RECIPE_FILE_BYTES).ok()?,
    )
    .await?;
    (committed == bytes).then_some(source)
}

async fn git_text(directory: &Path, args: &[&str]) -> Option<String> {
    String::from_utf8(git_bytes(directory, args, 1024 * 1024).await?)
        .ok()
        .map(|value| value.trim().to_owned())
}
async fn git_bytes(directory: &Path, args: &[&str], maximum: usize) -> Option<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .current_dir(directory)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
    bounded_output(&mut command, maximum).await.ok()
}

async fn bounded_output(command: &mut Command, maximum: usize) -> Result<Vec<u8>, AutoPkgError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|_| AutoPkgError::CatalogGenerationFailed)?;
    let stdout = child
        .stdout
        .take()
        .ok_or(AutoPkgError::CatalogGenerationFailed)?;
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let mut bytes = Vec::new();
        stdout
            .take(maximum as u64 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| AutoPkgError::CatalogGenerationFailed)?;
        if bytes.len() > maximum {
            return Err(AutoPkgError::CatalogGenerationFailed);
        }
        if !child
            .wait()
            .await
            .map_err(|_| AutoPkgError::CatalogGenerationFailed)?
            .success()
        {
            return Err(AutoPkgError::CatalogGenerationFailed);
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| AutoPkgError::CatalogGenerationFailed)?
}

/// Adds import closures to a materialized repository using the same rules as local inventory.
pub(super) fn enrich_repository(
    root: &Path,
    source: &PinnedSource,
    recipes: &mut [RecipeCatalogEntry],
    diagnostics: &mut Vec<RecipeCatalogDiagnostic>,
) -> Result<(), AutoPkgError> {
    let mut observations = BTreeMap::new();
    let mut duplicates = BTreeSet::new();
    let mut stack = vec![(root.to_path_buf(), 0)];
    let mut entries_seen = 0;
    while let Some((directory, depth)) = stack.pop() {
        for item in
            std::fs::read_dir(directory).map_err(|_| AutoPkgError::CatalogGenerationFailed)?
        {
            let item = item.map_err(|_| AutoPkgError::CatalogGenerationFailed)?;
            entries_seen += 1;
            if entries_seen > MAX_RECIPE_TREE_ENTRIES {
                return Err(AutoPkgError::CatalogGenerationFailed);
            }
            let kind = item
                .file_type()
                .map_err(|_| AutoPkgError::CatalogGenerationFailed)?;
            if kind.is_dir() && item.file_name() != ".git" {
                if depth >= MAX_RECIPE_TREE_DEPTH {
                    return Err(AutoPkgError::CatalogGenerationFailed);
                }
                stack.push((item.path(), depth + 1));
            }
            if !kind.is_file()
                || !is_recipe_path(&item.path())
                || item
                    .metadata()
                    .map_err(|_| AutoPkgError::CatalogGenerationFailed)?
                    .len()
                    > MAX_RECIPE_FILE_BYTES
            {
                continue;
            }
            let bytes = read_catalog_recipe(&item.path())?;
            let Some(document) = parse_recipe_document(&item.path(), &bytes) else {
                continue;
            };
            let Some(entry) = catalog_entry_from_document(&item.path(), &document) else {
                continue;
            };
            let identifier = entry.identifier.clone();
            let observation = ObservedRecipe {
                entry,
                source: Some(RecipeCatalogSource {
                    locator: source.url.clone(),
                    revision: source.commit.clone(),
                }),
                custom_processors: has_external_processors(&document),
                parent_trust: document
                    .get("ParentRecipeTrustInfo")
                    .is_some_and(serde_json::Value::is_object),
            };
            if observations
                .insert(identifier.clone(), observation)
                .is_some()
            {
                duplicates.insert(identifier);
            }
        }
    }
    let manifest = build_manifest(&observations, &duplicates, &source.url)?;
    for entry in recipes {
        entry.import_sources = manifest
            .recipes
            .iter()
            .find(|candidate| candidate.identifier == entry.identifier)
            .and_then(|candidate| candidate.import_sources.clone());
    }
    diagnostics.extend(manifest.diagnostics);
    diagnostics.sort_by(|a, b| (&a.identifier, &a.code).cmp(&(&b.identifier, &b.code)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use stabbur_jobs_core::{Capability, CapabilitySet};
    fn observed(id: &str, parent: Option<&str>, repo: &str) -> ObservedRecipe {
        ObservedRecipe {
            entry: RecipeCatalogEntry {
                identifier: id.into(),
                builder: "autopkg".into(),
                parents: parent.map(String::from).into_iter().collect(),
                required_capabilities: CapabilitySet::new([
                    Capability::new("builder.autopkg").unwrap(),
                    Capability::new("os.macos").unwrap(),
                ]),
                import_sources: None,
            },
            source: Some(RecipeCatalogSource {
                locator: format!("https://example.test/{repo}.git"),
                revision: "a".repeat(40),
            }),
            custom_processors: false,
            parent_trust: parent.is_some(),
        }
    }
    #[test]
    fn closures_preserve_overrides_and_reject_missing_ambiguous_or_dirty_parents() {
        let mut rows = BTreeMap::from([
            (
                "override".into(),
                observed("override", Some("parent"), "overrides"),
            ),
            ("parent".into(), observed("parent", None, "recipes")),
        ]);
        let sources = source_closure("override", &rows, &BTreeSet::new()).unwrap();
        assert_eq!(sources.as_slice().len(), 2);
        assert!(source_closure("override", &rows, &BTreeSet::from(["parent".into()])).is_err());
        rows.get_mut("parent").unwrap().source = None;
        assert_eq!(
            source_closure("override", &rows, &BTreeSet::new())
                .unwrap_err()
                .0,
            "unpinned_source"
        );
        rows.remove("parent");
        assert_eq!(
            source_closure("override", &rows, &BTreeSet::new())
                .unwrap_err()
                .0,
            "missing_parent"
        );
    }
    #[test]
    fn cycles_processors_and_conflicting_revisions_block_import_without_exposing_inputs() {
        let mut rows = BTreeMap::from([
            ("a".into(), observed("a", Some("b"), "recipes")),
            ("b".into(), observed("b", Some("a"), "recipes")),
        ]);
        assert_eq!(
            source_closure("a", &rows, &BTreeSet::new()).unwrap_err().0,
            "parent_cycle"
        );
        rows.get_mut("b").unwrap().entry.parents.clear();
        rows.get_mut("b").unwrap().source.as_mut().unwrap().revision = "b".repeat(40);
        assert_eq!(
            source_closure("a", &rows, &BTreeSet::new()).unwrap_err().0,
            "conflicting_sources"
        );
        rows.get_mut("a").unwrap().custom_processors = true;
        let manifest =
            build_manifest(&rows, &BTreeSet::new(), "stabbur-worker:fixture:autopkg").unwrap();
        assert!(manifest.recipes[0].import_sources.is_none());
        assert_eq!(manifest.diagnostics[0].code, "processor_dependency");
        assert!(!serde_json::to_string(&manifest).unwrap().contains("Input"));
    }
    #[tokio::test]
    async fn local_git_requires_committed_matching_bytes_and_clean_checkout() {
        async fn setup(root: &Path, args: &[&str]) {
            assert!(
                Command::new("git")
                    .args([
                        "-c",
                        "user.name=Fixture",
                        "-c",
                        "user.email=fixture@example.test",
                        "-c",
                        "commit.gpgsign=false",
                        "-c",
                        "core.hooksPath=/dev/null"
                    ])
                    .args(args)
                    .current_dir(root)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .unwrap()
                    .success()
            );
        }
        let temp = tempfile::tempdir().unwrap();
        setup(temp.path(), &["init", "--quiet"]).await;
        setup(
            temp.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://example.test/recipes.git",
            ],
        )
        .await;
        let path = temp.path().join("fixture.recipe");
        let bytes = b"fixture committed bytes";
        std::fs::write(&path, bytes).unwrap();
        setup(temp.path(), &["add", "."]).await;
        setup(temp.path(), &["commit", "--quiet", "-m", "Fixture"]).await;
        assert!(
            pinned_local_source(&path, bytes, &mut BTreeMap::new())
                .await
                .is_some()
        );
        assert!(
            pinned_local_source(&path, b"changed during scan", &mut BTreeMap::new())
                .await
                .is_none()
        );
        std::fs::write(&path, b"dirty").unwrap();
        assert!(
            pinned_local_source(&path, bytes, &mut BTreeMap::new())
                .await
                .is_none()
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn discovery_uses_machine_list_and_omits_native_inputs_and_paths() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let recipe = temp.path().join("private.recipe");
        std::fs::write(&recipe, "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>Identifier</key><string>example.private</string><key>Input</key><dict><key>TOKEN</key><string>synthetic-do-not-publish</string></dict></dict></plist>").unwrap();
        let list = temp.path().join("list.plist");
        plist::to_file_xml(
            &list,
            &vec![BTreeMap::from([("Path", recipe.to_str().unwrap())])],
        )
        .unwrap();
        let program = temp.path().join("autopkg");
        std::fs::write(&program, format!("#!/bin/sh\n[ \"$1 $2 $3\" = 'list-recipes --plist --show-all' ] || exit 1\ncat '{}'\n", list.display())).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let manifest = discover_autopkg_catalog(&program, None, "stabbur-worker:fixture:autopkg")
            .await
            .unwrap();
        assert_eq!(manifest.recipes.len(), 1);
        assert!(manifest.recipes[0].import_sources.is_none());
        let json = serde_json::to_string(&manifest).unwrap();
        assert!(!json.contains("synthetic-do-not-publish"));
        assert!(!json.contains(temp.path().to_str().unwrap()));
        assert_eq!(
            manifest,
            discover_autopkg_catalog(&program, None, "stabbur-worker:fixture:autopkg")
                .await
                .unwrap()
        );
    }
}
