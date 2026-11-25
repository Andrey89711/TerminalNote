//! On-demand local search. No persistent index; only matching paths leave the worker.
use crate::storage::{NoteEntry, NoteStore};
use std::{
    collections::HashSet,
    io,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
        Arc,
    },
};

pub struct Matches {
    pub paths: HashSet<PathBuf>,
    pub skipped: usize,
}

pub struct Search {
    pub results: Receiver<Matches>,
    cancelled: Arc<AtomicBool>,
}

impl Search {
    pub fn start(store: NoteStore, notes: Vec<NoteEntry>, query: &str) -> io::Result<Self> {
        let query = query.to_lowercase();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancelled);
        let (sender, results) = mpsc::channel();
        std::thread::Builder::new()
            .name("terminalnote-content-search".into())
            .spawn(move || {
                let mut matches = Matches {
                    paths: HashSet::new(),
                    skipped: 0,
                };
                for entry in notes {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let matched = entry.path.to_string_lossy().to_lowercase().contains(&query)
                        || entry.title.to_lowercase().contains(&query)
                        || match store.load(&entry.path) {
                            Ok(document) => document.text.to_lowercase().contains(&query),
                            Err(_) => {
                                matches.skipped += 1;
                                false
                            }
                        };
                    if matched {
                        matches.paths.insert(entry.path);
                    }
                }
                let _ = sender.send(matches);
            })?;
        Ok(Self { results, cancelled })
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}
