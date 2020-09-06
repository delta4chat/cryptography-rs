// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/*!
Embedded Python resources in a binary.
*/

use {
    super::filtering::{filter_btreemap, resolve_resource_names_from_files},
    super::standalone_builder::ExtensionModuleBuildState,
    crate::app_packaging::resource::{FileContent, FileManifest},
    anyhow::{anyhow, Result},
    python_packaging::bytecode::PythonBytecodeCompiler,
    python_packaging::policy::PythonResourcesPolicy,
    python_packaging::resource::{DataLocation, PythonExtensionModule},
    python_packaging::resource_collection::{
        CompiledResourcesCollection, ConcreteResourceLocation, PythonResourceCollector,
    },
    slog::{info, warn},
    std::collections::{BTreeMap, BTreeSet},
    std::io::Write,
    std::iter::FromIterator,
    std::path::Path,
};

/// Represents Python resources to embed in a binary.
///
/// This collection holds resources before packaging. This type is
/// transformed to `EmbeddedPythonResources` as part of packaging.
#[derive(Debug, Clone)]
pub struct PrePackagedResources {
    pub collector: PythonResourceCollector,
    extension_module_states: BTreeMap<String, ExtensionModuleBuildState>,
}

impl PrePackagedResources {
    pub fn new(policy: &PythonResourcesPolicy, cache_tag: &str) -> Self {
        Self {
            collector: PythonResourceCollector::new(policy, cache_tag),
            extension_module_states: BTreeMap::new(),
        }
    }

    /// Obtain the names of extension modules that will be compiled into libpython.
    ///
    /// These extension modules are statically linked into the binary. They
    /// aren't tracked as Python resources since they aren't part of the resources
    /// data structure.
    pub fn builtin_extension_module_names(&self) -> impl Iterator<Item = &String> {
        self.extension_module_states.keys()
    }

    /// Add an extension module from a Python distribution to be linked into the binary.
    ///
    /// The extension module will have its object files linked into the produced
    /// `libpython` and the extension module will be registered in the list of
    /// the set of extension modules available for import with Python's *builtin*
    /// importer.
    pub fn add_builtin_distribution_extension_module(
        &mut self,
        module: &PythonExtensionModule,
    ) -> Result<()> {
        // No policy check because distribution extension modules are special.

        self.extension_module_states.insert(
            module.name.clone(),
            ExtensionModuleBuildState {
                init_fn: module.init_fn.clone(),
                link_object_files: if module.builtin_default {
                    vec![]
                } else {
                    module.object_file_data.clone()
                },
                link_frameworks: BTreeSet::from_iter(module.link_libraries.iter().filter_map(
                    |link| {
                        if link.framework {
                            Some(link.name.clone())
                        } else {
                            None
                        }
                    },
                )),
                link_system_libraries: BTreeSet::from_iter(
                    module.link_libraries.iter().filter_map(|link| {
                        if link.system {
                            Some(link.name.clone())
                        } else {
                            None
                        }
                    }),
                ),
                link_static_libraries: BTreeSet::from_iter(
                    module.link_libraries.iter().filter_map(|link| {
                        if link.static_library.is_some() {
                            Some(link.name.clone())
                        } else {
                            None
                        }
                    }),
                ),
                link_dynamic_libraries: BTreeSet::from_iter(
                    module.link_libraries.iter().filter_map(|link| {
                        if link.dynamic_library.is_some() {
                            Some(link.name.clone())
                        } else {
                            None
                        }
                    }),
                ),
                link_external_libraries: BTreeSet::new(),
            },
        );

        Ok(())
    }

    /// Add an extension module to be linked into the binary.
    ///
    /// The object files for the extension module will be linked into the produced
    /// binary and the extension module will be made available for import from
    /// Python's _builtin_ importer.
    pub fn add_builtin_extension_module(&mut self, module: &PythonExtensionModule) -> Result<()> {
        if module.object_file_data.is_empty() {
            return Err(anyhow!(
                "cannot add extension module {} as builtin because it lacks object file data",
                module.name
            ));
        }

        self.collector.add_builtin_python_extension_module(module)?;

        self.extension_module_states.insert(
            module.name.clone(),
            ExtensionModuleBuildState {
                init_fn: module.init_fn.clone(),
                link_object_files: module.object_file_data.clone(),
                link_frameworks: BTreeSet::new(),
                link_system_libraries: BTreeSet::new(),
                link_static_libraries: BTreeSet::new(),
                link_dynamic_libraries: BTreeSet::new(),
                link_external_libraries: BTreeSet::from_iter(
                    module.link_libraries.iter().map(|l| l.name.clone()),
                ),
            },
        );

        Ok(())
    }

    /// Add an extension module shared library that should be imported from memory.
    pub fn add_in_memory_extension_module_shared_library(
        &mut self,
        module: &str,
        is_package: bool,
        data: &[u8],
    ) -> Result<()> {
        self.collector.add_python_extension_module(
            &PythonExtensionModule {
                name: module.to_string(),
                init_fn: None,
                extension_file_suffix: "".to_string(),
                shared_library: Some(DataLocation::Memory(data.to_vec())),
                object_file_data: vec![],
                is_package,
                link_libraries: vec![],
                is_stdlib: false,
                builtin_default: false,
                required: false,
                variant: None,
                licenses: None,
                license_texts: None,
                license_public_domain: None,
            },
            &ConcreteResourceLocation::InMemory,
        )?;

        Ok(())
    }

    /// Filter the entities in this instance against names in files.
    pub fn filter_from_files(
        &mut self,
        logger: &slog::Logger,
        files: &[&Path],
        glob_patterns: &[&str],
    ) -> Result<()> {
        let resource_names = resolve_resource_names_from_files(files, glob_patterns)?;

        warn!(logger, "filtering module entries");

        self.collector.filter_resources_mut(|resource| {
            if !resource_names.contains(&resource.name) {
                warn!(logger, "removing {}", resource.name);
                false
            } else {
                true
            }
        })?;

        warn!(logger, "filtering embedded extension modules");
        filter_btreemap(logger, &mut self.extension_module_states, &resource_names);

        Ok(())
    }

    /// Transform this instance into embedded resources data.
    ///
    /// This method performs actions necessary to produce entities which will allow the
    /// resources to be embedded in a binary.
    pub fn package(
        &self,
        logger: &slog::Logger,
        compiler: &mut dyn PythonBytecodeCompiler,
    ) -> Result<EmbeddedPythonResources> {
        let mut file_seen = false;
        for module in self.collector.find_dunder_file()? {
            file_seen = true;
            warn!(logger, "warning: {} contains __file__", module);
        }

        if file_seen {
            warn!(logger, "__file__ was encountered in some embedded modules");
            warn!(
                logger,
                "PyOxidizer does not set __file__ and this may create problems at run-time"
            );
            warn!(
                logger,
                "See https://github.com/indygreg/PyOxidizer/issues/69 for more"
            );
        }

        let resources = self.collector.compile_resources(compiler)?;

        Ok(EmbeddedPythonResources {
            resources,
            extension_modules: self.extension_module_states.clone(),
        })
    }
}

/// Holds state necessary to link libpython.
pub struct LibpythonLinkingInfo {
    /// Object files that need to be linked.
    pub object_files: Vec<DataLocation>,

    pub link_libraries: BTreeSet<String>,
    pub link_frameworks: BTreeSet<String>,
    pub link_system_libraries: BTreeSet<String>,
    pub link_libraries_external: BTreeSet<String>,
}

/// Represents Python resources to embed in a binary.
#[derive(Debug, Default, Clone)]
pub struct EmbeddedPythonResources<'a> {
    /// Resources to write to a packed resources data structure.
    resources: CompiledResourcesCollection<'a>,

    /// Holds state needed for adding extension modules to libpython.
    extension_modules: BTreeMap<String, ExtensionModuleBuildState>,
}

impl<'a> EmbeddedPythonResources<'a> {
    /// Write entities defining resources.
    pub fn write_blobs<W: Write>(&self, module_names: &mut W, resources: &mut W) -> Result<()> {
        for name in self.resources.resources.keys() {
            module_names
                .write_all(name.as_bytes())
                .expect("failed to write");
            module_names.write_all(b"\n").expect("failed to write");
        }

        self.resources.write_packed_resources_v1(resources)
    }

    /// Obtain a list of built-in extensions.
    ///
    /// The returned list will likely make its way to PyImport_Inittab.
    pub fn builtin_extensions(&self) -> Vec<(String, String)> {
        self.extension_modules
            .iter()
            .filter_map(|(name, state)| {
                if let Some(init_fn) = &state.init_fn {
                    Some((name.clone(), init_fn.clone()))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Obtain a FileManifest of extra files to install relative to the produced binary.
    pub fn extra_install_files(&self) -> Result<FileManifest> {
        let mut res = FileManifest::default();

        for (path, location, executable) in &self.resources.extra_files {
            res.add_file(
                path,
                &FileContent {
                    data: location.resolve()?,
                    executable: *executable,
                },
            )?;
        }

        Ok(res)
    }

    /// Resolve state needed to link a libpython.
    pub fn resolve_libpython_linking_info(
        &self,
        logger: &slog::Logger,
    ) -> Result<LibpythonLinkingInfo> {
        let mut object_files = Vec::new();
        let mut link_libraries = BTreeSet::new();
        let mut link_frameworks = BTreeSet::new();
        let mut link_system_libraries = BTreeSet::new();
        let mut link_libraries_external = BTreeSet::new();

        warn!(
            logger,
            "resolving inputs for {} extension modules...",
            self.extension_modules.len()
        );

        for (name, state) in &self.extension_modules {
            if !state.link_object_files.is_empty() {
                info!(
                    logger,
                    "adding {} object files for {} extension module",
                    state.link_object_files.len(),
                    name
                );
                object_files.extend(state.link_object_files.iter().cloned());
            }

            for framework in &state.link_frameworks {
                warn!(logger, "framework {} required by {}", framework, name);
                link_frameworks.insert(framework.clone());
            }

            for library in &state.link_system_libraries {
                warn!(logger, "system library {} required by {}", library, name);
                link_system_libraries.insert(library.clone());
            }

            for library in &state.link_static_libraries {
                warn!(logger, "static library {} required by {}", library, name);
                link_libraries.insert(library.clone());
            }

            for library in &state.link_dynamic_libraries {
                warn!(logger, "dynamic library {} required by {}", library, name);
                link_libraries.insert(library.clone());
            }

            for library in &state.link_external_libraries {
                warn!(logger, "dynamic library {} required by {}", library, name);
                link_libraries_external.insert(library.clone());
            }
        }

        Ok(LibpythonLinkingInfo {
            object_files,
            link_libraries,
            link_frameworks,
            link_system_libraries,
            link_libraries_external,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_CACHE_TAG: &str = "cpython-37";

    #[test]
    fn test_add_distribution_extension_module() -> Result<()> {
        let mut r =
            PrePackagedResources::new(&PythonResourcesPolicy::InMemoryOnly, DEFAULT_CACHE_TAG);
        let em = PythonExtensionModule {
            name: "foo.bar".to_string(),
            init_fn: None,
            extension_file_suffix: "".to_string(),
            builtin_default: false,
            object_file_data: vec![],
            shared_library: None,
            link_libraries: vec![],
            required: false,
            is_package: false,
            is_stdlib: false,
            variant: None,
            licenses: None,
            license_texts: None,
            license_public_domain: None,
        };

        r.add_builtin_distribution_extension_module(&em)?;
        assert_eq!(r.extension_module_states.len(), 1);
        assert_eq!(
            r.extension_module_states.get("foo.bar"),
            Some(&ExtensionModuleBuildState {
                init_fn: None,
                link_object_files: vec![],
                link_frameworks: BTreeSet::new(),
                link_system_libraries: BTreeSet::new(),
                link_static_libraries: BTreeSet::new(),
                link_dynamic_libraries: BTreeSet::new(),
                link_external_libraries: BTreeSet::new()
            })
        );

        Ok(())
    }

    #[test]
    fn test_add_extension_module_data() -> Result<()> {
        let mut r =
            PrePackagedResources::new(&PythonResourcesPolicy::InMemoryOnly, DEFAULT_CACHE_TAG);
        let em = PythonExtensionModule {
            name: "foo.bar".to_string(),
            init_fn: Some("".to_string()),
            extension_file_suffix: "".to_string(),
            shared_library: None,
            object_file_data: vec![DataLocation::Memory(vec![42])],
            is_package: false,
            link_libraries: vec![],
            is_stdlib: true,
            builtin_default: true,
            required: false,
            variant: None,
            licenses: None,
            license_texts: None,
            license_public_domain: None,
        };

        r.add_builtin_extension_module(&em)?;
        assert_eq!(r.extension_module_states.len(), 1);
        assert_eq!(
            r.extension_module_states.get("foo.bar"),
            Some(&ExtensionModuleBuildState {
                init_fn: Some("".to_string()),
                link_object_files: vec![DataLocation::Memory(vec![42])],
                link_frameworks: BTreeSet::new(),
                link_system_libraries: BTreeSet::new(),
                link_static_libraries: BTreeSet::new(),
                link_dynamic_libraries: BTreeSet::new(),
                link_external_libraries: BTreeSet::new()
            })
        );

        Ok(())
    }
}
