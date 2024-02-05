#![feature(rustc_private, stmt_expr_attributes)]
#![allow(
    clippy::manual_range_contains,
    clippy::useless_format,
    clippy::field_reassign_with_default,
    rustc::diagnostic_outside_of_impl,
    rustc::untranslatable_diagnostic,
)]

extern crate rustc_data_structures;
extern crate rustc_driver;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_log;
extern crate rustc_metadata;
extern crate rustc_middle;
extern crate rustc_session;

use std::env::{self, VarError};
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::str::FromStr;

use log::debug;

use rustc_data_structures::sync::Lrc;
use rustc_driver::Compilation;
use rustc_hir::{self as hir, Node};
use rustc_interface::interface::Config;
use rustc_middle::{
    middle::exported_symbols::{
        ExportedSymbol, SymbolExportInfo, SymbolExportKind, SymbolExportLevel,
    },
    query::LocalCrate,
    ty::TyCtxt,
    util::Providers,
};
use rustc_session::config::{CrateType, ErrorOutputType, OptLevel};
use rustc_session::search_paths::PathKind;
use rustc_session::{CtfeBacktrace, EarlyDiagCtxt};

use miri::{BacktraceStyle, BorrowTrackerMethod, ProvenanceMode, RetagFields};

struct MiriCompilerCalls {
    miri_config: miri::MiriConfig,
}

impl rustc_driver::Callbacks for MiriCompilerCalls {
    fn config(&mut self, config: &mut Config) {
        config.override_queries = Some(|_, providers| {
            providers.extern_queries.used_crate_source = |tcx, cnum| {
                let mut providers = Providers::default();
                rustc_metadata::provide(&mut providers);
                let mut crate_source = (providers.extern_queries.used_crate_source)(tcx, cnum);
                // HACK: rustc will emit "crate ... required to be available in rlib format, but
                // was not found in this form" errors once we use `tcx.dependency_formats()` if
                // there's no rlib provided, so setting a dummy path here to workaround those errors.
                Lrc::make_mut(&mut crate_source).rlib = Some((PathBuf::new(), PathKind::All));
                crate_source
            };
        });
    }

    fn after_analysis<'tcx>(
        &mut self,
        _: &rustc_interface::interface::Compiler,
        queries: &'tcx rustc_interface::Queries<'tcx>,
    ) -> Compilation {
        queries.global_ctxt().unwrap().enter(|tcx| {
            if tcx.sess.compile_status().is_err() {
                tcx.dcx().fatal("miri cannot be run on programs that fail compilation");
            }

            let early_dcx = EarlyDiagCtxt::new(tcx.sess.opts.error_format);
            init_late_loggers(&early_dcx, tcx);
            if !tcx.crate_types().contains(&CrateType::Executable) {
                tcx.dcx().fatal("miri only makes sense on bin crates");
            }

            let (entry_def_id, entry_type) = if let Some(entry_def) = tcx.entry_fn(()) {
                entry_def
            } else {
                tcx.dcx().fatal("miri can only run programs that have a main function");
            };
            let mut config = self.miri_config.clone();

            // Add filename to `miri` arguments.
            config.args.insert(0, tcx.sess.io.input.filestem().to_string());

            // Adjust working directory for interpretation.
            if let Some(cwd) = env::var_os("MIRI_CWD") {
                env::set_current_dir(cwd).unwrap();
            }

            if tcx.sess.opts.optimize != OptLevel::No {
                tcx.dcx().warn("Miri does not support optimizations. If you have enabled optimizations \
                    by selecting a Cargo profile (such as --release) which changes other profile settings \
                    such as whether debug assertions and overflow checks are enabled, those settings are \
                    still applied.");
            }
            if tcx.sess.mir_opt_level() > 0 {
                tcx.dcx().warn("You have explicitly enabled MIR optimizations, overriding Miri's default \
                    which is to completely disable them. Any optimizations may hide UB that Miri would \
                    otherwise detect, and it is not necessarily possible to predict what kind of UB will \
                    be missed. If you are enabling optimizations to make Miri run faster, we advise using \
                    cfg(miri) to shrink your workload instead. The performance benefit of enabling MIR \
                    optimizations is usually marginal at best.");
            }

            if let Some(return_code) = miri::eval_entry(tcx, entry_def_id, entry_type, config) {
                std::process::exit(
                    i32::try_from(return_code).expect("Return value was too large!"),
                );
            }
            tcx.dcx().abort_if_errors();
        });

        Compilation::Stop
    }
}

struct MiriBeRustCompilerCalls {
    target_crate: bool,
}

impl rustc_driver::Callbacks for MiriBeRustCompilerCalls {
    #[allow(rustc::potential_query_instability)] // rustc_codegen_ssa (where this code is copied from) also allows this lint
    fn config(&mut self, config: &mut Config) {
        if config.opts.prints.is_empty() && self.target_crate {
            // Queries overridden here affect the data stored in `rmeta` files of dependencies,
            // which will be used later in non-`MIRI_BE_RUSTC` mode.
            config.override_queries = Some(|_, local_providers| {
                // `exported_symbols` and `reachable_non_generics` provided by rustc always returns
                // an empty result if `tcx.sess.opts.output_types.should_codegen()` is false.
                local_providers.exported_symbols = |tcx, LocalCrate| {
                    let reachable_set = tcx.with_stable_hashing_context(|hcx| {
                        tcx.reachable_set(()).to_sorted(&hcx, true)
                    });
                    tcx.arena.alloc_from_iter(
                        // This is based on:
                        // https://github.com/rust-lang/rust/blob/2962e7c0089d5c136f4e9600b7abccfbbde4973d/compiler/rustc_codegen_ssa/src/back/symbol_export.rs#L62-L63
                        // https://github.com/rust-lang/rust/blob/2962e7c0089d5c136f4e9600b7abccfbbde4973d/compiler/rustc_codegen_ssa/src/back/symbol_export.rs#L174
                        reachable_set.into_iter().filter_map(|&local_def_id| {
                            // Do the same filtering that rustc does:
                            // https://github.com/rust-lang/rust/blob/2962e7c0089d5c136f4e9600b7abccfbbde4973d/compiler/rustc_codegen_ssa/src/back/symbol_export.rs#L84-L102
                            // Otherwise it may cause unexpected behaviours and ICEs
                            // (https://github.com/rust-lang/rust/issues/86261).
                            let is_reachable_non_generic = matches!(
                                tcx.hir_node_by_def_id(local_def_id),
                                Node::Item(&hir::Item {
                                    kind: hir::ItemKind::Static(..) | hir::ItemKind::Fn(..),
                                    ..
                                }) | Node::ImplItem(&hir::ImplItem {
                                    kind: hir::ImplItemKind::Fn(..),
                                    ..
                                })
                                if !tcx.generics_of(local_def_id).requires_monomorphization(tcx)
                            );
                            (is_reachable_non_generic
                                && tcx.codegen_fn_attrs(local_def_id).contains_extern_indicator())
                            .then_some((
                                ExportedSymbol::NonGeneric(local_def_id.to_def_id()),
                                // Some dummy `SymbolExportInfo` here. We only use
                                // `exported_symbols` in shims/foreign_items.rs and the export info
                                // is ignored.
                                SymbolExportInfo {
                                    level: SymbolExportLevel::C,
                                    kind: SymbolExportKind::Text,
                                    used: false,
                                },
                            ))
                        }),
                    )
                }
            });
        }
    }
}

fn show_error(msg: &impl std::fmt::Display) -> ! {
    eprintln!("fatal error: {msg}");
    std::process::exit(1)
}

macro_rules! show_error {
    ($($tt:tt)*) => { show_error(&format_args!($($tt)*)) };
}

fn rustc_logger_config() -> rustc_log::LoggerConfig {
    // Start with the usual env vars.
    let mut cfg = rustc_log::LoggerConfig::from_env("RUSTC_LOG");

    // Overwrite if MIRI_LOG is set.
    if let Ok(var) = env::var("MIRI_LOG") {
        // MIRI_LOG serves as default for RUSTC_LOG, if that is not set.
        if matches!(cfg.filter, Err(VarError::NotPresent)) {
            // We try to be a bit clever here: if `MIRI_LOG` is just a single level
            // used for everything, we only apply it to the parts of rustc that are
            // CTFE-related. Otherwise, we use it verbatim for `RUSTC_LOG`.
            // This way, if you set `MIRI_LOG=trace`, you get only the right parts of
            // rustc traced, but you can also do `MIRI_LOG=miri=trace,rustc_const_eval::interpret=debug`.
            if log::Level::from_str(&var).is_ok() {
                cfg.filter = Ok(format!(
                    "rustc_middle::mir::interpret={var},rustc_const_eval::interpret={var}"
                ));
            } else {
                cfg.filter = Ok(var);
            }
        }
        // Enable verbose entry/exit logging by default if MIRI_LOG is set.
        if matches!(cfg.verbose_entry_exit, Err(VarError::NotPresent)) {
            cfg.verbose_entry_exit = Ok(format!("1"));
        }
    }

    cfg
}

fn init_early_loggers(early_dcx: &EarlyDiagCtxt) {
    // Note that our `extern crate log` is *not* the same as rustc's; as a result, we have to
    // initialize them both, and we always initialize `miri`'s first.
    let env = env_logger::Env::new().filter("MIRI_LOG").write_style("MIRI_LOG_STYLE");
    env_logger::init_from_env(env);
    // Now for rustc. We only initialize `rustc` if the env var is set (so the user asked for it).
    // If it is not set, we avoid initializing now so that we can initialize later with our custom
    // settings, and *not* log anything for what happens before `miri` gets started.
    if env::var_os("RUSTC_LOG").is_some() {
        rustc_driver::init_logger(early_dcx, rustc_logger_config());
    }
}

fn init_late_loggers(early_dcx: &EarlyDiagCtxt, tcx: TyCtxt<'_>) {
    // If `RUSTC_LOG` is not set, then `init_early_loggers` did not call
    // `rustc_driver::init_logger`, so we have to do this now.
    if env::var_os("RUSTC_LOG").is_none() {
        rustc_driver::init_logger(early_dcx, rustc_logger_config());
    }

    // If `MIRI_BACKTRACE` is set and `RUSTC_CTFE_BACKTRACE` is not, set `RUSTC_CTFE_BACKTRACE`.
    // Do this late, so we ideally only apply this to Miri's errors.
    if let Some(val) = env::var_os("MIRI_BACKTRACE") {
        let ctfe_backtrace = match &*val.to_string_lossy() {
            "immediate" => CtfeBacktrace::Immediate,
            "0" => CtfeBacktrace::Disabled,
            _ => CtfeBacktrace::Capture,
        };
        *tcx.sess.ctfe_backtrace.borrow_mut() = ctfe_backtrace;
    }
}

/// Execute a compiler with the given CLI arguments and callbacks.
fn run_compiler(
    mut args: Vec<String>,
    target_crate: bool,
    callbacks: &mut (dyn rustc_driver::Callbacks + Send),
    using_internal_features: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> ! {
    if target_crate {
        // Miri needs a custom sysroot for target crates.
        // If no `--sysroot` is given, the `MIRI_SYSROOT` env var is consulted to find where
        // that sysroot lives, and that is passed to rustc.
        let sysroot_flag = "--sysroot";
        if !args.iter().any(|e| e == sysroot_flag) {
            // Using the built-in default here would be plain wrong, so we *require*
            // the env var to make sure things make sense.
            let miri_sysroot = env::var("MIRI_SYSROOT").unwrap_or_else(|_| {
                show_error!(
                    "Miri was invoked in 'target' mode without `MIRI_SYSROOT` or `--sysroot` being set"
                    )
            });

            args.push(sysroot_flag.to_owned());
            args.push(miri_sysroot);
        }
    }

    // Don't insert `MIRI_DEFAULT_ARGS`, in particular, `--cfg=miri`, if we are building
    // a "host" crate. That may cause procedural macros (and probably build scripts) to
    // depend on Miri-only symbols, such as `miri_resolve_frame`:
    // https://github.com/rust-lang/miri/issues/1760
    if target_crate {
        // Some options have different defaults in Miri than in plain rustc; apply those by making
        // them the first arguments after the binary name (but later arguments can overwrite them).
        args.splice(1..1, miri::MIRI_DEFAULT_ARGS.iter().map(ToString::to_string));
    }

    // Invoke compiler, and handle return code.
    let exit_code = rustc_driver::catch_with_exit_code(move || {
        rustc_driver::RunCompiler::new(&args, callbacks)
            .set_using_internal_features(using_internal_features)
            .run()
    });
    std::process::exit(exit_code)
}

/// Parses a comma separated list of `T` from the given string:
/// `<value1>,<value2>,<value3>,...`
fn parse_comma_list<T: FromStr>(input: &str) -> Result<Vec<T>, T::Err> {
    input.split(',').map(str::parse::<T>).collect()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn jemalloc_magic() {
    // These magic runes are copied from
    // <https://github.com/rust-lang/rust/blob/e89bd9428f621545c979c0ec686addc6563a394e/compiler/rustc/src/main.rs#L39>.
    // See there for further comments.
    use std::os::raw::{c_int, c_void};

    #[used]
    static _F1: unsafe extern "C" fn(usize, usize) -> *mut c_void = jemalloc_sys::calloc;
    #[used]
    static _F2: unsafe extern "C" fn(*mut *mut c_void, usize, usize) -> c_int =
        jemalloc_sys::posix_memalign;
    #[used]
    static _F3: unsafe extern "C" fn(usize, usize) -> *mut c_void = jemalloc_sys::aligned_alloc;
    #[used]
    static _F4: unsafe extern "C" fn(usize) -> *mut c_void = jemalloc_sys::malloc;
    #[used]
    static _F5: unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void = jemalloc_sys::realloc;
    #[used]
    static _F6: unsafe extern "C" fn(*mut c_void) = jemalloc_sys::free;

    // On OSX, jemalloc doesn't directly override malloc/free, but instead
    // registers itself with the allocator's zone APIs in a ctor. However,
    // the linker doesn't seem to consider ctors as "used" when statically
    // linking, so we need to explicitly depend on the function.
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn _rjem_je_zone_register();
        }

        #[used]
        static _F7: unsafe extern "C" fn() = _rjem_je_zone_register;
    }
}

fn main() {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    jemalloc_magic();

    let early_dcx = EarlyDiagCtxt::new(ErrorOutputType::default());

    // Snapshot a copy of the environment before `rustc` starts messing with it.
    // (`install_ice_hook` might change `RUST_BACKTRACE`.)
    let env_snapshot = env::vars_os().collect::<Vec<_>>();

    // If the environment asks us to actually be rustc, then do that.
    if let Some(crate_kind) = env::var_os("MIRI_BE_RUSTC") {
        // Earliest rustc setup.
        let using_internal_features =
            rustc_driver::install_ice_hook(rustc_driver::DEFAULT_BUG_REPORT_URL, |_| ());
        rustc_driver::init_rustc_env_logger(&early_dcx);

        let target_crate = if crate_kind == "target" {
            true
        } else if crate_kind == "host" {
            false
        } else {
            panic!("invalid `MIRI_BE_RUSTC` value: {crate_kind:?}")
        };

        // We cannot use `rustc_driver::main` as we need to adjust the CLI arguments.
        run_compiler(
            env::args().collect(),
            target_crate,
            &mut MiriBeRustCompilerCalls { target_crate },
            using_internal_features,
        )
    }

    // Add an ICE bug report hook.
    let using_internal_features =
        rustc_driver::install_ice_hook("https://github.com/rust-lang/miri/issues/new", |_| ());

    // Init loggers the Miri way.
    init_early_loggers(&early_dcx);

    // Parse our arguments and split them across `rustc` and `miri`.
    let mut miri_config = miri::MiriConfig::default();
    miri_config.env = env_snapshot;

    let mut rustc_args = vec![];
    let mut after_dashdash = false;

    // If user has explicitly enabled/disabled isolation
    let mut isolation_enabled: Option<bool> = None;
    for arg in env::args() {
        if rustc_args.is_empty() {
            // Very first arg: binary name.
            rustc_args.push(arg);
        } else if after_dashdash {
            // Everything that comes after `--` is forwarded to the interpreted crate.
            miri_config.args.push(arg);
        } else if arg == "--" {
            after_dashdash = true;
        } else if arg == "-Zmiri-disable-validation" {
            miri_config.validate = false;
        } else if arg == "-Zmiri-disable-stacked-borrows" {
            miri_config.borrow_tracker = None;
        } else if arg == "-Zmiri-tree-borrows" {
            miri_config.borrow_tracker = Some(BorrowTrackerMethod::TreeBorrows);
        } else if arg == "-Zmiri-unique-is-unique" {
            miri_config.unique_is_unique = true;
        } else if arg == "-Zmiri-disable-data-race-detector" {
            miri_config.data_race_detector = false;
            miri_config.weak_memory_emulation = false;
        } else if arg == "-Zmiri-disable-alignment-check" {
            miri_config.check_alignment = miri::AlignmentCheck::None;
        } else if arg == "-Zmiri-symbolic-alignment-check" {
            miri_config.check_alignment = miri::AlignmentCheck::Symbolic;
        } else if arg == "-Zmiri-check-number-validity" {
            eprintln!(
                "WARNING: the flag `-Zmiri-check-number-validity` no longer has any effect \
                        since it is now enabled by default"
            );
        } else if arg == "-Zmiri-disable-abi-check" {
            eprintln!(
                "WARNING: the flag `-Zmiri-disable-abi-check` is deprecated and planned to be removed.\n\
                If you have a use-case for it, please file an issue."
            );
            miri_config.check_abi = false;
        } else if arg == "-Zmiri-disable-isolation" {
            if matches!(isolation_enabled, Some(true)) {
                show_error!(
                    "-Zmiri-disable-isolation cannot be used along with -Zmiri-isolation-error"
                );
            } else {
                isolation_enabled = Some(false);
            }
            miri_config.isolated_op = miri::IsolatedOp::Allow;
        } else if arg == "-Zmiri-disable-leak-backtraces" {
            miri_config.collect_leak_backtraces = false;
        } else if arg == "-Zmiri-disable-weak-memory-emulation" {
            miri_config.weak_memory_emulation = false;
        } else if arg == "-Zmiri-track-weak-memory-loads" {
            miri_config.track_outdated_loads = true;
        } else if let Some(param) = arg.strip_prefix("-Zmiri-isolation-error=") {
            if matches!(isolation_enabled, Some(false)) {
                show_error!(
                    "-Zmiri-isolation-error cannot be used along with -Zmiri-disable-isolation"
                );
            } else {
                isolation_enabled = Some(true);
            }

            miri_config.isolated_op = match param {
                "abort" => miri::IsolatedOp::Reject(miri::RejectOpWith::Abort),
                "hide" => miri::IsolatedOp::Reject(miri::RejectOpWith::NoWarning),
                "warn" => miri::IsolatedOp::Reject(miri::RejectOpWith::Warning),
                "warn-nobacktrace" =>
                    miri::IsolatedOp::Reject(miri::RejectOpWith::WarningWithoutBacktrace),
                _ =>
                    show_error!(
                        "-Zmiri-isolation-error must be `abort`, `hide`, `warn`, or `warn-nobacktrace`"
                    ),
            };
        } else if arg == "-Zmiri-ignore-leaks" {
            miri_config.ignore_leaks = true;
            miri_config.collect_leak_backtraces = false;
        } else if arg == "-Zmiri-panic-on-unsupported" {
            miri_config.panic_on_unsupported = true;
        } else if arg == "-Zmiri-tag-raw-pointers" {
            eprintln!("WARNING: `-Zmiri-tag-raw-pointers` has no effect; it is enabled by default");
        } else if arg == "-Zmiri-strict-provenance" {
            miri_config.provenance_mode = ProvenanceMode::Strict;
        } else if arg == "-Zmiri-permissive-provenance" {
            miri_config.provenance_mode = ProvenanceMode::Permissive;
        } else if arg == "-Zmiri-mute-stdout-stderr" {
            miri_config.mute_stdout_stderr = true;
        } else if arg == "-Zmiri-retag-fields" {
            miri_config.retag_fields = RetagFields::Yes;
        } else if let Some(retag_fields) = arg.strip_prefix("-Zmiri-retag-fields=") {
            miri_config.retag_fields = match retag_fields {
                "all" => RetagFields::Yes,
                "none" => RetagFields::No,
                "scalar" => RetagFields::OnlyScalar,
                _ => show_error!("`-Zmiri-retag-fields` can only be `all`, `none`, or `scalar`"),
            };
        } else if arg == "-Zmiri-track-raw-pointers" {
            eprintln!(
                "WARNING: `-Zmiri-track-raw-pointers` has no effect; it is enabled by default"
            );
        } else if let Some(param) = arg.strip_prefix("-Zmiri-seed=") {
            if miri_config.seed.is_some() {
                show_error!("Cannot specify -Zmiri-seed multiple times!");
            }
            let seed = param.parse::<u64>().unwrap_or_else(|_| {
                show_error!("-Zmiri-seed must be an integer that fits into u64")
            });
            miri_config.seed = Some(seed);
        } else if let Some(_param) = arg.strip_prefix("-Zmiri-env-exclude=") {
            show_error!(
                "`-Zmiri-env-exclude` has been removed; unset env vars before starting Miri instead"
            );
        } else if let Some(param) = arg.strip_prefix("-Zmiri-env-forward=") {
            miri_config.forwarded_env_vars.push(param.to_owned());
        } else if let Some(param) = arg.strip_prefix("-Zmiri-track-pointer-tag=") {
            let ids: Vec<u64> = match parse_comma_list(param) {
                Ok(ids) => ids,
                Err(err) =>
                    show_error!(
                        "-Zmiri-track-pointer-tag requires a comma separated list of valid `u64` arguments: {}",
                        err
                    ),
            };
            for id in ids.into_iter().map(miri::BorTag::new) {
                if let Some(id) = id {
                    miri_config.tracked_pointer_tags.insert(id);
                } else {
                    show_error!("-Zmiri-track-pointer-tag requires nonzero arguments");
                }
            }
        } else if let Some(param) = arg.strip_prefix("-Zmiri-track-call-id=") {
            let ids: Vec<u64> = match parse_comma_list(param) {
                Ok(ids) => ids,
                Err(err) =>
                    show_error!(
                        "-Zmiri-track-call-id requires a comma separated list of valid `u64` arguments: {}",
                        err
                    ),
            };
            for id in ids.into_iter().map(miri::CallId::new) {
                if let Some(id) = id {
                    miri_config.tracked_call_ids.insert(id);
                } else {
                    show_error!("-Zmiri-track-call-id requires a nonzero argument");
                }
            }
        } else if let Some(param) = arg.strip_prefix("-Zmiri-track-alloc-id=") {
            let ids: Vec<miri::AllocId> = match parse_comma_list::<NonZeroU64>(param) {
                Ok(ids) => ids.into_iter().map(miri::AllocId).collect(),
                Err(err) =>
                    show_error!(
                        "-Zmiri-track-alloc-id requires a comma separated list of valid non-zero `u64` arguments: {}",
                        err
                    ),
            };
            miri_config.tracked_alloc_ids.extend(ids);
        } else if let Some(param) = arg.strip_prefix("-Zmiri-compare-exchange-weak-failure-rate=") {
            let rate = match param.parse::<f64>() {
                Ok(rate) if rate >= 0.0 && rate <= 1.0 => rate,
                Ok(_) =>
                    show_error!(
                        "-Zmiri-compare-exchange-weak-failure-rate must be between `0.0` and `1.0`"
                    ),
                Err(err) =>
                    show_error!(
                        "-Zmiri-compare-exchange-weak-failure-rate requires a `f64` between `0.0` and `1.0`: {}",
                        err
                    ),
            };
            miri_config.cmpxchg_weak_failure_rate = rate;
        } else if let Some(param) = arg.strip_prefix("-Zmiri-preemption-rate=") {
            let rate = match param.parse::<f64>() {
                Ok(rate) if rate >= 0.0 && rate <= 1.0 => rate,
                Ok(_) => show_error!("-Zmiri-preemption-rate must be between `0.0` and `1.0`"),
                Err(err) =>
                    show_error!(
                        "-Zmiri-preemption-rate requires a `f64` between `0.0` and `1.0`: {}",
                        err
                    ),
            };
            miri_config.preemption_rate = rate;
        } else if arg == "-Zmiri-report-progress" {
            // This makes it take a few seconds between progress reports on my laptop.
            miri_config.report_progress = Some(1_000_000);
        } else if let Some(param) = arg.strip_prefix("-Zmiri-report-progress=") {
            let interval = match param.parse::<u32>() {
                Ok(i) => i,
                Err(err) => show_error!("-Zmiri-report-progress requires a `u32`: {}", err),
            };
            miri_config.report_progress = Some(interval);
        } else if let Some(param) = arg.strip_prefix("-Zmiri-provenance-gc=") {
            let interval = match param.parse::<u32>() {
                Ok(i) => i,
                Err(err) => show_error!("-Zmiri-provenance-gc requires a `u32`: {}", err),
            };
            miri_config.gc_interval = interval;
        } else if let Some(param) = arg.strip_prefix("-Zmiri-measureme=") {
            miri_config.measureme_out = Some(param.to_string());
        } else if let Some(param) = arg.strip_prefix("-Zmiri-backtrace=") {
            miri_config.backtrace_style = match param {
                "0" => BacktraceStyle::Off,
                "1" => BacktraceStyle::Short,
                "full" => BacktraceStyle::Full,
                _ => show_error!("-Zmiri-backtrace may only be 0, 1, or full"),
            };
        } else if let Some(param) = arg.strip_prefix("-Zmiri-extern-so-file=") {
            let filename = param.to_string();
            if std::path::Path::new(&filename).exists() {
                if let Some(other_filename) = miri_config.external_so_file {
                    show_error!(
                        "-Zmiri-extern-so-file is already set to {}",
                        other_filename.display()
                    );
                }
                miri_config.external_so_file = Some(filename.into());
            } else {
                show_error!("-Zmiri-extern-so-file `{}` does not exist", filename);
            }
        } else if let Some(param) = arg.strip_prefix("-Zmiri-num-cpus=") {
            let num_cpus = match param.parse::<u32>() {
                Ok(i) => i,
                Err(err) => show_error!("-Zmiri-num-cpus requires a `u32`: {}", err),
            };

            miri_config.num_cpus = num_cpus;
        } else if let Some(param) = arg.strip_prefix("-Zmiri-force-page-size=") {
            let page_size = match param.parse::<u64>() {
                Ok(i) =>
                    if i.is_power_of_two() {
                        i * 1024
                    } else {
                        show_error!("-Zmiri-force-page-size requires a power of 2: {}", i)
                    },
                Err(err) => show_error!("-Zmiri-force-page-size requires a `u64`: {}", err),
            };

            miri_config.page_size = Some(page_size);
        } else {
            // Forward to rustc.
            rustc_args.push(arg);
        }
    }
    // `-Zmiri-unique-is-unique` should only be used with `-Zmiri-tree-borrows`
    if miri_config.unique_is_unique
        && !matches!(miri_config.borrow_tracker, Some(BorrowTrackerMethod::TreeBorrows))
    {
        show_error!(
            "-Zmiri-unique-is-unique only has an effect when -Zmiri-tree-borrows is also used"
        );
    }

    debug!("rustc arguments: {:?}", rustc_args);
    debug!("crate arguments: {:?}", miri_config.args);
    run_compiler(
        rustc_args,
        /* target_crate: */ true,
        &mut MiriCompilerCalls { miri_config },
        using_internal_features,
    )
}
