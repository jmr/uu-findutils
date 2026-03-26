// Copyright 2017 Google Inc.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT.

use std::cell::RefCell;
use std::error::Error;
use std::ffi::OsString;
use std::io::{stderr, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{Matcher, MatcherIO, WalkEntry};

enum Arg {
    FileArg(Vec<OsString>),
    LiteralArg(OsString),
}

impl Arg {
    fn new(a: &str) -> Self {
        let parts = a.split("{}").collect::<Vec<_>>();
        if parts.len() == 1 {
            // No {} present
            Arg::LiteralArg(OsString::from(a))
        } else {
            Arg::FileArg(parts.iter().map(OsString::from).collect())
        }
    }

    fn render(&self, path_to_file: &std::ffi::OsStr) -> OsString {
        match self {
            Arg::LiteralArg(ref a) => a.clone(),
            Arg::FileArg(ref parts) => parts.join(path_to_file),
        }
    }

    fn to_string_lossy(&self) -> String {
        match self {
            Arg::LiteralArg(ref a) => a.to_string_lossy().into_owned(),
            Arg::FileArg(ref parts) => {
                let s_parts: Vec<_> = parts.iter().map(|p| p.to_string_lossy()).collect();
                s_parts.join("{}")
            }
        }
    }
}

/// Helper to get the path to the file being matched, potentially relative to its parent
/// if `exec_in_parent_dir` is true (for -execdir).
fn get_path_to_file(file_info: &WalkEntry, exec_in_parent_dir: bool) -> PathBuf {
    if exec_in_parent_dir {
        if let Some(f) = file_info.path().file_name() {
            Path::new(".").join(f)
        } else {
            // For root directories or other cases without a file name, use the full path.
            Path::new(".").join(file_info.path())
        }
    } else {
        file_info.path().to_path_buf()
    }
}

pub struct SingleExecMatcher {
    executable: Arg,
    args: Vec<Arg>,
    exec_in_parent_dir: bool,
}

impl SingleExecMatcher {
    pub fn new(
        executable: &str,
        args: &[&str],
        exec_in_parent_dir: bool,
    ) -> Result<Self, Box<dyn Error>> {
        let transformed_args = args.iter().map(|&a| Arg::new(a)).collect();

        Ok(Self {
            executable: Arg::new(executable),
            args: transformed_args,
            exec_in_parent_dir,
        })
    }
}

impl Matcher for SingleExecMatcher {
    fn matches(&self, file_info: &WalkEntry, _: &mut MatcherIO) -> bool {
        let path_to_file = get_path_to_file(file_info, self.exec_in_parent_dir);

        // POSIX requires that a utility_name or argument consisting solely of "{}"
        // be replaced with the matched pathname. If it includes "{}" but is not
        // solely "{}", POSIX says the behavior is implementation-defined. We follow
        // GNU find and replace "{}" everywhere, including embedded within the utility_name.
        let exe_os_string = self.executable.render(path_to_file.as_os_str());
        let mut command = Command::new(&exe_os_string);

        for arg in &self.args {
            command.arg(arg.render(path_to_file.as_os_str()));
        }
        if self.exec_in_parent_dir {
            match file_info.path().parent() {
                None => {
                    // Root paths like "/" have no parent.  Run them from the root to match GNU find.
                    command.current_dir(file_info.path());
                }
                Some(parent) if parent == Path::new("") => {
                    // Paths like "foo" have a parent of "".  Avoid chdir("").
                }
                Some(parent) => {
                    command.current_dir(parent);
                }
            }
        }
        match command.status() {
            Ok(status) => status.success(),
            Err(e) => {
                let exe_str = exe_os_string.to_string_lossy();
                writeln!(&mut stderr(), "Failed to run {exe_str}: {e}").unwrap();
                false
            }
        }
    }

    fn has_side_effects(&self) -> bool {
        true
    }
}

pub struct MultiExecMatcher {
    executable: Arg,
    args: Vec<OsString>,
    exec_in_parent_dir: bool,
    /// Command to build while matching.
    command: RefCell<Option<argmax::Command>>,
}

impl MultiExecMatcher {
    pub fn new(
        executable: &str,
        args: &[&str],
        exec_in_parent_dir: bool,
    ) -> Result<Self, Box<dyn Error>> {
        let transformed_args = args.iter().map(OsString::from).collect();

        Ok(Self {
            executable: Arg::new(executable),
            args: transformed_args,
            exec_in_parent_dir,
            command: RefCell::new(None),
        })
    }

    /// Constructs a fresh command, pre-loaded with the fixed arguments.
    /// `first_path` is the first matched path for this batch; it is used as
    /// the executable when the executable expression is "{}".
    fn new_command(&self, first_path: &Path) -> argmax::Command {
        // POSIX requires that a utility_name or argument consisting solely of "{}"
        // be replaced with the matched pathname. If it includes "{}" but is not
        // solely "{}", POSIX says the behavior is implementation-defined. We follow
        // GNU find and replace "{}" everywhere, including embedded within the utility_name.
        let exe_os_string = self.executable.render(first_path.as_os_str());
        let mut command = argmax::Command::new(exe_os_string);
        command.try_args(&self.args).unwrap();
        command
    }

    fn run_command(&self, command: &mut argmax::Command, matcher_io: &mut MatcherIO) {
        match command.status() {
            Ok(status) => {
                if !status.success() {
                    matcher_io.set_exit_code(1);
                }
            }
            Err(e) => {
                let exe_str = self.executable.to_string_lossy();
                writeln!(&mut stderr(), "Failed to run {exe_str}: {e}").unwrap();
                matcher_io.set_exit_code(1);
            }
        }
    }
}

impl Matcher for MultiExecMatcher {
    fn matches(&self, file_info: &WalkEntry, matcher_io: &mut MatcherIO) -> bool {
        let path_to_file = get_path_to_file(file_info, self.exec_in_parent_dir);
        let mut command = self.command.borrow_mut();
        let command = command.get_or_insert_with(|| self.new_command(&path_to_file));

        // Build command, or dispatch it before when it is long enough.
        if command.try_arg(&path_to_file).is_err() {
            if self.exec_in_parent_dir {
                match file_info.path().parent() {
                    None => {
                        // Root paths like "/" have no parent.  Run them from the root to match GNU find.
                        command.current_dir(file_info.path());
                    }
                    Some(parent) if parent == Path::new("") => {
                        // Paths like "foo" have a parent of "".  Avoid chdir("").
                    }
                    Some(parent) => {
                        command.current_dir(parent);
                    }
                }
            }
            self.run_command(command, matcher_io);

            // Reset command status.
            *command = self.new_command(&path_to_file);
            if let Err(e) = command.try_arg(&path_to_file) {
                writeln!(
                    &mut stderr(),
                    "Cannot fit a single argument {}: {}",
                    &path_to_file.to_string_lossy(),
                    e
                )
                .unwrap();
                matcher_io.set_exit_code(1);
            }
        }
        true
    }

    fn finished_dir(&self, dir: &Path, matcher_io: &mut MatcherIO) {
        // Dispatch command for -execdir.
        if self.exec_in_parent_dir {
            let mut command = self.command.borrow_mut();
            if let Some(mut command) = command.take() {
                command.current_dir(Path::new(".").join(dir));
                self.run_command(&mut command, matcher_io);
            }
        }
    }

    fn finished(&self, matcher_io: &mut MatcherIO) {
        // Dispatch command for -exec.
        if !self.exec_in_parent_dir {
            let mut command = self.command.borrow_mut();
            if let Some(mut command) = command.take() {
                self.run_command(&mut command, matcher_io);
            }
        }
    }

    fn has_side_effects(&self) -> bool {
        true
    }
}

#[cfg(test)]
/// No tests here, because we need to call out to an external executable. See
/// `tests/exec_unit_tests.rs` instead.
mod tests {}
