// Copyright 2017 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use cargo::core::{PackageId, Shell, Target, TargetKind, Verbosity, Workspace};
use cargo::ops::{compile_with_exec, CompileFilter, CompileMode, CompileOptions, Context, Executor,
                 Packages, Unit};
use cargo::util::{homedir, important_paths, CargoResult, Config as CargoConfig, ConfigValue,
                  ProcessBuilder};
use serde_json;

use data::Analysis;
use build::{BufWriter, BuildResult, CompilationContext, Internals};
use build::environment::{self, Environment, EnvironmentLock};
use config::Config;
use vfs::Vfs;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs::{read_dir, remove_file};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

// Runs an in-process instance of Cargo.
pub(super) fn cargo(internals: &Internals) -> BuildResult {
    let workspace_mode = internals.config.lock().unwrap().workspace_mode;

    let compilation_cx = internals.compilation_cx.clone();
    let config = internals.config.clone();
    let vfs = internals.vfs.clone();
    let env_lock = internals.env_lock.clone();

    let diagnostics = Arc::new(Mutex::new(vec![]));
    let diagnostics_clone = diagnostics.clone();
    let analyses = Arc::new(Mutex::new(vec![]));
    let analyses_clone = analyses.clone();
    let out = Arc::new(Mutex::new(vec![]));
    let out_clone = out.clone();

    // Cargo may or may not spawn threads to run the various builds, since
    // we may be in separate threads we need to block and wait our thread.
    // However, if Cargo doesn't run a separate thread, then we'll just wait
    // forever. Therefore, we spawn an extra thread here to be safe.
    let handle = thread::spawn(
        || run_cargo(compilation_cx, config, vfs, env_lock, diagnostics, analyses, out),
    );

    match handle
        .join()
        .map_err(|_| "thread panicked".into())
        .and_then(|res| res)
    {
        Ok(_) if workspace_mode => {
            let diagnostics = Arc::try_unwrap(diagnostics_clone)
                .unwrap()
                .into_inner()
                .unwrap();
            let analyses = Arc::try_unwrap(analyses_clone)
                .unwrap()
                .into_inner()
                .unwrap();
            BuildResult::Success(diagnostics, analyses)
        }
        Ok(_) => BuildResult::Success(vec![], vec![]),
        Err(err) => {
            let stdout = String::from_utf8(out_clone.lock().unwrap().to_owned()).unwrap();
            info!("cargo failed\ncause: {}\nstdout: {}", err, stdout);
            BuildResult::Err
        }
    }
}

fn run_cargo(
    compilation_cx: Arc<Mutex<CompilationContext>>,
    rls_config: Arc<Mutex<Config>>,
    vfs: Arc<Vfs>,
    env_lock: Arc<EnvironmentLock>,
    compiler_messages: Arc<Mutex<Vec<String>>>,
    analyses: Arc<Mutex<Vec<Analysis>>>,
    out: Arc<Mutex<Vec<u8>>>,
) -> CargoResult<()> {
    // Lock early to guarantee synchronized access to env var for the scope of Cargo routine.
    // Additionally we need to pass inner lock to RlsExecutor, since it needs to hand it down
    // during exec() callback when calling linked compiler in parallel, for which we need to
    // guarantee consistent environment variables.
    let (lock_guard, inner_lock) = env_lock.lock();

    let build_dir = {
        let mut compilation_cx = compilation_cx.lock().unwrap();
        // Since Cargo build routine will try to regenerate the unit dep graph,
        // we need to clear the existing dep graph.
        compilation_cx.build_plan.clear();

        compilation_cx.build_dir.as_ref().unwrap().clone()
    };
    // Note that this may not be equal build_dir when inside a workspace member
    let manifest_path = important_paths::find_root_manifest_for_wd(None, &build_dir)?;
    trace!("root manifest_path: {:?}", &manifest_path);
    // Cargo constructs relative paths from the manifest dir, so we have to pop "Cargo.toml"
    let manifest_dir = manifest_path.parent().unwrap();

    let mut shell = Shell::from_write(Box::new(BufWriter(out.clone())));
    shell.set_verbosity(Verbosity::Quiet);

    let config = {
        let rls_config = rls_config.lock().unwrap();

        let target_dir = rls_config.target_dir.as_ref().map(|p| p as &Path);
        make_cargo_config(manifest_dir, target_dir, shell)
    };

    let ws = Workspace::new(&manifest_path, &config)?;

    // TODO: It might be feasible to keep this CargoOptions structure cached and regenerate
    // it on every relevant configuration change
    let (opts, rustflags, clear_env_rust_log) = {
        // We mustn't lock configuration for the whole build process
        let rls_config = rls_config.lock().unwrap();

        let opts = CargoOptions::new(&rls_config);
        trace!("Cargo compilation options:\n{:?}", opts);
        let rustflags = prepare_cargo_rustflags(&rls_config);

        // Warn about invalid specified bin target or package depending on current mode
        // TODO: Return client notifications along with diagnostics to inform the user
        if !rls_config.workspace_mode {
            let cur_pkg_targets = ws.current().unwrap().targets();

            if let &Some(ref build_bin) = rls_config.build_bin.as_ref() {
                let mut bins = cur_pkg_targets.iter().filter(|x| x.is_bin());
                if let None = bins.find(|x| x.name() == build_bin) {
                    warn!(
                        "cargo - couldn't find binary `{}` specified in `build_bin` configuration",
                        build_bin
                    );
                }
            }
        } else {
            for package in &opts.package {
                if let None = ws.members().find(|x| x.name() == package) {
                    warn!(
                        "cargo - couldn't find member package `{}` specified in `analyze_package` \
                         configuration",
                        package
                    );
                }
            }
        }

        (opts, rustflags, rls_config.clear_env_rust_log)
    };

    let spec = Packages::from_flags(ws.is_virtual(), opts.all, &opts.exclude, &opts.package)?;

    let compile_opts = CompileOptions {
        target: opts.target.as_ref().map(|t| &t[..]),
        spec: spec,
        filter: CompileFilter::new(
            opts.lib,
            &opts.bin,
            opts.bins,
            // TODO: Support more crate target types
            &[],
            false,
            &[],
            false,
            &[],
            false,
            false,
        ),
        features: &opts.features,
        all_features: opts.all_features,
        no_default_features: opts.no_default_features,
        ..CompileOptions::default(&config, CompileMode::Check)
    };

    // Create a custom environment for running cargo, the environment is reset afterwards automatically
    let mut env: HashMap<String, Option<OsString>> = HashMap::new();
    env.insert("RUSTFLAGS".to_owned(), Some(rustflags.into()));

    if clear_env_rust_log {
        env.insert("RUST_LOG".to_owned(), None);
    }

    let _restore_env = Environment::push_with_lock(&env, lock_guard);

    let exec = RlsExecutor::new(
        &ws,
        compilation_cx.clone(),
        rls_config.clone(),
        inner_lock,
        vfs,
        compiler_messages,
        analyses,
    );

    compile_with_exec(&ws, &compile_opts, Arc::new(exec))?;

    trace!(
        "Created build plan after Cargo compilation routine: {:?}",
        compilation_cx.lock().unwrap().build_plan
    );

    Ok(())
}

struct RlsExecutor {
    compilation_cx: Arc<Mutex<CompilationContext>>,
    cur_package_id: Mutex<Option<PackageId>>,
    config: Arc<Mutex<Config>>,
    /// Because of the Cargo API design, we first acquire outer lock before creating the executor
    /// and calling the compilation function. This, resulting, inner lock is used to synchronize
    /// env var access during underlying `rustc()` calls during parallel `exec()` callback threads.
    env_lock: environment::InnerLock,
    vfs: Arc<Vfs>,
    analyses: Arc<Mutex<Vec<Analysis>>>,
    workspace_mode: bool,
    /// Packages which are directly a member of the workspace, for which
    /// analysis and diagnostics will be provided
    member_packages: Mutex<HashSet<PackageId>>,
    /// JSON compiler messages emitted for each primary compiled crate
    compiler_messages: Arc<Mutex<Vec<String>>>,
}

impl RlsExecutor {
    fn new(
        ws: &Workspace,
        compilation_cx: Arc<Mutex<CompilationContext>>,
        config: Arc<Mutex<Config>>,
        env_lock: environment::InnerLock,
        vfs: Arc<Vfs>,
        compiler_messages: Arc<Mutex<Vec<String>>>,
        analyses: Arc<Mutex<Vec<Analysis>>>,
    ) -> RlsExecutor {
        let workspace_mode = config.lock().unwrap().workspace_mode;
        let (cur_package_id, member_packages) = if workspace_mode {
            let member_packages = ws.members().map(|x| x.package_id().clone()).collect();
            (None, member_packages)
        } else {
            let pkg_id = ws.current_opt()
                .expect("No current package in Cargo")
                .package_id()
                .clone();
            (Some(pkg_id), HashSet::new())
        };

        RlsExecutor {
            compilation_cx,
            cur_package_id: Mutex::new(cur_package_id),
            config,
            env_lock,
            vfs,
            analyses,
            workspace_mode,
            member_packages: Mutex::new(member_packages),
            compiler_messages,
        }
    }

    /// Returns wheter a given package is a primary one (every member of the
    /// workspace is considered as such).
    fn is_primary_crate(&self, id: &PackageId) -> bool {
        if self.workspace_mode {
            self.member_packages.lock().unwrap().contains(id)
        } else {
            let cur_package_id = self.cur_package_id.lock().unwrap();
            id
                == cur_package_id
                    .as_ref()
                    .expect("Executor has not been initialised")
        }
    }
}

impl Executor for RlsExecutor {
    /// Called after a rustc process invocation is prepared up-front for a given
    /// unit of work (may still be modified for runtime-known dependencies, when
    /// the work is actually executed). This is called even for a target that
    /// is fresh and won't be compiled.
    fn init(&self, cx: &Context, unit: &Unit) {
        let mut compilation_cx = self.compilation_cx.lock().unwrap();
        let plan = &mut compilation_cx.build_plan;
        let only_primary = |unit: &Unit| self.is_primary_crate(unit.pkg.package_id());

        if let Err(err) = plan.emplace_dep_with_filter(&unit, &cx, &only_primary) {
            error!("{:?}", err);
        }
    }

    fn force_rebuild(&self, unit: &Unit) -> bool {
        // In workspace_mode we need to force rebuild every package in the
        // workspace, even if it's not dirty at a time, to cache compiler
        // invocations in the build plan.
        // We only do a cargo build if we want to force rebuild the last
        // crate (e.g., because some args changed). Therefore we should
        // always force rebuild the primary crate.
        let id = unit.pkg.package_id();
        // FIXME build scripts - this will force rebuild build scripts as
        // well as the primary crate. But this is not too bad - it means
        // we will rarely rebuild more than we have to.
        self.is_primary_crate(id)
    }

    fn exec(&self, cargo_cmd: ProcessBuilder, id: &PackageId, target: &Target) -> CargoResult<()> {
        // Delete any stale data. We try and remove any json files with
        // the same crate name as Cargo would emit. This includes files
        // with the same crate name but different hashes, e.g., those
        // made with a different compiler.
        let cargo_args = cargo_cmd.get_args();
        let crate_name =
            parse_arg(cargo_args, "--crate-name").expect("no crate-name in rustc command line");
        trace!("exec: {}", crate_name);

        let out_dir = parse_arg(cargo_args, "--out-dir").expect("no out-dir in rustc command line");
        let analysis_dir = Path::new(&out_dir).join("save-analysis");
        if let Ok(dir_contents) = read_dir(&analysis_dir) {
            let lib_crate_name = "lib".to_owned() + &crate_name;
            for entry in dir_contents {
                let entry = entry.expect("unexpected error reading save-analysis directory");
                let name = entry.file_name();
                let name = name.to_str().unwrap();
                if (name.starts_with(&crate_name) || name.starts_with(&lib_crate_name))
                    && name.ends_with(".json")
                {
                    if let Err(e) = remove_file(entry.path()) {
                        debug!("Error deleting file, {}: {}", name, e);
                    }
                }
            }
        }

        // Prepare our own call to `rustc` as follows:
        // 1. Use $RUSTC wrapper if specified, otherwise use RLS executable
        //    as an rustc shim (needed to distribute via the stable channel)
        // 2. For non-primary packages or build scripts, execute the call
        // 3. Otherwise, we'll want to use the compilation to drive the analysis:
        //    i.  Modify arguments to account for the RLS settings (e.g.
        //        compiling under cfg(test) mode or passing a custom sysroot)
        //    ii. Execute the call and store the final args/envs to be used for
        //        later in-process execution of the compiler
        let mut cmd = cargo_cmd.clone();
        let rls_executable = env::args().next().unwrap();
        let sysroot =
            current_sysroot().expect("need to specify SYSROOT env var or use rustup or multirust");

        cmd.program(env::var("RUSTC").unwrap_or(rls_executable));
        cmd.env(::RUSTC_SHIM_ENV_VAR_NAME, "1");

        // We only want to intercept rustc call targeting current crate to cache
        // args/envs generated by cargo so we can run only rustc later ourselves
        // Currently we don't cache nor modify build script args
        let is_build_script = *target.kind() == TargetKind::CustomBuild;
        if !self.is_primary_crate(id) || is_build_script {
            let build_script_notice = if is_build_script {
                " (build script)"
            } else {
                ""
            };
            trace!("rustc not intercepted - {}{}", id.name(), build_script_notice);

            if ::CRATE_BLACKLIST.contains(&&*crate_name) {
                // By running the original command (rather than using our shim), we
                // avoid producing save-analysis data.
                trace!("crate is blacklisted");
                return cargo_cmd.exec();
            }
            // Only include public symbols in externally compiled deps data
            let mut save_config = ::data::config::Config::default();
            save_config.pub_only = true;
            let save_config = serde_json::to_string(&save_config)?;
            cmd.env("RUST_SAVE_ANALYSIS_CONFIG", &OsString::from(save_config));

            cmd.arg("--sysroot");
            cmd.arg(&sysroot);
            return cmd.exec();
        }

        trace!("rustc intercepted - args: {:?} envs: {:?}", cargo_args, cargo_cmd.get_envs());

        let mut args: Vec<_> = cargo_args
            .iter()
            .map(|a| a.clone().into_string().unwrap())
            .collect();

        {
            let config = self.config.lock().unwrap();
            let crate_type = parse_arg(cargo_args, "--crate-type");
            // Becase we only try to emulate `cargo test` using `cargo check`, so for now
            // assume crate_type arg (i.e. in `cargo test` it isn't specified for --test targets)
            // and build test harness only for final crate type
            let crate_type = crate_type.expect("no crate-type in rustc command line");
            let build_lib = *config.build_lib.as_ref();
            let is_final_crate_type = crate_type == "bin" || (crate_type == "lib" && build_lib);

            if config.cfg_test {
                // FIXME(#351) allow passing --test to lib crate-type when building a dependency
                if is_final_crate_type {
                    args.push("--test".to_owned());
                } else {
                    args.push("--cfg".to_owned());
                    args.push("test".to_owned());
                }
            }
            if config.sysroot.is_none() {
                args.push("--sysroot".to_owned());
                args.push(sysroot);
            }

            // We can't omit compilation here, because Cargo is going to expect to get
            // dep-info for this crate, so we shell out to rustc to get that.
            // This is not really ideal, because we are going to
            // compute this info anyway when we run rustc ourselves, but we don't do
            // that before we return to Cargo.
            // FIXME Don't do this. Start our build here rather than on another thread
            // so the dep-info is ready by the time we return from this callback.
            // NB: In `workspace_mode` regular compilation is performed here (and we don't
            // only calculate dep-info) so it should fix the problem mentioned above.
            let modified =
                args.iter()
                .map(|a| {
                    // Emitting only dep-info is possible only for final crate type, as
                    // as others may emit required metadata for dependent crate types
                    if a.starts_with("--emit") && is_final_crate_type && !self.workspace_mode {
                        "--emit=dep-info"
                    } else { a }
                })
                .collect::<Vec<_>>();
            cmd.args_replace(&modified);
        }

        // Cache executed command for the build plan
        {
            let mut cx = self.compilation_cx.lock().unwrap();
            cx.build_plan.cache_compiler_job(id, target, &cmd);
        }

        // Prepare modified cargo-generated args/envs for future rustc calls
        let rustc = cargo_cmd.get_program().to_owned().into_string().unwrap();
        args.insert(0, rustc);
        let envs = cargo_cmd.get_envs().clone();

        if self.workspace_mode {
            let build_dir = {
                let cx = self.compilation_cx.lock().unwrap();
                cx.build_dir.clone().unwrap()
            };

            let env_lock = self.env_lock.as_facade();

            match super::rustc::rustc(
                &self.vfs,
                &args,
                &envs,
                &build_dir,
                self.config.clone(),
                env_lock,
            ) {
                BuildResult::Success(mut messages, mut analysis) |
                BuildResult::Failure(mut messages, mut analysis) => {
                    self.compiler_messages.lock().unwrap().append(&mut messages);
                    self.analyses.lock().unwrap().append(&mut analysis);
                }
                _ => {}
            }
        } else {
            cmd.exec()?;
        }

        // Finally, store the modified cargo-generated args/envs for future rustc calls
        let mut compilation_cx = self.compilation_cx.lock().unwrap();
        compilation_cx.args = args;
        compilation_cx.envs = envs;

        Ok(())
    }
}

#[derive(Debug)]
struct CargoOptions {
    package: Vec<String>,
    target: Option<String>,
    lib: bool,
    bin: Vec<String>,
    bins: bool,
    all: bool,
    exclude: Vec<String>,
    all_features: bool,
    no_default_features: bool,
    features: Vec<String>,
}

impl Default for CargoOptions {
    fn default() -> CargoOptions {
        CargoOptions {
            package: vec![],
            target: None,
            lib: false,
            bin: vec![],
            bins: false,
            all: false,
            exclude: vec![],
            all_features: false,
            no_default_features: false,
            features: vec![],
        }
    }
}

impl CargoOptions {
    fn new(config: &Config) -> CargoOptions {
        if config.workspace_mode {
            let (package, all) = match config.analyze_package {
                Some(ref pkg_name) => (vec![pkg_name.clone()], false),
                None => (vec![], true),
            };

            CargoOptions {
                package,
                all,
                target: config.target.clone(),
                features: config.features.clone(),
                all_features: config.all_features,
                no_default_features: config.no_default_features,
                ..CargoOptions::default()
            }
        } else {
            // In single-crate mode we currently support only one crate target,
            // and if lib is set, then we ignore bin target config
            let (lib, bin) = match *config.build_lib.as_ref() {
                true => (true, vec![]),
                false => {
                    let bin = match *config.build_bin.as_ref() {
                        Some(ref bin) => vec![bin.clone()],
                        None => vec![],
                    };
                    (false, bin)
                }
            };

            CargoOptions {
                lib,
                bin,
                target: config.target.clone(),
                features: config.features.clone(),
                all_features: config.all_features,
                no_default_features: config.no_default_features,
                ..CargoOptions::default()
            }
        }
    }
}

fn prepare_cargo_rustflags(config: &Config) -> String {
    let mut flags = "--error-format=json ".to_owned();

    if let Some(ref sysroot) = config.sysroot {
        flags.push_str(&format!(" --sysroot {}", sysroot));
    }

    flags = format!(
        "{} {} {}",
        env::var("RUSTFLAGS").unwrap_or(String::new()),
        config.rustflags.as_ref().unwrap_or(&String::new()),
        flags
    );

    dedup_flags(&flags)
}

pub fn make_cargo_config(build_dir: &Path, target_dir: Option<&Path>, shell: Shell) -> CargoConfig {
    let config = CargoConfig::new(
        shell,
        // This is Cargo's cwd. We're using the actual cwd,
        // because Cargo will generate relative paths based
        // on this to source files it wants to compile
        env::current_dir().unwrap(),
        homedir(&build_dir).unwrap(),
    );

    // Cargo is expecting the config to come from a config file and keeps
    // track of the path to that file. We'll make one up, it shouldn't be
    // used for much. Cargo does use it for finding a root path. Since
    // we pass an absolute path for the build directory, that doesn't
    // matter too much. However, Cargo still takes the grandparent of this
    // path, so we need to have at least two path elements.
    let config_path = build_dir.join("config").join("rls-config.toml");

    let mut config_value_map = config.load_values().unwrap();
    {
        let build_value = config_value_map
            .entry("build".to_owned())
            .or_insert(ConfigValue::Table(HashMap::new(), config_path.clone()));

        let target_dir = target_dir
            .map(|d| d.to_str().unwrap().to_owned())
            .unwrap_or_else(|| {
                build_dir
                    .join("target")
                    .join("rls")
                    .to_str()
                    .unwrap()
                    .to_owned()
            });
        let td_value = ConfigValue::String(target_dir, config_path);
        if let &mut ConfigValue::Table(ref mut build_table, _) = build_value {
            build_table.insert("target-dir".to_owned(), td_value);
        } else {
            unreachable!();
        }
    }

    config.set_values(config_value_map).unwrap();
    config
}

fn parse_arg(args: &[OsString], arg: &str) -> Option<String> {
    for (i, a) in args.iter().enumerate() {
        if a == arg {
            return Some(args[i + 1].clone().into_string().unwrap());
        }
    }
    None
}

fn current_sysroot() -> Option<String> {
    let home = env::var("RUSTUP_HOME").or(env::var("MULTIRUST_HOME"));
    let toolchain = env::var("RUSTUP_TOOLCHAIN").or(env::var("MULTIRUST_TOOLCHAIN"));
    if let (Ok(home), Ok(toolchain)) = (home, toolchain) {
        Some(format!("{}/toolchains/{}", home, toolchain))
    } else {
        let rustc_exe = env::var("RUSTC").unwrap_or("rustc".to_owned());
        env::var("SYSROOT").map(|s| s.to_owned()).ok().or_else(|| {
            Command::new(rustc_exe)
                .arg("--print")
                .arg("sysroot")
                .output()
                .ok()
                .and_then(|out| String::from_utf8(out.stdout).ok())
                .map(|s| s.trim().to_owned())
        })
    }
}


/// flag_str is a string of command line args for Rust. This function removes any
/// duplicate flags.
fn dedup_flags(flag_str: &str) -> String {
    // The basic strategy here is that we split flag_str into a set of keys and
    // values and dedup any duplicate keys, using the last value in flag_str.
    // This is a bit complicated because of the variety of ways args can be specified.

    // Retain flags order to prevent complete project rebuild due to RUSTFLAGS fingerprint change
    let mut flags = BTreeMap::new();
    let mut bits = flag_str.split_whitespace().peekable();

    while let Some(bit) = bits.next() {
        let mut bit = bit.to_owned();
        // Handle `-Z foo` the same way as `-Zfoo`.
        if bit.len() == 2 && bits.peek().is_some() && !bits.peek().unwrap().starts_with('-') {
            let bit_clone = bit.clone();
            let mut bit_chars = bit_clone.chars();
            if bit_chars.next().unwrap() == '-' && bit_chars.next().unwrap() != '-' {
                bit.push_str(bits.next().unwrap());
            }
        }

        if bit.starts_with('-') {
            if bit.contains('=') {
                // Split only on the first equals sign (there may be
                // more than one)
                let bits: Vec<_> = bit.splitn(2, '=').collect();
                assert!(bits.len() == 2);
                flags.insert(bits[0].to_owned() + "=", bits[1].to_owned());
            } else {
                if bits.peek().is_some() && !bits.peek().unwrap().starts_with('-') {
                    flags.insert(bit, bits.next().unwrap().to_owned());
                } else {
                    flags.insert(bit, String::new());
                }
            }
        } else {
            // A standalone arg with no flag, no deduplication to do. We merge these
            // together, which is probably not ideal, but is simple.
            flags
                .entry(String::new())
                .or_insert(String::new())
                .push_str(&format!(" {}", bit));
        }
    }

    // Put the map back together as a string.
    let mut result = String::new();
    for (k, v) in &flags {
        if k.is_empty() {
            result.push_str(v);
        } else {
            result.push(' ');
            result.push_str(k);
            if !v.is_empty() {
                if !k.ends_with('=') {
                    result.push(' ');
                }
                result.push_str(v);
            }
        }
    }
    result
}

#[cfg(test)]
mod test {
    use super::dedup_flags;

    #[test]
    fn test_dedup_flags() {
        // These should all be preserved.
        assert!(dedup_flags("") == "");
        assert!(dedup_flags("-Zfoo") == " -Zfoo");
        assert!(dedup_flags("-Z foo") == " -Zfoo");
        assert!(dedup_flags("-Zfoo bar") == " -Zfoo bar");
        let result = dedup_flags("-Z foo foo bar");
        assert!(result.matches("foo").count() == 2);
        assert!(result.matches("bar").count() == 1);

        // These should dedup.
        assert!(dedup_flags("-Zfoo -Zfoo") == " -Zfoo");
        assert!(dedup_flags("-Zfoo -Zfoo -Zfoo") == " -Zfoo");
        let result = dedup_flags("-Zfoo -Zfoo -Zbar");
        assert!(result.matches("foo").count() == 1);
        assert!(result.matches("bar").count() == 1);
        let result = dedup_flags("-Zfoo -Zbar -Zfoo -Zbar -Zbar");
        assert!(result.matches("foo").count() == 1);
        assert!(result.matches("bar").count() == 1);
        assert!(dedup_flags("-Zfoo -Z foo") == " -Zfoo");

        assert!(dedup_flags("--error-format=json --error-format=json") == " --error-format=json");
        assert!(dedup_flags("--error-format=foo --error-format=json") == " --error-format=json");

        assert!(
            dedup_flags(
                "-C link-args=-fuse-ld=gold -C target-cpu=native -C link-args=-fuse-ld=gold"
            ) == " -Clink-args=-fuse-ld=gold -Ctarget-cpu=native"
        );
    }
}
