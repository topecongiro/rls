// Copyright 2016-2017 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

pub use self::cargo::make_cargo_config;

use data::Analysis;
use vfs::Vfs;
use config::Config;

use self::environment::EnvironmentLock;

use std::boxed::FnBox;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, Write};
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

mod environment;
mod cargo;
mod rustc;
mod plan;

use self::plan::{Plan as BuildPlan, WorkStatus};

/// Manages builds.
///
/// The IDE will request builds quickly (possibly on every keystroke), there is
/// no point running every one. We also avoid running more than one build at once.
/// We cannot cancel builds. It might be worth running builds in parallel or
/// canceling a started build.
///
/// High priority builds are started 'straightaway' (builds cannot be interrupted).
/// Normal builds are started after a timeout. A new build request cancels any
/// pending build requests.
///
/// From the client's point of view, a build request is not guaranteed to cause
/// a build. However, a build is guaranteed to happen and that build will begin
/// after the build request is received (no guarantee on how long after), and
/// that build is guaranteed to have finished before the build request returns.
///
/// There is no way for the client to specify that an individual request will
/// result in a build. However, you can tell from the result - if a build
/// was run, the build result will contain any errors or warnings and an indication
/// of success or failure. If the build was not run, the result indicates that
/// it was squashed.
///
/// The build queue should be used from the RLS main thread, it should not be
/// used from multiple threads. It will spawn threads itself as necessary.
//
// See comment on `request_build` for implementation notes.
pub struct BuildQueue {
    internals: Arc<Internals>,
    // The build queue - we only have one low and one high priority build waiting.
    // (low, high) priority builds.
    // This lock should only be held transiently.
    queued: Arc<Mutex<(Build, Build)>>,
}

/// Used when tracking modified files across different builds.
type FileVersion = u64;

// Information needed to run and configure builds.
struct Internals {
    // Arguments and environment with which we call rustc.
    // This can be further expanded for multi-crate target configuration.
    // This lock should only be held transiently.
    compilation_cx: Arc<Mutex<CompilationContext>>,
    env_lock: Arc<EnvironmentLock>,
    /// Set of files that were modified since last build.
    dirty_files: Arc<Mutex<HashMap<PathBuf, FileVersion>>>,
    vfs: Arc<Vfs>,
    // This lock should only be held transiently.
    config: Arc<Mutex<Config>>,
    building: AtomicBool,
}

#[derive(Debug)]
pub enum BuildResult {
    // Build was succesful, argument is warnings.
    Success(Vec<String>, Vec<Analysis>),
    // Build finished with errors, argument is errors and warnings.
    Failure(Vec<String>, Vec<Analysis>),
    // Build was coelesced with another build.
    Squashed,
    // There was an error attempting to build.
    Err,
}

/// Priority for a build request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildPriority {
    /// Run this build as soon as possible (e.g., on save or explicit build request) (not currently used).
    Immediate,
    /// Immediate, plus re-run Cargo.
    Cargo,
    /// A regular build request (e.g., on a minor edit).
    Normal,
}

/// Information passed to Cargo/rustc to build.
#[derive(Debug)]
struct CompilationContext {
    /// args and envs are saved from Cargo and passed to rustc.
    args: Vec<String>,
    envs: HashMap<String, Option<OsString>>,
    /// The build directory is supplied by the client and passed to Cargo.
    build_dir: Option<PathBuf>,
    /// Build plan, which should know all the inter-package/target dependencies
    /// along with args/envs. Only contains inter-package dep-graph for now.
    build_plan: BuildPlan,
}

impl CompilationContext {
    fn new() -> CompilationContext {
        CompilationContext {
            args: vec![],
            envs: HashMap::new(),
            build_dir: None,
            build_plan: BuildPlan::new(),
        }
    }
}

/// Status of the build queue.
///
/// Pending should only be replaced if it is built or squashed. InProgress can be
/// replaced by None or Pending when appropriate. That is, Pending means something
/// is ready and something else may or may not be being built.
enum Build {
    // A build is in progress.
    InProgress,
    // A build is queued.
    Pending(PendingBuild),
    // No build.
    None,
}

/// Represents a queued build.
struct PendingBuild {
    build_dir: PathBuf,
    priority: BuildPriority,
    built_files: HashMap<PathBuf, FileVersion>,
    // Closure to execute once the build is complete.
    and_then: Box<FnBox(BuildResult) + Send + 'static>,
}

impl Build {
    fn is_pending(&self) -> bool {
        match *self {
            Build::Pending(_) => true,
            _ => false,
        }
    }

    // True if the build is waiting and where it should be impossible for one to
    // be in progress.
    fn is_pending_fresh(&self) -> bool {
        match *self {
            Build::Pending(_) => true,
            Build::InProgress => unreachable!(),
            Build::None => false,
        }
    }

    fn as_pending(self) -> PendingBuild {
        match self {
            Build::Pending(b) => b,
            _ => unreachable!(),
        }
    }
}

impl BuildQueue {
    pub fn new(vfs: Arc<Vfs>, config: Arc<Mutex<Config>>) -> BuildQueue {
        BuildQueue {
            internals: Arc::new(Internals::new(vfs, config)),
            queued: Arc::new(Mutex::new((Build::None, Build::None))),
        }
    }

    // Requests a build (see comments on BuildQueue for what that means).
    //
    // Now for the complicated bits. Not all builds are equal - they might have
    // different arguments, build directory, etc. Lets call all such things the
    // context for the build. We don't try and compare contexts but rely on some
    // invariants:
    // * context can only change if the build priority is `Cargo` or the build_dir
    //   changes (in the latter case we upgrade the priority to `Cargo`).
    // * If the context changes, all previous build requests can be ignored (even
    //   if they change the context themselves).
    // * If there are multiple requests with the same context, we can skip all
    //   but the most recent.
    // * A pending request is obsolete (and may be discarded) if a more recent
    //   request has happened.
    //
    // ## implementation
    //
    // This layer of the build queue is single-threaded and we aim to return quickly.
    // A single build thread is spawned to do any building (we never do parallel
    // builds so that we don't hog the CPU, we might want to change that in the
    // future).
    //
    // There is never any point in queuing more than one build of each priority
    // (we might want to do a high priority build, then a low priority one). So
    // our build queue is just a single slot (for each priority). We record if a
    // build is waiting and if not, if a build is running.
    //
    // `and_then` is a closure to run after a build has completed or been squashed.
    // It must return quickly and without blocking. If it has work to do, it should
    // spawn a thread to do it.
    pub fn request_build<F>(&self, new_build_dir: &Path, mut priority: BuildPriority, and_then: F)
    where
        F: FnOnce(BuildResult) + Send + 'static,
    {
        trace!("request_build {:?}", priority);
        let needs_compilation_ctx_from_cargo = {
            let context = self.internals.compilation_cx.lock().unwrap();
            context.args.is_empty() && context.envs.is_empty()
        };
        if needs_compilation_ctx_from_cargo {
            priority = BuildPriority::Cargo;
        }

        let build = PendingBuild {
            build_dir: new_build_dir.to_owned(),
            built_files: self.internals.dirty_files.lock().unwrap().clone(),
            priority,
            and_then: Box::new(and_then),
        };

        let queued_clone = self.queued.clone();
        let internals_clone = self.internals.clone();

        let mut queued = self.queued.lock().unwrap();
        Self::push_build(&mut queued, build);

        // Need to spawn while holding the lock on queued so that we don't race.
        if !self.internals.building.swap(true, Ordering::SeqCst) {
            thread::spawn(move || {
                BuildQueue::run_thread(queued_clone, &internals_clone);
                let building = internals_clone.building.swap(false, Ordering::SeqCst);
                assert!(building);
            });
        }
    }

    // Takes the unlocked build queue and pushes an incoming build onto it.
    fn push_build(queued: &mut (Build, Build), mut build: PendingBuild) {
        if build.priority == BuildPriority::Normal {
            if let Build::None = queued.0 {
                if let Build::None = queued.1 {
                    // If there are no builds pending or running, we can start one
                    // immediately.
                    build.priority = BuildPriority::Immediate;
                    queued.1 = Build::Pending(build);
                    return;
                }
            }
            Self::squash_build(&mut queued.0);
            queued.0 = Build::Pending(build);
        } else {
            Self::squash_build(&mut queued.0);
            Self::squash_build(&mut queued.1);
            queued.1 = Build::Pending(build);
        }
    }

    // Takes a reference to a build in the queue in preparation for pushing a
    // new build into the queue. The build is removed (if it exists) and its
    // closure is notified that the build is squashed.
    fn squash_build(build: &mut Build) {
        let mut old_build = Build::None;
        mem::swap(build, &mut old_build);
        if let Build::Pending(build) = old_build {
            let and_then = build.and_then;
            and_then(BuildResult::Squashed);
        }
    }

    // Run the build thread. This thread will keep going until the build queue is
    // empty, then terminate.
    fn run_thread(queued: Arc<Mutex<(Build, Build)>>, internals: &Internals) {
        loop {
            // Find the next build to run, or terminate if there are no builds.
            let build = {
                let mut queued = queued.lock().unwrap();
                if queued.1.is_pending_fresh() {
                    let mut build = Build::InProgress;
                    mem::swap(&mut queued.1, &mut build);
                    build.as_pending()
                } else if queued.0.is_pending_fresh() {
                    let mut build = Build::InProgress;
                    mem::swap(&mut queued.0, &mut build);
                    build.as_pending()
                } else {
                    return;
                }
            };

            let and_then = build.and_then;

            // Normal priority threads sleep before starting up.
            if build.priority == BuildPriority::Normal {
                let wait_to_build = {
                    // Release lock before we sleep
                    let config = internals.config.lock().unwrap();
                    config.wait_to_build
                };
                trace!("sleeping");
                thread::sleep(Duration::from_millis(wait_to_build));

                // Check if a new build arrived while we were sleeping.
                let interupt = {
                    let queued = queued.lock().unwrap();
                    queued.0.is_pending() || queued.1.is_pending()
                };
                if interupt {
                    and_then(BuildResult::Squashed);
                    continue;
                }
            }

            // Run the build.
            let result = internals.run_build(&build.build_dir, build.priority, &build.built_files);
            // Assert that the build was not squashed.
            if let BuildResult::Squashed = result {
                unreachable!();
            }
            and_then(result);

            // Remove the in-progress marker from the build queue.
            let mut queued = queued.lock().unwrap();
            if let Build::InProgress = queued.1 {
                queued.1 = Build::None;
            } else if let Build::InProgress = queued.0 {
                queued.0 = Build::None;
            }
        }
    }

    /// Marks a given versioned file as dirty since last build. The dirty flag
    /// will be cleared by a successful build that builds this or a more recent
    /// version of this file.
    pub fn mark_file_dirty(&self, file: PathBuf, version: FileVersion) {
        trace!("Marking file as dirty: {:?} ({})", file, version);
        self.internals
            .dirty_files
            .lock()
            .unwrap()
            .insert(file, version);
    }
}

impl Internals {
    fn new(vfs: Arc<Vfs>, config: Arc<Mutex<Config>>) -> Internals {
        Internals {
            compilation_cx: Arc::new(Mutex::new(CompilationContext::new())),
            vfs,
            config,
            dirty_files: Arc::new(Mutex::new(HashMap::new())),
            // Since environment is global mutable state and we can run multiple server
            // instances, be sure to use a global lock to ensure env var consistency
            env_lock: EnvironmentLock::get(),
            building: AtomicBool::new(false),
        }
    }

    // Entry point method for building.
    fn run_build(
        &self,
        new_build_dir: &Path,
        priority: BuildPriority,
        built_files: &HashMap<PathBuf, FileVersion>,
    ) -> BuildResult {
        trace!("run_build, {:?} {:?}", new_build_dir, priority);

        // Check if the build directory changed and update it.
        {
            let mut compilation_cx = self.compilation_cx.lock().unwrap();
            if compilation_cx
                .build_dir
                .as_ref()
                .map_or(true, |dir| dir != new_build_dir)
            {
                // We'll need to re-run cargo in this case.
                assert!(priority == BuildPriority::Cargo);
                (*compilation_cx).build_dir = Some(new_build_dir.to_owned());
            }

            if priority == BuildPriority::Cargo {
                // Killing these args indicates we'll do a full Cargo build.
                compilation_cx.args = vec![];
                compilation_cx.envs = HashMap::new();
            }
        }

        let result = self.build();
        // On a successful build, clear dirty files that were successfuly built
        // now. It's possible that a build was scheduled with given files, but
        // user later changed them. These should still be left as dirty (not built).
        match *&result {
            BuildResult::Success(_, _) | BuildResult::Failure(_, _) => {
                let mut dirty_files = self.dirty_files.lock().unwrap();
                dirty_files.retain(|file, dirty_version| {
                    built_files
                        .get(file)
                        .map(|built_version| built_version < dirty_version)
                        .unwrap_or(false)
                });
                trace!("Files still dirty after the build: {:?}", *dirty_files);
            }
            _ => {}
        };
        result
    }

    // Build the project.
    fn build(&self) -> BuildResult {
        trace!("running build");
        // When we change build directory (presumably because the IDE is
        // changing project), we must do a cargo build of the whole project.
        // Otherwise we just use rustc directly.
        //
        // The 'full cargo build' is a `cargo check` customised and run
        // in-process. Cargo will shell out to call rustc (this means the
        // the compiler available at runtime must match the compiler linked to
        // the RLS). All but the last crate are built as normal, we intercept
        // the call to the last crate and do our own rustc build. We cache the
        // command line args and environment so we can avoid running Cargo in
        // the future.
        //
        // Our 'short' rustc build runs rustc directly and in-process (we must
        // do this so we can load changed code from the VFS, rather than from
        // disk).

        // Don't hold this lock when we run Cargo.
        let needs_to_run_cargo = self.compilation_cx.lock().unwrap().args.is_empty();
        let workspace_mode = self.config.lock().unwrap().workspace_mode;

        if workspace_mode {
            // If the build plan has already been cached, use it, unless Cargo
            // has to be specifically rerun (e.g. when build scripts changed)
            let work = {
                let modified: Vec<_> = self.dirty_files.lock().unwrap().keys().cloned().collect();
                let cx = self.compilation_cx.lock().unwrap();
                cx.build_plan.prepare_work(&modified)
            };
            return match work {
                // In workspace_mode, cargo performs the full build and returns
                // appropriate diagnostics/analysis data
                WorkStatus::NeedsCargo => cargo::cargo(self),
                WorkStatus::Execute(job_queue) => job_queue.execute(self),
            };
        // In single package mode Cargo needs to be run to cache args/envs for
        // future rustc calls
        } else if needs_to_run_cargo {
            if let BuildResult::Err = cargo::cargo(self) {
                return BuildResult::Err;
            }
        }

        let compile_cx = self.compilation_cx.lock().unwrap();
        let args = &compile_cx.args;
        assert!(!args.is_empty());
        let envs = &compile_cx.envs;
        let build_dir = compile_cx.build_dir.as_ref().unwrap();
        let env_lock = self.env_lock.as_facade();
        rustc::rustc(&self.vfs, args, envs, build_dir, self.config.clone(), env_lock)
    }
}

// A threadsafe buffer for writing.
struct BufWriter(Arc<Mutex<Vec<u8>>>);

impl Write for BufWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}
