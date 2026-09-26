//! Runs `darklua process --watch` on a temporary project and checks that the
//! outputs follow changes to the files they depend on.

use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

use tempfile::{tempdir_in, TempDir};

const TIMEOUT: Duration = Duration::from_secs(30);
/// Longer than darklua's debounce window.
const SETTLE: Duration = Duration::from_secs(1);

struct WatchProcess {
    child: Child,
    directory: TempDir,
}

impl WatchProcess {
    fn start(directory: TempDir) -> Self {
        let log = File::create(directory.path().join("darklua.log")).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_darklua"))
            .args(["process", "src", "out", "--config", "darklua.json"])
            .args(["--watch", "--verbose", "--verbose"])
            .current_dir(directory.path())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("unable to start darklua");

        Self { child, directory }
    }

    fn path(&self, path: &str) -> PathBuf {
        self.directory.path().join(path)
    }

    fn wait_until(&self, what: &str, mut condition: impl FnMut() -> bool) {
        let start = Instant::now();

        while !condition() {
            if start.elapsed() > TIMEOUT {
                let log = fs::read_to_string(self.path("darklua.log")).unwrap_or_default();
                panic!("timed out waiting for {}\n\ndarklua output:\n{}", what, log);
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_for_output(&self, path: &str, expected: &str) {
        let output = self.path(path);
        self.wait_until(&format!("`{path}` to contain `{expected}`"), || {
            fs::read_to_string(&output).is_ok_and(|content| content.contains(expected))
        });
    }

    /// Checks that darklua stops re-running once nothing changes, so it
    /// doesn't react to its own reads of the files it watches or to the log
    /// it writes next to them.
    fn assert_settles(&self) {
        let runs = || {
            fs::read_to_string(self.path("darklua.log"))
                .unwrap_or_default()
                .matches("changes detected")
                .count()
        };

        thread::sleep(SETTLE);
        let settled_runs = runs();
        thread::sleep(SETTLE * 2);
        assert_eq!(runs(), settled_runs, "darklua kept re-running");
    }
}

impl Drop for WatchProcess {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

fn project(files: &[(&str, &str)]) -> TempDir {
    let directory =
        tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("unable to create temporary directory");

    for (path, content) in files {
        write(&directory.path().join(path), content);
    }

    directory
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Replaces a file the way Rojo and many editors do: write a temporary file,
/// then rename it over the original.
fn replace(path: &Path, content: &str) {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, content).unwrap();
    fs::rename(&temporary, path).unwrap();
}

fn sourcemap(module_name: &str) -> String {
    format!(
        r#"{{"name": "project", "className": "DataModel", "children": [
            {{"name": "ReplicatedStorage", "className": "ReplicatedStorage", "children": [
                {{"name": "A", "className": "ModuleScript", "filePaths": ["src/A.luau"]}},
                {{"name": "{module_name}", "className": "ModuleScript", "filePaths": ["src/B.luau"]}}
            ]}}
        ]}}"#
    )
}

const CONVERT_REQUIRE_CONFIG: &str = r#"{"rules": [{
    "rule": "convert_require",
    "current": {"name": "luau"},
    "target": {"name": "roblox", "rojo_sourcemap": "./sourcemap.json"}
}]}"#;

#[test]
fn follows_a_dependency_replaced_by_renames() {
    let watch = WatchProcess::start(project(&[
        ("darklua.json", CONVERT_REQUIRE_CONFIG),
        ("sourcemap.json", &sourcemap("First")),
        ("src/A.luau", "local B = require(\"./B\")\nreturn B\n"),
        ("src/B.luau", "return 1\n"),
    ]));
    watch.wait_for_output("out/A.luau", "First");

    // A watch on the file itself is lost after the first replacement on Linux.
    // Back-to-back replacements can hide that, so space them out.
    for module_name in ["Second", "Third", "Fourth"] {
        thread::sleep(SETTLE);
        replace(&watch.path("sourcemap.json"), &sourcemap(module_name));
        watch.wait_for_output("out/A.luau", module_name);
    }

    watch.assert_settles();
}

fn inject_value_config(value: &str) -> String {
    format!(
        r#"{{"rules": [{{"rule": "inject_global_value", "identifier": "VALUE", "value": "{value}"}}]}}"#
    )
}

#[test]
fn follows_a_config_replaced_by_renames() {
    let watch = WatchProcess::start(project(&[
        ("darklua.json", &inject_value_config("first")),
        ("src/A.luau", "return VALUE\n"),
    ]));
    watch.wait_for_output("out/A.luau", "first");

    for value in ["second", "third"] {
        thread::sleep(SETTLE);
        replace(&watch.path("darklua.json"), &inject_value_config(value));
        watch.wait_for_output("out/A.luau", value);
    }

    watch.assert_settles();
}

#[test]
fn processes_a_recreated_file_with_its_new_content() {
    let watch = WatchProcess::start(project(&[
        ("darklua.json", r#"{"rules": []}"#),
        ("src/A.luau", "return 'first'\n"),
    ]));
    watch.wait_for_output("out/A.luau", "first");

    // Past the debounce window, so the removal and the new file land in
    // separate batches.
    fs::remove_file(watch.path("src/A.luau")).unwrap();
    thread::sleep(SETTLE);
    write(&watch.path("src/A.luau"), "return 'second'\n");
    watch.wait_for_output("out/A.luau", "second");
}

#[test]
fn removes_the_output_of_a_removed_file() {
    let watch = WatchProcess::start(project(&[
        ("darklua.json", r#"{"rules": []}"#),
        ("src/A.luau", "return 'a'\n"),
        ("src/B.luau", "return 'b'\n"),
    ]));
    watch.wait_for_output("out/A.luau", "a");

    fs::remove_file(watch.path("src/A.luau")).unwrap();
    let output = watch.path("out/A.luau");
    watch.wait_until("`out/A.luau` to be removed", || !output.exists());
}
