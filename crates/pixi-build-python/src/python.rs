use std::{collections::BTreeMap, ffi::OsStr, path::PathBuf, str::FromStr};

use miette::IntoDiagnostic;
use pixi_build_backend::{
    traits::{BuildConfigurationProvider, RequirementsProvider},
    ProjectModel, Targets,
};
use pyproject_toml::PyProjectToml;
use rattler_build::{
    console_utils::LoggingOutputHandler,
    hash::HashInfo,
    metadata::{BuildConfiguration, Directories, PackagingSettings, PlatformWithVirtualPackages},
    recipe::{
        parser::{Build, Package, PathSource, Python, Requirements, ScriptContent, Source},
        variable::Variable,
        Recipe,
    },
    NormalizedKey,
};
use rattler_conda_types::{
    package::{ArchiveType, EntryPoint},
    ChannelConfig, NoArchType, PackageName, Platform,
};
use rattler_package_streaming::write::CompressionLevel;

use crate::{
    build_script::{BuildPlatform, BuildScriptContext, Installer},
    config::PythonBackendConfig,
};

pub struct PythonBuildBackend<P: ProjectModel> {
    pub(crate) logging_output_handler: LoggingOutputHandler,
    pub(crate) manifest_path: PathBuf,
    pub(crate) manifest_root: PathBuf,
    pub(crate) project_model: P,
    pub(crate) config: PythonBackendConfig,
    pub(crate) cache_dir: Option<PathBuf>,
    pub(crate) pyproject_manifest: Option<PyProjectToml>,
}

impl<P: ProjectModel> PythonBuildBackend<P> {
    /// Returns a new instance of [`PythonBuildBackend`] by reading the manifest
    /// at the given path.
    pub fn new(
        manifest_path: PathBuf,
        project_model: P,
        config: PythonBackendConfig,
        logging_output_handler: LoggingOutputHandler,
        cache_dir: Option<PathBuf>,
    ) -> miette::Result<Self> {
        // Determine the root directory of the manifest
        let manifest_root = manifest_path
            .parent()
            .ok_or_else(|| miette::miette!("the project manifest must reside in a directory"))?
            .to_path_buf();

        let pyproject_manifest = if manifest_path
            .file_name()
            .and_then(OsStr::to_str)
            .map(|str| str.to_lowercase())
            == Some("pyproject.toml".to_string())
        {
            // Load the manifest as a pyproject
            let contents = fs_err::read_to_string(&manifest_path).into_diagnostic()?;

            // Load the manifest as a pyproject
            Some(toml_edit::de::from_str(&contents).into_diagnostic()?)
        } else {
            None
        };

        Ok(Self {
            manifest_path,
            manifest_root,
            project_model,
            config,
            logging_output_handler,
            cache_dir,
            pyproject_manifest,
        })
    }

    /// Read the entry points from the pyproject.toml and return them as a list.
    ///
    /// If the manifest is not a pyproject.toml file no entry-points are added.
    pub(crate) fn entry_points(&self) -> Vec<EntryPoint> {
        let scripts = self
            .pyproject_manifest
            .as_ref()
            .and_then(|p| p.project.as_ref())
            .and_then(|p| p.scripts.as_ref());

        scripts
            .into_iter()
            .flatten()
            .flat_map(|(name, entry_point)| {
                EntryPoint::from_str(&format!("{name} = {entry_point}"))
            })
            .collect()
    }

    /// Constructs a [`Recipe`] that will build the python package into a conda
    /// package.
    ///
    /// If the package is editable, the recipe will not include the source but
    /// only references to the original source files.
    ///
    /// Script entry points are read from the pyproject and added as entry
    /// points in the conda package.
    pub(crate) fn recipe(
        &self,
        host_platform: Platform,
        channel_config: &ChannelConfig,
        editable: bool,
        variant: &BTreeMap<NormalizedKey, Variable>,
    ) -> miette::Result<Recipe> {
        // TODO: remove this env var override as soon as we have profiles
        let editable = std::env::var("BUILD_EDITABLE_PYTHON")
            .map(|val| val == "true")
            .unwrap_or(editable);

        // Parse the package name and version from the manifest
        let name = PackageName::from_str(self.project_model.name()).into_diagnostic()?;
        let version = self.project_model.version().clone().ok_or_else(|| {
            miette::miette!("a version is missing from the package but it is required")
        })?;

        // Determine whether the package should be built as a noarch package or as a
        // generic package.
        let noarch_type = if self.config.noarch() {
            NoArchType::python()
        } else {
            NoArchType::none()
        };

        // Construct python specific settings
        let python = Python {
            entry_points: self.entry_points(),
            ..Python::default()
        };

        let requirements =
            self.requirements(&self.project_model, host_platform, channel_config, variant)?;

        let installer = Installer::determine_installer::<P>(
            &self.project_model.dependencies(Some(host_platform)),
        );

        // Create a build script
        let build_platform = Platform::current();
        let build_number = 0;

        let build_script = BuildScriptContext {
            installer,
            build_platform: if build_platform.is_windows() {
                BuildPlatform::Windows
            } else {
                BuildPlatform::Unix
            },
            editable,
            manifest_root: self.manifest_root.clone(),
        }
        .render();

        // Define the sources of the package.
        let source = if editable {
            // In editable mode we don't include the source in the package, the package will
            // refer back to the original source.
            Vec::new()
        } else {
            Vec::from([Source::Path(PathSource {
                // TODO: How can we use a git source?
                path: self.manifest_root.clone(),
                sha256: None,
                md5: None,
                patches: vec![],
                target_directory: None,
                file_name: None,
                use_gitignore: true,
            })])
        };

        Ok(Recipe {
            schema_version: 1,
            package: Package {
                version: version.into(),
                name,
            },
            context: Default::default(),
            cache: None,
            source,
            build: Build {
                number: build_number,
                string: Default::default(),

                // skip: Default::default(),
                script: ScriptContent::Commands(build_script).into(),
                noarch: noarch_type,

                python,
                // dynamic_linking: Default::default(),
                // always_copy_files: Default::default(),
                // always_include_files: Default::default(),
                // merge_build_and_host_envs: false,
                // variant: Default::default(),
                // prefix_detection: Default::default(),
                // post_process: vec![],
                // files: Default::default(),
                ..Build::default()
            },
            requirements,
            tests: vec![],
            about: Default::default(),
            extra: Default::default(),
        })
    }


}

impl<P: ProjectModel> RequirementsProvider<P> for PythonBuildBackend<P> {
    fn build_tool_names(
        &self,
        dependencies: &pixi_build_backend::traits::Dependencies<
            <<P as ProjectModel>::Targets as Targets>::Spec,
        >,
    ) -> Vec<String> {
        let installer = Installer::determine_installer::<P>(dependencies);

        // Ensure python and pip/uv are available in the host dependencies section.
        [installer.package_name().to_string(), "python".to_string()].to_vec()
    }

    fn add_build_tools<'a>(
        &'a self,
        dependencies: &mut pixi_build_backend::traits::Dependencies<
            'a,
            <<P as ProjectModel>::Targets as Targets>::Spec,
        >,
        empty_spec: &'a <<P as ProjectModel>::Targets as Targets>::Spec,
        build_tools: &'a [String],
    ) {
        for pkg_name in build_tools.iter() {
            if dependencies.host.contains_key(&pkg_name) {
                // If the host dependencies already contain the package,
                // we don't need to add it again.
                continue;
            }

            dependencies.host.insert(pkg_name, empty_spec);
        }
    }

    fn post_process_requirements(
        &self,
        _requirements: &mut Requirements,
        _host_platform: Platform,
    ) {
        // No post processing is needed for python requirements
    }
}

impl<P: ProjectModel> BuildConfigurationProvider<P> for PythonBuildBackend<P> {
    fn construct_configuration(
        &self,
        recipe: &Recipe,
        channels: Vec<rattler_conda_types::ChannelUrl>,
        build_platform: PlatformWithVirtualPackages,
        host_platform: PlatformWithVirtualPackages,
        variant: BTreeMap<NormalizedKey, Variable>,
        directories: Directories,
    ) -> BuildConfiguration {
        BuildConfiguration {
            // TODO: NoArch??
            target_platform: Platform::NoArch,
            host_platform,
            build_platform,
            hash: HashInfo::from_variant(&variant, &recipe.build.noarch),
            variant,
            directories,
            channels,
            channel_priority: Default::default(),
            solve_strategy: Default::default(),
            timestamp: chrono::Utc::now(),
            subpackages: Default::default(), // TODO: ???
            packaging_settings: PackagingSettings::from_args(
                ArchiveType::Conda,
                CompressionLevel::default(),
            ),
            store_recipe: false,
            force_colors: true,
            sandbox_config: None,
        }
    }
}

#[cfg(test)]
mod tests {

    use std::{collections::BTreeMap, path::PathBuf};

    use pixi_build_backend::traits::RequirementsProvider;
    use pixi_build_type_conversions::to_project_model_v1;
    use pixi_manifest::Manifests;
    use rattler_build::{console_utils::LoggingOutputHandler, recipe::Recipe};
    use rattler_conda_types::{ChannelConfig, Platform};
    use tempfile::tempdir;

    use crate::{config::PythonBackendConfig, python::PythonBuildBackend};

    fn recipe(manifest_source: &str, config: PythonBackendConfig) -> Recipe {
        let tmp_dir = tempdir().unwrap();
        let tmp_manifest = tmp_dir.path().join("pixi.toml");
        std::fs::write(&tmp_manifest, manifest_source).unwrap();
        let manifest = Manifests::from_workspace_manifest_path(tmp_manifest.clone()).unwrap();
        let package = manifest.value.package.unwrap();
        let channel_config = ChannelConfig::default_with_root_dir(tmp_dir.path().to_path_buf());
        let project_model = to_project_model_v1(&package.value, &channel_config).unwrap();

        let python_backend = PythonBuildBackend::new(
            tmp_manifest,
            project_model,
            config,
            LoggingOutputHandler::default(),
            None,
        )
        .unwrap();

        python_backend
            .recipe(
                Platform::current(),
                &channel_config,
                false,
                &BTreeMap::new(),
            )
            .unwrap()
    }

    #[test]
    fn test_noarch_none() {
        insta::assert_yaml_snapshot!(recipe(r#"
        [workspace]
        platforms = []
        channels = []
        preview = ["pixi-build"]

        [package]
        name = "foobar"
        version = "0.1.0"

        [package.build]
        backend = { name = "pixi-build-python", version = "*" }
        "#, PythonBackendConfig {
            noarch: Some(false),
        }), {
            ".source[0].path" => "[ ... path ... ]",
            ".build.script" => "[ ... script ... ]",
        });
    }

    #[test]
    fn test_noarch_python() {
        insta::assert_yaml_snapshot!(recipe(r#"
        [workspace]
        platforms = []
        channels = []
        preview = ["pixi-build"]

        [package]
        name = "foobar"
        version = "0.1.0"

        [package.build]
        backend = { name = "pixi-build-python", version = "*" }
        "#, PythonBackendConfig::default()), {
            ".source[0].path" => "[ ... path ... ]",
            ".build.script" => "[ ... script ... ]",
        });
    }

    #[tokio::test]
    async fn test_setting_host_and_build_requirements() {
        let package_with_host_and_build_deps = r#"
        [workspace]
        name = "test-reqs"
        channels = ["conda-forge"]
        platforms = ["osx-arm64"]
        preview = ["pixi-build"]

        [package]
        name = "test-reqs"
        version = "1.2.3"

        [package.host-dependencies]
        hatchling = "*"

        [package.build-dependencies]
        boltons = "*"

        [package.run-dependencies]
        foobar = ">=3.2.1"

        [package.build]
        backend = { name = "pixi-build-python", version = "*" }
        "#;

        let tmp_dir = tempdir().unwrap();
        let tmp_manifest = tmp_dir.path().join("pixi.toml");

        // write the raw string into the file
        std::fs::write(&tmp_manifest, package_with_host_and_build_deps).unwrap();

        let manifest = Manifests::from_workspace_manifest_path(tmp_manifest.clone()).unwrap();
        let package = manifest.value.package.unwrap();
        let channel_config = ChannelConfig::default_with_root_dir(tmp_dir.path().to_path_buf());
        let project_model = to_project_model_v1(&package.value, &channel_config).unwrap();
        let python_backend = PythonBuildBackend::new(
            package.provenance.path,
            project_model,
            PythonBackendConfig::default(),
            LoggingOutputHandler::default(),
            None,
        )
        .unwrap();

        let channel_config = ChannelConfig::default_with_root_dir(PathBuf::new());

        let host_platform = Platform::current();
        let variant = BTreeMap::new();

        let reqs = python_backend
            .requirements(
                &python_backend.project_model,
                host_platform,
                &channel_config,
                &variant,
            )
            .unwrap();

        insta::assert_yaml_snapshot!(reqs);

        let recipe = python_backend.recipe(host_platform, &channel_config, false, &BTreeMap::new());
        insta::assert_yaml_snapshot!(recipe.unwrap(), {
            ".source[0].path" => "[ ... path ... ]",
            ".build.script" => "[ ... script ... ]",
        });
    }
}
