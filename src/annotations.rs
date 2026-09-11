use std::{
    ffi::OsString,
    fs::{self, File},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NormalizedRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl NormalizedRect {
    pub fn from_points(a: [f32; 2], b: [f32; 2]) -> Self {
        let x = a[0].min(b[0]);
        let y = a[1].min(b[1]);
        Self {
            x,
            y,
            width: (a[0] - b[0]).abs(),
            height: (a[1] - b[1]).abs(),
        }
    }

    pub fn contains(self, point: [f32; 2]) -> bool {
        point[0] >= self.x
            && point[0] <= self.x + self.width
            && point[1] >= self.y
            && point[1] <= self.y + self.height
    }

    fn is_valid(self) -> bool {
        [self.x, self.y, self.width, self.height]
            .into_iter()
            .all(f32::is_finite)
            && self.x >= 0.0
            && self.y >= 0.0
            && self.width > 0.0
            && self.height > 0.0
            && self.x + self.width <= 1.001
            && self.y + self.height <= 1.001
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Annotation {
    pub id: u64,
    pub page_index: usize,
    pub rects: Vec<NormalizedRect>,
    pub selected_text: String,
    pub note: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct AnnotationFile {
    schema_version: u32,
    document_fingerprint: String,
    annotations: Vec<Annotation>,
}

#[derive(Debug)]
pub struct AnnotationStore {
    path: PathBuf,
    fingerprint: String,
    annotations: Vec<Annotation>,
    next_id: u64,
    dirty_since: Option<Instant>,
    preserve_existing: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadWarning {
    #[error("could not read annotation sidecar: {0}")]
    Read(#[from] io::Error),
    #[error("could not parse annotation sidecar: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("annotation sidecar uses unsupported schema version {0}")]
    Schema(u32),
    #[error("annotation sidecar belongs to a different version of this PDF")]
    Fingerprint,
    #[error("annotation sidecar contains invalid annotation data")]
    InvalidData,
}

impl AnnotationStore {
    pub fn load(
        pdf_path: &Path,
        fingerprint: String,
        page_count: usize,
    ) -> (Self, Option<LoadWarning>) {
        let path = sidecar_path(pdf_path);
        let mut store = Self {
            path: path.clone(),
            fingerprint,
            annotations: Vec::new(),
            next_id: 1,
            dirty_since: None,
            preserve_existing: false,
        };

        if !path.exists() {
            return (store, None);
        }

        let loaded = (|| -> Result<AnnotationFile, LoadWarning> {
            let bytes = fs::read(&path)?;
            let file: AnnotationFile = serde_json::from_slice(&bytes)?;
            if file.schema_version != SCHEMA_VERSION {
                return Err(LoadWarning::Schema(file.schema_version));
            }
            if file.document_fingerprint != store.fingerprint {
                return Err(LoadWarning::Fingerprint);
            }
            if !valid_annotations(&file.annotations, page_count) {
                return Err(LoadWarning::InvalidData);
            }
            Ok(file)
        })();

        match loaded {
            Ok(file) => {
                store.next_id = file
                    .annotations
                    .iter()
                    .map(|annotation| annotation.id)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                store.annotations = file.annotations;
                (store, None)
            }
            Err(warning) => {
                store.preserve_existing = true;
                (store, Some(warning))
            }
        }
    }

    pub fn annotations(&self) -> &[Annotation] {
        &self.annotations
    }

    pub fn annotation_mut(&mut self, id: u64) -> Option<&mut Annotation> {
        self.annotations
            .iter_mut()
            .find(|annotation| annotation.id == id)
    }

    pub fn add(
        &mut self,
        page_index: usize,
        rects: Vec<NormalizedRect>,
        selected_text: String,
    ) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.annotations.push(Annotation {
            id,
            page_index,
            rects,
            selected_text,
            note: String::new(),
        });
        self.mark_dirty();
        id
    }

    pub fn remove(&mut self, id: u64) -> bool {
        let old_len = self.annotations.len();
        self.annotations.retain(|annotation| annotation.id != id);
        let changed = self.annotations.len() != old_len;
        if changed {
            self.mark_dirty();
        }
        changed
    }

    pub fn mark_dirty(&mut self) {
        self.dirty_since = Some(Instant::now());
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty_since.is_some()
    }

    pub fn save_if_due(&mut self, delay: Duration) -> Result<bool, io::Error> {
        if self
            .dirty_since
            .is_some_and(|dirty_since| dirty_since.elapsed() >= delay)
        {
            self.save()?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn save(&mut self) -> Result<(), io::Error> {
        if self.dirty_since.is_none() {
            return Ok(());
        }

        if self.preserve_existing && self.path.exists() {
            let backup = append_suffix(&self.path, ".bak");
            if !backup.exists() {
                fs::copy(&self.path, backup)?;
            }
            self.preserve_existing = false;
        }

        let temp_path = append_suffix(&self.path, ".tmp");
        let file = File::create(&temp_path)?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(
            &mut writer,
            &AnnotationFile {
                schema_version: SCHEMA_VERSION,
                document_fingerprint: self.fingerprint.clone(),
                annotations: self.annotations.clone(),
            },
        )
        .map_err(io::Error::other)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        fs::rename(temp_path, &self.path)?;
        self.dirty_since = None;
        Ok(())
    }
}

fn sidecar_path(pdf_path: &Path) -> PathBuf {
    append_suffix(pdf_path, ".spv.json")
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn valid_annotations(annotations: &[Annotation], page_count: usize) -> bool {
    let mut ids = std::collections::HashSet::new();
    annotations.iter().all(|annotation| {
        annotation.id > 0
            && ids.insert(annotation.id)
            && annotation.page_index < page_count
            && !annotation.rects.is_empty()
            && annotation.rects.iter().all(|rect| rect.is_valid())
            && annotation.selected_text.len() <= 1_000_000
            && annotation.note.len() <= 1_000_000
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_rect_orders_points_and_hit_tests() {
        let rect = NormalizedRect::from_points([0.8, 0.7], [0.2, 0.3]);
        assert!((rect.x - 0.2).abs() < f32::EPSILON);
        assert!((rect.y - 0.3).abs() < f32::EPSILON);
        assert!((rect.width - 0.6).abs() < f32::EPSILON);
        assert!((rect.height - 0.4).abs() < f32::EPSILON);
        assert!(rect.contains([0.5, 0.5]));
        assert!(!rect.contains([0.1, 0.5]));
    }

    #[test]
    fn rejects_duplicate_ids_and_bad_coordinates() {
        let annotation = Annotation {
            id: 1,
            page_index: 0,
            rects: vec![NormalizedRect {
                x: 0.1,
                y: 0.1,
                width: 0.2,
                height: 0.1,
            }],
            selected_text: "hello".into(),
            note: String::new(),
        };
        assert!(valid_annotations(std::slice::from_ref(&annotation), 1));
        assert!(!valid_annotations(&[annotation.clone(), annotation], 1));
    }

    #[test]
    fn sidecar_json_round_trips() {
        let file = AnnotationFile {
            schema_version: SCHEMA_VERSION,
            document_fingerprint: "abc123".into(),
            annotations: vec![Annotation {
                id: 8,
                page_index: 2,
                rects: vec![NormalizedRect {
                    x: 0.1,
                    y: 0.2,
                    width: 0.3,
                    height: 0.04,
                }],
                selected_text: "Selected text".into(),
                note: "Remember this".into(),
            }],
        };
        let json = serde_json::to_vec(&file).expect("serialize sidecar");
        let decoded: AnnotationFile = serde_json::from_slice(&json).expect("parse sidecar");
        assert_eq!(decoded.schema_version, SCHEMA_VERSION);
        assert_eq!(decoded.document_fingerprint, "abc123");
        assert_eq!(decoded.annotations, file.annotations);
    }
}
