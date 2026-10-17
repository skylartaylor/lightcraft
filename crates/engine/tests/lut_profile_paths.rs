//! Synthetic regressions for untrusted persisted LUT profile paths (#2123).
//! No real user files or network shares are accessed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use lightcraft_catalog::MemStore;
use lightcraft_engine::Session;
use lightcraft_engine::cmd::lut_profiles::LutProfile;
use lightcraft_engine::library::LibraryStores;
use lightcraft_pipeline::lut;
use serde_json::json;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("lc-profile-paths-{}-{time}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn id(&self, suffix: &str) -> String {
        format!("lut:{}-{suffix}", self.0.file_name().unwrap().to_string_lossy())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn cube(title: &str) -> String {
    format!("TITLE \"{title}\"\nLUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n")
}

fn profile(id: String, file: &Path) -> LutProfile {
    LutProfile { id, name: "Synthetic LUT".into(), group: "Tests".into(), file: file.to_string_lossy().into_owned() }
}

fn stores(dir: &Path, profiles: &[LutProfile], on_disk: bool) -> LibraryStores {
    let files = MemStore::default();
    files.set("prefs.json", serde_json::to_vec(&json!({"lutProfiles": profiles})).unwrap());
    LibraryStores { dir: dir.to_path_buf(), catalog: Box::new(MemStore::default()), files: Box::new(files), on_disk }
}

#[test]
fn library_open_skips_outside_traversal_and_oversized_profiles() {
    let tmp = Scratch::new();
    let library = tmp.0.join("library");
    let profiles = library.join("Profiles");
    std::fs::create_dir_all(&profiles).unwrap();
    let text = cube("synthetic");
    let good = profiles.join("good.cube");
    let external = tmp.0.join("outside.cube");
    let sibling = library.join("sentinel.cube");
    for file in [&good, &external, &sibling] {
        std::fs::write(file, &text).unwrap();
    }
    let huge = profiles.join("large.cube");
    // A valid LUT followed by one oversized comment. Without the bound it really would
    // register, rather than merely failing to parse an all-zero sparse fixture.
    std::fs::write(&huge, format!("{text}#")).unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(&huge).unwrap();
    f.set_len(64 * 1024 * 1024 + 1).unwrap();
    drop(f);
    let directory = profiles.join("directory.cube");
    std::fs::create_dir(&directory).unwrap();
    let entries = vec![
        profile(tmp.id("good"), &good),
        profile(tmp.id("outside"), &external),
        profile(tmp.id("traversal"), &profiles.join("../sentinel.cube")),
        profile(tmp.id("large"), &huge),
        profile(tmp.id("directory"), &directory),
    ];
    let mut session = Session::new();
    session.open_library_in(stores(&library, &entries, true), false).unwrap();
    assert!(lut::get(&entries[0].id).is_some(), "ordinary in-library LUT loads");
    for entry in &entries[1..] {
        assert!(lut::get(&entry.id).is_none(), "unsafe profile registered: {}", entry.file);
    }
    for entry in &entries[1..3] {
        session.execute("profile.deleteImported", &json!({"id": entry.id})).unwrap();
    }
    assert_eq!(std::fs::read_to_string(&external).unwrap(), text);
    assert_eq!(std::fs::read_to_string(&sibling).unwrap(), text);
    session.execute("profile.deleteImported", &json!({"id": entries[0].id})).unwrap();
    assert!(!good.exists());
    assert!(lut::get(&entries[0].id).is_none());
}

#[test]
fn library_switch_uses_the_new_library_for_profile_containment() {
    let tmp = Scratch::new();
    let first = tmp.0.join("first");
    let second = tmp.0.join("second");
    std::fs::create_dir_all(second.join("Profiles")).unwrap();
    let file = second.join("Profiles/new.cube");
    std::fs::write(&file, cube("new-library")).unwrap();
    let entry = profile(tmp.id("switch"), &file);
    let mut session = Session::new();
    session.open_library_in(stores(&first, &[], true), false).unwrap();
    session.open_library_in(stores(&second, std::slice::from_ref(&entry), true), false).unwrap();
    assert!(lut::get(&entry.id).is_some());
    lut::unregister(&entry.id);
}

#[test]
fn non_disk_preferences_never_reload_host_files() {
    let tmp = Scratch::new();
    std::fs::create_dir(tmp.0.join("Profiles")).unwrap();
    let file = tmp.0.join("Profiles/external.cube");
    std::fs::write(&file, cube("non-disk")).unwrap();
    let entry = profile(tmp.id("non-disk"), &file);
    let mut session = Session::new();
    session.open_library_in(stores(&tmp.0, std::slice::from_ref(&entry), false), false).unwrap();
    assert!(lut::get(&entry.id).is_none());
    session.execute("profile.deleteImported", &json!({"id": entry.id})).unwrap();
    assert!(file.is_file());
}

#[test]
fn explicit_import_reopen_delete_and_ephemeral_import_still_work() {
    let tmp = Scratch::new();
    let library = tmp.0.join("library");
    let source = tmp.0.join("source.cube");
    std::fs::write(&source, cube(&tmp.id("import"))).unwrap();
    let mut session = Session::new();
    session.open_library_in(stores(&library, &[], true), false).unwrap();
    session.execute("profile.import", &json!({"paths": [source]})).unwrap();
    let imported = session.lut_profiles.clone();
    assert_eq!(imported.len(), 1);
    assert!(Path::new(&imported[0].file).starts_with(library.join("Profiles")));
    assert!(lut::get(&imported[0].id).is_some());
    lut::unregister(&imported[0].id);
    session.open_library_in(stores(&library, &imported, true), false).unwrap();
    assert!(lut::get(&imported[0].id).is_some(), "persisted import reloads");
    session.execute("profile.deleteImported", &json!({"id": imported[0].id})).unwrap();
    assert!(!Path::new(&imported[0].file).exists());
    assert!(source.is_file(), "deleting the copy preserves the selected source");

    let mut ephemeral = Session::new();
    ephemeral.execute("profile.import", &json!({"paths": [source]})).unwrap();
    let imported = ephemeral.lut_profiles[0].clone();
    assert!(lut::get(&imported.id).is_some(), "ephemeral import registers directly");
    assert!(imported.file.is_empty(), "ephemeral import needs no shared temporary file");
    ephemeral.execute("profile.deleteImported", &json!({"id": imported.id})).unwrap();
    assert!(lut::get(&imported.id).is_none());
}

#[cfg(any(unix, windows))]
fn link(from: &Path, to: &Path, directory: bool) -> bool {
    #[cfg(unix)]
    let result = {
        let _ = directory;
        std::os::unix::fs::symlink(from, to)
    };
    #[cfg(windows)]
    let result = if directory { std::os::windows::fs::symlink_dir(from, to) } else { std::os::windows::fs::symlink_file(from, to) };
    match result {
        Ok(()) => true,
        Err(e) => {
            lightcraft_testkit::skip!(System, "cannot create synthetic symlink: {e}");
            false
        }
    }
}

#[cfg(any(unix, windows))]
#[test]
fn profiles_root_parent_and_leaf_links_are_not_followed() {
    let tmp = Scratch::new();
    let outside = tmp.0.join("outside");
    std::fs::create_dir(&outside).unwrap();
    let sentinel = outside.join("sentinel.cube");
    let text = cube("linked-sentinel");
    std::fs::write(&sentinel, &text).unwrap();
    for kind in ["root", "parent", "leaf"] {
        let library = tmp.0.join(kind);
        std::fs::create_dir(&library).unwrap();
        let profiles = library.join("Profiles");
        let path = if kind == "root" {
            if !link(&outside, &profiles, true) {
                return;
            }
            profiles.join("sentinel.cube")
        } else {
            std::fs::create_dir(&profiles).unwrap();
            if kind == "parent" {
                if !link(&outside, &profiles.join("nested"), true) {
                    return;
                }
                profiles.join("nested/sentinel.cube")
            } else {
                let leaf = profiles.join("sentinel.cube");
                if !link(&sentinel, &leaf, false) {
                    return;
                }
                leaf
            }
        };
        let entry = profile(tmp.id(kind), &path);
        let mut session = Session::new();
        session.open_library_in(stores(&library, std::slice::from_ref(&entry), true), false).unwrap();
        assert!(lut::get(&entry.id).is_none(), "followed {kind} symlink on open");
        session.execute("profile.deleteImported", &json!({"id": entry.id})).unwrap();
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), text, "followed {kind} symlink on delete");
        assert!(std::fs::symlink_metadata(&path).is_ok(), "unsafe entry must not be removed");
    }
}

#[cfg(any(unix, windows))]
#[test]
fn importing_refuses_a_redirected_profiles_directory() {
    let tmp = Scratch::new();
    let library = tmp.0.join("library");
    let outside = tmp.0.join("outside");
    std::fs::create_dir(&library).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let sentinel = outside.join("sentinel");
    std::fs::write(&sentinel, b"keep").unwrap();
    if !link(&outside, &library.join("Profiles"), true) {
        return;
    }
    let source = tmp.0.join("source.cube");
    std::fs::write(&source, cube(&tmp.id("redirected-import"))).unwrap();
    let mut session = Session::new();
    session.open_library_in(stores(&library, &[], true), false).unwrap();
    assert!(session.execute("profile.import", &json!({"paths": [source]})).is_err());
    assert!(session.lut_profiles.is_empty());
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1, "nothing created outside Profiles");
}

#[cfg(any(unix, windows))]
#[test]
fn importing_claims_a_new_name_instead_of_overwriting_existing_entries() {
    let tmp = Scratch::new();
    let library = tmp.0.join("library");
    let profiles = library.join("Profiles");
    std::fs::create_dir_all(&profiles).unwrap();
    let title = tmp.id("collisions");
    let slug = lightcraft_engine::presets::slug(&format!("Imported-{title}"));
    let sentinel = tmp.0.join("sentinel.cube");
    std::fs::write(&sentinel, b"keep").unwrap();
    let planted = profiles.join(format!("{slug}.cube"));
    if !link(&sentinel, &planted, false) {
        return;
    }
    let dangling = profiles.join(format!("{slug}-2.cube"));
    let missing = tmp.0.join("missing.cube");
    if !link(&missing, &dangling, false) {
        return;
    }
    let occupied = profiles.join(format!("{slug}-3.cube"));
    std::fs::write(&occupied, b"keep existing").unwrap();
    std::fs::create_dir(profiles.join(format!("{slug}-4.cube"))).unwrap();
    let source = tmp.0.join("source.cube");
    let text = cube(&title);
    std::fs::write(&source, &text).unwrap();
    let mut session = Session::new();
    session.open_library_in(stores(&library, &[], true), false).unwrap();
    session.execute("profile.import", &json!({"paths": [source]})).unwrap();
    let imported = &session.lut_profiles[0];
    assert_eq!(imported.id, format!("lut:{slug}-5"));
    assert_eq!(std::fs::read_to_string(&imported.file).unwrap(), text);
    assert!(lut::get(&imported.id).is_some());
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
    assert_eq!(std::fs::read(&occupied).unwrap(), b"keep existing");
    assert!(!missing.exists());
    assert!(std::fs::symlink_metadata(&planted).unwrap().file_type().is_symlink());
    assert!(std::fs::symlink_metadata(&dangling).unwrap().file_type().is_symlink());
    lut::unregister(&imported.id);
}

#[cfg(unix)]
#[test]
fn profile_hard_links_are_not_read() {
    let tmp = Scratch::new();
    let profiles = tmp.0.join("Profiles");
    std::fs::create_dir(&profiles).unwrap();
    let sentinel = tmp.0.join("sentinel.cube");
    std::fs::write(&sentinel, cube("hard-link")).unwrap();
    let file = profiles.join("linked.cube");
    std::fs::hard_link(&sentinel, &file).unwrap();
    let entry = profile(tmp.id("hard-link"), &file);
    let mut session = Session::new();
    session.open_library_in(stores(&tmp.0, std::slice::from_ref(&entry), true), false).unwrap();
    assert!(lut::get(&entry.id).is_none());
}
