//! Imported profiles: `.cube` 3D LUTs used as creative profiles (Amount 0–200 % like the
//! built-in ones). A library on disk keeps a copy of each file in its `Profiles/` folder; the
//! list lives in the library's prefs and the LUTs are registered with the pipeline when the
//! library opens.

use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use serde_json::{Value, json};

use super::{CommandSpec, always, bad, cmd, str_param};
use crate::{Result, Session};

/// One imported LUT profile.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LutProfile {
    /// `lut:<slug>`.
    pub id: String,
    pub name: String,
    pub group: String,
    /// The library's `.cube` copy, or empty for an ephemeral in-memory import.
    pub file: String,
}

fn profiles_dir(s: &Session) -> Option<PathBuf> {
    s.library.as_ref().filter(|l| l.on_disk).map(|l| l.dir.join("Profiles"))
}

// Persisted preferences are untrusted. This cap applies to loading the library's copies, not
// to explicitly selected import sources (which are registered directly by `import`).
const MAX_PROFILE_BYTES: u64 = 64 * 1024 * 1024;

fn invalid_profile_path() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "profile must be a regular file inside the library's Profiles directory")
}

/// Check containment without touching the filesystem. In particular, never probe a persisted
/// UNC/device path just to discover that it is outside the library.
fn profile_relative_path<'a>(profiles: &Path, file: &'a Path) -> io::Result<&'a Path> {
    // Trust only the exact selected library prefix. It can itself be relative, canonical
    // Windows syntax, or a network library; an unrelated namespace never reaches metadata.
    let relative = file.strip_prefix(profiles).map_err(|_| invalid_profile_path())?;
    if relative.as_os_str().is_empty()
        || relative.components().any(|c| match c {
            Component::Normal(name) => !ordinary_profile_component(&name.to_string_lossy()),
            _ => true,
        })
    {
        return Err(invalid_profile_path());
    }
    Ok(relative)
}

fn ordinary_profile_component(name: &str) -> bool {
    // Apply Windows rules on every platform: do not let a portable library carry alternate
    // data streams, DOS device names, or names that become traversal after Win32 trimming.
    if name.contains(['\\', ':']) || name.ends_with([' ', '.']) {
        return false;
    }
    let stem = name.split('.').next().unwrap_or_default().trim_end_matches(' ').to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$") {
        return false;
    }
    !["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(*prefix).is_some_and(|suffix| matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"))
    })
}

fn redirected(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Includes directory junctions and other reparse points, not just symbolic links.
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    metadata.file_type().is_symlink()
}

fn check_profile_directory(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || redirected(&metadata) {
        return Err(invalid_profile_path());
    }
    Ok(())
}

/// Reject static links in Profiles and every descendant before opening or deleting a file.
/// The selected library's own ancestry is trusted. These path checks do not defend against a
/// local process concurrently replacing an ancestor directory; that needs handle-relative I/O.
fn profile_file(library_dir: &Path, file: &str) -> io::Result<PathBuf> {
    let profiles = library_dir.join("Profiles");
    let relative = profile_relative_path(&profiles, Path::new(file))?;
    let mut directory = profiles.clone();
    check_profile_directory(&directory)?;
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            directory.push(component);
            check_profile_directory(&directory)?;
        }
    }
    let path = profiles.join(relative);
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.is_file() || redirected(&metadata) {
        return Err(invalid_profile_path());
    }
    Ok(path)
}

/// Register the library's imported LUTs; missing, unsafe or oversized copies are skipped.
/// `library_dir` is the library being opened, not the session's still-current old library.
/// Non-disk libraries never read host paths from preferences. Ephemeral imports register their
/// parsed LUT directly in `import`, so they do not need to reload a temporary file here.
pub fn register_all(s: &Session, library_dir: Option<&Path>) {
    let Some(dir) = library_dir else { return };
    for p in &s.lut_profiles {
        let text = profile_file(dir, &p.file)
            .and_then(|path| lightcraft_catalog::safe_file::read_regular(&path, MAX_PROFILE_BYTES))
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok());
        if let Some(l) = text.and_then(|t| lightcraft_pipeline::lut::Lut3d::parse_cube(&t).ok()) {
            lightcraft_pipeline::lut::register(&p.id, l);
        }
    }
}

/// `.cube` files in `paths` (folders searched, zip bundles opened), and the files that couldn't
/// be read with the reason (`[name, reason]`).
fn cube_files(paths: &[String]) -> (Vec<(String, String, Option<String>)>, Vec<Value>) {
    // (name, text, group)
    let mut out = Vec::new();
    let mut failed = Vec::new();
    let stem = |f: &Path| f.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    for p in paths {
        let path = Path::new(p);
        if path.is_dir() {
            let cube = |f: &Path| f.extension().is_some_and(|x| x.eq_ignore_ascii_case("cube"));
            for ep in crate::walk::files_in(path, None, crate::walk::Limits::default(), cube).files {
                match std::fs::read_to_string(&ep) {
                    Ok(t) => {
                        let group = ep.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().to_string());
                        out.push((stem(&ep), t, group));
                    }
                    Err(e) => failed.push(json!([stem(&ep), e.to_string()])),
                }
            }
        } else if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")) {
            let b = match std::fs::read(path) {
                Ok(b) => b,
                Err(e) => {
                    failed.push(json!([stem(path), e.to_string()]));
                    continue;
                }
            };
            for (inner, data) in crate::preset_import::read_zip(&b).unwrap_or_default() {
                if inner.to_ascii_lowercase().ends_with(".cube") {
                    let ip = Path::new(&inner);
                    let group = ip
                        .parent()
                        .and_then(|d| d.file_name())
                        .map(|n| n.to_string_lossy().to_string())
                        .or_else(|| path.file_stem().map(|n| n.to_string_lossy().to_string()));
                    out.push((
                        ip.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                        String::from_utf8_lossy(&data).to_string(),
                        group,
                    ));
                }
            }
        } else {
            match std::fs::read_to_string(path) {
                Ok(t) => out.push((stem(path), t, None)),
                Err(e) => failed.push(json!([stem(path), e.to_string()])),
            }
        }
    }
    (out, failed)
}

/// Claim a never-before-existing destination and write only through that handle. Existing
/// names (including dangling links) are collisions, never overwrite targets. A write failure
/// may leave an unreferenced partial copy; do not unlink a name another process could replace.
fn store_imported_profile(s: &Session, dir: Option<&Path>, slug: &str, text: &str) -> io::Result<(String, String)> {
    for n in 1..=10_000 {
        let id = if n == 1 { format!("lut:{slug}") } else { format!("lut:{slug}-{n}") };
        if s.lut_profiles.iter().any(|p| p.id == id) {
            continue;
        }
        let Some(dir) = dir else {
            // Registration below owns the parsed LUT for the session. There is no need to
            // create a predictable shared temporary file just to remember an ephemeral LUT.
            return Ok((id, String::new()));
        };
        check_profile_directory(dir)?;
        let path = dir.join(format!("{}.cube", id.trim_start_matches("lut:")));
        let mut file = match lightcraft_catalog::safe_file::open_regular(&path, std::fs::OpenOptions::new().write(true).create_new(true)) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists || std::fs::symlink_metadata(&path).is_ok() => continue,
            Err(e) => return Err(e),
        };
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        lightcraft_catalog::safe_file::sync_dir(dir);
        return Ok((id, path.to_string_lossy().into_owned()));
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free imported profile name"))
}

fn import(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "profile.import";
    let paths: Vec<String> =
        p.get("paths").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
    if paths.is_empty() {
        return Err(bad(C, "no paths"));
    }
    let dir = profiles_dir(s);
    if let Some(d) = &dir {
        let prepare = || -> io::Result<()> {
            match std::fs::symlink_metadata(d) {
                Ok(_) => check_profile_directory(d)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => std::fs::create_dir_all(d)?,
                Err(e) => return Err(e),
            }
            check_profile_directory(d)
        };
        prepare().map_err(|e| bad(C, format!("{}: {e}", d.display())))?;
    }
    let (files, mut failed) = cube_files(&paths);
    let mut imported = Vec::new();
    for (name, text, group) in files {
        let lut = match lightcraft_pipeline::lut::Lut3d::parse_cube(&text) {
            Ok(l) => l,
            Err(e) => {
                failed.push(json!([name, e]));
                continue;
            }
        };
        let display = lut.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| name.clone());
        let group = super::super::preset_import::group_from_dir(&group.unwrap_or_default()).unwrap_or_else(|| "Imported".into());
        let slug = crate::presets::slug(&format!("{group}-{display}"));
        let (id, file) = store_imported_profile(s, dir.as_deref(), &slug, &text).map_err(|e| bad(C, e.to_string()))?;
        lightcraft_pipeline::lut::register(&id, lut);
        s.lut_profiles.push(LutProfile { id: id.clone(), name: display.clone(), group: group.clone(), file });
        imported.push(json!({"id": id, "name": display, "group": group}));
    }
    s.save_prefs()?;
    // presets that name these profiles use them now
    crate::profile_refs::refresh(s);
    Ok(json!({"imported": imported, "failed": failed}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "profile.import",
            "Import Profiles",
            [],
            None,
            "{paths: [.cube file, folder or .zip]} — 3D LUTs become creative profiles (Amount 0–200 %; rendered on the CPU) in the profile browser's groups (folder / zip name, else Imported) → {imported: [{id, name, group}], failed}",
            always,
            import
        ),
        cmd!("profile.deleteImported", "Delete Imported Profile", [], None, "{id: lut:…}", always, |s, p| {
            let id = str_param(p, "id").ok_or_else(|| bad("profile.deleteImported", "missing id"))?.to_string();
            let Some(i) = s.lut_profiles.iter().position(|x| x.id == id) else {
                return Err(bad("profile.deleteImported", format!("no imported profile `{id}`")));
            };
            let gone = s.lut_profiles.remove(i);
            if let Some(library) = s.library.as_ref().filter(|l| l.on_disk)
                && let Ok(file) = profile_file(&library.dir, &gone.file)
            {
                let _ = std::fs::remove_file(file);
            }
            lightcraft_pipeline::lut::unregister(&id);
            s.save_prefs()?;
            crate::profile_refs::refresh(s);
            Ok(json!({"deleted": id}))
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_profile_paths_are_rejected_before_filesystem_access() {
        // These paths need not exist: namespace and containment validation is purely lexical.
        let root = Path::new("test-library/Profiles");
        for file in [
            "elsewhere/look.cube",
            "test-library/Profiles-other/look.cube",
            "test-library/Profiles/../sentinel.cube",
            "test-library/Profiles/sub/../../sentinel.cube",
            "test-library/Profiles/look.cube:stream",
            "test-library/Profiles/NUL.cube",
            "test-library/Profiles/CON .cube",
            "test-library/Profiles/COM1.cube",
            "test-library/Profiles/LPT¹.cube",
            "test-library/Profiles/CONOUT$",
            "test-library/Profiles/.. /look.cube",
            "test-library/Profiles/sub\\..\\sentinel.cube",
            "//server/share/look.cube",
            r"\\server\share\look.cube",
            r"\\?\C:\outside\look.cube",
            r"\\.\pipe\look.cube",
            r"\??\C:\outside\look.cube",
        ] {
            assert_eq!(profile_relative_path(root, Path::new(file)).unwrap_err().kind(), io::ErrorKind::InvalidInput, "{file}");
        }
        assert!(profile_relative_path(root, root).is_err());
        assert_eq!(profile_relative_path(root, &root.join("group/look.cube")).unwrap(), Path::new("group/look.cube"));
        // A user-selected relative library remains supported; traversal in the untrusted
        // part of a profile path is still rejected.
        let relative_root = Path::new("../test-library/Profiles");
        assert_eq!(profile_relative_path(relative_root, &relative_root.join("look.cube")).unwrap(), Path::new("look.cube"));
        assert!(profile_relative_path(relative_root, &relative_root.join("../sentinel.cube")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn selected_windows_library_prefixes_are_not_confused_with_outside_namespaces() {
        for root in [r"C:\library\Profiles", r"\\?\C:\library\Profiles", r"\\server\share\library\Profiles"] {
            let root = Path::new(root);
            assert_eq!(profile_relative_path(root, &root.join("look.cube")).unwrap(), Path::new("look.cube"));
            assert!(profile_relative_path(root, Path::new(r"\\other\share\look.cube")).is_err());
            assert!(profile_relative_path(root, Path::new(r"\\.\pipe\look.cube")).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_profile_fifo_is_rejected_without_opening_it() {
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("lc-profile-fifo-{}-{unique}", std::process::id()));
        let profiles = dir.join("Profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        let fifo = profiles.join("pipe.cube");
        let result = std::process::Command::new("mkfifo").arg(&fifo).output();
        if !result.as_ref().is_ok_and(|r| r.status.success()) {
            lightcraft_testkit::skip!(System, "mkfifo unavailable: {result:?}");
            std::fs::remove_dir_all(dir).unwrap();
            return;
        }
        // No writer exists. Testing the validation step directly means a regression fails
        // an assertion rather than hanging the suite in a blocking FIFO read.
        let result = profile_file(&dir, &fifo.to_string_lossy());
        std::fs::remove_dir_all(dir).unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
}
