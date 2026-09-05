use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FeatureLane {
    Asterisk22,
    Latest,
}

impl FeatureLane {
    fn selected() -> Option<Self> {
        let lane_22 = env::var_os("CARGO_FEATURE_ASTERISK_22").is_some();
        let lane_latest = env::var_os("CARGO_FEATURE_ASTERISK_LATEST").is_some();
        if !lane_22 && !lane_latest {
            return None;
        }
        assert_ne!(
            lane_22, lane_latest,
            "select exactly one of the asterisk-22 or asterisk-latest features"
        );
        Some(if lane_22 {
            Self::Asterisk22
        } else {
            Self::Latest
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Asterisk22 => "22",
            Self::Latest => "latest",
        }
    }
}

struct BuildPaths {
    source: PathBuf,
    build: PathBuf,
}

impl BuildPaths {
    fn discover() -> Self {
        let crate_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
        let source = absolute(
            env::var_os("ASTERISK_SOURCE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| crate_dir.join("asterisk")),
        );
        let build = absolute(
            env::var_os("ASTERISK_BUILD_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| source.clone()),
        );
        Self { source, build }
    }

    fn validate(&self) {
        require_file(
            &self.build.join("include/asterisk.h"),
            "ASTERISK_BUILD_DIR must point at a configured Asterisk build tree",
        );
        require_file(
            &self.build.join("include/asterisk/autoconfig.h"),
            "run Asterisk ./configure before building the SCCP module",
        );
        require_file(
            &self.build.join("include/asterisk/buildopts.h"),
            "generate Asterisk build headers before building the SCCP module",
        );
        require_file(
            &self.source.join("include/asterisk/channel.h"),
            "ASTERISK_SOURCE_DIR must point at an Asterisk source tree",
        );
    }

    fn include_dirs(&self) -> [PathBuf; 2] {
        [self.build.join("include"), self.source.join("include")]
    }
}

fn main() {
    emit_rerun_contract();
    generate_telemetry_bindings();

    let Some(lane) = FeatureLane::selected() else {
        return;
    };
    let target_os = validate_target();
    let paths = BuildPaths::discover();
    paths.validate();
    let version = validate_version(&paths.source, lane);
    emit_build_identity(&paths.build, &version, lane);
    generate_bindings(&paths);

    // Cargo always prefixes Unix cdylibs with `lib`; packaging installs the
    // resulting ELF object as chan_sccp2.so. The soname records that load name.
    if target_os == "linux" {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,chan_sccp2.so");
    }
}

#[cfg(feature = "telemetry")]
fn generate_telemetry_bindings() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let proto = manifest.join("proto/sccp/debug/v1/sccp_debug.proto");
    let include = manifest.join("proto");
    let protoc = protoc_bin_vendored::protoc_bin_path()
        .expect("vendored protobuf compiler is unavailable for this build target");
    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc);
    config
        .compile_protos(&[proto.as_path()], &[include.as_path()])
        .expect("unable to generate debug telemetry protobuf bindings");
    println!("cargo:rerun-if-changed={}", proto.display());
}

#[cfg(not(feature = "telemetry"))]
fn generate_telemetry_bindings() {}

fn emit_rerun_contract() {
    println!("cargo:rerun-if-env-changed=ASTERISK_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=ASTERISK_BUILD_DIR");
    println!("cargo:rerun-if-env-changed=ASTERISK_VERSION");
    println!("cargo:rerun-if-env-changed=SCCP_ALLOW_UNSUPPORTED_TARGET");
}

fn validate_target() -> String {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let supported = target_os == "linux" && matches!(target_arch.as_str(), "x86_64" | "aarch64");
    assert!(
        supported || env::var_os("SCCP_ALLOW_UNSUPPORTED_TARGET").is_some(),
        "supported module targets are Linux x86_64 and aarch64, got {target_os} {target_arch}"
    );
    target_os
}

fn validate_version(source_dir: &Path, lane: FeatureLane) -> String {
    match lane {
        FeatureLane::Asterisk22 => {
            let version = env::var("ASTERISK_VERSION")
                .ok()
                .or_else(|| detect_version(source_dir))
                .unwrap_or_else(|| {
                    panic!(
                        "unable to determine the Asterisk version; set ASTERISK_VERSION to the exact release"
                    )
                });
            let major = numeric_major(&version).unwrap_or_else(|| {
                panic!("unable to extract an Asterisk major version from {version:?}")
            });
            assert_eq!(
                major, 22,
                "selected asterisk-22, but ASTERISK_SOURCE_DIR reports Asterisk {version}"
            );
            version
        }
        FeatureLane::Latest => {
            let mainline = source_mainline(source_dir).unwrap_or_else(|| {
                panic!(
                    "unable to determine the Asterisk source line from {}",
                    source_dir.join("configure.ac").display()
                )
            });
            assert_eq!(
                mainline, "master",
                "selected asterisk-latest, but ASTERISK_SOURCE_DIR reports Asterisk source line {mainline:?}"
            );
            let version = detect_version(source_dir).unwrap_or_else(|| {
                panic!(
                    "unable to determine the exact upstream master identity from {}",
                    source_dir.display()
                )
            });
            assert!(
                version.starts_with("GIT-master-"),
                "selected asterisk-latest, but ASTERISK_SOURCE_DIR reports Asterisk {version}"
            );
            version
        }
    }
}

fn emit_build_identity(build_dir: &Path, version: &str, lane: FeatureLane) {
    let buildopts = fs::read_to_string(build_dir.join("include/asterisk/buildopts.h"))
        .ok()
        .and_then(|contents| quoted_define(&contents, "AST_BUILDOPT_SUM"))
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=SCCP_ASTERISK_VERSION={version}");
    println!("cargo:rustc-env=SCCP_ASTERISK_BUILDOPT_SUM={buildopts}");
    println!("cargo:rustc-env=SCCP_ASTERISK_LANE={}", lane.name());
}

fn generate_bindings(paths: &BuildPaths) {
    // Keep Asterisk's namespace rooted at `include`. Adding
    // `include/asterisk` itself would make its `features.h` shadow glibc's
    // system header of the same name on Linux.
    let include_dirs = paths.include_dirs();

    // Generate the sole native ABI surface from the exact configured Asterisk
    // headers. Repository-owned records and callbacks are implemented in
    // Rust; there is no wrapper header or C compilation step.
    let mut sys_bindings = bindgen::Builder::default()
        .header_contents(
            "sccp_asterisk_sys.h",
            r#"
#include "asterisk.h"
#include "asterisk/abstract_jb.h"
#include "asterisk/astdb.h"
#include "asterisk/ast_version.h"
#include "asterisk/astobj2.h"
#include "asterisk/audiohook.h"
#include "asterisk/bridge.h"
#include "asterisk/causes.h"
#include "asterisk/callerid.h"
#include "asterisk/ccss.h"
#include "asterisk/channel.h"
#include "asterisk/channelstate.h"
#include "asterisk/cli.h"
#include "asterisk/config.h"
#include "asterisk/devicestate.h"
#include "asterisk/format_cache.h"
#include "asterisk/format_cap.h"
#include "asterisk/frame.h"
#include "asterisk/http.h"
#include "asterisk/iostream.h"
#include "asterisk/json.h"
#include "asterisk/lock.h"
#include "asterisk/logger.h"
#include "asterisk/manager.h"
#include "asterisk/mixmonitor.h"
#include "asterisk/module.h"
#include "asterisk/musiconhold.h"
#include "asterisk/mwi.h"
#include "asterisk/netsock2.h"
#include "asterisk/parking.h"
#include "asterisk/paths.h"
#include "asterisk/pbx.h"
#include "asterisk/pickup.h"
#include "asterisk/rtp_engine.h"
#include "asterisk/sched.h"
#include "asterisk/stasis.h"
#include "asterisk/stasis_channels.h"
#include "asterisk/stream.h"
#include "asterisk/strings.h"
#include "asterisk/sorcery.h"
#include "asterisk/translate.h"
#include "asterisk/utils.h"
"#,
        )
        .allowlist_function("(__)?a(st|o2)_.*")
        .allowlist_function("astman_.*")
        .allowlist_function("ao2_.*")
        .allowlist_function("_ast_.*")
        .allowlist_function("pbx_.*")
        .allowlist_function("stasis_.*")
        .allowlist_function("__stasis_.*")
        .allowlist_type("ast_.*")
        .allowlist_type("ao2_.*")
        .allowlist_var("AST_.*")
        .allowlist_var("STASIS_.*")
        .allowlist_var("ast_.*")
        .allowlist_var("LOG_.*")
        .allowlist_var("__LOG_.*")
        .allowlist_var("EVENT_FLAG_.*")
        .allowlist_var("CONFIG_FLAG_.*")
        .allowlist_var("AO2_.*")
        .allowlist_recursively(true)
        .derive_debug(false)
        .derive_default(false)
        .prepend_enum_name(false)
        .generate_comments(true)
        .layout_tests(true)
        .clang_arg("-DAST_MODULE=\"chan_sccp2\"")
        .clang_arg("-DAST_MODULE_SELF_SYM=__internal_chan_sccp2_self")
        .clang_arg("-fblocks")
        .clang_arg("-Wall")
        .clang_arg("-Wextra")
        .clang_arg("-Werror")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    for include in &include_dirs {
        sys_bindings = sys_bindings.clang_arg(format!("-isystem{}", include.display()));
    }
    let output_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    sys_bindings
        .generate()
        .unwrap_or_else(|error| panic!("unable to generate direct Asterisk ABI bindings: {error}"))
        .write_to_file(output_dir.join("asterisk_sys.rs"))
        .expect("unable to write direct Asterisk ABI bindings");
}

fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        env::current_dir().unwrap().join(path)
    }
}

fn require_file(path: &Path, help: &str) {
    assert!(path.is_file(), "{} is missing: {help}", path.display());
}

fn detect_version(source_dir: &Path) -> Option<String> {
    let script = source_dir.join("build_tools/make_version");
    if let Ok(output) = Command::new(script).arg(source_dir).output()
        && output.status.success()
        && let Ok(version) = String::from_utf8(output.stdout)
    {
        let version = version.trim();
        if !version.is_empty() && !version.starts_with("UNKNOWN__") {
            return Some(version.to_owned());
        }
    }

    // Asterisk's helper treats a submodule's `.git` file as if there were no
    // repository. Release sources carry their numeric line in `configure.ac`;
    // use it directly before reproducing Git identity for the master lane.
    let mainline = source_mainline(source_dir)?;
    if numeric_major(&mainline).is_some() {
        return Some(mainline);
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(source_dir)
        .args(["describe", "--long", "--always", "--tags", "--dirty=M"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let description = String::from_utf8(output.stdout).ok()?;
    let description = description.trim();
    (!description.is_empty()).then(|| format!("GIT-{mainline}-{description}"))
}

fn source_mainline(source_dir: &Path) -> Option<String> {
    let configure = fs::read_to_string(source_dir.join("configure.ac")).ok()?;
    let prefix = "AC_INIT([asterisk], [";
    configure.lines().find_map(|line| {
        let version = line.trim().strip_prefix(prefix)?.split(']').next()?;
        (!version.is_empty()).then(|| version.to_owned())
    })
}

fn numeric_major(version: &str) -> Option<u32> {
    version.split('.').next()?.parse().ok()
}

fn quoted_define(contents: &str, name: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("#define")?.trim();
        let rest = rest.strip_prefix(name)?.trim();
        Some(rest.trim_matches('"').to_owned())
    })
}
