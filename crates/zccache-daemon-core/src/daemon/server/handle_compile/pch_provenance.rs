//! Bind unsigned imported PCH bytes to the parent PCH that consumed them.

use super::super::*;
use crate::fingerprint::pch_inputs::{inputs_stat_match, parse_pch_inputs, PchInput};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::io::{Read, Write};

pub(super) const SIDECAR_SUFFIX: &str = ".zccache-imports.json";

#[derive(Serialize, Deserialize)]
struct Provenance {
    version: u32,
    pch_blake3: String,
    imports: Vec<Import>,
}

#[derive(Serialize, Deserialize)]
struct Import {
    path: String,
    blake3: String,
}

fn invalid(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("PCH import provenance: {error}"),
    )
}

pub(super) fn sidecar_path(pch: &Path) -> NormalizedPath {
    let mut path = pch.as_os_str().to_owned();
    path.push(SIDECAR_SUFFIX);
    NormalizedPath::new(Path::new(&path))
}

fn imports(bytes: &[u8]) -> std::io::Result<Vec<PchInput>> {
    Ok(parse_pch_inputs(bytes)
        .map_err(invalid)?
        .unwrap_or_default()
        .into_iter()
        .filter(|input| input.imported_pch)
        .collect())
}

/// Capture a successful producer's imported bytes before its private view dies.
#[cfg(test)]
pub(super) fn record_pch_imports(
    pch: &Path,
    overlay_path: Option<&Path>,
) -> std::io::Result<Option<NormalizedPath>> {
    if overlay_path.is_some() {
        return record_pch_imports_for_output(pch, pch, overlay_path);
    }
    // These fixtures have just compiled against known, unchanged imports.
    // Production producers freeze these snapshots before executing clang.
    let mut plan = crate::fingerprint::pch_inputs::PchServePlan::default();
    for mut input in imports(&std::fs::read(pch)?)? {
        input.content_xxh3 = Some(crate::fingerprint::pch_inputs::content_xxh3(
            &std::fs::read(&input.path)?,
        ));
        plan.require_snapshot(&input).map_err(invalid)?;
    }
    let directory = tempfile::tempdir()?;
    let overlay = crate::fingerprint::pch_inputs::write_pch_overlay(&plan, directory.path())
        .map_err(invalid)?;
    record_pch_imports_for_output(pch, pch, Some(&overlay))
}

/// The compiler may have written the primary PCH into a private staging root.
/// Bind those exact bytes and publish the companion beside the logical output.
pub(in crate::daemon::server) fn record_pch_imports_for_output(
    pch: &Path,
    logical_output: &Path,
    overlay_path: Option<&Path>,
) -> std::io::Result<Option<NormalizedPath>> {
    let bytes = std::fs::read(pch)?;
    let records = imports(&bytes)?;
    if records.is_empty() {
        match std::fs::remove_file(sidecar_path(logical_output)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        return Ok(None);
    }
    let mut mappings = HashMap::new();
    if let Some(overlay) = overlay_path {
        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(overlay)?).map_err(invalid)?;
        let roots = document["roots"]
            .as_array()
            .ok_or_else(|| invalid("overlay roots missing"))?;
        for root in roots {
            let name = root["name"]
                .as_str()
                .ok_or_else(|| invalid("overlay name missing"))?;
            let external = root["external-contents"]
                .as_str()
                .ok_or_else(|| invalid("overlay snapshot missing"))?;
            mappings.insert(NormalizedPath::new(name), NormalizedPath::new(external));
        }
    }
    let imports = records
        .into_iter()
        .map(|record| {
            let key = NormalizedPath::new(&record.path);
            let visible = mappings.get(&key).ok_or_else(|| {
                invalid(format!("executed import snapshot missing: {}", record.path))
            })?;
            let mut file = std::fs::File::open(visible)?;
            let metadata = file.metadata()?;
            let seconds =
                kernal_api::platform::fs::FileTime::from_last_modification_time(&metadata)
                    .unix_seconds();
            if metadata.len() != record.size
                || (record.mtime_secs != 0
                    && u64::try_from(seconds).ok() != Some(record.mtime_secs))
            {
                return Err(invalid(format!(
                    "import changed before producer provenance capture: {}",
                    record.path
                )));
            }
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            let after = file.metadata()?;
            if bytes.len() as u64 != record.size || after.modified()? != metadata.modified()? {
                return Err(invalid(format!(
                    "import changed during producer provenance capture: {}",
                    record.path
                )));
            }
            Ok(Import {
                path: record.path,
                blake3: crate::hash::hash_bytes(&bytes).to_hex(),
            })
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let provenance = Provenance {
        version: 1,
        pch_blake3: crate::hash::hash_bytes(&bytes).to_hex(),
        imports,
    };
    let destination = sidecar_path(logical_output);
    let parent = destination
        .parent()
        .ok_or_else(|| invalid("sidecar has no parent directory"))?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(temporary.as_file_mut(), &provenance).map_err(invalid)?;
    temporary.as_file_mut().flush()?;
    temporary
        .persist(&destination)
        .map_err(|error| error.error)?;
    Ok(Some(destination))
}

fn validate(bytes: &[u8], sidecar: &[u8], records: &[PchInput]) -> std::io::Result<Provenance> {
    let provenance: Provenance = serde_json::from_slice(sidecar).map_err(invalid)?;
    if provenance.version != 1 || provenance.pch_blake3 != crate::hash::hash_bytes(bytes).to_hex() {
        return Err(invalid("parent bytes do not match the recorded producer"));
    }
    if provenance.imports.len() != records.len()
        || records.iter().any(|record| {
            !provenance
                .imports
                .iter()
                .any(|import| import.path == record.path)
        })
    {
        return Err(invalid("import list does not match parent records"));
    }
    Ok(provenance)
}

/// Existing artifact packaging owns this companion; it is not a separate cache.
pub(super) fn cached_import_sidecar(
    pch: &Path,
    bytes: &[u8],
) -> std::io::Result<Option<(NormalizedPath, Vec<u8>)>> {
    let records = imports(bytes)?;
    if records.is_empty() {
        return Ok(None);
    }
    let path = sidecar_path(pch);
    let sidecar = std::fs::read(&path)?;
    validate(bytes, &sidecar, &records)?;
    Ok(Some((NormalizedPath::new(path), sidecar)))
}

/// Detached publication must read immutable bytes from this compile's owned
/// directory, since another producer may replace the logical companion.
pub(super) fn preserve_import_sidecar(
    logical: &Path,
    bytes: &[u8],
    owner: &mut Option<tempfile::TempDir>,
    temporary_root: &Path,
) -> std::io::Result<NormalizedPath> {
    if owner.is_none() {
        *owner = Some(
            tempfile::Builder::new()
                .prefix("pch-provenance-persist-")
                .tempdir_in(temporary_root)?,
        );
    }
    let directory = owner
        .as_ref()
        .ok_or_else(|| invalid("private companion owner missing"))?;
    let name = logical
        .file_name()
        .ok_or_else(|| invalid("sidecar filename missing"))?;
    let path = NormalizedPath::new(directory.path()).join(name);
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)?;
    file.write_all(bytes)?;
    Ok(NormalizedPath::new(path))
}

fn verify_current(
    provenance: &Provenance,
    visited: &mut std::collections::HashSet<NormalizedPath>,
    verified: &mut HashMap<String, u64>,
) -> std::io::Result<()> {
    for import in &provenance.imports {
        let path = Path::new(&import.path);
        let bytes = std::fs::read(path)?;
        if crate::hash::hash_bytes(&bytes).to_hex() != import.blake3 {
            return Err(invalid(format!(
                "imported bytes changed: {}",
                path.display()
            )));
        }
        verified.insert(
            import.path.clone(),
            crate::fingerprint::pch_inputs::content_xxh3(&bytes),
        );
        verify_loaded(path, &bytes, visited, verified)?;
    }
    Ok(())
}

fn verify_loaded(
    pch: &Path,
    bytes: &[u8],
    visited: &mut std::collections::HashSet<NormalizedPath>,
    verified: &mut HashMap<String, u64>,
) -> std::io::Result<()> {
    if !visited.insert(NormalizedPath::new(pch)) {
        return Ok(());
    }
    let records = imports(bytes)?;
    if records.is_empty() {
        return Ok(());
    }
    match std::fs::read(sidecar_path(pch)) {
        Ok(sidecar) => verify_current(&validate(bytes, &sidecar, &records)?, visited, verified),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // External PCHs may use clang's native size/mtime validation, but
            // no private snapshot may hide changed unsigned import metadata.
            if !inputs_stat_match(&records) {
                return Err(invalid(
                    "producer baseline missing for changed imported metadata",
                ));
            }
            for record in records {
                let path = Path::new(&record.path);
                let bytes = std::fs::read(path)?;
                verified.insert(
                    record.path.clone(),
                    crate::fingerprint::pch_inputs::content_xxh3(&bytes),
                );
                verify_loaded(path, &bytes, visited, verified)?;
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Verify loaded PCHs recursively. Never invent a baseline from current bytes.
#[cfg(test)]
pub(super) fn verify_pch_imports(pch: &Path) -> std::io::Result<()> {
    verified_pch_import_hashes(pch).map(|_| ())
}

/// Freeze each imported identity from the same bytes that pass provenance
/// validation. Snapshot creation must recheck these xxh3 values.
pub(super) fn verified_pch_import_hashes(pch: &Path) -> std::io::Result<HashMap<String, u64>> {
    let mut verified = HashMap::new();
    verify_loaded(
        pch,
        &std::fs::read(pch)?,
        &mut std::collections::HashSet::new(),
        &mut verified,
    )?;
    Ok(verified)
}

fn payload_bytes(payload: &CachedPayload) -> std::io::Result<Cow<'_, [u8]>> {
    match payload {
        CachedPayload::Bytes(bytes) => Ok(Cow::Borrowed(bytes.as_slice())),
        CachedPayload::File(path) => Ok(Cow::Owned(std::fs::read(path)?)),
    }
}

/// Every cached parent with imports needs its original producer baseline,
/// including old cache entries whose imported timestamps happen to match.
pub(super) fn verify_cached_imports(
    primary: &CachedPayload,
    manifest: Option<&CachedPayload>,
) -> std::io::Result<()> {
    let bytes = payload_bytes(primary)?;
    let records = imports(&bytes)?;
    if records.is_empty() {
        return Ok(());
    }
    let manifest = manifest.ok_or_else(|| invalid("cached producer baseline missing"))?;
    let sidecar = payload_bytes(manifest)?;
    let provenance = validate(&bytes, &sidecar, &records)?;
    verify_current(
        &provenance,
        &mut std::collections::HashSet::new(),
        &mut HashMap::new(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn unavailable_compiler(reason: &str, required: bool) {
        assert!(
            !required,
            "ZCCACHE_PCH_TESTS_REQUIRED=1 requires Clang >= 19 for provenance tests: {reason}"
        );
        eprintln!("skipping PCH provenance fixture: {reason}");
    }

    fn clang_compiler() -> Option<&'static NormalizedPath> {
        static COMPILER: std::sync::OnceLock<Result<NormalizedPath, String>> =
            std::sync::OnceLock::new();
        let result = COMPILER.get_or_init(|| {
            let compiler = crate::test_support::find_clang()
                .ok_or_else(|| "Clang compiler is unavailable".to_owned())?;
            let output = Command::new(&compiler)
                .arg("--version")
                .output()
                .map_err(|error| format!("{}: {error}", compiler.display()))?;
            let version = String::from_utf8_lossy(&output.stdout);
            let major = version
                .split("version ")
                .nth(1)
                .and_then(|version| version.split('.').next())
                .and_then(|major| major.parse::<u32>().ok());
            if !output.status.success() || major.is_none_or(|major| major < 19) {
                return Err(format!(
                    "{} does not provide supported Clang >= 19: {}",
                    compiler.display(),
                    version.trim()
                ));
            }
            Ok(compiler)
        });
        match result {
            Ok(compiler) => Some(compiler),
            Err(reason) => {
                unavailable_compiler(
                    reason,
                    std::env::var("ZCCACHE_PCH_TESTS_REQUIRED").as_deref() == Ok("1"),
                );
                None
            }
        }
    }

    #[test]
    #[should_panic(expected = "ZCCACHE_PCH_TESTS_REQUIRED=1 requires Clang >= 19")]
    fn required_missing_compiler_is_a_failure() {
        unavailable_compiler("Clang compiler is unavailable", true);
    }

    fn clang(root: &Path, args: &[&str]) {
        let compiler = clang_compiler().expect("fixture must establish compiler prerequisites");
        let output = Command::new(compiler)
            .current_dir(root)
            .args(args)
            .args([
                "-Werror=invalid-pch",
                "-fpch-validate-input-files-content",
                "-Wno-unused-command-line-argument",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn fixture() -> Option<tempfile::TempDir> {
        clang_compiler()?;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(
            root.join("base.h"),
            "#pragma once\nconstexpr int value = 1;\nstruct A { int x; };\n",
        )
        .unwrap();
        std::fs::write(
            root.join("top.h"),
            "#pragma once\nconstexpr int top_value = value;\n",
        )
        .unwrap();
        clang(
            root,
            &[
                "-x",
                "c++-header",
                "base.h",
                "-o",
                "base.pch",
                "-verify-pch",
            ],
        );
        clang(
            root,
            &[
                "-x",
                "c++-header",
                "top.h",
                "-include-pch",
                "base.pch",
                "-o",
                "top.pch",
                "-verify-pch",
            ],
        );
        Some(directory)
    }

    #[test]
    fn producer_recording_requires_executed_import_snapshot() {
        let Some(directory) = fixture() else { return };
        let root = directory.path();
        let top = root.join("top.pch");
        assert!(
            record_pch_imports_for_output(&top, &top, None).is_err(),
            "physical imports cannot establish which bytes the producer actually loaded"
        );
    }

    #[test]
    fn cached_parent_requires_bound_original_manifest() {
        let Some(directory) = fixture() else { return };
        let root = directory.path();
        let pch = root.join("top.pch");
        record_pch_imports(&pch, None).unwrap();
        let mut bytes = std::fs::read(&pch).unwrap();
        let manifest = CachedPayload::Bytes(Arc::new(std::fs::read(sidecar_path(&pch)).unwrap()));
        let primary = CachedPayload::Bytes(Arc::new(bytes.clone()));
        verify_cached_imports(&primary, Some(&manifest)).unwrap();
        assert!(verify_cached_imports(&primary, None).is_err());
        let corrupt = CachedPayload::Bytes(Arc::new(b"{}".to_vec()));
        assert!(verify_cached_imports(&primary, Some(&corrupt)).is_err());
        bytes.push(0);
        let replaced = CachedPayload::Bytes(Arc::new(bytes));
        assert!(verify_cached_imports(&replaced, Some(&manifest)).is_err());
    }

    #[test]
    fn imported_hashes_freeze_verified_bytes_without_rewinding_original() {
        let Some(directory) = fixture() else { return };
        let root = directory.path();
        let top = root.join("top.pch");
        let base = root.join("base.pch");
        record_pch_imports(&top, None).unwrap();
        let moved = std::fs::metadata(&base).unwrap().modified().unwrap()
            + std::time::Duration::from_secs(100);
        std::fs::File::options()
            .write(true)
            .open(&base)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(moved))
            .unwrap();
        let hashes = verified_pch_import_hashes(&top).unwrap();
        let expected = crate::fingerprint::pch_inputs::content_xxh3(&std::fs::read(&base).unwrap());
        let record = imports(&std::fs::read(&top).unwrap())
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(hashes[&record.path], expected);
        assert_eq!(std::fs::metadata(&base).unwrap().modified().unwrap(), moved);
        std::fs::remove_file(sidecar_path(&top)).unwrap();
        assert!(
            verified_pch_import_hashes(&top).is_err(),
            "external unsigned import drift needs a producer baseline"
        );
    }

    #[test]
    fn recording_uses_frozen_import_despite_same_size_same_mtime_replacement() {
        let Some(directory) = fixture() else { return };
        let root = directory.path();
        let base = root.join("base.pch");
        let top = root.join("top.pch");
        let original = std::fs::read(&base).unwrap();
        let original_time = std::fs::metadata(&base).unwrap().modified().unwrap();
        let snapshot = root.join("base-snapshot.pch");
        std::fs::write(&snapshot, &original).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&snapshot)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(original_time))
            .unwrap();
        let record = imports(&std::fs::read(&top).unwrap())
            .unwrap()
            .pop()
            .unwrap();
        let overlay = root.join("overlay.json");
        std::fs::write(&overlay, serde_json::to_vec(&serde_json::json!({"version":0,"use-external-names":false,"roots":[{"type":"file","name":record.path,"external-contents":snapshot}]})).unwrap()).unwrap();
        std::fs::write(
            root.join("base.h"),
            "#pragma once\nconstexpr int value = 2;\nstruct A { int x; };\n",
        )
        .unwrap();
        clang(
            root,
            &[
                "-x",
                "c++-header",
                "base.h",
                "-o",
                "base.pch",
                "-verify-pch",
            ],
        );
        std::fs::File::options()
            .write(true)
            .open(&base)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(original_time))
            .unwrap();
        assert!(record_pch_imports_for_output(&top, &top, None).is_err());
        record_pch_imports(&top, Some(&overlay)).unwrap();
        let manifest: Provenance =
            serde_json::from_slice(&std::fs::read(sidecar_path(&top)).unwrap()).unwrap();
        assert_eq!(
            manifest.imports[0].blake3,
            crate::hash::hash_bytes(&original).to_hex()
        );
        assert!(verify_pch_imports(&top).is_err());
    }

    #[test]
    fn detached_companion_snapshot_survives_logical_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let logical = directory.path().join("top.pch.zccache-imports.json");
        std::fs::write(&logical, b"old producer").unwrap();
        let mut owner = None;
        let private =
            preserve_import_sidecar(&logical, b"old producer", &mut owner, directory.path())
                .unwrap();
        std::fs::write(&logical, b"next producer").unwrap();
        assert_eq!(std::fs::read(private).unwrap(), b"old producer");
    }

    #[test]
    fn producer_without_imports_clears_obsolete_companion() {
        let Some(directory) = fixture() else { return };
        let root = directory.path();
        let logical = root.join("top.pch");
        record_pch_imports(&logical, None).unwrap();
        assert!(sidecar_path(&logical).exists());
        assert!(
            record_pch_imports_for_output(&root.join("base.pch"), &logical, None)
                .unwrap()
                .is_none()
        );
        assert!(!sidecar_path(&logical).exists());
    }

    #[test]
    fn same_size_rebuilt_import_cannot_inherit_old_parent_provenance() {
        let Some(directory) = fixture() else { return };
        let root = directory.path();
        let original = std::fs::read(root.join("base.pch")).unwrap();
        record_pch_imports(&root.join("top.pch"), None).unwrap();
        std::fs::write(
            root.join("base.h"),
            "#pragma once\nconstexpr int value = 2;\nstruct A { int x; };\n",
        )
        .unwrap();
        clang(
            root,
            &[
                "-x",
                "c++-header",
                "base.h",
                "-o",
                "base.pch",
                "-verify-pch",
            ],
        );
        let replacement = std::fs::read(root.join("base.pch")).unwrap();
        assert_eq!(original.len(), replacement.len());
        assert_ne!(
            crate::hash::hash_bytes(&original),
            crate::hash::hash_bytes(&replacement)
        );
        assert!(
            verify_pch_imports(&root.join("top.pch")).is_err(),
            "same-size replacement must be refused even if clang import timestamps match"
        );
    }
}
