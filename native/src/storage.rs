// SPDX-License-Identifier: GPL-3.0-or-later

//! Filesystem-backed recording library.
//!
//! JSON sidecars next to the media are the only source of truth: there is no
//! database, thumbnail cache, or recursive crawl. `Storage` owns the configured
//! recording directory plus the capture root's `replay`/`regular`/`staging`
//! directories, and provides the scan, finalize, mutate, delete, evict, and
//! startup-sweep operations. FFmpeg work lives in `media_jobs`; this module
//! never spawns a process.
//!
//! `finalize` takes the already-combined media produced by the media worker, so
//! storage stays process-free. `update`, `delete`, and `enforce_limit` take
//! the scanned `LibraryEntry`, which already carries its
//! sidecar/media paths, so no second in-memory index is needed.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::{IgnoredAny, Visitor};
use serde::{Deserialize, Serialize};

use crate::domain::{
    ActivityDetails, Category, GameFlavor, LibraryEntry, MediaFacts, Outcome, PlayerSummary,
    RecordingDraft, RecordingId, StorageLimit, TimelineItem, TimelineShape,
};
use crate::recorder::CaptureArtifacts;

/// Schema version written into every native sidecar.
pub const SIDECAR_SCHEMA_VERSION: u32 = 1;

/// Directory (under the storage root) the startup sweep quarantines
/// unreferenced artifacts into.
pub const RECOVERY_DIR: &str = "Recovery";

const MEDIA_EXTENSION: &str = "mp4";
const SIDECAR_EXTENSION: &str = "json";

/// Media produced by the media worker for a finished capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CombinedMedia {
    /// Exclusively created temp file holding the final playable media.
    pub temp_media: PathBuf,
    /// Usable replay lead-in actually present at the front of `temp_media`.
    /// Zero for the regular-only fallback.
    pub actual_replay_ms: u64,
    /// Length after the tail was cut off; `None` keeps the whole capture.
    pub trimmed_ms: Option<u64>,
    pub facts: MediaFacts,
}

/// One sidecar that could not be turned into an entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedEntry {
    pub sidecar_path: PathBuf,
    pub reason: String,
}

/// Result of a library scan. Holds no file handles or UI objects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LibraryIndex {
    /// Reverse chronological.
    pub entries: Arc<Vec<LibraryEntry>>,
    pub skipped: Vec<SkippedEntry>,
    /// Bounded summary: how many unrelated/unsupported files were ignored.
    pub ignored_files: usize,
}

impl LibraryIndex {
    /// Fold in an entry the media worker just wrote, keeping the newest-first
    /// order a full scan produces. No file is read.
    pub fn upsert_entry(&mut self, entry: LibraryEntry) {
        let entries = Arc::make_mut(&mut self.entries);
        if let Some(existing) = entries
            .iter()
            .position(|candidate| candidate.id == entry.id)
        {
            entries.remove(existing);
        }
        let position = match entries.binary_search_by(|probe| entry_order(probe, &entry)) {
            Ok(position) | Err(position) => position,
        };
        entries.insert(position, entry);
    }

    /// Drop the given ids (those actually present).
    pub fn remove_entries(&mut self, ids: &[RecordingId]) {
        Arc::make_mut(&mut self.entries).retain(|entry| !ids.contains(&entry.id));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryUpdate {
    Protected(bool),
    /// An empty or whitespace-only tag clears it.
    Tag(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeleteResult {
    pub deleted: Vec<RecordingId>,
    pub failures: Vec<(RecordingId, String)>,
    /// A deletion removed the media but could not remove its sidecar, so the
    /// library no longer matches the directory. The caller has to rescan.
    pub partially_deleted: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvictionResult {
    pub evicted: Vec<RecordingId>,
    pub remaining_bytes: u64,
    /// A deletion removed the media but could not remove its sidecar, so the
    /// library no longer matches the directory even though nothing was fully
    /// evicted. The caller has to rescan.
    pub partially_deleted: bool,
    /// Protected content alone exceeds a positive limit; nothing was deleted
    /// for it.
    pub protected_over_limit: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Quarantined files, by their new location under `Recovery/`.
    pub quarantined: Vec<PathBuf>,
    pub failures: Vec<String>,
}

pub struct Storage {
    root: PathBuf,
    replay_dir: PathBuf,
    regular_dir: PathBuf,
    staging_dir: PathBuf,
}

impl Storage {
    /// `root` is the configured recording directory; `capture_root` holds GSR's
    /// `replay`, `regular`, and `staging` subdirectories.
    pub fn new(root: impl Into<PathBuf>, capture_root: impl AsRef<Path>) -> Self {
        let capture_root = capture_root.as_ref();
        Self {
            root: root.into(),
            replay_dir: capture_root.join("replay"),
            regular_dir: capture_root.join("regular"),
            staging_dir: capture_root.join("staging"),
        }
    }

    pub fn staging_dir(&self) -> &Path {
        &self.staging_dir
    }

    /// Create the directories this module writes into.
    pub fn prepare(&self) -> io::Result<()> {
        fs::create_dir_all(&self.root)?;
        fs::create_dir_all(&self.staging_dir)
    }

    // --- Scan ---

    /// Read every sidecar at the configured directory level. Unrelated files are
    /// counted, unreadable sidecars are reported, and nothing is repaired.
    pub fn scan(&self) -> LibraryIndex {
        let mut index = LibraryIndex::default();
        // Loaded in directory order, sorted once at the end.
        let mut scanned: Vec<LibraryEntry> = Vec::new();

        let Ok(read_dir) = fs::read_dir(&self.root) else {
            return index;
        };
        let mut paths: Vec<PathBuf> = read_dir
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .map(|entry| entry.path())
            .collect();
        // Deterministic order regardless of directory iteration order.
        paths.sort();

        for path in paths {
            match path.extension().and_then(|value| value.to_str()) {
                Some(SIDECAR_EXTENSION) => {}
                // Media is discovered through its sidecar; the startup sweep
                // deals with anything unreferenced.
                Some(MEDIA_EXTENSION) => continue,
                _ => {
                    index.ignored_files += 1;
                    continue;
                }
            }

            match self.load_sidecar(&path) {
                Ok(entry) => scanned.push(entry),
                Err(reason) => index.skipped.push(SkippedEntry {
                    sidecar_path: path,
                    reason,
                }),
            }
        }

        // The one library ordering: newest first, ties broken by media path.
        scanned.sort_by(entry_order);
        index.entries = Arc::new(scanned);
        index
    }

    fn load_sidecar(&self, path: &Path) -> Result<LibraryEntry, String> {
        let text = fs::read_to_string(path).map_err(|error| format!("unreadable: {error}"))?;
        let sidecar = serde_json::from_str::<NativeSidecar>(&text)
            .map_err(|error| format!("invalid sidecar: {error}"))?;
        if sidecar.schema_version > SIDECAR_SCHEMA_VERSION {
            return Err(format!(
                "sidecar schema version {} is newer than {SIDECAR_SCHEMA_VERSION}",
                sidecar.schema_version
            ));
        }
        let media_path = self.root.join(&sidecar.media_file);
        self.check_owned(&media_path)?;
        let has_content = media_has_content(&media_path)?;
        let mut entry = sidecar.into_entry(media_path, path.to_path_buf());
        entry.media.has_content = has_content;
        entry.validate().map_err(|error| error.to_string())?;
        Ok(entry)
    }

    // --- Finalization ---

    /// Turn a finished draft plus its combined media into a library entry:
    /// write the sidecar temp, rename media then sidecar, and only then remove
    /// the GSR intermediates. A crash between the renames leaves media the
    /// startup sweep quarantines; nothing playable is deleted.
    pub fn finalize(
        &self,
        draft: &RecordingDraft,
        artifacts: &CaptureArtifacts,
        media: &CombinedMedia,
    ) -> io::Result<LibraryEntry> {
        let media_start_ms = artifacts.regular_started_at_ms - media.actual_replay_ms as i64;
        let regular_ms = artifacts
            .regular_stopped_at_ms
            .saturating_sub(artifacts.regular_started_at_ms)
            .max(0) as u64;
        let captured_ms = media.actual_replay_ms + regular_ms;
        let duration_ms = media
            .trimmed_ms
            .map_or(captured_ms, |trimmed| trimmed.min(captured_ms));

        let title = draft
            .title
            .clone()
            .unwrap_or_else(|| default_title(&draft.category));
        let stem = unique_stem(&draft.id, draft.started_at_ms, &title);
        let (media_path, sidecar_path) = self.claim_output(&stem)?;

        let entry = LibraryEntry {
            id: draft.id.clone(),
            media_path,
            sidecar_path,
            category: draft.category.clone(),
            flavor: draft.flavor.clone(),
            title,
            start_unix_ms: draft.started_at_ms,
            duration_ms,
            outcome: draft.outcome.unwrap_or(Outcome::Unknown),
            protected: false,
            tag: None,
            player: draft.player.clone(),
            details: draft.details.clone(),
            timeline: shift_timeline(
                &draft.timeline,
                draft.started_at_ms,
                media_start_ms,
                duration_ms,
            ),
            media: media.facts.clone(),
        };
        self.write_new_entry(&entry, &media.temp_media)?;

        // Both final names exist: the intermediates are now safe to remove.
        if let Some(replay) = artifacts.replay.as_deref() {
            let _ = fs::remove_file(replay);
        }
        let _ = fs::remove_file(&artifacts.regular);

        Ok(entry)
    }

    /// Claim `<stem>.mp4` exclusively (adding a numeric suffix on collision) and
    /// return it with its sidecar path. Identifiers, not titles, provide the
    /// uniqueness; the suffix only guards an exact-name collision.
    pub fn claim_output(&self, stem: &str) -> io::Result<(PathBuf, PathBuf)> {
        for attempt in 0..100u32 {
            let candidate = if attempt == 0 {
                stem.to_owned()
            } else {
                format!("{stem} ({attempt})")
            };
            let media_path = self.root.join(format!("{candidate}.{MEDIA_EXTENSION}"));
            let sidecar_path = self.root.join(format!("{candidate}.{SIDECAR_EXTENSION}"));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&media_path)
            {
                Ok(_) => return Ok((media_path, sidecar_path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free output name",
        ))
    }

    /// Write the sidecar temp, move the media into place, then rename the
    /// sidecar. `source_media` is consumed.
    pub fn write_new_entry(&self, entry: &LibraryEntry, source_media: &Path) -> io::Result<()> {
        entry
            .validate()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        let sidecar_temp = temp_sibling(&entry.sidecar_path);
        let json = NativeSidecar::from_entry(entry, &self.root).to_json()?;
        write_atomic(&sidecar_temp, None, json.as_bytes())?;
        move_file(source_media, &entry.media_path)?;
        fs::rename(&sidecar_temp, &entry.sidecar_path)
    }

    // --- Mutation and deletion ---

    /// Rewrite only the sidecar, atomically, from the typed model.
    pub fn update(&self, entry: &LibraryEntry, change: &EntryUpdate) -> io::Result<LibraryEntry> {
        self.check_owned(&entry.sidecar_path)
            .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error))?;
        let text = fs::read_to_string(&entry.sidecar_path)?;
        let mut sidecar: NativeSidecar = serde_json::from_str(&text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;

        let mut updated = entry.clone();
        match change {
            EntryUpdate::Protected(protected) => updated.protected = *protected,
            EntryUpdate::Tag(tag) => {
                updated.tag = if tag.trim().is_empty() {
                    None
                } else {
                    Some(tag.clone())
                };
            }
        }
        sidecar.protected = updated.protected;
        sidecar.tag = updated.tag.clone();

        let temp = temp_sibling(&entry.sidecar_path);
        write_atomic(
            &temp,
            Some(&entry.sidecar_path),
            sidecar.to_json()?.as_bytes(),
        )?;
        Ok(updated)
    }

    /// Remove media plus sidecar for each entry, reporting per-entry failures.
    /// Symlinks and paths outside the root are refused.
    pub fn delete(&self, entries: &[LibraryEntry]) -> DeleteResult {
        let mut result = DeleteResult::default();
        for entry in entries {
            match self.delete_one(entry) {
                Ok(()) => result.deleted.push(entry.id.clone()),
                Err(error) => {
                    // Same rule as eviction: only a sidecar-stage failure
                    // leaves the media gone while its sidecar remains.
                    if error.starts_with("sidecar:") {
                        result.partially_deleted = true;
                    }
                    result.failures.push((entry.id.clone(), error));
                }
            }
        }
        result
    }

    fn delete_one(&self, entry: &LibraryEntry) -> Result<(), String> {
        self.check_owned(&entry.media_path)?;
        self.check_owned(&entry.sidecar_path)?;
        fs::remove_file(&entry.media_path).map_err(|error| format!("media: {error}"))?;
        match fs::remove_file(&entry.sidecar_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("sidecar: {error}")),
        }
    }

    fn check_owned(&self, path: &Path) -> Result<(), String> {
        // The library is deliberately flat. Requiring a direct child closes
        // the intermediate-directory symlink race: a leaf swapped to a
        // symlink is itself unlinked by deletion, never followed.
        if path.parent() != Some(self.root.as_path()) {
            return Err(format!(
                "{} is not a direct child of the storage root",
                path.display()
            ));
        }
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!("{} is a symlink", path.display()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        Ok(())
    }

    // --- Storage limit ---

    /// Evict the oldest unprotected recordings until the library fits the limit.
    /// Unlimited returns without eviction; unrecognized files are never touched.
    pub fn enforce_limit(&self, limit: StorageLimit, entries: &[LibraryEntry]) -> EvictionResult {
        let mut result = EvictionResult::default();
        let usage = |entries: &[LibraryEntry]| -> u64 {
            entries
                .iter()
                .map(|entry| self.entry_size(entry))
                .fold(0u64, u64::saturating_add)
        };

        let StorageLimit::Gib(gib) = limit else {
            result.remaining_bytes = usage(entries);
            return result;
        };
        let Some(limit_bytes) = gib.get().checked_mul(1024 * 1024 * 1024) else {
            result.remaining_bytes = usage(entries);
            return result;
        };

        let mut sized: Vec<(&LibraryEntry, u64)> = entries
            .iter()
            .map(|entry| (entry, self.entry_size(entry)))
            .collect();
        let mut used: u64 = sized
            .iter()
            .map(|(_, size)| *size)
            .fold(0u64, u64::saturating_add);
        let protected_bytes: u64 = sized
            .iter()
            .filter(|(entry, _)| entry.protected)
            .map(|(_, size)| *size)
            .fold(0u64, u64::saturating_add);
        result.protected_over_limit = protected_bytes > limit_bytes;

        // Oldest first; the tie-break keeps the order deterministic.
        sized.sort_by(|left, right| {
            left.0
                .start_unix_ms
                .cmp(&right.0.start_unix_ms)
                .then_with(|| left.0.media_path.cmp(&right.0.media_path))
        });

        for (entry, size) in sized {
            if used <= limit_bytes {
                break;
            }
            if entry.protected {
                continue;
            }
            match self.delete_one(entry) {
                Ok(()) => {
                    used = used.saturating_sub(size);
                    result.evicted.push(entry.id.clone());
                }
                Err(error) => {
                    // `delete_one` unlinks the media before the sidecar and
                    // tags which stage failed. Only a sidecar failure means
                    // the media is actually gone; a media failure (already
                    // missing, not owned, a symlink) freed nothing, and
                    // probing `exists()` cannot tell those apart. Credit the
                    // space for the real case rather than evicting more, and
                    // make the caller rescan.
                    if error.starts_with("sidecar:") {
                        used = used.saturating_sub(size);
                        result.partially_deleted = true;
                    }
                }
            }
        }

        result.remaining_bytes = used;
        result
    }

    fn entry_size(&self, entry: &LibraryEntry) -> u64 {
        let media = fs::metadata(&entry.media_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        let sidecar = fs::metadata(&entry.sidecar_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        media.saturating_add(sidecar)
    }

    // --- Recovery sweeps ---

    /// Quarantine every media/GSR/`.tmp` artifact in the storage, replay,
    /// regular, and staging directories that no sidecar references. `index` is
    /// a fresh scan of this storage: its entries name their media, and only
    /// the sidecars it skipped are read again. Runs at startup, before capture
    /// is armed. Nothing is deleted, repaired, or claimed by name/time
    /// proximity.
    pub fn sweep_orphans(&self, index: &LibraryIndex) -> RecoveryReport {
        self.sweep(Some(&self.referenced_media(index)))
    }

    /// Quarantine everything in the replay, regular, and staging directories
    /// but leave the storage folder alone. The caller guarantees no capture or
    /// media job still owns a file there.
    pub fn sweep_capture_dirs(&self) -> RecoveryReport {
        self.sweep(None)
    }

    /// Quarantine one capture's GSR intermediates, nothing else.
    pub fn quarantine_capture(&self, artifacts: &CaptureArtifacts, reason: &str) -> RecoveryReport {
        let mut report = RecoveryReport::default();
        let recovery_dir = self.root.join(RECOVERY_DIR);
        for path in artifacts.replay.iter().chain([&artifacts.regular]) {
            match self.quarantine(&recovery_dir, path, reason) {
                Ok(moved) => report.quarantined.push(moved),
                Err(error) => report.failures.push(format!("{}: {error}", path.display())),
            }
        }
        report
    }

    /// Sweep the capture directories, plus the storage folder when the media
    /// its sidecars reference is known.
    fn sweep(&self, referenced: Option<&HashSet<PathBuf>>) -> RecoveryReport {
        let mut report = RecoveryReport::default();
        let recovery_dir = self.root.join(RECOVERY_DIR);

        let mut directories = Vec::new();
        if referenced.is_some() {
            directories.push((
                self.root.clone(),
                "unreferenced media or interrupted write in the storage folder",
            ));
        }
        directories.extend([
            (self.replay_dir.clone(), "replay artifact with no recording"),
            (
                self.regular_dir.clone(),
                "regular artifact with no recording",
            ),
            (self.staging_dir.clone(), "media job intermediate"),
        ]);

        for (directory, reason) in directories {
            let Ok(read_dir) = fs::read_dir(&directory) else {
                continue;
            };
            let mut paths: Vec<PathBuf> = read_dir
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
                .map(|entry| entry.path())
                .collect();
            paths.sort();

            for path in paths {
                if directory == self.root {
                    let extension = path.extension().and_then(|value| value.to_str());
                    let sweepable = matches!(extension, Some(MEDIA_EXTENSION) | Some("tmp"));
                    if !sweepable || referenced.is_some_and(|media| media.contains(&path)) {
                        continue;
                    }
                }
                match self.quarantine(&recovery_dir, &path, reason) {
                    Ok(moved) => report.quarantined.push(moved),
                    Err(error) => report.failures.push(format!("{}: {error}", path.display())),
                }
            }
        }
        report
    }

    fn referenced_media(&self, index: &LibraryIndex) -> HashSet<PathBuf> {
        let mut referenced: HashSet<PathBuf> = index
            .entries
            .iter()
            .map(|entry| entry.media_path.clone())
            .collect();
        for skipped in &index.skipped {
            let Ok(text) = fs::read_to_string(&skipped.sidecar_path) else {
                continue;
            };
            // A sidecar the scanner rejects can still own its media, so the
            // reference is probed without materializing the document.
            let Ok(probe) = serde_json::from_str::<SidecarProbe>(&text) else {
                continue;
            };
            match probe.media_file.as_deref() {
                Some(media_file) => {
                    referenced.insert(self.root.join(media_file));
                }
                None => {
                    referenced.insert(skipped.sidecar_path.with_extension(MEDIA_EXTENSION));
                }
            }
        }
        referenced
    }

    fn quarantine(&self, recovery_dir: &Path, path: &Path, reason: &str) -> io::Result<PathBuf> {
        fs::create_dir_all(recovery_dir)?;
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "artifact".to_owned());
        let mut destination = recovery_dir.join(&name);
        let mut attempt = 1;
        while destination.exists() {
            destination = recovery_dir.join(format!("{attempt}-{name}"));
            attempt += 1;
        }
        move_file(path, &destination)?;
        let mut reason_file = File::create(reason_path(&destination))?;
        writeln!(reason_file, "{reason}\noriginal path: {}", path.display())?;
        Ok(destination)
    }
}

/// Lenient probe of which media file a sidecar names, for sidecars the scan
/// rejects: unknown fields are streamed past instead of materialized.
#[derive(Debug, Default)]
struct SidecarProbe {
    media_file: Option<String>,
}

impl<'de> Deserialize<'de> for SidecarProbe {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ProbeVisitor;

        impl<'de> Visitor<'de> for ProbeVisitor {
            type Value = SidecarProbe;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON sidecar")
            }

            // Whatever is not an object is neither native nor a media
            // reference; the sibling media name keeps applying, as before.
            fn visit_bool<E>(self, _value: bool) -> Result<SidecarProbe, E> {
                Ok(SidecarProbe::default())
            }

            fn visit_i64<E>(self, _value: i64) -> Result<SidecarProbe, E> {
                Ok(SidecarProbe::default())
            }

            fn visit_u64<E>(self, _value: u64) -> Result<SidecarProbe, E> {
                Ok(SidecarProbe::default())
            }

            fn visit_f64<E>(self, _value: f64) -> Result<SidecarProbe, E> {
                Ok(SidecarProbe::default())
            }

            fn visit_str<E>(self, _value: &str) -> Result<SidecarProbe, E> {
                Ok(SidecarProbe::default())
            }

            fn visit_unit<E>(self) -> Result<SidecarProbe, E> {
                Ok(SidecarProbe::default())
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<SidecarProbe, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                while sequence.next_element::<IgnoredAny>()?.is_some() {}
                Ok(SidecarProbe::default())
            }

            fn visit_map<M>(self, mut map: M) -> Result<SidecarProbe, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                let mut probe = SidecarProbe::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        // A duplicated key keeps the last occurrence.
                        "media_file" => probe.media_file = map.next_value::<MediaFile>()?.0,
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(probe)
            }
        }

        deserializer.deserialize_any(ProbeVisitor)
    }
}

/// A sidecar `media_file`: a string names the media; every other value —
/// `null`, a number, a container — is consumed leniently and falls back to
/// the sibling media name.
struct MediaFile(Option<String>);

impl<'de> Deserialize<'de> for MediaFile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct MediaFileVisitor;

        impl<'de> Visitor<'de> for MediaFileVisitor {
            type Value = MediaFile;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a media file name")
            }

            fn visit_str<E>(self, name: &str) -> Result<MediaFile, E> {
                Ok(MediaFile(Some(name.to_owned())))
            }

            fn visit_bool<E>(self, _value: bool) -> Result<MediaFile, E> {
                Ok(MediaFile(None))
            }

            fn visit_i64<E>(self, _value: i64) -> Result<MediaFile, E> {
                Ok(MediaFile(None))
            }

            fn visit_u64<E>(self, _value: u64) -> Result<MediaFile, E> {
                Ok(MediaFile(None))
            }

            fn visit_f64<E>(self, _value: f64) -> Result<MediaFile, E> {
                Ok(MediaFile(None))
            }

            fn visit_unit<E>(self) -> Result<MediaFile, E> {
                Ok(MediaFile(None))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<MediaFile, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                while sequence.next_element::<IgnoredAny>()?.is_some() {}
                Ok(MediaFile(None))
            }

            fn visit_map<M>(self, mut map: M) -> Result<MediaFile, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                while map.next_key::<IgnoredAny>()?.is_some() {
                    map.next_value::<IgnoredAny>()?;
                }
                Ok(MediaFile(None))
            }
        }

        deserializer.deserialize_any(MediaFileVisitor)
    }
}

// --- Native sidecar ---

/// Fields Warcraft Recorder wrote that no longer exist (combat meter,
/// combatants, activity hash) are ignored on reading and dropped on rewrite.
#[derive(Debug, Serialize, Deserialize)]
struct NativeSidecar {
    schema_version: u32,
    /// Media file name relative to the storage root.
    media_file: String,
    id: RecordingId,
    category: Category,
    flavor: GameFlavor,
    title: String,
    start_unix_ms: i64,
    duration_ms: u64,
    outcome: Outcome,
    protected: bool,
    tag: Option<String>,
    player: Option<PlayerSummary>,
    details: ActivityDetails,
    timeline: Vec<TimelineItem>,
    media: MediaFacts,
}

impl NativeSidecar {
    fn from_entry(entry: &LibraryEntry, root: &Path) -> Self {
        let media_file = entry
            .media_path
            .strip_prefix(root)
            .unwrap_or(&entry.media_path)
            .to_string_lossy()
            .into_owned();
        Self {
            schema_version: SIDECAR_SCHEMA_VERSION,
            media_file,
            id: entry.id.clone(),
            category: entry.category.clone(),
            flavor: entry.flavor.clone(),
            title: entry.title.clone(),
            start_unix_ms: entry.start_unix_ms,
            duration_ms: entry.duration_ms,
            outcome: entry.outcome,
            protected: entry.protected,
            tag: entry.tag.clone(),
            player: entry.player.clone(),
            details: entry.details.clone(),
            timeline: entry.timeline.clone(),
            media: entry.media.clone(),
        }
    }

    fn into_entry(self, media_path: PathBuf, sidecar_path: PathBuf) -> LibraryEntry {
        LibraryEntry {
            id: self.id,
            media_path,
            sidecar_path,
            category: self.category,
            flavor: self.flavor,
            title: self.title,
            start_unix_ms: self.start_unix_ms,
            duration_ms: self.duration_ms,
            outcome: self.outcome,
            protected: self.protected,
            tag: self.tag,
            player: self.player,
            details: self.details,
            timeline: self.timeline,
            media: self.media,
        }
    }

    fn to_json(&self) -> io::Result<String> {
        serde_json::to_string(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
    }
}

// --- Helpers ---

/// The library's one ordering: newest first, ties broken by media path.
fn entry_order(left: &LibraryEntry, right: &LibraryEntry) -> std::cmp::Ordering {
    right
        .start_unix_ms
        .cmp(&left.start_unix_ms)
        .then_with(|| left.media_path.cmp(&right.media_path))
}

/// Convert timeline offsets relative to the activity start into media offsets.
/// Markers that fall before the media starts are clipped away.
fn shift_timeline(
    timeline: &[TimelineItem],
    activity_start_ms: i64,
    media_start_ms: i64,
    duration_ms: u64,
) -> Vec<TimelineItem> {
    let lead_in_ms = activity_start_ms - media_start_ms;
    let mut shifted = Vec::new();
    for item in timeline {
        let start = item.start_ms() as i64 + lead_in_ms;
        let end = item.end_ms().map(|end| end as i64 + lead_in_ms);
        if start > duration_ms as i64 {
            continue;
        }
        match item.shape() {
            TimelineShape::Point => {
                if start < 0 {
                    continue;
                }
                shifted.push(TimelineItem::point(
                    item.kind().clone(),
                    start as u64,
                    item.label().map(str::to_owned),
                    item.outcome(),
                    item.player_reference().map(str::to_owned),
                ));
            }
            TimelineShape::Span => {
                let end = end.unwrap_or(start);
                if end < 0 {
                    continue;
                }
                let clamped_end = (end as u64).min(duration_ms);
                if let Ok(span) = TimelineItem::span(
                    item.kind().clone(),
                    (start.max(0) as u64).min(clamped_end),
                    clamped_end,
                    item.label().map(str::to_owned),
                    item.outcome(),
                    item.player_reference().map(str::to_owned),
                ) {
                    shifted.push(span);
                }
            }
        }
    }
    shifted
}

pub fn sanitize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_space = false;
    for character in name.chars() {
        let character = match character {
            '<' | '>' | ':' | '"' | '/' | '|' | '?' | '*' | '\\' => ' ',
            other if other.is_control() => ' ',
            other => other,
        };
        if character == ' ' {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(character);
            last_space = false;
        }
    }
    out.trim().to_owned()
}

/// `activity-<start>-<identifier> - <sanitized title>`: uniqueness comes from
/// the identifier, never from the title.
pub fn unique_stem(id: &RecordingId, start_unix_ms: i64, title: &str) -> String {
    let short: String = id
        .as_str()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    let stem = format!("activity-{start_unix_ms}-{short}");
    let title = sanitize_name(title);
    if title.is_empty() {
        stem
    } else {
        format!("{stem} - {title}")
    }
}

fn default_title(category: &Category) -> String {
    match category {
        Category::Manual => "Manual recording".to_owned(),
        Category::Clip => "Clip".to_owned(),
        other => format!("{other:?}"),
    }
}

fn media_has_content(path: &Path) -> Result<bool, String> {
    match fs::metadata(path) {
        // Scanning must never decode media, so a nonzero length is the only
        // content check here; the media worker validates real outputs.
        Ok(metadata) if metadata.is_file() => Ok(metadata.len() > 0),
        Ok(_) => Err(format!("media path {} is not a file", path.display())),
        Err(error) => Err(format!("media file {}: {error}", path.display())),
    }
}

fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

fn reason_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".recovery.txt");
    path.with_file_name(name)
}

fn write_atomic(temp: &Path, final_path: Option<&Path>, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    match final_path {
        Some(final_path) => fs::rename(temp, final_path),
        None => Ok(()),
    }
}

/// Rename, falling back to copy plus remove across filesystems.
fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(from, to)?;
            fs::remove_file(from)
        }
    }
}

/// Wall-clock milliseconds; used for generated clip dates.
pub fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Unique temporary root for one test tree, `wr-<prefix>-<uuid>`.
#[cfg(test)]
pub(crate) fn test_root(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!("wr-{prefix}-{}", uuid::Uuid::new_v4()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    use crate::domain::{Codec, TimelineKind};

    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(name: &str) -> Self {
            let root = test_root(&format!("storage-{name}"));
            let tree = Self { root };
            tree.storage().prepare().expect("prepare");
            fs::create_dir_all(tree.capture_root().join("replay")).expect("replay dir");
            fs::create_dir_all(tree.capture_root().join("regular")).expect("regular dir");
            tree
        }

        fn library(&self) -> PathBuf {
            self.root.join("recordings with space")
        }

        fn capture_root(&self) -> PathBuf {
            self.root.join("capture")
        }

        fn storage(&self) -> Storage {
            Storage::new(self.library(), self.capture_root())
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.library().join(relative);
            fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            fs::write(&path, contents).expect("write");
            path
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Write `count` valid native sidecars, the first `protected_count` of
    /// them protected, plus a placeholder media file for each: the same
    /// on-disk shape finalize produces, without running the capture pipeline.
    fn install_native_fixtures(
        tree: &TempTree,
        count: usize,
        protected_count: usize,
    ) -> Vec<String> {
        (0..count)
            .map(|index| {
                let name = format!("native-{index:02}");
                let id = uuid::Uuid::new_v4();
                let protected = index < protected_count;
                let start = 1_772_323_200_000 + index as i64;
                let sidecar = format!(
                    r#"{{"schema_version":1,"media_file":"{name}.mp4","id":"{id}","category":"map_runs","flavor":"poe2","title":"Native {index}","start_unix_ms":{start},"duration_ms":60000,"outcome":"complete","protected":{protected},"combatants":[],"timeline":[],"details":{{"kind":"map_run","area_id":"MapBluff","map_name":"Bluff","area_level":80,"seed":7,"deaths":0,"portal_trips":0,"away_ms":0}},"media":{{"has_content":true}}}}"#
                );
                tree.write(&format!("{name}.json"), &sidecar);
                tree.write(&format!("{name}.mp4"), "fake media bytes");
                name
            })
            .collect()
    }

    fn draft(id: &RecordingId) -> RecordingDraft {
        RecordingDraft {
            id: id.clone(),
            category: Category::MapRuns,
            flavor: GameFlavor::Poe2,
            started_at_ms: 1_772_323_200_000,
            overrun_ms: 0,
            details: ActivityDetails::MapRun {
                area_id: "MapHiddenGrotto".to_owned(),
                map_name: "Hidden Grotto".to_owned(),
                area_level: 80,
                seed: 7,
                deaths: 1,
                portal_trips: 1,
                away_ms: 60_000,
            },
            player: Some(PlayerSummary {
                name: "Testone".to_owned(),
            }),
            timeline: vec![
                TimelineItem::point(
                    TimelineKind::Death,
                    1_000,
                    Some("Testone".to_owned()),
                    None,
                    None,
                ),
                TimelineItem::span(TimelineKind::Activity, 0, 60_000, None, None, None)
                    .expect("span"),
            ],
            outcome: Some(Outcome::Complete),
            ended_at_ms: Some(1_772_323_260_000),
            duration_ms: Some(75_000),
            title: Some("Hidden Grotto".to_owned()),
        }
    }

    fn artifacts(tree: &TempTree, with_replay: bool) -> CaptureArtifacts {
        let regular = tree
            .capture_root()
            .join("regular/Video_2026-03-01_00-00-05.mkv");
        fs::write(&regular, "regular bytes").expect("regular");
        let replay = with_replay.then(|| {
            let replay = tree
                .capture_root()
                .join("replay/Replay_2026-03-01_00-00-00.mkv");
            fs::write(&replay, "replay bytes").expect("replay");
            replay
        });
        CaptureArtifacts {
            replay,
            regular,
            requested_replay_ms: 8_000,
            // The regular recording starts five seconds after the activity.
            regular_started_at_ms: 1_772_323_205_000,
            regular_stopped_at_ms: 1_772_323_275_000,
        }
    }

    #[test]
    fn scan_accepts_zero_byte_performance_placeholders_without_decoding() {
        let tree = TempTree::new("zero-byte-corpus");
        let storage = tree.storage();
        let names = install_native_fixtures(&tree, 3, 0);
        let media = tree.library().join(&names[0]).with_extension("mp4");
        fs::write(&media, []).expect("truncate placeholder media");

        let index = storage.scan();
        assert_eq!(index.entries.len(), names.len());
        assert!(index.skipped.is_empty());
        assert!(
            index
                .entries
                .iter()
                .find(|entry| entry.media_path == media)
                .is_some_and(|entry| !entry.media.has_content)
        );
    }

    #[test]
    fn finalize_writes_media_relative_markers_and_survives_a_rescan() {
        let tree = TempTree::new("finalize");
        let storage = tree.storage();
        let temp_media = tree.capture_root().join("staging/combined.mp4");
        fs::write(&temp_media, "final media bytes").expect("temp media");

        let id = RecordingId::new();
        let draft = draft(&id);
        let artifacts = artifacts(&tree, true);
        let entry = storage
            .finalize(
                &draft,
                &artifacts,
                &CombinedMedia {
                    temp_media: temp_media.clone(),
                    // Eight seconds of usable replay in front of a regular
                    // recording that started five seconds after the activity.
                    trimmed_ms: None,
                    actual_replay_ms: 8_000,
                    facts: MediaFacts {
                        fps: Some(60),
                        width: None,
                        height: None,
                        codec: Some(Codec::H264),
                        has_content: true,
                    },
                },
            )
            .expect("finalize");

        // Media start is five seconds before the activity, so every marker
        // shifts by the usable replay lead-in.
        assert_eq!(entry.duration_ms, 8_000 + 70_000);
        assert_eq!(entry.timeline[0].start_ms(), 4_000);
        assert_eq!(entry.timeline[0].end_ms(), None);
        assert_eq!(entry.timeline[1].start_ms(), 3_000);
        assert_eq!(entry.timeline[1].end_ms(), Some(63_000));
        assert!(entry.media_path.starts_with(tree.library()));
        assert!(entry.media_path.to_string_lossy().contains("Hidden Grotto"));
        assert_eq!(
            fs::read_to_string(&entry.media_path).expect("media"),
            "final media bytes"
        );
        assert!(!temp_media.exists());
        // Intermediates are gone only after both final names exist.
        assert!(!artifacts.regular.exists());
        assert!(!artifacts.replay.expect("replay").exists());

        let index = storage.scan();
        assert_eq!(index.entries.as_ref(), &vec![entry.clone()]);
    }

    #[test]
    fn regular_only_finalization_clips_markers_before_the_media_start() {
        let tree = TempTree::new("finalize-regular");
        let storage = tree.storage();
        let temp_media = tree.capture_root().join("staging/combined.mp4");
        fs::write(&temp_media, "final media bytes").expect("temp media");

        let entry = storage
            .finalize(
                &draft(&RecordingId::new()),
                &artifacts(&tree, false),
                &CombinedMedia {
                    temp_media,
                    trimmed_ms: None,
                    actual_replay_ms: 0,
                    facts: MediaFacts {
                        fps: None,
                        width: None,
                        height: None,
                        codec: None,
                        has_content: true,
                    },
                },
            )
            .expect("finalize");

        assert_eq!(entry.duration_ms, 70_000);
        // The death at +1 s and the first five seconds of the away span
        // are not in the media; the point is dropped, the span is truncated.
        assert_eq!(entry.timeline.len(), 1);
        assert_eq!(entry.timeline[0].start_ms(), 0);
        assert_eq!(entry.timeline[0].end_ms(), Some(55_000));
    }

    #[test]
    fn startup_sweep_quarantines_interruption_leftovers_only() {
        let tree = TempTree::new("sweep");
        let storage = tree.storage();
        install_native_fixtures(&tree, 11, 2);
        let kept = tree.library().join("native-00.mp4");

        // One row per interruption state the architecture can actually create.
        let orphan_media = tree.write("activity-1772323200000-orphan.mp4", "media with no sidecar");
        let orphan_temp = tree.write("activity-1772323200000-orphan.json.tmp", "{}");
        let replay = tree
            .capture_root()
            .join("replay/Replay_2026-03-01_00-00-00.mkv");
        fs::write(&replay, "replay").expect("replay");
        let regular = tree
            .capture_root()
            .join("regular/Video_2026-03-01_00-00-05.mkv");
        fs::write(&regular, "regular").expect("regular");
        let staging = tree.capture_root().join("staging/replay-trim-1.mkv");
        fs::write(&staging, "trim").expect("staging");

        let report = storage.sweep_orphans(&storage.scan());
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.quarantined.len(), 5);
        for original in [&orphan_media, &orphan_temp, &replay, &regular, &staging] {
            assert!(!original.exists(), "{} was not swept", original.display());
        }
        for moved in &report.quarantined {
            assert!(moved.starts_with(tree.library().join(RECOVERY_DIR)));
            let reason = moved.with_file_name(format!(
                "{}.recovery.txt",
                moved.file_name().expect("name").to_string_lossy()
            ));
            assert!(
                fs::read_to_string(reason)
                    .expect("reason")
                    .contains("original path:")
            );
        }

        // Sidecar-referenced media is untouched, and the library still scans.
        assert!(kept.exists());
        assert_eq!(storage.scan().entries.len(), 11);
    }

    #[test]
    fn updates_rewrite_native_sidecars_from_the_typed_model() {
        let tree = TempTree::new("update");
        let storage = tree.storage();
        // A sidecar Warcraft Recorder wrote, with fields that no longer exist.
        let names = install_native_fixtures(&tree, 1, 0);
        let native = storage.scan().entries[0].clone();
        assert!(native.sidecar_path.ends_with(format!("{}.json", names[0])));

        let updated = storage
            .update(&native, &EntryUpdate::Protected(true))
            .expect("protect native");
        let tagged = storage
            .update(&updated, &EntryUpdate::Tag("  keeper  ".to_owned()))
            .expect("tag native");
        assert_eq!(tagged.tag.as_deref(), Some("  keeper  "));
        assert!(tagged.protected);
        let written = fs::read_to_string(&tagged.sidecar_path).expect("sidecar");
        assert!(!written.contains("combatants"));
        let reloaded = storage
            .scan()
            .entries
            .iter()
            .find(|entry| entry.id == native.id)
            .cloned()
            .expect("rescan");
        assert_eq!(reloaded, tagged);

        // Clearing the tag removes it.
        let cleared = storage
            .update(&tagged, &EntryUpdate::Tag("   ".to_owned()))
            .expect("clear tag");
        assert_eq!(cleared.tag, None);
    }

    #[test]
    fn deletion_reports_per_entry_failures_and_refuses_paths_outside_the_root() {
        let tree = TempTree::new("delete");
        let storage = tree.storage();
        install_native_fixtures(&tree, 3, 0);
        let index = storage.scan();

        let mut good = index.entries[0].clone();
        let mut missing = index.entries[1].clone();
        fs::remove_file(&missing.media_path).expect("remove media");
        let mut outside = index.entries[2].clone();
        outside.media_path = tree.root.join("escaped.mp4");
        fs::write(&outside.media_path, "outside").expect("outside");

        let result = storage.delete(&[good.clone(), missing.clone(), outside.clone()]);
        assert_eq!(result.deleted, vec![good.id.clone()]);
        assert_eq!(result.failures.len(), 2);
        assert!(result.failures[0].1.contains("media"));
        assert!(result.failures[1].1.contains("not a direct child"));
        assert!(!good.media_path.exists() && !good.sidecar_path.exists());
        assert!(outside.media_path.exists());

        // The sidecar of a partially removed entry stays for the next scan.
        good.media_path = PathBuf::new();
        missing.media_path = PathBuf::new();
        assert!(missing.sidecar_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn deletion_refuses_a_path_through_a_symlinked_directory() {
        use std::os::unix::fs::symlink;

        let tree = TempTree::new("delete-parent-symlink");
        let storage = tree.storage();
        install_native_fixtures(&tree, 1, 0);
        let mut entry = storage.scan().entries[0].clone();

        let outside = tree.root.join("outside");
        fs::create_dir(&outside).expect("outside directory");
        let media = outside.join("escaped.mp4");
        let sidecar = outside.join("escaped.json");
        fs::write(&media, "outside media").expect("outside media");
        fs::write(&sidecar, "{}").expect("outside sidecar");
        let link = tree.library().join("linked");
        symlink(&outside, &link).expect("directory symlink");
        entry.media_path = link.join("escaped.mp4");
        entry.sidecar_path = link.join("escaped.json");

        let result = storage.delete(&[entry]);
        assert!(result.deleted.is_empty());
        assert_eq!(result.failures.len(), 1);
        assert!(result.failures[0].1.contains("not a direct child"));
        assert!(media.exists() && sidecar.exists());
    }

    #[test]
    fn storage_limits_evict_only_unprotected_recordings_oldest_first() {
        let tree = TempTree::new("evict");
        let storage = tree.storage();
        install_native_fixtures(&tree, 11, 2);
        let entries = storage.scan().entries;

        // Unlimited never evicts.
        let unlimited = storage.enforce_limit(StorageLimit::Unlimited, &entries);
        assert!(unlimited.evicted.is_empty());
        assert!(unlimited.remaining_bytes > 0);
        assert_eq!(storage.scan().entries.len(), entries.len());

        // A limit far above usage evicts nothing either.
        let gib = StorageLimit::Gib(NonZeroU64::new(1).expect("nonzero"));
        assert!(storage.enforce_limit(gib, &entries).evicted.is_empty());

        // Give every media file a real size, then force eviction with a
        // one-GiB limit by pretending the library is larger: use padded files.
        let big = 400 * 1024;
        for entry in entries.iter() {
            fs::write(&entry.media_path, vec![0u8; big]).expect("pad");
        }
        let entries = storage.scan().entries;
        let protected: Vec<&LibraryEntry> =
            entries.iter().filter(|entry| entry.protected).collect();
        assert_eq!(protected.len(), 2);

        let tiny = Storage::new(tree.library(), tree.capture_root());
        let result = tiny.enforce_limit(
            StorageLimit::Gib(NonZeroU64::new(1).expect("nonzero")),
            &entries,
        );
        assert!(
            result.evicted.is_empty(),
            "1 GiB fits the whole fixture library"
        );

        // Shrink the limit below the protected content to prove the report.
        let mut protected_entries: Vec<LibraryEntry> = entries
            .iter()
            .filter(|entry| entry.protected)
            .cloned()
            .collect();
        protected_entries.push(
            entries
                .iter()
                .find(|entry| !entry.protected)
                .expect("unprotected")
                .clone(),
        );
        for entry in &protected_entries {
            fs::write(&entry.media_path, vec![0u8; 512 * 1024 * 1024]).expect("pad");
        }
        let result = storage.enforce_limit(
            StorageLimit::Gib(NonZeroU64::new(1).expect("nonzero")),
            &protected_entries,
        );
        assert!(result.protected_over_limit);
        assert_eq!(result.evicted.len(), 1);
        let evicted = protected_entries
            .iter()
            .find(|entry| result.evicted.contains(&entry.id))
            .expect("evicted entry");
        assert!(!evicted.protected);
        assert!(!evicted.media_path.exists());
        for entry in protected_entries.iter().filter(|entry| entry.protected) {
            assert!(entry.media_path.exists(), "protected entry was evicted");
        }
    }

    impl SkippedEntry {
        fn path_is(&self, name: &str) -> bool {
            self.sidecar_path.ends_with(name) && self.reason.starts_with("invalid sidecar")
        }
    }

    #[test]
    fn invalid_sidecars_are_skipped_with_a_diagnostic() {
        let tree = TempTree::new("skip");
        let storage = tree.storage();
        tree.write("broken.json", "{ not json");
        tree.write("broken.mp4", "media");
        tree.write(
            "no-media.json",
            r#"{"schema_version":1,"media_file":"missing.mp4","id":"0d8a0e10-1a2b-4c3d-8e4f-aabbccddeeff","category":"map_runs","flavor":"poe2","title":"No media","start_unix_ms":1,"duration_ms":10,"outcome":"unknown","protected":false,"timeline":[],"details":{"kind":"map_run","area_id":"MapBluff","map_name":"Bluff","area_level":80,"seed":7,"deaths":0,"portal_trips":0,"away_ms":0},"media":{"has_content":true}}"#,
        );
        tree.write("no-category.json", r#"{"schema_version":1,"media_file":"no-category.mp4","id":"0d8a0e10-1a2b-4c3d-8e4f-aabbccddeeff","flavor":"poe2","title":"No category","start_unix_ms":1,"duration_ms":10,"outcome":"unknown","protected":false,"timeline":[],"details":{"kind":"map_run","area_id":"MapBluff","map_name":"Bluff","area_level":80,"seed":7,"deaths":0,"portal_trips":0,"away_ms":0},"media":{"has_content":true}}"#);
        tree.write("no-category.mp4", "media");

        let index = storage.scan();
        assert!(index.entries.is_empty());
        assert_eq!(index.skipped.len(), 3);
        assert!(
            index
                .skipped
                .iter()
                .any(|skipped| skipped.path_is("broken.json"))
        );
        assert!(
            index
                .skipped
                .iter()
                .any(|skipped| skipped.reason.contains("media file"))
        );
        assert!(
            index
                .skipped
                .iter()
                .any(|skipped| skipped.reason.contains("missing field `category`"))
        );
    }

    /// The sweep resolves references from sidecars whatever the scanner thinks
    /// of them: a sidecar that names its media, the sibling fallback when no
    /// `media_file` is found (even a non-string one), and valid JSON that is
    /// not an object; malformed JSON references nothing and its media is swept.
    #[test]
    fn sweep_honors_references_from_sidecars_that_fail_to_load() {
        let tree = TempTree::new("sweep-references");
        let storage = tree.storage();
        tree.write(
            "rejected.json",
            r#"{"schema_version":null,"media_file":"named.mp4"}"#,
        );
        tree.write("named.mp4", "media");
        tree.write("sibling.json", r#"{"category":"map_runs","duration":10}"#);
        tree.write("sibling.mp4", "media");
        tree.write("wrongtype.json", r#"{"media_file":5}"#);
        tree.write("wrongtype.mp4", "media");
        // Valid JSON that is not an object: no reference at all, but the
        // sibling media name keeps applying.
        tree.write("nonobject.json", r#"["not","an","object"]"#);
        tree.write("nonobject.mp4", "media");
        // Malformed JSON references nothing, so its media is swept.
        tree.write("truncated.json", r#"{"media_file":"lost.mp4""#);
        tree.write("lost.mp4", "media");
        let orphan = tree.write("orphan.mp4", "unreferenced media");

        let report = storage.sweep_orphans(&storage.scan());
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.quarantined.len(), 2);
        for moved in &report.quarantined {
            assert!(moved.starts_with(tree.library().join(RECOVERY_DIR)));
        }
        assert!(!orphan.exists());
        assert!(!tree.library().join("lost.mp4").exists());
        assert!(tree.library().join("named.mp4").exists());
        assert!(tree.library().join("sibling.mp4").exists());
        assert!(tree.library().join("wrongtype.mp4").exists());
    }
}
