use std::{
    collections::{HashMap, HashSet},
    env,
    hash::Hash,
    io, iter,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, Instant},
};

use darklua_core::{Options, Resources, WorkerTree};
use notify::{event::ModifyKind, EventKind, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, DebouncedEvent};

use crate::cli::{error::CliError, process::Options as ProcessOptions, CommandResult};

use super::report_process;

const FILE_WATCHING_DEBOUNCE_DURATION_MILLIS: u64 = 400;
const DEFAULT_CONFIG_PATHS: [&str; 2] = [".darklua.json", ".darklua.json5"];

enum WatcherSignal {
    Exit,
    Watch(PathBuf, RecursiveMode),
    Unwatch(PathBuf),
}

pub struct FileWatcher {
    input_path: PathBuf,
    canonical_input_path: Option<PathBuf>,
    config_paths: Vec<PathBuf>,
    resources: Resources,
    sender: Sender<WatcherSignal>,
    receiver: Option<Receiver<WatcherSignal>>,
    worker_tree: Option<WorkerTree>,
    process_option: ProcessOptions,
    /// Canonical paths of the watched files outside of the input.
    extra_file_watch: HashSet<PathBuf>,
    /// Directories watched for those files, from their canonical path to the
    /// path they are watched with.
    extra_directory_watch: HashMap<PathBuf, PathBuf>,
    links_file_watch: HashSet<(PathBuf, PathBuf)>,
    current_working_path: Option<PathBuf>,
}

impl FileWatcher {
    pub fn new(process_option: &ProcessOptions) -> Self {
        let (sender, receiver) = mpsc::channel();

        let config_paths = match &process_option.config {
            Some(config) => vec![config.clone()],
            None => DEFAULT_CONFIG_PATHS
                .iter()
                .map(PathBuf::from)
                .filter(|path| path.exists())
                .collect(),
        };

        Self {
            input_path: process_option.input_path.clone(),
            canonical_input_path: process_option.input_path.canonicalize().ok(),
            config_paths,
            resources: Resources::from_file_system(),
            sender,
            receiver: Some(receiver),
            worker_tree: None,
            process_option: process_option.clone(),
            extra_file_watch: Default::default(),
            extra_directory_watch: Default::default(),
            links_file_watch: Default::default(),
            current_working_path: env::current_dir().ok(),
        }
    }

    fn run_worker_tree(&mut self) {
        let options = self.build_options();

        let process_start_time = Instant::now();

        if let Some(worker_tree) = self.worker_tree.as_mut() {
            log_darklua_error(worker_tree.process(&self.resources, options), || ());
        } else {
            self.worker_tree = log_darklua_error(
                darklua_core::process(&self.resources, options).map(Some),
                || None,
            );
        }

        if let Some(worker_tree) = self.worker_tree.as_mut() {
            report_process("processed", worker_tree, process_start_time.elapsed()).ok();
        }

        self.update_extra_file_watch();
    }

    fn build_options(&self) -> Options {
        self.process_option.get_process_options()
    }

    pub fn start(mut self) -> CommandResult {
        self.run_worker_tree();
        self.setup_ctrl_exit()?;

        let receiver = self
            .receiver
            .take()
            .expect("file watcher channel receiver should exist");

        let input_path = self.input_path.clone();

        for link_path in iter_all_links(input_path.clone()) {
            if let Ok(link_location) = link_path.read_link() {
                self.send_watch_signal(&link_location, RecursiveMode::Recursive);

                self.links_file_watch.insert((link_location, link_path));
            }
        }

        let mut debouncer = new_debouncer(
            Duration::from_millis(FILE_WATCHING_DEBOUNCE_DURATION_MILLIS),
            None,
            move |events: DebounceEventResult| match events {
                Ok(events) => {
                    if self.process_events(events) {
                        log::debug!("changes detected, re-running process");
                        self.run_worker_tree();
                    }
                }
                Err(errors) => {
                    for err in errors {
                        log::error!(
                            "an error occured while watching file system for changes: {}",
                            err
                        );
                    }
                }
            },
        )
        .map_err(|err| {
            log::error!("unable to create file watcher: {}", err);
            CliError::new(1)
        })?;

        log::debug!("start watching file system on {}", input_path.display());

        debouncer
            .watch(&input_path, RecursiveMode::Recursive)
            .map_err(|err| {
                log::error!(
                    "unable to start watching file system at `{}`: {}",
                    input_path.display(),
                    err
                );
                CliError::new(1)
            })?;

        log::debug!("waiting for Ctrl-C to close the program");

        loop {
            match receiver.recv().expect("Could not receive from channel.") {
                WatcherSignal::Exit => break,
                WatcherSignal::Watch(path, recursive_mode) => {
                    log::debug!("start file watching on '{}'", path.display());
                    match debouncer.watch(&path, recursive_mode) {
                        Ok(()) => {}
                        Err(err) => {
                            log::error!(
                                "unable to start watching file system at `{}`: {}",
                                path.display(),
                                err
                            );
                        }
                    }
                }
                WatcherSignal::Unwatch(path) => {
                    log::debug!("stop file watching on '{}'", path.display());
                    match debouncer.unwatch(&path) {
                        Ok(()) => {}
                        Err(err) => {
                            log::error!(
                                "unable to stop watching file system at `{}`: {}",
                                path.display(),
                                err
                            );
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn send_watch_signal(&self, location: &Path, recursive_mode: RecursiveMode) {
        if let Err(err) = self
            .sender
            .send(WatcherSignal::Watch(location.to_path_buf(), recursive_mode))
        {
            log::warn!(
                "unable to send signal to watch '{}': {}",
                location.display(),
                err
            );
        }
    }

    fn send_unwatch_signal(&self, path: &Path) {
        if let Err(err) = self.sender.send(WatcherSignal::Unwatch(path.to_path_buf())) {
            log::warn!(
                "unable to send signal to unwatch '{}': {}",
                path.display(),
                err
            );
        }
    }

    fn setup_ctrl_exit(&self) -> Result<(), CliError> {
        let sender = self.sender.clone();
        ctrlc::set_handler(move || {
            sender
                .send(WatcherSignal::Exit)
                .expect("unable to send signal to terminate")
        })
        .map_err(|err| {
            log::error!("unable to set Ctrl-C handler: {}", err);
            CliError::new(1)
        })?;
        Ok(())
    }

    /// Applies the events to the worker tree, and returns whether any of them
    /// concern the watched files.
    fn process_events(&mut self, mut events: Vec<DebouncedEvent>) -> bool {
        self.retain_relevant_events(&mut events);
        if events.is_empty() {
            return false;
        }
        let current_path = self.current_working_path.as_ref();
        let resources = &self.resources;

        let worker_tree = if let Some(worker_tree) = self.worker_tree.as_mut() {
            worker_tree
        } else {
            return true;
        };

        log::debug!("file watch has detected changes");

        let mut has_created = false;

        for event in events {
            let links = &self.links_file_watch;

            let mut paths_iterator = event.event.paths.iter().map(|path| {
                links
                    .iter()
                    .find_map(|(link_location, link_path)| {
                        path.starts_with(link_location)
                            .then_some(link_path.as_path())
                    })
                    .or_else(|| {
                        current_path.and_then(|current_path| path.strip_prefix(current_path).ok())
                    })
                    .unwrap_or(path)
            });

            if log::log_enabled!(log::Level::Trace) {
                let event_display = match event.kind {
                    EventKind::Any => Some("unknown operation on"),
                    EventKind::Create(_) => Some("created"),
                    EventKind::Modify(_) => Some("modified"),
                    EventKind::Remove(_) => Some("removed"),
                    EventKind::Access(_) | EventKind::Other => None,
                };
                if let Some(event_display) = event_display {
                    for path in event.event.paths.iter() {
                        log::trace!("file watcher: {} '{}'", event_display, path.display());
                    }
                }
            }

            match event.kind {
                EventKind::Any => {
                    for path in paths_iterator {
                        has_created = true;
                        worker_tree.source_changed(path);
                    }
                }
                EventKind::Create(_create_kind) => {
                    // macOS FSEvents keeps reporting a recently created path as
                    // "created" on later writes, and the debouncer folds a
                    // create+modify pair into a single Create. Only collecting
                    // new work here left in-place edits of known files
                    // re-processed from cached content, so their output never
                    // changed. Treat every path as changed, like `Any`.
                    for path in paths_iterator {
                        has_created = true;
                        worker_tree.source_changed(path);
                        reconcile_directory(worker_tree, resources, path);
                    }
                }
                EventKind::Modify(modify_kind) => {
                    if let ModifyKind::Name(_rename_mode) = modify_kind {
                        // A rename can be a file moving *away* (source gone) or one
                        // arriving *here* — including an editor "atomic save" (write
                        // a temp file, then rename it over the target). RenameMode is
                        // unreliable across platforms (macOS FSEvents often reports
                        // `Any`), so decide by whether the path still exists: present
                        // means re-process it; absent means it was renamed away.
                        for path in paths_iterator {
                            if path.exists() {
                                worker_tree.source_changed(path);
                                reconcile_directory(worker_tree, resources, path);
                            } else {
                                worker_tree.remove_source(path);
                            }
                        }
                        has_created = true;
                    } else {
                        for path in paths_iterator {
                            worker_tree.source_changed(path);
                        }
                    }
                }
                EventKind::Remove(_remove_kind) => {
                    // Like renames, a remove event can be stale by the time the
                    // debounced batch is processed: git (checkout, merge) and
                    // atomic-save editors replace files with unlink+create, and
                    // the recreate lands in the same debounce window. Removing
                    // the source then queues its output for deletion, which
                    // still executes even though later events in the batch
                    // re-add the file — permanently dropping the output. Decide
                    // by whether the path exists now: present means re-process,
                    // absent means actually removed.
                    for path in paths_iterator {
                        if path.exists() {
                            has_created = true;
                            worker_tree.source_changed(path);
                            reconcile_directory(worker_tree, resources, path);
                        } else {
                            worker_tree.remove_source(path);
                        }
                    }
                }
                EventKind::Access(_) | EventKind::Other => {}
            }
        }

        // Remove events can be lost entirely: macOS reports the deletion of a
        // recently created file with its "created" flag still set, and the
        // debouncer cancels the pair out. Any other event in the batch (a
        // sourcemap rewrite, a sibling edit) is a chance to notice sources that
        // are gone and clean their outputs.
        let input_path = self.input_path.clone();
        if let Some(worker_tree) = self.worker_tree.as_mut() {
            worker_tree.remove_missing_sources(&self.resources, &input_path);
        }

        if has_created {
            self.worker_collect_work();
            self.update_links();
        }

        true
    }

    /// Drops the events that can't change the output:
    ///
    /// - Access events, which include darklua itself opening the
    ///   configuration or a sourcemap to read it. Re-running on them would
    ///   read those files again and loop.
    /// - The paths reported only because they share a directory with a file
    ///   watched outside of the input, like a log darklua writes to.
    fn retain_relevant_events(&self, events: &mut Vec<DebouncedEvent>) {
        events.retain(|event| !event.kind.is_access());

        if self.extra_directory_watch.is_empty() {
            return;
        }

        let mut canonical_parents: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
        let mut is_watched = |path: &Path| {
            let parent = match path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent,
                Some(_) => Path::new("."),
                None => return true,
            };
            let canonical_parent = canonical_parents
                .entry(parent.to_path_buf())
                .or_insert_with(|| parent.canonicalize().ok());

            match (canonical_parent.as_ref(), path.file_name()) {
                (Some(canonical_parent), Some(file_name))
                    if self.extra_directory_watch.contains_key(canonical_parent) =>
                {
                    self.extra_file_watch
                        .contains(&canonical_parent.join(file_name))
                        || self
                            .links_file_watch
                            .iter()
                            .any(|(link_location, _)| path.starts_with(link_location))
                }
                _ => true,
            }
        };

        events.retain_mut(|event| {
            // Events without paths (like a rescan request) concern everything.
            if event.event.paths.is_empty() {
                return true;
            }
            event.event.paths.retain(|path| is_watched(path));
            !event.event.paths.is_empty()
        });
    }

    fn worker_collect_work(&mut self) {
        let options = self.build_options();
        if let Some(worker_tree) = self.worker_tree.as_mut() {
            log_darklua_error(worker_tree.collect_work(&self.resources, &options), || ());
        }
    }

    /// Watches the files outside of the input that processing depends on,
    /// like a Rojo sourcemap, and the configuration file.
    ///
    /// Each file is watched through its directory. A watch on the file itself
    /// follows the file's inode on Linux (inotify), so it stops reporting
    /// changes once the file is replaced by a rename, which is how Rojo and
    /// many editors write files atomically.
    fn update_extra_file_watch(&mut self) {
        let dependencies = self
            .worker_tree
            .iter()
            .flat_map(|worker_tree| worker_tree.iter_external_dependencies());

        let mut files = HashSet::new();
        let mut directories = HashMap::new();

        for file in self
            .config_paths
            .iter()
            .map(PathBuf::as_path)
            .chain(dependencies)
        {
            let directory = match file.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent,
                _ => Path::new("."),
            };
            let (Ok(canonical_directory), Some(file_name)) =
                (directory.canonicalize(), file.file_name())
            else {
                log::warn!(
                    "unable to watch '{}': its directory does not exist",
                    file.display()
                );
                continue;
            };

            files.insert(canonical_directory.join(file_name));

            // Files in the input are already watched with it.
            let in_input = self
                .canonical_input_path
                .as_ref()
                .is_some_and(|input| canonical_directory.starts_with(input));
            if !in_input {
                directories
                    .entry(canonical_directory)
                    .or_insert_with(|| directory.to_path_buf());
            }
        }

        for (canonical_directory, directory) in &self.extra_directory_watch {
            if !directories.contains_key(canonical_directory) {
                self.send_unwatch_signal(directory);
            }
        }
        for (canonical_directory, directory) in &directories {
            if !self.extra_directory_watch.contains_key(canonical_directory) {
                self.send_watch_signal(directory, RecursiveMode::NonRecursive);
            }
        }

        self.extra_file_watch = files;
        self.extra_directory_watch = directories;
    }

    fn update_links(&mut self) {
        let new_links: HashSet<_> = iter_all_links(self.input_path.clone())
            .filter_map(|link_path| {
                link_path
                    .read_link()
                    .ok()
                    .map(|link_location| (link_location, link_path))
            })
            .collect();

        diff_sets(
            &new_links,
            &self.links_file_watch,
            |(link_location, _link_path)| {
                self.send_watch_signal(link_location, RecursiveMode::Recursive);
            },
            |(link_location, _link_path)| {
                self.send_unwatch_signal(link_location);
            },
        );

        self.links_file_watch = new_links;
    }
}

/// A directory that still exists after a rename/create/remove event may have
/// lost files without their own events (e.g. `mv new old` over an existing
/// directory). Drop the sources that are gone so their outputs get cleaned.
fn reconcile_directory(worker_tree: &mut WorkerTree, resources: &Resources, path: &Path) {
    if path.is_dir() {
        worker_tree.remove_missing_sources(resources, path);
    }
}

fn diff_sets<T: Eq + Hash>(
    new_set: &HashSet<T>,
    previous_set: &HashSet<T>,
    on_added: impl Fn(&T),
    on_removed: impl Fn(&T),
) {
    for item in previous_set.difference(new_set) {
        on_removed(item);
    }

    for item in new_set.difference(previous_set) {
        on_added(item);
    }
}

fn iter_all_links(location: PathBuf) -> impl Iterator<Item = PathBuf> {
    let mut unknown_paths = vec![location];
    let mut links = Vec::new();
    let mut dir_entries = Vec::new();

    iter::from_fn(move || loop {
        if let Some(location) = unknown_paths.pop() {
            match location.symlink_metadata() {
                Ok(metadata) => {
                    if metadata.is_symlink() {
                        links.push(location);
                    } else if metadata.is_dir() {
                        dir_entries.push(location.to_path_buf());
                    };
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => {
                    log::warn!(
                        "unable to read metadata from file `{}`: {}",
                        location.display(),
                        err
                    );
                }
            }
        } else if let Some(dir_location) = dir_entries.pop() {
            match dir_location.read_dir() {
                Ok(read_dir) => {
                    for entry in read_dir {
                        match entry {
                            Ok(entry) => {
                                unknown_paths.push(entry.path());
                            }
                            Err(err) => {
                                log::warn!(
                                    "unable to read directory entry `{}`: {}",
                                    dir_location.display(),
                                    err
                                );
                            }
                        }
                    }
                }
                Err(err) => {
                    log::warn!(
                        "unable to read directory `{}`: {}",
                        dir_location.display(),
                        err
                    );
                }
            }
        } else if let Some(path) = links.pop() {
            break Some(path);
        } else {
            break None;
        }
    })
}

fn log_darklua_error<T>(
    result: Result<T, darklua_core::DarkluaError>,
    else_result: impl Fn() -> T,
) -> T {
    result
        .inspect_err(|err| {
            log::error!("{}", err);
        })
        .unwrap_or_else(|_| else_result())
}
