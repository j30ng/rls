// FIXME: switch to something more ergonomic here, once available.
// (Currently, there is no way to opt into sysroot crates without `extern crate`.)
#[allow(unused_extern_crates)]
extern crate getopts;
#[allow(unused_extern_crates)]
extern crate rustc;
#[allow(unused_extern_crates)]
extern crate rustc_codegen_utils;
#[allow(unused_extern_crates)]
extern crate rustc_driver;
#[allow(unused_extern_crates)]
extern crate rustc_errors;
#[allow(unused_extern_crates)]
extern crate rustc_interface;
#[allow(unused_extern_crates)]
extern crate rustc_metadata;
#[allow(unused_extern_crates)]
extern crate rustc_resolve;
#[allow(unused_extern_crates)]
extern crate rustc_save_analysis;
#[allow(unused_extern_crates)]
extern crate syntax;

use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::io;
use std::mem;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use log::trace;
use rls_data::Analysis;
use rls_vfs::Vfs;

use self::rustc::session::config::Input;
use self::rustc::session::Session;
use self::rustc_driver::{run_compiler, Compilation};
use self::rustc_interface::interface;
use self::rustc_save_analysis as save;
use self::rustc_save_analysis::CallbackHandler;
use self::syntax::edition::Edition as RustcEdition;
use self::syntax::source_map::{FileLoader, RealFileLoader};
use crate::build::environment::{Environment, EnvironmentLockFacade};
use crate::build::plan::{Crate, Edition};
use crate::build::{BufWriter, BuildResult};
use crate::config::{ClippyPreference, Config};

// Runs a single instance of Rustc.
pub(crate) fn rustc(
    vfs: &Vfs,
    args: &[String],
    envs: &HashMap<String, Option<OsString>>,
    cwd: Option<&Path>,
    build_dir: &Path,
    rls_config: Arc<Mutex<Config>>,
    env_lock: &EnvironmentLockFacade,
) -> BuildResult {
    trace!(
        "rustc - args: `{:?}`, envs: {:?}, cwd: {:?}, build dir: {:?}",
        args,
        envs,
        cwd,
        build_dir
    );

    let changed = vfs.get_cached_files();

    let mut envs = envs.clone();

    let clippy_preference = {
        let config = rls_config.lock().unwrap();
        if config.clear_env_rust_log {
            envs.insert(String::from("RUST_LOG"), None);
        }

        config.clippy_preference
    };

    let lock_environment = |envs, cwd| {
        let (guard, _) = env_lock.lock();
        Environment::push_with_lock(envs, cwd, guard)
    };

    let CompilationResult { result, stderr, analysis, input_files } = match std::env::var(
        "RLS_OUT_OF_PROCESS",
    ) {
        #[cfg(feature = "ipc")]
        Ok(..) => run_out_of_process(changed.clone(), &args, &envs, clippy_preference)
            .unwrap_or_else(|_| {
                run_in_process(changed, &args, clippy_preference, lock_environment(&envs, cwd))
            }),
        #[cfg(not(feature = "ipc"))]
        Ok(..) => {
            log::warn!("Support for out-of-process compilation was not compiled. Rebuild with 'ipc' feature enabled");
            run_in_process(changed, &args, clippy_preference, lock_environment(&envs, cwd))
        }
        Err(..) => run_in_process(changed, &args, clippy_preference, lock_environment(&envs, cwd)),
    };

    let stderr = String::from_utf8(stderr).unwrap();
    log::debug!("rustc - stderr: {}", &stderr);
    let stderr_json_msgs: Vec<_> = stderr.lines().map(String::from).collect();

    let analysis = analysis.map(|analysis| vec![analysis]).unwrap_or_else(Vec::new);
    log::debug!("rustc: analysis read successfully?: {}", !analysis.is_empty());

    let cwd = cwd.unwrap_or_else(|| Path::new(".")).to_path_buf();

    BuildResult::Success(cwd, stderr_json_msgs, analysis, input_files, result.is_ok())
}

/// Resulting data from compiling a crate (in the rustc sense)
pub struct CompilationResult {
    /// Whether compilation was succesful
    result: Result<(), ()>,
    stderr: Vec<u8>,
    analysis: Option<Analysis>,
    // TODO: Move to Vec<PathBuf>
    input_files: HashMap<PathBuf, HashSet<Crate>>,
}

#[cfg(feature = "ipc")]
fn run_out_of_process(
    changed: HashMap<PathBuf, String>,
    args: &[String],
    envs: &HashMap<String, Option<OsString>>,
    clippy_preference: ClippyPreference,
) -> Result<CompilationResult, ()> {
    let analysis = Arc::default();
    let input_files = Arc::default();

    let ipc_server =
        super::ipc::start_with_all(changed, Arc::clone(&analysis), Arc::clone(&input_files))?;

    // Compiling out of process is only supported by our own shim
    let rustc_shim = env::current_exe()
        .ok()
        .and_then(|x| x.to_str().map(String::from))
        .expect("Couldn't set executable for RLS rustc shim");

    let output = Command::new(rustc_shim)
        .env(crate::RUSTC_SHIM_ENV_VAR_NAME, "1")
        .env("RLS_IPC_ENDPOINT", ipc_server.endpoint())
        .env("RLS_CLIPPY_PREFERENCE", clippy_preference.to_string())
        .args(args.iter().skip(1))
        .envs(envs.iter().filter_map(|(k, v)| v.as_ref().map(|v| (k, v))))
        .output()
        .map_err(|_| ());

    let result = match &output {
        Ok(output) if output.status.code() == Some(0) => Ok(()),
        _ => Err(()),
    };
    // NOTE: Make sure that we pass JSON error format
    let stderr = output.map(|out| out.stderr).unwrap_or_default();

    ipc_server.close();

    let input_files = unwrap_shared(input_files, "Other ref dropped by closed IPC server");
    let analysis = unwrap_shared(analysis, "Other ref dropped by closed IPC server");
    // FIXME(#25): given that we are running the compiler directly, there is no need
    // to serialize the error messages -- we should pass them in memory.
    Ok(CompilationResult { result, stderr, analysis, input_files })
}

fn run_in_process(
    changed: HashMap<PathBuf, String>,
    args: &[String],
    clippy_preference: ClippyPreference,
    environment_lock: Environment<'_>,
) -> CompilationResult {
    let mut callbacks = RlsRustcCalls { clippy_preference, ..Default::default() };
    let input_files = Arc::clone(&callbacks.input_files);
    let analysis = Arc::clone(&callbacks.analysis);

    let args: Vec<_> = if cfg!(feature = "clippy") && clippy_preference != ClippyPreference::Off {
        // Allow feature gating in the same way as `cargo clippy`
        let mut clippy_args = vec!["--cfg".to_owned(), r#"feature="cargo-clippy""#.to_owned()];

        if clippy_preference == ClippyPreference::OptIn {
            // `OptIn`: Require explicit `#![warn(clippy::all)]` annotation in each workspace crate
            clippy_args.push("-A".to_owned());
            clippy_args.push("clippy::all".to_owned());
        }

        args.iter().map(ToOwned::to_owned).chain(clippy_args).collect()
    } else {
        args.to_owned()
    };

    // rustc explicitly panics in `run_compiler()` on compile failure, regardless
    // of whether it encounters an ICE (internal compiler error) or not.
    // TODO: Change librustc_driver behaviour to distinguish between ICEs and
    // regular compilation failure with errors?
    let stderr = Arc::default();
    let result = std::panic::catch_unwind({
        let stderr = Arc::clone(&stderr);
        || {
            rustc_driver::catch_fatal_errors(move || {
                // Replace stderr so we catch most errors.
                run_compiler(
                    &args,
                    &mut callbacks,
                    Some(Box::new(ReplacedFileLoader::new(changed))),
                    Some(Box::new(BufWriter(stderr))),
                )
            })
        }
    })
    .map(|_| ())
    .map_err(|_| ());
    // Explicitly drop the global environment lock
    mem::drop(environment_lock);

    let stderr = unwrap_shared(stderr, "Other ref dropped by scoped compilation");
    let input_files = unwrap_shared(input_files, "Other ref dropped by scoped compilation");
    let analysis = unwrap_shared(analysis, "Other ref dropped by scoped compilation");

    CompilationResult { result, stderr, analysis, input_files }
}

// Our compiler controller. We mostly delegate to the default rustc
// controller, but use our own callback for save-analysis.
#[derive(Clone, Default)]
struct RlsRustcCalls {
    analysis: Arc<Mutex<Option<Analysis>>>,
    input_files: Arc<Mutex<HashMap<PathBuf, HashSet<Crate>>>>,
    clippy_preference: ClippyPreference,
}

impl rustc_driver::Callbacks for RlsRustcCalls {
    fn config(&mut self, config: &mut interface::Config) {
        // This also prevents the compiler from dropping expanded AST, which we
        // still need in the `after_analysis` callback in order to process and
        // pass the computed analysis in-memory.
        config.opts.debugging_opts.save_analysis = true;
    }

    fn after_parsing(&mut self, _compiler: &interface::Compiler) -> Compilation {
        #[cfg(feature = "clippy")]
        {
            if self.clippy_preference != ClippyPreference::Off {
                clippy_after_parse_callback(_compiler);
            }
        }

        Compilation::Continue
    }

    fn after_expansion(&mut self, compiler: &interface::Compiler) -> Compilation {
        let sess = compiler.session();
        let input = compiler.input();
        let crate_name = compiler.crate_name().unwrap().peek().clone();

        let cwd = &sess.working_dir.0;

        let src_path = match input {
            Input::File(ref name) => Some(name.to_path_buf()),
            Input::Str { .. } => None,
        }
        .and_then(|path| src_path(Some(cwd), path));

        let krate = Crate {
            name: crate_name.to_owned(),
            src_path,
            disambiguator: sess.local_crate_disambiguator().to_fingerprint().as_value(),
            edition: match sess.edition() {
                RustcEdition::Edition2015 => Edition::Edition2015,
                RustcEdition::Edition2018 => Edition::Edition2018,
            },
        };

        // We populate the file -> edition mapping only after expansion since it
        // can pull additional input files
        let mut input_files = self.input_files.lock().unwrap();
        for file in fetch_input_files(sess) {
            input_files.entry(file).or_default().insert(krate.clone());
        }

        Compilation::Continue
    }

    fn after_analysis(&mut self, compiler: &interface::Compiler) -> Compilation {
        let input = compiler.input();
        let crate_name = compiler.crate_name().unwrap().peek().clone();

        // Guaranteed to not be dropped yet in the pipeline thanks to the
        // `config.opts.debugging_opts.save_analysis` value being set to `true`.
        let expanded_crate = &compiler.expansion().unwrap().peek().0;
        compiler.global_ctxt().unwrap().peek_mut().enter(|tcx| {
            // There are two ways to move the data from rustc to the RLS, either
            // directly or by serialising and deserialising. We only want to do
            // the latter when there are compatibility issues between crates.

            // This version passes via JSON, it is more easily backwards compatible.
            // save::process_crate(state.tcx.unwrap(),
            //                     state.expanded_crate.unwrap(),
            //                     state.analysis.unwrap(),
            //                     state.crate_name.unwrap(),
            //                     state.input,
            //                     None,
            //                     save::DumpHandler::new(state.out_dir,
            //                                            state.crate_name.unwrap()));
            // This version passes directly, it is more efficient.
            save::process_crate(
                tcx,
                &expanded_crate,
                &crate_name,
                &input,
                None,
                CallbackHandler {
                    callback: &mut |a| {
                        let mut analysis = self.analysis.lock().unwrap();
                        let a = unsafe { mem::transmute(a.clone()) };
                        *analysis = Some(a);
                    },
                },
            );
        });

        Compilation::Continue
    }
}

#[cfg(feature = "clippy")]
fn clippy_after_parse_callback(compiler: &interface::Compiler) {
    use self::rustc_driver::plugin::registry::Registry;

    let sess = compiler.session();
    let mut registry = Registry::new(
        sess,
        compiler
            .parse()
            .expect(
                "at this compilation stage \
                 the crate must be parsed",
            )
            .peek()
            .span,
    );
    registry.args_hidden = Some(Vec::new());

    let conf = clippy_lints::read_conf(&registry);
    clippy_lints::register_plugins(&mut registry, &conf);

    let Registry {
        early_lint_passes, late_lint_passes, lint_groups, llvm_passes, attributes, ..
    } = registry;
    let mut ls = sess.lint_store.borrow_mut();
    for pass in early_lint_passes {
        ls.register_early_pass(Some(sess), true, false, pass);
    }
    for pass in late_lint_passes {
        ls.register_late_pass(Some(sess), true, false, false, pass);
    }

    for (name, (to, deprecated_name)) in lint_groups {
        ls.register_group(Some(sess), true, name, deprecated_name, to);
    }
    clippy_lints::register_pre_expansion_lints(sess, &mut ls, &conf);
    clippy_lints::register_renamed(&mut ls);

    sess.plugin_llvm_passes.borrow_mut().extend(llvm_passes);
    sess.plugin_attributes.borrow_mut().extend(attributes);
}

fn fetch_input_files(sess: &Session) -> Vec<PathBuf> {
    let cwd = &sess.working_dir.0;

    sess.source_map()
        .files()
        .iter()
        .filter(|fmap| fmap.is_real_file())
        .filter(|fmap| !fmap.is_imported())
        .map(|fmap| fmap.name.to_string())
        .map(|fmap| src_path(Some(cwd), fmap).unwrap())
        .collect()
}

/// Tries to read a file from a list of replacements, and if the file is not
/// there, then reads it from disk, by delegating to `RealFileLoader`.
struct ReplacedFileLoader {
    replacements: HashMap<PathBuf, String>,
    real_file_loader: RealFileLoader,
}

impl ReplacedFileLoader {
    fn new(replacements: HashMap<PathBuf, String>) -> ReplacedFileLoader {
        ReplacedFileLoader { replacements, real_file_loader: RealFileLoader }
    }
}

impl FileLoader for ReplacedFileLoader {
    fn file_exists(&self, path: &Path) -> bool {
        self.real_file_loader.file_exists(path)
    }

    fn abs_path(&self, path: &Path) -> Option<PathBuf> {
        self.real_file_loader.abs_path(path)
    }

    fn read_file(&self, path: &Path) -> io::Result<String> {
        if let Some(abs_path) = self.abs_path(path) {
            if self.replacements.contains_key(&abs_path) {
                return Ok(self.replacements[&abs_path].clone());
            }
        }
        self.real_file_loader.read_file(path)
    }
}

pub(super) fn current_sysroot() -> Option<String> {
    let home = env::var("RUSTUP_HOME").or_else(|_| env::var("MULTIRUST_HOME"));
    let toolchain = env::var("RUSTUP_TOOLCHAIN").or_else(|_| env::var("MULTIRUST_TOOLCHAIN"));
    if let (Ok(home), Ok(toolchain)) = (home, toolchain) {
        Some(format!("{}/toolchains/{}", home, toolchain))
    } else {
        let rustc_exe = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
        env::var("SYSROOT").ok().or_else(|| {
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

pub fn src_path(cwd: Option<&Path>, path: impl AsRef<Path>) -> Option<PathBuf> {
    let path = path.as_ref();

    Some(match (cwd, path.is_absolute()) {
        (_, true) => path.to_owned(),
        (Some(cwd), _) => cwd.join(path),
        (None, _) => std::env::current_dir().ok()?.join(path),
    })
}

fn unwrap_shared<T: std::fmt::Debug>(shared: Arc<Mutex<T>>, msg: &'static str) -> T {
    Arc::try_unwrap(shared).expect(msg).into_inner().unwrap()
}
