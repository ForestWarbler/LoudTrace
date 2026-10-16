use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

const STORE_VERSION: u64 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnalysisMode {
    #[default]
    Track,
    Album,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    pub id: Uuid,
    pub path: PathBuf,
}

impl Track {
    pub fn new(path: PathBuf) -> Self {
        Self {
            id: Uuid::new_v4(),
            path,
        }
    }

    pub fn display_name(&self) -> String {
        self.path
            .file_name()
            .unwrap_or_else(|| self.path.as_os_str())
            .to_string_lossy()
            .into_owned()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub tracks: Vec<Track>,
    pub mode: AnalysisMode,
}

impl Project {
    pub fn new(name: String) -> Self {
        let name = name.trim();
        Self {
            id: Uuid::new_v4(),
            name: if name.is_empty() {
                "Untitled Project".to_owned()
            } else {
                name.to_owned()
            },
            tracks: Vec::new(),
            mode: AnalysisMode::default(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Library {
    pub projects: Vec<Project>,
}

#[derive(Serialize, Deserialize)]
struct StoreEnvelope {
    version: u64,
    library: Library,
}

pub struct ProjectStore {
    path: PathBuf,
}

impl ProjectStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn default_path() -> PathBuf {
        if let Some(directory) = std::env::var_os("LOUDTRACE_DATA_DIR")
            && !directory.is_empty()
        {
            return PathBuf::from(directory).join("projects.json");
        }

        directories::ProjectDirs::from("", "", "LoudTrace")
            .map(|dirs| dirs.data_local_dir().to_path_buf())
            .unwrap_or_else(|| std::env::temp_dir().join("LoudTrace"))
            .join("projects.json")
    }

    pub fn acquire_lock(&self) -> Result<fs::File, String> {
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not access the project directory: {error}"))?;
        let path = parent.join(".lock");
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("Could not open the project library lock: {error}"))?;
        lock.try_lock()
            .map_err(|error| format!("Could not lock the project library: {error}"))?;
        // Keep this handle alive for the entire writable application session.
        // Never delete the lock file: another process may already hold it open.
        Ok(lock)
    }

    pub fn load(&self) -> Result<Library, String> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Library::default());
            }
            Err(error) => {
                return Err(format!(
                    "Could not read project library {}: {error}",
                    self.path.display()
                ));
            }
        };

        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Project library is not valid JSON: {error}"))?;
        let version = value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "Project library has no valid format version".to_owned())?;
        if version != STORE_VERSION {
            return Err(format!(
                "Unsupported project library version {version}; this application supports version {STORE_VERSION}"
            ));
        }

        let envelope: StoreEnvelope = serde_json::from_value(value)
            .map_err(|error| format!("Project library has an invalid structure: {error}"))?;
        validate_library(&envelope.library)?;
        Ok(envelope.library)
    }

    pub fn save(&self, library: &Library) -> Result<(), String> {
        validate_library(library)?;

        // Never replace an unreadable, corrupt, or newer-format library.
        self.load()?;

        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create project directory: {error}"))?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(|error| format!("Could not create temporary project file: {error}"))?;

        let envelope = StoreEnvelope {
            version: STORE_VERSION,
            library: library.clone(),
        };
        serde_json::to_writer_pretty(&mut temporary, &envelope)
            .map_err(|error| format!("Could not serialize project library: {error}"))?;
        temporary
            .write_all(b"\n")
            .and_then(|()| temporary.flush())
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|error| format!("Could not write project library: {error}"))?;

        // The temporary file shares the destination filesystem, so replacement is atomic.
        temporary
            .persist(&self.path)
            .map_err(|error| format!("Could not replace project library: {error}"))?;

        // Some filesystems do not support directory synchronization. The file itself
        // is already synchronized, and a rejected directory sync must not imply rollback.
        #[cfg(unix)]
        if let Ok(directory) = fs::File::open(parent) {
            let _ = directory.sync_all();
        }

        Ok(())
    }
}

fn validate_library(library: &Library) -> Result<(), String> {
    let mut ids = HashSet::new();
    for project in &library.projects {
        if !ids.insert(project.id) {
            return Err("Project library contains duplicate identifiers".to_owned());
        }
        if project.name.trim().is_empty() {
            return Err("Project names cannot be empty".to_owned());
        }
        for track in &project.tracks {
            if !ids.insert(track.id) {
                return Err("Project library contains duplicate identifiers".to_owned());
            }
            if track.path.as_os_str().is_empty() {
                return Err("Track paths cannot be empty".to_owned());
            }
        }
    }
    Ok(())
}

// This stamp detects ordinary file changes for UI invalidation. It is not a
// content hash; every explicit analysis must read the audio file again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStamp {
    pub len: u64,
    pub modified: SystemTime,
}

pub fn file_stamp(path: &Path) -> Result<FileStamp, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Could not access {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    Ok(FileStamp {
        len: metadata.len(),
        modified: metadata
            .modified()
            .map_err(|error| format!("Could not read modification time: {error}"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_library() -> Library {
        let mut project = Project::new("  夜间专辑  ".to_owned());
        project.mode = AnalysisMode::Album;
        project
            .tracks
            .push(Track::new(PathBuf::from("/music/歌曲 🦉.wav")));
        Library {
            projects: vec![project],
        }
    }

    #[test]
    fn roundtrip_preserves_unicode_paths_ids_and_mode() {
        let directory = tempfile::tempdir().unwrap();
        let store = ProjectStore::new(directory.path().join("nested/projects.json"));
        let library = sample_library();
        store.save(&library).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].id, library.projects[0].id);
        assert_eq!(loaded.projects[0].name, "夜间专辑");
        assert_eq!(loaded.projects[0].mode, AnalysisMode::Album);
        assert_eq!(
            loaded.projects[0].tracks[0].id,
            library.projects[0].tracks[0].id
        );
        assert_eq!(
            loaded.projects[0].tracks[0].path,
            library.projects[0].tracks[0].path
        );
        assert_eq!(loaded.projects[0].tracks[0].display_name(), "歌曲 🦉.wav");
    }

    #[test]
    fn missing_library_is_empty_without_creating_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing/projects.json");
        let store = ProjectStore::new(path.clone());
        assert!(store.load().unwrap().projects.is_empty());
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn corrupt_library_is_preserved_even_on_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("projects.json");
        let damaged = b"{ truncated file";
        fs::write(&path, damaged).unwrap();
        let store = ProjectStore::new(path.clone());
        assert!(store.load().unwrap_err().contains("valid JSON"));
        assert!(store.save(&Library::default()).is_err());
        assert_eq!(fs::read(path).unwrap(), damaged);
    }

    #[test]
    fn newer_library_format_is_preserved_even_with_unknown_structure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("projects.json");
        let newer = br#"{"version":2,"future_schema":{"anything":true}}"#;
        fs::write(&path, newer).unwrap();
        let store = ProjectStore::new(path.clone());
        assert!(store.load().unwrap_err().contains("Unsupported"));
        assert!(store.save(&sample_library()).is_err());
        assert_eq!(fs::read(path).unwrap(), newer);
    }

    #[test]
    fn invalid_save_leaves_previous_library_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("projects.json");
        let store = ProjectStore::new(path.clone());
        let mut library = sample_library();
        store.save(&library).unwrap();
        let original = fs::read(&path).unwrap();
        library.projects.push(library.projects[0].clone());
        assert!(store.save(&library).unwrap_err().contains("duplicate"));
        assert_eq!(fs::read(path).unwrap(), original);
        assert_eq!(store.load().unwrap().projects.len(), 1);
    }

    #[test]
    fn valid_save_atomically_replaces_previous_library() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("projects.json");
        let store = ProjectStore::new(path);
        let mut library = sample_library();
        store.save(&library).unwrap();
        library.projects[0].name = "Revised album".to_owned();
        store.save(&library).unwrap();
        assert_eq!(store.load().unwrap().projects[0].name, "Revised album");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn serialization_failure_preserves_old_file_and_removes_temporary_file() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("projects.json");
        let store = ProjectStore::new(path.clone());
        let mut library = sample_library();
        store.save(&library).unwrap();
        let original = fs::read(&path).unwrap();
        library.projects[0].tracks[0].path = PathBuf::from(OsString::from_vec(vec![0xff]));
        assert!(store.save(&library).unwrap_err().contains("serialize"));
        assert_eq!(fs::read(path).unwrap(), original);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn blank_names_are_defaulted_at_creation_and_rejected_when_persisting() {
        assert_eq!(Project::new(" \n ".to_owned()).name, "Untitled Project");
        let mut library = sample_library();
        library.projects[0].name = " \t ".to_owned();
        assert!(validate_library(&library).is_err());
    }

    #[test]
    fn duplicate_track_identifiers_and_empty_paths_are_rejected() {
        let mut library = sample_library();
        let track = library.projects[0].tracks[0].clone();
        library.projects[0].tracks.push(track);
        assert!(
            validate_library(&library)
                .unwrap_err()
                .contains("duplicate")
        );
        library.projects[0].tracks.pop();
        library.projects[0].tracks[0].path = PathBuf::new();
        assert!(validate_library(&library).unwrap_err().contains("paths"));
    }

    #[test]
    fn stamp_detects_replacement_and_missing_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("song.wav");
        fs::write(&path, b"first").unwrap();
        let before = file_stamp(&path).unwrap();
        fs::write(&path, b"second version").unwrap();
        assert_ne!(file_stamp(&path).unwrap(), before);
        fs::remove_file(&path).unwrap();
        assert!(file_stamp(&path).is_err());
        assert!(file_stamp(directory.path()).is_err());
    }
}
