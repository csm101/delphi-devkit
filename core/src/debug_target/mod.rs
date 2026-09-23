//! The **debug target** of a Delphi project: a debugger-agnostic description
//! of what debugging that project means — which executable to launch or
//! attach to, the symbol files next to it, the project's own module when it
//! is not that executable, where the sources are, the arguments, and what is
//! missing or stale. It is the same information the IDE gathers for *Run
//! with debugger*, computed from DevKit's project state, the dproj, the
//! compiler's `rsvars.bat` and the IDE's per-user registry settings.
//!
//! Nothing here belongs to a particular debugger. A debug adapter's VS Code
//! extension (or its MCP server) asks DevKit for the target and maps it onto
//! its own launch attributes; hand-written configurations shrink to a
//! project reference. The callers — CLI `ddk debug-target`, the MCP
//! `delphi_get_debug_target` tool, the LSP `debug/target` method — are thin
//! wrappers around [`crate::commands::cmd_debug_target`].
//!
//! Everything read from outside the project state — `rsvars.bat`, the IDE's
//! registry settings — comes in through [`IdeSettings`], so the builder is a
//! pure function of its inputs and tests need neither a Delphi installation
//! nor `HKCU`.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::delphilsp::{IdeLibrarySettings, IdeRegistryRoot};
use crate::projects::{CompilerConfiguration, IdeEnvironment, MacroMap, Project};
use crate::utils::normalize_path;

/// What kind of binary the project produces, which decides how it is
/// debugged: a program is launched itself, a package or a DLL is loaded by
/// its Host Application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DebugTargetKind {
    Program,
    Package,
    Library,
}

/// The symbol files a debugger reads next to the launched executable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolFiles {
    /// The linker map (`DCC_MapFile=3`): source lines and public names.
    pub map: String,
    /// The remote-debug symbols (`DCC_RemoteDebug`): locals, types, expressions.
    pub rsm: String,
}

/// A module the target process loads at run time whose debug information the
/// debugger should bind up front: the project's own package or DLL, or the
/// project's own program when a Host Application launches it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugModule {
    /// The module's file name (`libAbout290.bpl`), how a loaded module is matched.
    pub name: String,
    /// The built module on disk, when found.
    pub binary: Option<String>,
    pub map: Option<String>,
    pub rsm: Option<String>,
    /// The compiled package (`.dcp`), the rich debug information of a BPL.
    pub dcp: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugTarget {
    /// The managed project's id; `None` for an ad-hoc project file.
    pub project_id: Option<usize>,
    pub project: String,
    /// The `.dproj` when there is one, else the `.dpr`/`.dpk`.
    pub project_file: String,
    /// The `.dpr`/`.dpk` main source, when known.
    pub main_source: Option<String>,
    pub kind: DebugTargetKind,
    /// The process to launch or attach to: the program's own exe, or the
    /// Host Application that loads a package, a DLL — or the program itself,
    /// when one is configured for it.
    pub executable: String,
    /// The Host Application when the executable is one.
    pub host_application: Option<String>,
    /// Product name of the compiler the project builds with.
    pub compiler: String,
    pub config: String,
    pub platform: String,
    /// `32` or `64` for a Windows platform; `None` (with a warning) otherwise.
    pub bitness: Option<u8>,
    /// Symbol files expected next to `executable`.
    pub symbols: SymbolFiles,
    /// The project directory.
    pub source_root: String,
    /// Existing directories to resolve unit sources from, most specific
    /// first: the project directory, the dproj's unit search and include
    /// paths, the IDE's Library Path and Browsing Path for the platform, and
    /// the compiler's `source` tree.
    pub source_search_paths: Vec<String>,
    /// The project's own binary whenever `executable` is not it: the
    /// package or DLL a host loads, or the program a Host Application starts.
    pub modules: Vec<DebugModule>,
    /// Command-line arguments: the dproj's `Debugger_RunParams` fused with the
    /// saved Start Parameters, exactly as `Run` passes them.
    pub args: Vec<String>,
    /// Human-readable problems that will degrade or break a session:
    /// missing executable or module, missing or stale symbols, a non-Windows
    /// platform, and every input that could not be read (an unparsable dproj,
    /// a missing `rsvars.bat`), since the target is then described from
    /// less than the IDE would use.
    pub warnings: Vec<String>,
}

impl std::fmt::Display for DebugTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.kind {
            DebugTargetKind::Program => "program",
            DebugTargetKind::Package => "package",
            DebugTargetKind::Library => "library",
        };
        writeln!(f, "Debug target for \"{}\" ({kind}, {} {}, {}):", self.project, self.config, self.platform, self.compiler)?;
        writeln!(f, "  project file: {}", self.project_file)?;
        writeln!(f, "  executable:   {}", self.executable)?;
        if let Some(host) = &self.host_application {
            writeln!(f, "  host app:     {host}")?;
        }
        writeln!(f, "  map / rsm:    {} / {}", self.symbols.map, self.symbols.rsm)?;
        for module in &self.modules {
            writeln!(f, "  module:       {} -> {}", module.name, module.binary.as_deref().unwrap_or("(not built)"))?;
        }
        if !self.args.is_empty() {
            writeln!(f, "  args:         {}", self.args.join(" "))?;
        }
        writeln!(f, "  source root:  {}", self.source_root)?;
        writeln!(f, "  source paths: {} directories", self.source_search_paths.len())?;
        if !self.warnings.is_empty() {
            writeln!(f, "Warnings:")?;
            for warning in &self.warnings {
                writeln!(f, "- {warning}")?;
            }
        }
        Ok(())
    }
}

// ─── IDE inputs ──────────────────────────────────────────────────────────────

/// The inputs that come from the Delphi installation rather than from the
/// project: its macro environment and the IDE's per-platform library
/// settings. [`InstalledIde`] reads the real ones; a test supplies values.
pub trait IdeSettings {
    /// The installation's macro environment (`rsvars.bat`, the IDE's
    /// environment-variable overrides, derived data directories).
    fn environment(&self) -> Result<IdeEnvironment>;
    /// The IDE's Library Path, Browsing Path and package output defaults
    /// for `platform`.
    fn library_settings(&self, platform: &str) -> IdeLibrarySettings;
}

/// The installation a compiler configuration describes: `rsvars.bat` on
/// disk, the IDE settings under the version's registry root.
pub struct InstalledIde<'a> {
    pub compiler: &'a CompilerConfiguration,
}

impl IdeSettings for InstalledIde<'_> {
    fn environment(&self) -> Result<IdeEnvironment> {
        IdeEnvironment::read(Path::new(&self.compiler.installation_path), &bds_version(self.compiler))
    }

    fn library_settings(&self, platform: &str) -> IdeLibrarySettings {
        IdeRegistryRoot::for_bds_version(self.compiler.product_version).library_settings(platform)
    }
}

/// `"23.0"` for Delphi 12: the registry and Documents segment of the version.
fn bds_version(compiler: &CompilerConfiguration) -> String {
    format!("{}.0", compiler.product_version)
}

// ─── Building ────────────────────────────────────────────────────────────────

/// Describes the debug target of a project that builds with `compiler`,
/// reading the installation's `rsvars.bat` and the IDE's registry settings.
pub fn build_debug_target(project: &Project, compiler: &CompilerConfiguration) -> Result<DebugTarget> {
    build_debug_target_with(project, compiler, &InstalledIde { compiler })
}

/// [`build_debug_target`] with the IDE inputs supplied by `ide`. Pure with
/// respect to global state: everything needed is passed in.
pub fn build_debug_target_with(
    project: &Project,
    compiler: &CompilerConfiguration,
    ide: &dyn IdeSettings,
) -> Result<DebugTarget> {
    let mut warnings = Vec::new();
    let context = TargetContext::new(project, compiler, ide, &mut warnings);

    let kind = context.kind();
    // DevKit's own override wins; then the dproj read live for the described
    // config/platform (the persisted discovery was made for the *active*
    // ones, and a `--platform` override may select a different host, or
    // none). The persisted value is the fallback only when the dproj could
    // not be evaluated: a dproj that was read and names no host for this
    // platform means there is none.
    let usable = |value: &String| !value.trim().is_empty() && !value.contains("$(");
    let host_application = project
        .host_application
        .clone()
        .filter(usable)
        .or_else(|| context.dproj_host_application())
        .or_else(|| {
            if context.group.is_some() {
                return None;
            }
            project.dproj_host_application.clone().filter(usable)
        });
    let executable = match (kind, &host_application, &project.exe) {
        (_, Some(host), _) => host.clone(),
        (DebugTargetKind::Program, None, Some(exe)) => exe.clone(),
        (DebugTargetKind::Program, None, None) => bail!(
            "Project \"{}\" has no executable to debug. Compile it first.",
            project.name
        ),
        (_, None, _) => bail!(
            "{} \"{}\" has no Host Application to debug through. Set one via Project > Options > Debugger \
             in the Delphi IDE, or DevKit's \"Set Host Application\".",
            if kind == DebugTargetKind::Package { "Package" } else { "Library" },
            project.name
        ),
    };
    // The launched executable's own symbols matter only when it is the
    // project's program; a host's symbols are optional, the module's count.
    let launches_own_program = kind == DebugTargetKind::Program && host_application.is_none();
    check_executable_artefacts(&executable, launches_own_program, &mut warnings);

    let bitness = match context.platform.to_lowercase().as_str() {
        "win32" => Some(32),
        "win64" | "win64x" => Some(64),
        other => {
            warnings.push(format!("Platform {other} is not a Windows target; a Windows debugger cannot debug it."));
            None
        }
    };

    let modules = match kind {
        DebugTargetKind::Program if launches_own_program => Vec::new(),
        DebugTargetKind::Program => context.hosted_program_module(&mut warnings).into_iter().collect(),
        DebugTargetKind::Package => context.package_module(&executable, &mut warnings).into_iter().collect(),
        DebugTargetKind::Library => context.library_module(&mut warnings).into_iter().collect(),
    };

    let source_search_paths = context.source_search_paths();

    let args = crate::commands::fuse_run_params(project.dproj_run_params.clone(), project.start_parameters.clone())
        .map(|joined| crate::commands::split_run_args(&joined))
        .unwrap_or_default();

    let main_source = project.dpr.clone().or_else(|| project.dpk.clone());
    let project_file = project
        .dproj
        .clone()
        .or_else(|| main_source.clone())
        .unwrap_or_else(|| project.directory.clone());

    Ok(DebugTarget {
        project_id: Some(project.id),
        project: project.name.clone(),
        project_file: json_path(&project_file),
        main_source: main_source.as_deref().map(json_path),
        kind,
        symbols: SymbolFiles {
            map: json_path(&sibling(&executable, "map")),
            rsm: json_path(&sibling(&executable, "rsm")),
        },
        executable: json_path(&executable),
        host_application: host_application.as_deref().map(json_path),
        compiler: compiler.product_name.clone(),
        config: context.config.clone(),
        platform: context.platform.clone(),
        bitness,
        source_root: json_path(&normalize_path(&project.directory).to_string_lossy()),
        source_search_paths,
        modules,
        args,
        warnings,
    })
}

/// Everything the builder needs, resolved once: the effective configuration
/// and platform, the dproj evaluated for them, the macro map that expands
/// `$(NAME)` the way the IDE would, and the IDE's library settings. Every
/// input that fails to load is reported in `warnings` and replaced by the
/// best available fallback, never silently.
struct TargetContext<'a> {
    project: &'a Project,
    compiler: &'a CompilerConfiguration,
    config: String,
    platform: String,
    /// The dproj's merged property group for config/platform, `$(…)` expanded.
    group: Option<dproj_rs::dproj::PropertyGroup>,
    macros: MacroMap,
    library: IdeLibrarySettings,
}

impl<'a> TargetContext<'a> {
    fn new(
        project: &'a Project,
        compiler: &'a CompilerConfiguration,
        ide: &dyn IdeSettings,
        warnings: &mut Vec<String>,
    ) -> Self {
        let environment = ide.environment().unwrap_or_else(|error| {
            warnings.push(format!(
                "The IDE environment of {} could not be read ({error}); $(BDS)-relative paths will stay unresolved.",
                compiler.product_name
            ));
            IdeEnvironment::default()
        });
        let mut macros = environment.macros(Path::new(&compiler.installation_path));
        macros.set("ProjectDir", project.directory.clone());
        macros.set("ProjectName", project.name.clone());
        // `<DllSuffix>$(Auto)</DllSuffix>`: the IDE's automatic LIBSUFFIX is
        // the package version (`290` for Delphi 12).
        macros.set("Auto", compiler.package_version.to_string());

        let dproj = project.dproj.as_deref().and_then(|path| {
            dproj_rs::DprojBuilder::new()
                .env(macros.as_env())
                .from_file(path)
                .map_err(|error| {
                    warnings.push(format!(
                        "Could not evaluate {path} ({error}); the target is described from DevKit's recorded \
                         project state alone, so config/platform, host application and search paths may be incomplete."
                    ));
                })
                .ok()
        });
        let (config, platform) = effective_config_platform(project, dproj.as_ref());
        let group = dproj.as_ref().and_then(|dproj| {
            dproj
                .active_property_group_for(&config, &platform)
                .map_err(|error| {
                    warnings.push(format!(
                        "The dproj defines no property group for {config}/{platform} ({error}); output directories, \
                         host application and search paths from the dproj are unavailable."
                    ));
                })
                .ok()
        });
        macros.set("Config", config.clone());
        macros.set("Configuration", config.clone());
        macros.set("Platform", platform.clone());

        let library = ide.library_settings(&platform);
        if library.search_path.is_none() && library.browsing_path.is_none() {
            warnings.push(format!(
                "No IDE Library Path found for {platform} under {}; only the project's own paths and the \
                 compiler's source tree are searched for sources.",
                IdeRegistryRoot::for_bds_version(compiler.product_version).key_path()
            ));
        }

        TargetContext { project, compiler, config, platform, group, macros, library }
    }

    fn kind(&self) -> DebugTargetKind {
        if self.project.dpk.is_some() {
            return DebugTargetKind::Package;
        }
        let generates_dll = self
            .group
            .as_ref()
            .and_then(|group| group.project_properties.gen_dll.as_deref())
            .map(|value| value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if generates_dll {
            return DebugTargetKind::Library;
        }
        DebugTargetKind::Program
    }

    /// The dproj's own `Debugger_HostApplication` for the effective
    /// configuration/platform, read live and macro-expanded: the fallback
    /// when the persisted project state predates host discovery or is stale.
    fn dproj_host_application(&self) -> Option<String> {
        let raw = self.group.as_ref()?.other.get("Debugger_HostApplication")?;
        let host = self.macros.expand(raw.trim());
        if host.is_empty() || host.contains("$(") {
            return None;
        }
        Some(absolutize(&host, &self.project.directory).to_string_lossy().to_string())
    }

    /// The file name stem of the built package/DLL: the project name plus
    /// its `LIBSUFFIX`.
    fn binary_stem(&self) -> String {
        format!("{}{}", self.project.name, self.lib_suffix())
    }

    /// The `LIBSUFFIX` of the package/DLL: the dproj's `DllSuffix` (already
    /// macro-expanded, `$(Auto)` included), else the `{$LIBSUFFIX}` directive
    /// of the main source — the only place a hand-maintained package
    /// declares it — where `AUTO` means the compiler's package version.
    fn lib_suffix(&self) -> String {
        let declared = self
            .group
            .as_ref()
            .and_then(|group| group.other.get("DllSuffix"))
            .map(|suffix| suffix.trim().to_string())
            .filter(|suffix| !suffix.is_empty())
            .or_else(|| self.project.dpk.as_deref().and_then(lib_suffix_directive));
        match declared {
            None => String::new(),
            Some(suffix) if suffix.eq_ignore_ascii_case("auto") || suffix.eq_ignore_ascii_case("$(Auto)") => {
                self.compiler.package_version.to_string()
            }
            Some(suffix) => {
                let expanded = self.macros.expand(&suffix);
                if expanded.contains("$(") { String::new() } else { expanded }
            }
        }
    }

    // ─── Modules ─────────────────────────────────────────────────────────

    /// A program started by a Host Application is still the module whose
    /// symbols matter; the host launches it, DevKit knows where it is.
    fn hosted_program_module(&self, warnings: &mut Vec<String>) -> Option<DebugModule> {
        let Some(exe) = self.project.exe.as_deref() else {
            warnings.push(format!(
                "Program \"{}\" is started by a Host Application but has no executable of its own recorded. Compile it first.",
                self.project.name
            ));
            return Some(DebugModule {
                name: format!("{}.exe", self.project.name),
                binary: None,
                map: None,
                rsm: None,
                dcp: None,
            });
        };
        if !Path::new(exe).exists() {
            warnings.push(format!("Executable not found: {exe}. Compile the project first."));
        } else {
            check_module_artefacts(exe, warnings);
        }
        Some(DebugModule {
            name: file_name(exe),
            binary: Some(json_path(exe)),
            map: Some(json_path(&sibling(exe, "map"))),
            rsm: Some(json_path(&sibling(exe, "rsm"))),
            dcp: None,
        })
    }

    /// The package's own `.bpl`, searched where the IDE puts one — the
    /// dproj's `DCC_BplOutput`, the IDE's default package output, the
    /// hosting executable's directory, `.\<Platform>\<Config>` — and its
    /// `.dcp`. Without a built `.bpl` the debugger treats the package as a
    /// black box, so that is a warning, not an error.
    fn package_module(&self, host: &str, warnings: &mut Vec<String>) -> Option<DebugModule> {
        let bpl_name = format!("{}.bpl", self.binary_stem());
        let mut directories = Vec::new();
        push_dir(&mut directories, self.dproj_output(|options| options.bpl_output.clone()));
        push_dir(&mut directories, self.expanded_dir(self.library.package_dpl_output.as_deref()));
        directories.extend(self.common_output_dirs("Bpl"));
        push_dir(&mut directories, Path::new(host).parent().map(Path::to_path_buf));
        directories.push(PathBuf::from(&self.project.directory).join(&self.platform).join(&self.config));

        let Some(binary) = find_built_file(&directories, &bpl_name) else {
            let searched: Vec<String> = directories.iter().map(|d| d.to_string_lossy().to_string()).collect();
            warnings.push(format!(
                "No built {bpl_name} found for package \"{}\" (searched: {}). Compile it first, or the debugger will treat it as a black box.",
                self.project.name,
                searched.join("; ")
            ));
            return Some(DebugModule { name: bpl_name, binary: None, map: None, rsm: None, dcp: None });
        };
        check_module_artefacts(&binary, warnings);

        let dcp_name = format!("{}.dcp", self.project.name);
        let mut dcp_directories = Vec::new();
        push_dir(&mut dcp_directories, self.dproj_output(|options| options.dcp_output.clone()));
        push_dir(&mut dcp_directories, self.expanded_dir(self.library.package_dcp_output.as_deref()));
        dcp_directories.extend(self.common_output_dirs("Dcp"));
        push_dir(&mut dcp_directories, Path::new(&binary).parent().map(Path::to_path_buf));
        let dcp = find_built_file(&dcp_directories, &dcp_name);
        if dcp.is_none() {
            warnings.push(format!(
                "No {dcp_name} found for package \"{}\": the debugger will lack the package's rich debug information.",
                self.project.name
            ));
        }

        Some(DebugModule {
            name: file_name(&binary),
            map: Some(json_path(&sibling(&binary, "map"))),
            rsm: Some(json_path(&sibling(&binary, "rsm"))),
            dcp: dcp.as_deref().map(json_path),
            binary: Some(json_path(&binary)),
        })
    }

    /// The DLL a library project builds: DevKit records its output as the
    /// program-style `<stem>.exe`; the DLL sits in the same directory.
    fn library_module(&self, warnings: &mut Vec<String>) -> Option<DebugModule> {
        let dll_name = format!("{}.dll", self.binary_stem());
        let output_dir = self
            .project
            .exe
            .as_deref()
            .and_then(|exe| Path::new(exe).parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from(&self.project.directory).join(&self.platform).join(&self.config));
        let Some(binary) = find_built_file(&[output_dir.clone()], &dll_name) else {
            warnings.push(format!(
                "No built {dll_name} found for library \"{}\" in {}. Compile it first, or the debugger will treat it as a black box.",
                self.project.name,
                output_dir.to_string_lossy()
            ));
            return Some(DebugModule { name: dll_name, binary: None, map: None, rsm: None, dcp: None });
        };
        check_module_artefacts(&binary, warnings);
        Some(DebugModule {
            name: file_name(&binary),
            map: Some(json_path(&sibling(&binary, "map"))),
            rsm: Some(json_path(&sibling(&binary, "rsm"))),
            dcp: None,
            binary: Some(json_path(&binary)),
        })
    }

    /// One expanded, absolutized output directory read from the dproj's
    /// merged property group (dproj-rs has already expanded `$(…)` there).
    fn dproj_output(&self, select: impl Fn(&dproj_rs::dproj::DccOptions) -> Option<String>) -> Option<PathBuf> {
        let raw = self.group.as_ref().and_then(|group| select(&group.dcc_options))?;
        self.expanded_dir(Some(&raw))
    }

    /// Expands and absolutizes a directory value; `None` when a macro stays
    /// unresolved (not a usable directory).
    fn expanded_dir(&self, raw: Option<&str>) -> Option<PathBuf> {
        let raw = raw?.trim();
        if raw.is_empty() {
            return None;
        }
        let expanded = self.macros.expand(raw);
        if expanded.contains("$(") {
            return None;
        }
        Some(absolutize(&expanded, &self.project.directory))
    }

    /// `$(BDSCOMMONDIR)\<kind>\<platform>` then `$(BDSCOMMONDIR)\<kind>`: the
    /// IDE's default package output, platform subdirectory first (Win32
    /// builds land in the root).
    fn common_output_dirs(&self, kind: &str) -> Vec<PathBuf> {
        let Some(root) = self.expanded_dir(Some(&format!("$(BDSCOMMONDIR)\\{kind}"))) else {
            return Vec::new();
        };
        vec![root.join(&self.platform), root]
    }

    // ─── Sources ─────────────────────────────────────────────────────────

    /// The directories a debugger should scan for unit sources, most
    /// specific first and without duplicates: the project directory, the
    /// dproj's unit search path and include path (a `{$I}` line is
    /// attributed to the `.inc` file, so the debugger must find it too), the
    /// IDE's Library Path and Browsing Path for the platform — the browsing
    /// path is where the sources behind third-party components live — and
    /// the compiler's own `source` tree. Only existing directories are kept.
    fn source_search_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        push_unique(&mut paths, PathBuf::from(&self.project.directory));
        let dproj_paths = [
            self.group.as_ref().and_then(|group| group.dcc_options.unit_search_path.clone()),
            self.group.as_ref().and_then(|group| group.dcc_options.include_path.clone()),
        ];
        let ide_paths = [self.library.search_path.clone(), self.library.browsing_path.clone()];
        for list in dproj_paths.into_iter().chain(ide_paths).flatten() {
            for entry in list.split(';') {
                if let Some(dir) = self.expanded_dir(Some(entry)) {
                    push_unique(&mut paths, dir);
                }
            }
        }
        if let Some(dir) = self.expanded_dir(Some("$(BDS)\\source")) {
            push_unique(&mut paths, dir);
        }
        paths
            .into_iter()
            .filter(|dir| dir.is_dir())
            .map(|dir| json_path(&dir.to_string_lossy()))
            .collect()
    }
}

// ─── Project evaluation helpers ──────────────────────────────────────────────

fn effective_config_platform(project: &Project, dproj: Option<&dproj_rs::Dproj>) -> (String, String) {
    match dproj {
        Some(dproj) => project.effective_config_platform(dproj),
        _ => (
            project.active_configuration.clone().unwrap_or_else(|| "Debug".to_string()),
            project.active_platform.clone().unwrap_or_else(|| "Win32".to_string()),
        ),
    }
}

lazy_static::lazy_static! {
    /// `{$LIBSUFFIX '290'}` or `{$LIBSUFFIX AUTO}` in a `.dpk`.
    static ref LIBSUFFIX_DIRECTIVE: regex::Regex =
        regex::Regex::new(r"(?i)\{\$LIBSUFFIX\s+(?:'(?P<quoted>[^']*)'|(?P<auto>AUTO))\s*\}").unwrap();
}

/// The `LIBSUFFIX` a main source declares itself, when it does.
fn lib_suffix_directive(dpk_path: &str) -> Option<String> {
    let source = std::fs::read_to_string(dpk_path).ok()?;
    let captures = LIBSUFFIX_DIRECTIVE.captures(&source)?;
    if captures.name("auto").is_some() {
        return Some("AUTO".to_string());
    }
    captures.name("quoted").map(|m| m.as_str().trim().to_string())
}

// ─── Artefact checks ─────────────────────────────────────────────────────────

/// Flags a missing executable and, when it is the project's own program,
/// missing or stale `.map`/`.rsm` next to it. A host application's symbols
/// are optional: it is the loaded module that matters then.
fn check_executable_artefacts(executable: &str, own_program: bool, warnings: &mut Vec<String>) {
    if !Path::new(executable).exists() {
        warnings.push(format!("Executable not found: {executable}. Compile the project first."));
        return;
    }
    if own_program {
        check_symbols_next_to(executable, "the executable", warnings);
    }
}

fn check_module_artefacts(binary: &str, warnings: &mut Vec<String>) {
    check_symbols_next_to(binary, &file_name(binary), warnings);
}

/// How much older than its binary a symbol file may be before it counts as
/// stale. Within one build the linker writes the `.map` and `.rsm` *before*
/// it finishes the executable (measured: 0.3–0.6 s earlier on an MSBuild
/// build of a mid-sized program), and a large link takes longer than that, so
/// only a gap that cannot belong to the same build is reported.
const STALE_SYMBOLS_TOLERANCE: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Missing symbols cost features; symbols left over from an earlier build are
/// worse — breakpoints land on wrong lines and locals read as garbage.
fn check_symbols_next_to(binary: &str, what: &str, warnings: &mut Vec<String>) {
    let binary_time = modified_time(binary).map(|time| time - STALE_SYMBOLS_TOLERANCE);
    for (extension, effect) in [
        ("map", "no source lines: breakpoints and stepping will not work"),
        ("rsm", "variable inspection and expression evaluation will be severely limited"),
    ] {
        let symbol_file = sibling(binary, extension);
        if !Path::new(&symbol_file).exists() {
            warnings.push(format!(
                "Missing .{extension} next to {what} ({effect}). Compile with debug info (Compile for Debugging)."
            ));
            continue;
        }
        if let (Some(binary_time), Some(symbol_time)) = (binary_time, modified_time(&symbol_file)) {
            if symbol_time < binary_time {
                warnings.push(format!(
                    "The .{extension} next to {what} is older than the binary: stale symbols make breakpoints land on \
                     wrong lines. Recompile with debug info (Compile for Debugging)."
                ));
            }
        }
    }
}

fn modified_time(path: &str) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|metadata| metadata.modified()).ok()
}

// ─── File helpers ────────────────────────────────────────────────────────────

/// The first directory holding exactly `file_name`. An exact name only: the
/// stem already carries the `LIBSUFFIX`, and a looser match in a shared
/// output directory would pick up another package's binary.
fn find_built_file(directories: &[PathBuf], file_name: &str) -> Option<String> {
    directories
        .iter()
        .map(|dir| dir.join(file_name))
        .find(|candidate| candidate.is_file())
        .map(|found| normalize_path(found).to_string_lossy().to_string())
}

fn absolutize(dir: &str, base: &str) -> PathBuf {
    let path = PathBuf::from(dir);
    let absolute = if path.is_relative() { PathBuf::from(base).join(path) } else { path };
    normalize_path(absolute)
}

fn push_dir(directories: &mut Vec<PathBuf>, dir: Option<PathBuf>) {
    if let Some(dir) = dir {
        directories.push(dir);
    }
}

fn push_unique(paths: &mut Vec<PathBuf>, candidate: PathBuf) {
    let candidate = normalize_path(candidate);
    let exists = paths
        .iter()
        .any(|p| p.to_string_lossy().eq_ignore_ascii_case(&candidate.to_string_lossy()));
    if !exists {
        paths.push(candidate);
    }
}

fn sibling(path: &str, extension: &str) -> String {
    PathBuf::from(path).with_extension(extension).to_string_lossy().to_string()
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Forward slashes: valid on Windows, and readable inside JSON.
fn json_path(path: &str) -> String {
    path.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;

    /// A Delphi 12 installation that exists only as values: no disk, no registry.
    struct FakeIde {
        environment: Option<IdeEnvironment>,
        library: IdeLibrarySettings,
    }

    impl FakeIde {
        fn new() -> Self {
            let rsvars: HashMap<String, String> = [
                ("BDS", r"C:\Delphi\23.0"),
                ("BDSCOMMONDIR", r"C:\Users\Public\Documents\Embarcadero\Studio\23.0"),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
            FakeIde {
                environment: Some(IdeEnvironment { rsvars, ..Default::default() }),
                library: IdeLibrarySettings::default(),
            }
        }

        fn with_variable(mut self, name: &str, value: &str) -> Self {
            if let Some(environment) = &mut self.environment {
                environment.ide_variables.push((name.to_string(), value.to_string()));
            }
            self
        }

        fn unavailable() -> Self {
            FakeIde { environment: None, library: IdeLibrarySettings::default() }
        }
    }

    impl IdeSettings for FakeIde {
        fn environment(&self) -> Result<IdeEnvironment> {
            self.environment.clone().ok_or_else(|| anyhow::anyhow!("rsvars.bat not found"))
        }

        fn library_settings(&self, _platform: &str) -> IdeLibrarySettings {
            self.library.clone()
        }
    }

    fn compiler() -> CompilerConfiguration {
        CompilerConfiguration {
            condition: "VER360".into(),
            product_name: "Delphi 12.0 Athens".into(),
            product_version: 23,
            package_version: 290,
            compiler_version: 36,
            installation_path: r"C:\Delphi\23.0".into(),
            build_arguments: Vec::new(),
        }
    }

    fn project(dir: &Path, name: &str) -> Project {
        Project { id: 7, name: name.into(), directory: dir.to_string_lossy().to_string(), ..Default::default() }
    }

    fn touch(path: PathBuf) -> String {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"x").unwrap();
        path.to_string_lossy().to_string()
    }

    /// The `TestPkg64.dproj` fixture: a package whose Debug/Win64 host is
    /// `$(VEGADIR)\FieldHost64.exe`, `-flag1` as run parameters, unit search
    /// path `.\$(Platform)\$(Config)`.
    fn package_from_fixture(dir: &Path) -> Project {
        let dproj = dir.join("TestPkg64.dproj");
        fs::write(&dproj, include_str!("../../tests/fixtures/TestPkg64.dproj")).unwrap();
        fs::write(dir.join("TestPkg.dpk"), "package TestPkg;\nend.\n").unwrap();
        let mut project = project(dir, "TestPkg64");
        project.dproj = Some(dproj.to_string_lossy().to_string());
        project.dpk = Some(dir.join("TestPkg.dpk").to_string_lossy().to_string());
        project
    }

    #[test]
    fn a_program_target_lists_its_exe_and_symbols() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = touch(tmp.path().join("Demo.exe"));
        let mut project = project(tmp.path(), "Demo");
        project.dpr = Some(tmp.path().join("Demo.dpr").to_string_lossy().to_string());
        project.exe = Some(exe);
        project.dproj_run_params = Some("-a".into());
        project.start_parameters = Some("\"b c\"".into());

        let target = build_debug_target_with(&project, &compiler(), &FakeIde::new()).unwrap();
        assert_eq!(target.kind, DebugTargetKind::Program);
        assert_eq!(target.bitness, Some(32));
        assert!(target.executable.ends_with("/Demo.exe"));
        assert!(target.symbols.rsm.ends_with("/Demo.rsm"));
        assert_eq!(target.args, vec!["-a", "b c"]);
        assert!(target.modules.is_empty());
        assert_eq!(target.source_search_paths[0], json_path(&normalize_path(tmp.path()).to_string_lossy()));
        assert!(target.warnings.iter().any(|w| w.contains("Missing .map")));
    }

    #[test]
    fn a_program_with_a_host_application_keeps_its_own_symbols_as_a_module() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = touch(tmp.path().join("out").join("Plugin.exe"));
        touch(tmp.path().join("out").join("Plugin.map"));
        let host = touch(tmp.path().join("Host.exe"));
        let mut project = project(tmp.path(), "Plugin");
        project.exe = Some(exe);
        project.host_application = Some(host);

        let target = build_debug_target_with(&project, &compiler(), &FakeIde::new()).unwrap();
        assert_eq!(target.kind, DebugTargetKind::Program);
        assert!(target.executable.ends_with("/Host.exe"));
        // The host's symbols are not the program's: nothing is expected next to it.
        assert!(!target.warnings.iter().any(|w| w.contains("the executable")), "{:?}", target.warnings);
        let module = &target.modules[0];
        assert_eq!(module.name, "Plugin.exe");
        assert!(module.map.as_deref().unwrap().ends_with("/out/Plugin.map"));
        assert!(target.warnings.iter().any(|w| w.contains("Plugin.exe") && w.contains("Missing .rsm")));
    }

    #[test]
    fn a_package_target_resolves_host_platform_and_search_paths_from_the_dproj() {
        let tmp = tempfile::tempdir().unwrap();
        let project = package_from_fixture(tmp.path());
        fs::create_dir_all(tmp.path().join("Win64").join("Debug")).unwrap();
        let ide = FakeIde::new().with_variable("VEGADIR", r"C:\Athens\hydra_2");

        // Nothing persisted about the host: the dproj is read live, for the
        // dproj's default platform (Win64), with the IDE variable expanded.
        let target = build_debug_target_with(&project, &compiler(), &ide).unwrap();
        assert_eq!(target.kind, DebugTargetKind::Package);
        assert_eq!((target.config.as_str(), target.platform.as_str(), target.bitness), ("Debug", "Win64", Some(64)));
        assert_eq!(target.executable.to_lowercase(), "c:/athens/hydra_2/fieldhost64.exe");
        assert_eq!(target.host_application, Some(target.executable.clone()));
        assert_eq!(target.modules[0].name, "TestPkg64.bpl");
        assert!(target.modules[0].binary.is_none());
        let unit_output = json_path(&normalize_path(tmp.path().join("Win64").join("Debug")).to_string_lossy());
        assert!(target.source_search_paths.contains(&unit_output), "{:?}", target.source_search_paths);
        assert!(target.warnings.iter().any(|w| w.contains("Executable not found")));
        assert!(target.warnings.iter().any(|w| w.contains("No built TestPkg64.bpl")));
    }

    #[test]
    fn a_package_target_uses_the_persisted_state_and_finds_its_bpl_next_to_the_host() {
        let tmp = tempfile::tempdir().unwrap();
        let mut project = package_from_fixture(tmp.path());
        let ide_env = vec![("VEGADIR".to_string(), tmp.path().join("vega").to_string_lossy().to_string())];
        project.discover_paths(&ide_env).unwrap();
        let host = touch(tmp.path().join("vega").join("FieldHost64.exe"));
        touch(tmp.path().join("vega").join("TestPkg64.bpl"));
        touch(tmp.path().join("vega").join("TestPkg64.dcp"));
        let ide = FakeIde::new().with_variable("VEGADIR", &tmp.path().join("vega").to_string_lossy());

        let target = build_debug_target_with(&project, &compiler(), &ide).unwrap();
        assert_eq!(target.executable, json_path(&normalize_path(&host).to_string_lossy()));
        assert_eq!(target.args, vec!["-flag1"]);
        let module = &target.modules[0];
        assert!(module.binary.as_deref().unwrap().ends_with("/vega/TestPkg64.bpl"));
        assert!(module.dcp.as_deref().unwrap().ends_with("/vega/TestPkg64.dcp"));
        assert!(target.warnings.iter().any(|w| w.contains("TestPkg64.bpl") && w.contains("Missing .map")));

        // Describing another platform re-reads the host for it: the dproj's
        // Debug host is `$(ProjectDir)\hosts\DebugHost.exe`, not the Win64 one.
        project.active_platform = Some("Win32".into());
        let target = build_debug_target_with(&project, &compiler(), &ide).unwrap();
        assert_eq!(target.platform, "Win32");
        assert!(target.executable.ends_with("/hosts/DebugHost.exe"), "{}", target.executable);
    }

    #[test]
    fn a_package_with_an_automatic_libsuffix_is_found_by_its_exact_name_in_the_bpl_output() {
        let tmp = tempfile::tempdir().unwrap();
        let dproj = tmp.path().join("TestPkgSuffix.dproj");
        fs::write(&dproj, include_str!("../../tests/fixtures/TestPkgSuffix.dproj")).unwrap();
        let dpk = tmp.path().join("TestPkgSuffix.dpk");
        fs::write(&dpk, "package TestPkgSuffix;\nend.\n").unwrap();
        let mut project = project(tmp.path(), "TestPkgSuffix");
        project.dproj = Some(dproj.to_string_lossy().to_string());
        project.dpk = Some(dpk.to_string_lossy().to_string());
        touch(tmp.path().join("hosts").join("Host.exe"));
        // A stale namesake that a prefix match would have preferred.
        touch(tmp.path().join("bpl").join("TestPkgSuffixOld.bpl"));
        touch(tmp.path().join("bpl").join("TestPkgSuffix290.bpl"));
        touch(tmp.path().join("dcp").join("TestPkgSuffix.dcp"));
        fs::create_dir_all(tmp.path().join("inc")).unwrap();

        let target = build_debug_target_with(&project, &compiler(), &FakeIde::new()).unwrap();
        assert!(target.executable.ends_with("/hosts/Host.exe"));
        let module = &target.modules[0];
        assert_eq!(module.name, "TestPkgSuffix290.bpl");
        assert!(module.binary.as_deref().unwrap().ends_with("/bpl/TestPkgSuffix290.bpl"));
        assert!(module.dcp.as_deref().unwrap().ends_with("/dcp/TestPkgSuffix.dcp"));
        let include = json_path(&normalize_path(tmp.path().join("inc")).to_string_lossy());
        assert!(target.source_search_paths.contains(&include), "{:?}", target.source_search_paths);
    }

    #[test]
    fn a_libsuffix_directive_in_the_dpk_names_the_bpl_when_the_dproj_is_silent() {
        let tmp = tempfile::tempdir().unwrap();
        let dpk = tmp.path().join("Demo.dpk");
        fs::write(&dpk, "package Demo;\n{$LIBSUFFIX 'D29'}\nend.\n").unwrap();
        let host = touch(tmp.path().join("Host.exe"));
        touch(tmp.path().join("Host.exe").with_file_name("DemoD29.bpl"));
        let mut project = project(tmp.path(), "Demo");
        project.dpk = Some(dpk.to_string_lossy().to_string());
        project.host_application = Some(host);

        let target = build_debug_target_with(&project, &compiler(), &FakeIde::new()).unwrap();
        assert_eq!(target.modules[0].name, "DemoD29.bpl");
        assert!(target.modules[0].binary.is_some());

        fs::write(&dpk, "package Demo;\n{$LIBSUFFIX AUTO}\nend.\n").unwrap();
        let target = build_debug_target_with(&project, &compiler(), &FakeIde::new()).unwrap();
        assert_eq!(target.modules[0].name, "Demo290.bpl");
    }

    #[test]
    fn a_library_target_binds_its_dll() {
        let tmp = tempfile::tempdir().unwrap();
        let dproj = tmp.path().join("TestLib.dproj");
        fs::write(&dproj, include_str!("../../tests/fixtures/TestLib.dproj")).unwrap();
        let mut project = project(tmp.path(), "TestLib");
        project.dproj = Some(dproj.to_string_lossy().to_string());
        // DevKit records a library's output the way it records a program's.
        project.exe = Some(tmp.path().join("out").join("TestLib.exe").to_string_lossy().to_string());
        touch(tmp.path().join("Host.exe"));
        touch(tmp.path().join("out").join("TestLib.dll"));

        let target = build_debug_target_with(&project, &compiler(), &FakeIde::new()).unwrap();
        assert_eq!(target.kind, DebugTargetKind::Library, "{:?}", target.warnings);
        assert!(target.executable.ends_with("/Host.exe"), "{}", target.executable);
        assert_eq!(target.modules[0].name, "TestLib.dll");
        assert!(target.modules[0].binary.as_deref().unwrap().ends_with("/out/TestLib.dll"));
    }

    #[test]
    fn a_package_without_host_is_an_error_not_a_target() {
        let tmp = tempfile::tempdir().unwrap();
        let mut project = project(tmp.path(), "Demo");
        project.dpk = Some(tmp.path().join("Demo.dpk").to_string_lossy().to_string());
        let error = build_debug_target_with(&project, &compiler(), &FakeIde::new()).unwrap_err().to_string();
        assert!(error.contains("Host Application"), "{error}");
    }

    #[test]
    fn unreadable_inputs_are_reported_not_swallowed() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = touch(tmp.path().join("Demo.exe"));
        let dproj = tmp.path().join("Demo.dproj");
        fs::write(&dproj, "<Project><PropertyGroup><Config>").unwrap();
        let mut project = project(tmp.path(), "Demo");
        project.dproj = Some(dproj.to_string_lossy().to_string());
        project.exe = Some(exe);

        let target = build_debug_target_with(&project, &compiler(), &FakeIde::unavailable()).unwrap();
        assert!(target.warnings.iter().any(|w| w.contains("rsvars.bat not found")), "{:?}", target.warnings);
        assert!(target.warnings.iter().any(|w| w.contains("Could not evaluate")), "{:?}", target.warnings);
        assert!(target.warnings.iter().any(|w| w.contains("No IDE Library Path")), "{:?}", target.warnings);
    }

    #[test]
    fn symbol_checks_report_missing_and_stale_files() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("Demo.exe");
        let map = tmp.path().join("Demo.map");
        fs::write(&map, b"map").unwrap();
        fs::write(&exe, b"exe").unwrap();
        // A map from an earlier build: well outside the same-build tolerance.
        let an_hour_ago = SystemTime::now() - std::time::Duration::from_secs(3600);
        fs::File::options().write(true).open(&map).unwrap().set_modified(an_hour_ago).unwrap();
        let mut warnings = Vec::new();
        check_executable_artefacts(&exe.to_string_lossy(), true, &mut warnings);
        assert!(warnings.iter().any(|w| w.contains(".map") && w.contains("older")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("Missing .rsm")), "{warnings:?}");
    }
}
