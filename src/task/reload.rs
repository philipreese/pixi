use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use thiserror::Error;
use tokio::sync::mpsc::{self, Receiver};
use tracing::{info, warn};
use wax;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;

use crate::task::ExecutableTask;

/// Errors that can occur when watching files.
#[derive(Debug, Error)]
pub enum FileWatchError {
    /// An error occurred while watching files.
    #[error("Error watching files: {0}")]
    WatchError(#[from] notify::Error),

    /// An I/O error occurred.
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    /// Task execution error.
    #[error("Task execution error: {0}")]
    TaskExecutionError(String),

    /// Pattern error.
    #[error("Pattern error: {0}")]
    PatternError(#[from] wax::BuildError),
}

/// Config for the auto-reload feature.
pub struct AutoReloadConfig {
    /// Duration to debounce file change events.
    pub debounce: Duration,
}

impl Default for AutoReloadConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(500),
        }
    }
}

/// Watches files for changes and triggers task execution when they change.
pub struct FileWatcher {
    _watcher: RecommendedWatcher,
    rx: Receiver<Result<notify::Event, notify::Error>>,
    watched_paths: Vec<PathBuf>,
}

impl FileWatcher {
    /// Creates a new file watcher that watches the specified paths.
    pub fn new(paths: &[impl AsRef<Path>]) -> Result<Self, FileWatchError> {
        // Create a channel to receive events
        let (tx, rx) = mpsc::channel(100);

        // Create a watcher
        let mut watcher = RecommendedWatcher::new(
            move |res| {
                let _ = tx.blocking_send(res);
            },
            Config::default(),
        )?;

        let mut watched_paths = Vec::new();

        // Convert to concrete PathBuf collection first
        let concrete_paths: Vec<PathBuf> = paths
            .iter()
            .map(|p| p.as_ref().to_path_buf())
            .collect();

        // Now use parallel iterator on concrete type
        let path_results: Vec<Result<Vec<PathBuf>, FileWatchError>> = concrete_paths
            .par_iter()
            .map(|path| {
                let mut paths_to_watch = Vec::new();
                let path_str = path.to_string_lossy();

                // Check if this is a glob pattern
                if path_str.contains('*') || path_str.contains('?') || path_str.contains('[') {
                    info!("Detected glob pattern: {}", path_str);

                    // Use wax crate to expand the pattern
                    let pattern = wax::Glob::new(&path_str)?;
                    let entries = pattern.walk(&current_dir);

                    // Collect entries into Vec first, then process in parallel
                    let entries_vec: Vec<_> = entries.collect();
                    
                    // Use std::sync::atomic for thread-safe found_match
                    let found_match_atomic = std::sync::atomic::AtomicBool::new(false);
                    let paths_mutex = std::sync::Mutex::new(Vec::new());
                    
                    entries_vec.par_iter().for_each(|entry| {
                        match entry {
                            Ok(entry) => {
                                found_match_atomic.store(true, std::sync::atomic::Ordering::Relaxed);
                                // Convert WalkEntry to PathBuf
                                let path = entry.path().to_path_buf();
                                if path.exists() {
                                    if let Ok(mut paths) = paths_mutex.lock() {
                                        paths.push(path.clone());
                                    }
                                    info!("Found path from glob: {}", path.display());
                                }
                            }
                            Err(e) => warn!("Error in glob pattern '{}': {}", path_str, e),
                        }
                    });
                    
                    // Get processed paths
                    let found_match = found_match_atomic.load(std::sync::atomic::Ordering::Relaxed);
                    if let Ok(processed_paths) = paths_mutex.lock() {
                        paths_to_watch.extend(processed_paths.iter().cloned());
                    }

                    // If no matches found, watch the parent directory
                    if !found_match {
                        info!(
                            "No existing files match glob pattern '{}', watching current directory",
                            path_str
                        );
                        paths_to_watch.push(current_dir.clone());
                    }
                } else {
                    // Regular path handling
                    if path.exists() {
                        paths_to_watch.push(path.to_path_buf());
                    } else {
                        info!("Path does not exist, skipping: {}", path.display());
                        // Try to watch the parent directory if it exists
                        if let Some(parent) = path.parent() {
                            if parent.exists() {
                                info!("Watching parent directory instead: {}", parent.display());
                                paths_to_watch.push(parent.to_path_buf());
                            }
                        }
                    }
                }

                Ok(paths_to_watch)
            })
            .collect();

        // Process results and set up watchers
        for result in path_results {
            match result {
                Ok(paths) => {
                    for path in paths {
                        let mode = if path.is_dir() {
                            RecursiveMode::Recursive
                        } else {
                            RecursiveMode::NonRecursive
                        };
                        watcher.watch(&path, mode)?;
                        watched_paths.push(path.to_path_buf());
                        info!("Watching path: {}", path.display());
                    }
                }
                Err(e) => return Err(e),
            }
        }

        if watched_paths.is_empty() {
            warn!("No paths are being watched! Auto-reload will not work.");
        } else {
            info!("Watching paths: {:?}", watched_paths);
        }

        Ok(Self {
            _watcher: watcher,
            rx,
            watched_paths,
        })
    }

    /// Creates a file watcher from a task with watched_files.
    pub fn from_task(task: &ExecutableTask<'_>) -> Result<Option<Self>, FileWatchError> {
        // Get inputs from the task
        let inputs = match task.task().as_execute() {
            Some(execute) => {
                if execute.inputs.is_none() {
                    return Ok(None);
                }
                execute.inputs.clone().expect("inputs should not be None") // Unwrap the Option<Vec<String>>
            },
            _ => return Ok(None),
        };

        // Convert the glob patterns to absolute paths
        let root_path = task.project().root();
        let paths: Vec<PathBuf> = inputs
            .iter()
            .map(|pattern| root_path.join(pattern))
            .collect();

        Ok(Some(Self::new(&paths)?))
    }

    /// Returns the paths being watched.
    pub fn watched_paths(&self) -> &[PathBuf] {
        &self.watched_paths
    }

    /// Returns the next file change event.
    pub async fn next_event(&mut self) -> Option<Result<notify::Event, FileWatchError>> {
        self.rx.recv().await.map(|res| res.map_err(|e| e.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::time::sleep;

    /// Helper function to create a test file in a directory
    async fn create_test_file(dir: &std::path::Path, filename: &str, content: &str) -> PathBuf {
        let file_path = dir.join(filename);
        tokio::fs::write(&file_path, content).await.unwrap();
        file_path
    }

    #[tokio::test]
    async fn test_file_watcher_detects_changes() {
        // Create a temporary directory
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");

        tokio::fs::write(&file_path, "initial content")
            .await
            .unwrap();

        let mut watcher = FileWatcher::new(&[file_path.clone()]).unwrap();

        // Spawn a task to modify the file after a short delay
        let file_path_clone = file_path.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            tokio::fs::write(&file_path_clone, "updated content")
                .await
                .unwrap();
        });

        let event = watcher.next_event().await;

        assert!(event.is_some());

        // Verify the event is a modification
        if let Some(Ok(event)) = event {
            match event.kind {
                notify::event::EventKind::Modify(_)
                | notify::event::EventKind::Create(_)
                | notify::event::EventKind::Access(_) => {
                    // On some systems/filesystems, writing to a file can be reported as creating a new file
                }
                other => panic!("Expected Modify or Create event, got {:?}", other),
            }
        } else {
            panic!("Expected Ok event");
        }
    }

    #[tokio::test]
    async fn test_file_watcher_detects_creation() {
        // Create a temporary directory
        let dir = tempdir().unwrap();
        let parent_dir = dir.path();

        // Create a watcher for the directory
        let mut watcher = FileWatcher::new(&[parent_dir]).unwrap();

        let file_path = parent_dir.join("new_file.txt");
        let file_path_clone = file_path.clone();

        tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            tokio::fs::write(&file_path_clone, "new file content")
                .await
                .unwrap();
        });

        // Wait for an event
        let mut create_event_received = false;
        for _ in 0..3 {
            if let Some(Ok(event)) = watcher.next_event().await {
                if let notify::event::EventKind::Create(_) = event.kind {
                    create_event_received = true;
                    break;
                }
            }
        }

        assert!(create_event_received, "Should have received a Create event");
    }

    #[tokio::test]
    async fn test_file_watcher_detects_deletion() {
        // Create a temporary directory
        let dir = tempdir().unwrap();
        let file_path = create_test_file(dir.path(), "to_delete.txt", "delete me").await;

        // Create a watcher
        let mut watcher = FileWatcher::new(&[file_path.clone()]).unwrap();

        let file_path_clone = file_path.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            tokio::fs::remove_file(&file_path_clone).await.unwrap();
        });

        // Wait for events
        let mut delete_event_received = false;
        for _ in 0..3 {
            if let Some(Ok(event)) = watcher.next_event().await {
                println!("event: {:?}", event);
                if let notify::event::EventKind::Remove(_) | notify::event::EventKind::Modify(_) =
                    event.kind
                {
                    delete_event_received = true;
                    break;
                }
            }
        }

        assert!(delete_event_received, "Should have received a Remove event");
    }

    #[tokio::test]
    async fn test_file_watcher_non_existent_path() {
        let dir = tempdir().unwrap();
        let non_existent_path = dir.path().join("does_not_exist.txt");

        // Create a watcher for non-existent file (should watch parent)
        let watcher = FileWatcher::new(&[non_existent_path.clone()]).unwrap();

        // Should be watching the parent directory
        assert_eq!(watcher.watched_paths().len(), 1);
        assert_eq!(watcher.watched_paths()[0], dir.path());
    }

    #[tokio::test]
    async fn test_file_watcher_multiple_paths() {
        let dir = tempdir().unwrap();
        let file1 = create_test_file(dir.path(), "file1.txt", "file1").await;
        let file2 = create_test_file(dir.path(), "file2.txt", "file2").await;

        // Create a watcher for multiple files
        let watcher = FileWatcher::new(&[file1.clone(), file2.clone()]).unwrap();

        // Should be watching both files
        assert_eq!(watcher.watched_paths().len(), 2);
        assert!(watcher.watched_paths().contains(&file1));
        assert!(watcher.watched_paths().contains(&file2));
    }
}
