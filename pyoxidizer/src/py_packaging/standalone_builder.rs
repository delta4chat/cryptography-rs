// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use {
    super::binary::{
        EmbeddedPythonContext, EmbeddedResourcesBlobs, LibpythonLinkMode, PythonBinaryBuilder,
        PythonLinkingInfo,
    },
    super::config::{EmbeddedPythonConfig, RawAllocator},
    super::distribution::{BinaryLibpythonLinkMode, PythonDistribution},
    super::embedded_resource::{EmbeddedPythonResources, PrePackagedResources},
    super::libpython::link_libpython,
    super::packaging_tool::{find_resources, pip_install, read_virtualenv, setup_py_install},
    super::standalone_distribution::StandaloneDistribution,
    crate::app_packaging::resource::FileContent,
    anyhow::{anyhow, Result},
    python_packaging::bytecode::BytecodeCompiler,
    python_packaging::policy::{PythonPackagingPolicy, PythonResourcesPolicy},
    python_packaging::resource::{
        BytecodeOptimizationLevel, DataLocation, PythonExtensionModule,
        PythonModuleBytecodeFromSource, PythonModuleSource, PythonPackageDistributionResource,
        PythonPackageResource, PythonResource,
    },
    python_packaging::resource_collection::{
        ConcreteResourceLocation, PrePackagedResource, PythonResourceCollector,
    },
    slog::warn,
    std::collections::{BTreeSet, HashMap},
    std::convert::TryFrom,
    std::path::{Path, PathBuf},
    std::sync::Arc,
    tempdir::TempDir,
};

/// Holds state necessary to link an extension module into libpython.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionModuleBuildState {
    /// Extension C initialization function.
    pub init_fn: Option<String>,

    /// Object files to link into produced binary.
    pub link_object_files: Vec<DataLocation>,

    /// Frameworks this extension module needs to link against.
    pub link_frameworks: BTreeSet<String>,

    /// System libraries this extension module needs to link against.
    pub link_system_libraries: BTreeSet<String>,

    /// Static libraries this extension module needs to link against.
    pub link_static_libraries: BTreeSet<String>,

    /// Dynamic libraries this extension module needs to link against.
    pub link_dynamic_libraries: BTreeSet<String>,

    /// Dynamic libraries this extension module needs to link against.
    pub link_external_libraries: BTreeSet<String>,
}

/// A self-contained Python executable before it is compiled.
#[derive(Clone, Debug)]
pub struct StandalonePythonExecutableBuilder {
    /// The target triple we are running on.
    host_triple: String,

    /// The target triple we are building for.
    target_triple: String,

    /// The name of the executable to build.
    exe_name: String,

    /// The Python distribution being used to build this executable.
    distribution: Arc<Box<StandaloneDistribution>>,

    /// How libpython should be linked.
    link_mode: LibpythonLinkMode,

    /// Whether the built binary is capable of loading dynamically linked
    /// extension modules from memory.
    supports_in_memory_dynamically_linked_extension_loading: bool,

    /// Policy to apply to added resources.
    packaging_policy: PythonPackagingPolicy,

    /// Python resources to be embedded in the binary.
    resources: PrePackagedResources,

    /// Configuration of the embedded Python interpreter.
    config: EmbeddedPythonConfig,

    /// Path to python executable that can be invoked at build time.
    python_exe: PathBuf,
}

impl StandalonePythonExecutableBuilder {
    #[allow(clippy::too_many_arguments)]
    pub fn from_distribution(
        distribution: Arc<Box<StandaloneDistribution>>,
        host_triple: String,
        target_triple: String,
        exe_name: String,
        link_mode: BinaryLibpythonLinkMode,
        packaging_policy: PythonPackagingPolicy,
        config: EmbeddedPythonConfig,
    ) -> Result<Box<dyn PythonBinaryBuilder>> {
        let python_exe = distribution.python_exe.clone();
        let cache_tag = distribution.cache_tag.clone();

        let (supports_static_libpython, supports_dynamic_libpython) =
            distribution.libpython_link_support();

        let link_mode = match link_mode {
            BinaryLibpythonLinkMode::Default => {
                if supports_static_libpython {
                    LibpythonLinkMode::Static
                } else if supports_dynamic_libpython {
                    LibpythonLinkMode::Dynamic
                } else {
                    return Err(anyhow!("no link modes supported; please report this bug"));
                }
            }
            BinaryLibpythonLinkMode::Static => {
                if !supports_static_libpython {
                    return Err(anyhow!(
                        "Python distribution does not support statically linking libpython"
                    ));
                }

                LibpythonLinkMode::Static
            }
            BinaryLibpythonLinkMode::Dynamic => {
                if !supports_dynamic_libpython {
                    return Err(anyhow!(
                        "Python distribution does not support dynamically linking libpython"
                    ));
                }

                LibpythonLinkMode::Dynamic
            }
        };

        let supports_in_memory_dynamically_linked_extension_loading =
            distribution.supports_in_memory_dynamically_linked_extension_loading();

        let mut builder = Box::new(Self {
            host_triple,
            target_triple,
            exe_name,
            distribution,
            link_mode,
            supports_in_memory_dynamically_linked_extension_loading,
            packaging_policy: packaging_policy.clone(),
            resources: PrePackagedResources::new(
                packaging_policy.get_resources_policy(),
                &cache_tag,
            ),
            config,
            python_exe,
        });

        builder.add_distribution_resources(&packaging_policy)?;

        Ok(builder)
    }

    fn add_distribution_resources(&mut self, policy: &PythonPackagingPolicy) -> Result<()> {
        for ext in self.packaging_policy.resolve_python_extension_modules(
            self.distribution.extension_modules.values(),
            &self.target_triple,
        )? {
            self.add_distribution_extension_module(&ext)?;
        }

        for source in self.distribution.source_modules()? {
            if policy.filter_python_resource(&source.clone().into()) {
                self.add_module_source(&source)?;
            }

            let bytecode = source.as_bytecode_module(BytecodeOptimizationLevel::Zero);

            if policy.filter_python_resource(&bytecode.clone().into()) {
                self.add_module_bytecode(&bytecode)?;
            }
        }

        for resource in self.distribution.resource_datas()? {
            if policy.filter_python_resource(&resource.clone().into()) {
                self.add_package_resource(&resource)?;
            }
        }

        Ok(())
    }

    /// Build a Python library suitable for linking.
    ///
    /// This will take the underlying distribution, resources, and
    /// configuration and produce a new executable binary.
    fn resolve_python_linking_info(
        &self,
        logger: &slog::Logger,
        opt_level: &str,
        resources: &EmbeddedPythonResources,
    ) -> Result<PythonLinkingInfo> {
        let libpythonxy_filename;
        let mut cargo_metadata: Vec<String> = Vec::new();
        let libpythonxy_data;
        let libpython_filename: Option<PathBuf>;
        let libpyembeddedconfig_data: Option<Vec<u8>>;
        let libpyembeddedconfig_filename: Option<PathBuf>;

        match self.link_mode {
            LibpythonLinkMode::Static => {
                let temp_dir = TempDir::new("pyoxidizer-build-exe")?;
                let temp_dir_path = temp_dir.path();

                warn!(
                    logger,
                    "generating custom link library containing Python..."
                );
                let library_info = link_libpython(
                    logger,
                    &self.distribution,
                    resources,
                    &temp_dir_path,
                    &self.host_triple,
                    &self.target_triple,
                    opt_level,
                )?;

                libpythonxy_filename =
                    PathBuf::from(library_info.libpython_path.file_name().unwrap());
                cargo_metadata.extend(library_info.cargo_metadata);

                libpythonxy_data = std::fs::read(&library_info.libpython_path)?;
                libpython_filename = None;
                libpyembeddedconfig_filename = Some(PathBuf::from(
                    library_info.libpyembeddedconfig_path.file_name().unwrap(),
                ));
                libpyembeddedconfig_data =
                    Some(std::fs::read(&library_info.libpyembeddedconfig_path)?);
            }

            LibpythonLinkMode::Dynamic => {
                libpythonxy_filename = PathBuf::from("pythonXY.lib");
                libpythonxy_data = Vec::new();
                libpython_filename = self.distribution.libpython_shared_library.clone();
                libpyembeddedconfig_filename = None;
                libpyembeddedconfig_data = None;
            }
        }

        Ok(PythonLinkingInfo {
            libpythonxy_filename,
            libpythonxy_data,
            libpython_filename,
            libpyembeddedconfig_filename,
            libpyembeddedconfig_data,
            cargo_metadata,
        })
    }
}

impl PythonBinaryBuilder for StandalonePythonExecutableBuilder {
    fn clone_box(&self) -> Box<dyn PythonBinaryBuilder> {
        Box::new(self.clone())
    }

    fn name(&self) -> String {
        self.exe_name.clone()
    }

    fn libpython_link_mode(&self) -> LibpythonLinkMode {
        self.link_mode
    }

    fn cache_tag(&self) -> &str {
        self.distribution.cache_tag()
    }

    fn python_packaging_policy(&self) -> &PythonPackagingPolicy {
        &self.packaging_policy
    }

    fn python_exe_path(&self) -> &Path {
        &self.python_exe
    }

    fn resource_collector(&self) -> &PythonResourceCollector {
        &self.resources.collector
    }

    fn iter_resources<'a>(
        &'a self,
    ) -> Box<dyn Iterator<Item = (&'a String, &'a PrePackagedResource)> + 'a> {
        Box::new(self.resources.collector.iter_resources())
    }

    fn builtin_extension_module_names<'a>(&'a self) -> Box<dyn Iterator<Item = &'a String> + 'a> {
        Box::new(self.resources.builtin_extension_module_names())
    }

    fn pip_install(
        &self,
        logger: &slog::Logger,
        verbose: bool,
        install_args: &[String],
        extra_envs: &HashMap<String, String>,
    ) -> Result<Vec<PythonResource>> {
        pip_install(
            logger,
            &**self.distribution,
            self.link_mode,
            verbose,
            install_args,
            extra_envs,
        )
    }

    fn read_package_root(
        &self,
        logger: &slog::Logger,
        path: &Path,
        packages: &[String],
    ) -> Result<Vec<PythonResource>> {
        Ok(find_resources(&logger, &**self.distribution, path, None)?
            .iter()
            .filter_map(|x| {
                if x.is_in_packages(packages) {
                    Some(x.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>())
    }

    fn read_virtualenv(&self, logger: &slog::Logger, path: &Path) -> Result<Vec<PythonResource>> {
        read_virtualenv(logger, &**self.distribution, path)
    }

    fn setup_py_install(
        &self,
        logger: &slog::Logger,
        package_path: &Path,
        verbose: bool,
        extra_envs: &HashMap<String, String>,
        extra_global_arguments: &[String],
    ) -> Result<Vec<PythonResource>> {
        setup_py_install(
            logger,
            &**self.distribution,
            self.link_mode,
            package_path,
            verbose,
            extra_envs,
            extra_global_arguments,
        )
    }

    fn add_in_memory_module_source(&mut self, module: &PythonModuleSource) -> Result<()> {
        self.resources
            .collector
            .add_python_module_source(module, &ConcreteResourceLocation::InMemory)
    }

    fn add_relative_path_module_source(
        &mut self,
        prefix: &str,
        module: &PythonModuleSource,
    ) -> Result<()> {
        self.resources.collector.add_python_module_source(
            module,
            &ConcreteResourceLocation::RelativePath(prefix.to_string()),
        )
    }

    fn add_in_memory_module_bytecode(
        &mut self,
        module: &PythonModuleBytecodeFromSource,
    ) -> Result<()> {
        self.resources
            .collector
            .add_python_module_bytecode_from_source(module, &ConcreteResourceLocation::InMemory)
    }

    fn add_relative_path_module_bytecode(
        &mut self,
        prefix: &str,
        module: &PythonModuleBytecodeFromSource,
    ) -> Result<()> {
        self.resources
            .collector
            .add_python_module_bytecode_from_source(
                module,
                &ConcreteResourceLocation::RelativePath(prefix.to_string()),
            )
    }

    fn add_in_memory_package_resource(&mut self, resource: &PythonPackageResource) -> Result<()> {
        self.resources
            .collector
            .add_python_package_resource(resource, &ConcreteResourceLocation::InMemory)
    }

    fn add_relative_path_package_resource(
        &mut self,
        prefix: &str,
        resource: &PythonPackageResource,
    ) -> Result<()> {
        self.resources.collector.add_python_package_resource(
            resource,
            &ConcreteResourceLocation::RelativePath(prefix.to_string()),
        )
    }

    fn add_in_memory_package_distribution_resource(
        &mut self,
        resource: &PythonPackageDistributionResource,
    ) -> Result<()> {
        self.resources
            .collector
            .add_package_distribution_resource(resource, &ConcreteResourceLocation::InMemory)
    }

    fn add_relative_path_package_distribution_resource(
        &mut self,
        prefix: &str,
        resource: &PythonPackageDistributionResource,
    ) -> Result<()> {
        self.resources.collector.add_package_distribution_resource(
            resource,
            &ConcreteResourceLocation::RelativePath(prefix.to_string()),
        )
    }

    fn add_builtin_distribution_extension_module(
        &mut self,
        extension_module: &PythonExtensionModule,
    ) -> Result<()> {
        self.resources
            .add_builtin_distribution_extension_module(&extension_module)
    }

    fn add_in_memory_distribution_extension_module(
        &mut self,
        extension_module: &PythonExtensionModule,
    ) -> Result<()> {
        if !self.supports_in_memory_dynamically_linked_extension_loading {
            return Err(anyhow!(
                "loading extension modules from memory not supported by this build configuration"
            ));
        }

        self.resources
            .collector
            .add_python_extension_module(extension_module, &ConcreteResourceLocation::InMemory)
    }

    fn add_relative_path_distribution_extension_module(
        &mut self,
        prefix: &str,
        extension_module: &PythonExtensionModule,
    ) -> Result<()> {
        if self.distribution.is_extension_module_file_loadable() {
            self.resources.collector.add_python_extension_module(
                extension_module,
                &ConcreteResourceLocation::RelativePath(prefix.to_string()),
            )
        } else {
            Err(anyhow!(
                "loading extension modules from files not supported by this build configuration"
            ))
        }
    }

    fn add_distribution_extension_module(
        &mut self,
        extension_module: &PythonExtensionModule,
    ) -> Result<()> {
        // Distribution extensions are special in that we allow them to be
        // builtin extensions, even if it violates the resources policy that prohibits
        // memory loading.

        // Builtins always get added as such.
        if extension_module.builtin_default {
            return self.add_builtin_distribution_extension_module(&extension_module);
        }

        match self.packaging_policy.get_resources_policy().clone() {
            PythonResourcesPolicy::InMemoryOnly => match self.link_mode {
                LibpythonLinkMode::Static => {
                    self.add_builtin_distribution_extension_module(&extension_module)
                }
                LibpythonLinkMode::Dynamic => {
                    self.add_in_memory_distribution_extension_module(&extension_module)
                }
            },
            PythonResourcesPolicy::FilesystemRelativeOnly(prefix) => match self.link_mode {
                LibpythonLinkMode::Static => {
                    self.add_builtin_distribution_extension_module(&extension_module)
                }
                LibpythonLinkMode::Dynamic => {
                    self.add_relative_path_distribution_extension_module(&prefix, &extension_module)
                }
            },
            PythonResourcesPolicy::PreferInMemoryFallbackFilesystemRelative(prefix) => {
                match self.link_mode {
                    LibpythonLinkMode::Static => {
                        self.add_builtin_distribution_extension_module(&extension_module)
                    }
                    LibpythonLinkMode::Dynamic => {
                        // Try in-memory and fall back to file-based if that fails.
                        let mut res =
                            self.add_in_memory_distribution_extension_module(&extension_module);

                        if res.is_err() {
                            res = self.add_relative_path_distribution_extension_module(
                                &prefix,
                                &extension_module,
                            )
                        }

                        res
                    }
                }
            }
        }
    }

    fn add_in_memory_dynamic_extension_module(
        &mut self,
        extension_module: &PythonExtensionModule,
    ) -> Result<()> {
        if self.supports_in_memory_dynamically_linked_extension_loading
            && extension_module.shared_library.is_some()
        {
            self.resources
                .add_in_memory_extension_module_shared_library(
                    &extension_module.name,
                    extension_module.is_package,
                    &extension_module
                        .shared_library
                        .as_ref()
                        .unwrap()
                        .resolve()?,
                )
        } else if !extension_module.object_file_data.is_empty() {
            // TODO we shouldn't be adding a builtin extension module from this API.
            self.resources
                .add_builtin_extension_module(extension_module)
        } else if extension_module.shared_library.is_some() {
            Err(anyhow!(
                "loading extension modules from memory not supported by this build configuration"
            ))
        } else {
            Err(anyhow!(
                "cannot load extension module from memory due to missing object files"
            ))
        }
    }

    fn add_relative_path_dynamic_extension_module(
        &mut self,
        prefix: &str,
        extension_module: &PythonExtensionModule,
    ) -> Result<()> {
        if extension_module.shared_library.is_none() {
            return Err(anyhow!(
                "extension module instance has no shared library data"
            ));
        }

        if self.distribution.is_extension_module_file_loadable() {
            self.resources.collector.add_python_extension_module(
                extension_module,
                &ConcreteResourceLocation::RelativePath(prefix.to_string()),
            )
        } else {
            Err(anyhow!(
                "loading extension modules from files not supported by this build configuration"
            ))
        }
    }

    fn add_dynamic_extension_module(
        &mut self,
        extension_module: &PythonExtensionModule,
    ) -> Result<()> {
        if extension_module.shared_library.is_none() {
            return Err(anyhow!(
                "extension module instance has no shared library data"
            ));
        }

        match self.packaging_policy.get_resources_policy().clone() {
            PythonResourcesPolicy::InMemoryOnly => {
                if self.supports_in_memory_dynamically_linked_extension_loading {
                    self.resources
                        .add_in_memory_extension_module_shared_library(
                            &extension_module.name,
                            extension_module.is_package,
                            &extension_module
                                .shared_library
                                .as_ref()
                                .unwrap()
                                .resolve()?,
                        )
                } else {
                    Err(anyhow!("in-memory-only resources policy active but in-memory extension module importing not supported by this configuration: cannot load {}", extension_module.name))
                }
            }
            PythonResourcesPolicy::FilesystemRelativeOnly(ref prefix) => {
                if self.distribution.is_extension_module_file_loadable() {
                    self.resources.collector.add_python_extension_module(
                        extension_module,
                        &ConcreteResourceLocation::RelativePath(prefix.to_string()),
                    )
                } else {
                    Err(anyhow!("filesystem-relative-only policy active but file-based extension module loading not supported by this configuration"))
                }
            }
            PythonResourcesPolicy::PreferInMemoryFallbackFilesystemRelative(ref prefix) => {
                if self.supports_in_memory_dynamically_linked_extension_loading {
                    self.resources
                        .add_in_memory_extension_module_shared_library(
                            &extension_module.name,
                            extension_module.is_package,
                            &extension_module
                                .shared_library
                                .as_ref()
                                .unwrap()
                                .resolve()?,
                        )
                } else if self.distribution.is_extension_module_file_loadable() {
                    self.resources.collector.add_python_extension_module(
                        extension_module,
                        &ConcreteResourceLocation::RelativePath(prefix.to_string()),
                    )
                } else {
                    Err(anyhow!("prefer-in-memory-fallback-filesystem-relative policy active but could not find a mechanism to add an extension module"))
                }
            }
        }
    }

    fn add_static_extension_module(
        &mut self,
        extension_module: &PythonExtensionModule,
    ) -> Result<()> {
        self.resources
            .add_builtin_extension_module(extension_module)
    }

    fn filter_resources_from_files(
        &mut self,
        logger: &slog::Logger,
        files: &[&Path],
        glob_patterns: &[&str],
    ) -> Result<()> {
        self.resources
            .filter_from_files(logger, files, glob_patterns)
    }

    fn requires_jemalloc(&self) -> bool {
        self.config.raw_allocator == RawAllocator::Jemalloc
    }

    fn to_embedded_python_context(
        &self,
        logger: &slog::Logger,
        opt_level: &str,
    ) -> Result<EmbeddedPythonContext> {
        let resources = {
            let mut compiler = BytecodeCompiler::new(&self.python_exe)?;

            self.resources.package(logger, &mut compiler)?
        };

        let mut extra_files = resources.extra_install_files()?;
        let linking_info = self.resolve_python_linking_info(logger, opt_level, &resources)?;
        let resources = EmbeddedResourcesBlobs::try_from(resources)?;

        if self.link_mode == LibpythonLinkMode::Dynamic {
            if let Some(p) = &self.distribution.libpython_shared_library {
                let manifest_path = Path::new(p.file_name().unwrap());
                let content = FileContent {
                    data: std::fs::read(&p)?,
                    executable: false,
                };

                extra_files.add_file(&manifest_path, &content)?;
            }
        }

        Ok(EmbeddedPythonContext {
            config: self.config.clone(),
            linking_info,
            resources,
            extra_files,
            host_triple: self.host_triple.clone(),
            target_triple: self.target_triple.clone(),
        })
    }
}

#[cfg(test)]
pub mod tests {
    use {
        super::*,
        crate::py_packaging::distribution::{BinaryLibpythonLinkMode, DistributionFlavor},
        crate::python_distributions::PYTHON_DISTRIBUTIONS,
        crate::testutil::*,
        lazy_static::lazy_static,
        python_packaging::policy::ExtensionModuleFilter,
    };

    lazy_static! {
        pub static ref WINDOWS_TARGET_TRIPLES: Vec<&'static str> =
            vec!["i686-pc-windows-msvc", "x86_64-pc-windows-msvc"];
    }

    pub type StandaloneBuilderContext = (
        Arc<Box<StandaloneDistribution>>,
        Box<dyn PythonBinaryBuilder>,
    );

    /// Defines construction options for a `StandalonePythonExecutableBuilder`.
    ///
    /// This is mostly intended to be used by tests, to reduce boilerplate for
    /// constructing instances.
    pub struct StandalonePythonExecutableBuilderOptions {
        pub logger: Option<slog::Logger>,
        pub host_triple: String,
        pub target_triple: String,
        pub distribution_flavor: DistributionFlavor,
        pub app_name: String,
        pub libpython_link_mode: BinaryLibpythonLinkMode,
        pub extension_module_filter: ExtensionModuleFilter,
        pub resources_policy: PythonResourcesPolicy,
    }

    impl Default for StandalonePythonExecutableBuilderOptions {
        fn default() -> Self {
            // Grab default values from a default policy so they stay in sync.
            let default_policy = PythonPackagingPolicy::default();

            Self {
                logger: None,
                host_triple: env!("HOST").to_string(),
                target_triple: env!("HOST").to_string(),
                distribution_flavor: DistributionFlavor::Standalone,
                app_name: "testapp".to_string(),
                libpython_link_mode: BinaryLibpythonLinkMode::Default,
                extension_module_filter: default_policy.get_extension_module_filter().clone(),
                resources_policy: default_policy.get_resources_policy().clone(),
            }
        }
    }

    impl StandalonePythonExecutableBuilderOptions {
        pub fn new_builder(&self) -> Result<StandaloneBuilderContext> {
            let logger = if let Some(logger) = &self.logger {
                logger.clone()
            } else {
                get_logger()?
            };

            let record = PYTHON_DISTRIBUTIONS
                .find_distribution(&self.target_triple, &self.distribution_flavor)
                .ok_or_else(|| anyhow!("could not find Python distribution"))?;

            let distribution = get_distribution(&record.location)?;

            let mut policy = PythonPackagingPolicy::default();
            policy.set_extension_module_filter(self.extension_module_filter.clone());
            policy.set_resources_policy(self.resources_policy.clone());

            let config = EmbeddedPythonConfig::default();

            Ok((
                distribution.clone(),
                distribution.as_python_executable_builder(
                    &logger,
                    &self.host_triple,
                    &self.target_triple,
                    &self.app_name,
                    self.libpython_link_mode.clone(),
                    &policy,
                    &config,
                )?,
            ))
        }
    }

    pub fn get_embedded(logger: &slog::Logger) -> Result<EmbeddedPythonContext> {
        let options = StandalonePythonExecutableBuilderOptions::default();
        let (_, exe) = options.new_builder()?;

        exe.to_embedded_python_context(logger, "0")
    }

    #[test]
    fn test_write_embedded_files() -> Result<()> {
        let logger = get_logger()?;
        let embedded = get_embedded(&logger)?;
        let temp_dir = tempdir::TempDir::new("pyoxidizer-test")?;

        embedded.write_files(temp_dir.path())?;

        Ok(())
    }

    #[test]
    fn test_minimal_extensions_present() -> Result<()> {
        let options = StandalonePythonExecutableBuilderOptions::default();
        let (distribution, builder) = options.new_builder()?;

        let expected = distribution
            .extension_modules
            .iter()
            .filter_map(|(_, extensions)| {
                if extensions.default_variant().is_minimally_required() {
                    Some(extensions.default_variant().name.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        // Sanity check.
        assert!(expected.contains(&"_io".to_string()));

        for name in &expected {
            // All extensions annotated as required in the distribution are marked
            // as built-ins.
            assert!(builder.builtin_extension_module_names().any(|x| x == name));

            // Built-in extension modules shouldn't be annotated as resources.
            assert!(!builder.iter_resources().any(|(x, _)| x == name));
        }

        Ok(())
    }

    #[test]
    fn test_linux_extensions_sanity() -> Result<()> {
        let options = StandalonePythonExecutableBuilderOptions {
            target_triple: "x86_64-unknown-linux-gnu".to_string(),
            extension_module_filter: ExtensionModuleFilter::All,
            ..StandalonePythonExecutableBuilderOptions::default()
        };

        let (distribution, builder) = options.new_builder()?;

        let builtin_names = builder.builtin_extension_module_names().collect::<Vec<_>>();

        // All extensions compiled as built-ins by default.
        for (name, _) in distribution.extension_modules.iter() {
            assert!(builtin_names.contains(&name));
        }

        Ok(())
    }

    #[test]
    fn test_linux_musl_extensions_sanity() -> Result<()> {
        let options = StandalonePythonExecutableBuilderOptions {
            target_triple: "x86_64-unknown-linux-musl".to_string(),
            extension_module_filter: ExtensionModuleFilter::All,
            ..StandalonePythonExecutableBuilderOptions::default()
        };

        let (distribution, builder) = options.new_builder()?;

        // All extensions for musl Linux are built-in because dynamic linking
        // not possible.
        for name in distribution.extension_modules.keys() {
            assert!(builder.builtin_extension_module_names().any(|e| name == e));
        }

        Ok(())
    }

    #[test]
    fn test_macos_extensions_sanity() -> Result<()> {
        let options = StandalonePythonExecutableBuilderOptions {
            target_triple: "x86_64-apple-darwin".to_string(),
            extension_module_filter: ExtensionModuleFilter::All,
            ..StandalonePythonExecutableBuilderOptions::default()
        };

        let (distribution, builder) = options.new_builder()?;

        let builtin_names = builder.builtin_extension_module_names().collect::<Vec<_>>();

        // All extensions compiled as built-ins by default.
        for (name, _) in distribution.extension_modules.iter() {
            assert!(builtin_names.contains(&name));
        }

        Ok(())
    }

    #[test]
    fn test_windows_dynamic_extensions_sanity() -> Result<()> {
        for target in WINDOWS_TARGET_TRIPLES.iter() {
            let options = StandalonePythonExecutableBuilderOptions {
                target_triple: target.to_string(),
                extension_module_filter: ExtensionModuleFilter::All,
                ..StandalonePythonExecutableBuilderOptions::default()
            };

            let (distribution, builder) = options.new_builder()?;

            let builtin_names = builder.builtin_extension_module_names().collect::<Vec<_>>();

            // In-core extensions are compiled as built-ins.
            for (name, variants) in distribution.extension_modules.iter() {
                let builtin_default = variants.iter().any(|e| e.builtin_default);
                assert_eq!(builtin_names.contains(&name), builtin_default);
            }

            // Required extensions are compiled as built-in.
            // This assumes that are extensions annotated as required are built-in.
            // But this is an implementation detail. If this fails, it might be OK.
            for (name, variants) in distribution.extension_modules.iter() {
                // !required does not mean it is missing, however!
                if variants.iter().any(|e| e.required) {
                    assert!(builtin_names.contains(&name));
                }
            }
        }

        Ok(())
    }

    #[test]
    fn test_windows_dynamic_distribution_dynamic_extension_files() -> Result<()> {
        for target in WINDOWS_TARGET_TRIPLES.iter() {
            let options = StandalonePythonExecutableBuilderOptions {
                target_triple: target.to_string(),
                extension_module_filter: ExtensionModuleFilter::Minimal,
                resources_policy: PythonResourcesPolicy::FilesystemRelativeOnly("lib".to_string()),
                ..StandalonePythonExecutableBuilderOptions::default()
            };

            let (distribution, mut builder): (
                Arc<Box<StandaloneDistribution>>,
                Box<dyn PythonBinaryBuilder>,
            ) = options.new_builder()?;

            // When loading resources from the filesystem, dynamically linked
            // extension modules should be manifested as filesystem files and
            // library dependencies should be captured.

            let ssl_extension = distribution
                .extension_modules
                .get("_ssl")
                .unwrap()
                .default_variant();
            builder.add_distribution_extension_module(ssl_extension)?;

            let extensions = builder
                .iter_resources()
                .filter_map(|(_, r)| {
                    if r.relative_path_extension_module_shared_library.is_some() {
                        Some(r)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            assert_eq!(
                extensions.len(),
                1,
                "only manually added extension present when using minimal extension mode"
            );
            let ssl = &extensions[0];
            assert_eq!(ssl.name, "_ssl");

            let (path, _) = ssl
                .relative_path_extension_module_shared_library
                .as_ref()
                .unwrap();
            assert_eq!(path, &PathBuf::from("lib/_ssl"));

            let shared_libraries = builder
                .iter_resources()
                .filter_map(|(_, r)| {
                    if r.relative_path_shared_library.is_some() {
                        Some(r)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            assert_eq!(
                shared_libraries.len(),
                2,
                "pulled in shared library dependencies for _ssl"
            );

            let lib_suffix = match *target {
                "i686-pc-windows-msvc" => "",
                "x86_64-pc-windows-msvc" => "-x64",
                _ => panic!("unexpected target: {}", target),
            };

            assert_eq!(
                shared_libraries[0].name,
                format!("libcrypto-1_1{}", lib_suffix)
            );
            assert_eq!(
                shared_libraries[0]
                    .relative_path_shared_library
                    .as_ref()
                    .unwrap()
                    .0,
                "lib"
            );

            assert_eq!(
                shared_libraries[1].name,
                format!("libssl-1_1{}", lib_suffix)
            );
        }
        Ok(())
    }

    #[test]
    fn test_windows_static_extensions_sanity() -> Result<()> {
        for target in WINDOWS_TARGET_TRIPLES.iter() {
            let options = StandalonePythonExecutableBuilderOptions {
                target_triple: target.to_string(),
                distribution_flavor: DistributionFlavor::StandaloneStatic,
                extension_module_filter: ExtensionModuleFilter::All,
                ..StandalonePythonExecutableBuilderOptions::default()
            };

            let (distribution, builder) = options.new_builder()?;

            let builtin_names = builder.builtin_extension_module_names().collect::<Vec<_>>();

            // All distribution extensions are built-ins in static Windows
            // distributions.
            for name in distribution.extension_modules.keys() {
                assert!(builtin_names.contains(&name));
            }
        }

        Ok(())
    }
}
