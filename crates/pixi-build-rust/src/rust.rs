use std::{collections::BTreeMap, path::PathBuf, str::FromStr};

use crate::{build_script::BuildScriptContext, config::RustBackendConfig};
use miette::IntoDiagnostic;
use pixi_build_backend::common::{PackageRequirements, SourceRequirements};
use pixi_build_backend::{
    ProjectModel,
    cache::{add_sccache, enable_sccache, sccache_tools},
    common::{BuildConfigurationParams, requirements},
    compilers::default_compiler,
    traits::project::new_spec,
};
use pixi_build_types::ProjectModelV1;
use rattler_build::metadata::Debug;
use rattler_build::recipe::Recipe;
use rattler_build::recipe::parser::BuildString;
use rattler_build::{
    NormalizedKey,
    console_utils::LoggingOutputHandler,
    hash::HashInfo,
    metadata::{BuildConfiguration, PackagingSettings},
};
use rattler_conda_types::Version;
use rattler_conda_types::{MatchSpec, NoArchType, PackageName, Platform, package::ArchiveType};
use rattler_package_streaming::write::CompressionLevel;
use recipe_stage0::recipe::{
    Build, ConditionalList, IntermediateRecipe, Item, Package, Source, Value,
};

pub struct RustBuildBackend<P: ProjectModel> {
    pub(crate) logging_output_handler: LoggingOutputHandler,
    pub(crate) manifest_path: PathBuf,
    pub(crate) manifest_root: PathBuf,
    pub(crate) project_model: P,
    pub(crate) config: RustBackendConfig,
    pub(crate) cache_dir: Option<PathBuf>,
}

impl<P: ProjectModel> RustBuildBackend<P> {
    /// Returns a new instance of [`RustBuildBackend`].
    pub fn new(
        manifest_path: PathBuf,
        project_model: P,
        config: RustBackendConfig,
        logging_output_handler: LoggingOutputHandler,
        cache_dir: Option<PathBuf>,
    ) -> miette::Result<Self> {
        // Determine the root directory of the manifest
        let manifest_root = manifest_path
            .parent()
            .ok_or_else(|| miette::miette!("the project manifest must reside in a directory"))?
            .to_path_buf();

        Ok(Self {
            manifest_path: manifest_path.to_path_buf(),
            manifest_root,
            project_model,
            config,
            logging_output_handler,
            cache_dir,
        })
    }

    /// Returns the matchspecs for the compiler packages.
    /// That should be included in the build section of the recipe.
    /// TODO: Should we also take into account other compilers like
    /// c or cxx?
    fn compiler_packages(&self, target_platform: Platform) -> Vec<MatchSpec> {
        let mut compilers = vec![];

        if let Some(name) = default_compiler(target_platform, "rust") {
            // TODO: Read this from variants
            // TODO: Read the version specification from variants
            let compiler_package = PackageName::new_unchecked(format!("{name}_{target_platform}"));
            compilers.push(MatchSpec::from(compiler_package));
        }

        compilers
    }

    /// Constructs a [`Recipe`] that will build the Rust package into a conda
    /// package.
    pub(crate) fn recipe(
        &self,
        host_platform: Platform,
        variant: &BTreeMap<NormalizedKey, rattler_build::recipe::variable::Variable>,
    ) -> miette::Result<(Recipe, SourceRequirements<P>)> {
        // Parse the package name and version from the manifest
        let name = PackageName::from_str(self.project_model.name()).into_diagnostic()?;
        let version = self.project_model.version().clone().ok_or_else(|| {
            miette::miette!("a version is missing from the package but it is required")
        })?;

        let noarch_type = NoArchType::none();

        let (has_sccache, requirements) = self.requirements(host_platform, variant)?;

        let has_openssl = self
            .project_model
            .dependencies(Some(host_platform))
            .contains(&"openssl".into());

        let build_number = 0;

        let build_script = BuildScriptContext {
            source_dir: self.manifest_root.display().to_string(),
            extra_args: self.config.extra_args.clone(),
            has_openssl,
            has_sccache,
            is_bash: !Platform::current().is_windows(),
        }
        .render();

        let hash_info = HashInfo::from_variant(variant, &noarch_type);

        Ok((
            Recipe {
                schema_version: 1,
                package: rattler_build::recipe::parser::Package {
                    version: version.into(),
                    name,
                },
                context: Default::default(),
                cache: None,
                // Sometimes Rust projects are part of a workspace, so we need to
                // include the entire source project and set the source directory
                // to the root of the package.
                source: vec![],
                build: rattler_build::recipe::parser::Build {
                    number: build_number,
                    string: BuildString::Resolved(BuildString::compute(&hash_info, build_number)),
                    script: rattler_build::recipe::parser::Script {
                        content: rattler_build::recipe::parser::ScriptContent::Commands(
                            build_script,
                        ),
                        env: self.config.env.clone(),
                        ..Default::default()
                    },
                    noarch: noarch_type,
                    ..rattler_build::recipe::parser::Build::default()
                },
                requirements: requirements.requirements,
                tests: vec![],
                about: Default::default(),
                extra: Default::default(),
            },
            requirements.source,
        ))
    }

    pub(crate) fn requirements(
        &self,
        host_platform: Platform,
        variant: &BTreeMap<NormalizedKey, rattler_build::recipe::variable::Variable>,
    ) -> miette::Result<(bool, PackageRequirements<P>)> {
        let project_model = &self.project_model;
        let mut sccache_enabled = false;

        let mut dependencies = project_model.dependencies(Some(host_platform));

        let empty_spec = new_spec::<P>();

        let cache_tools = sccache_tools();

        if enable_sccache(std::env::vars().collect()) {
            sccache_enabled = true;
            add_sccache::<P>(&mut dependencies, &cache_tools, &empty_spec);
        }

        let mut package_requirements = requirements::<P>(dependencies, variant)?;

        package_requirements.requirements.build.extend(
            self.compiler_packages(host_platform)
                .into_iter()
                .map(rattler_build::recipe::parser::Dependency::Spec),
        );

        Ok((sccache_enabled, package_requirements))
    }
}

/// Construct a build configuration for the given recipe and parameters.
pub(crate) fn construct_configuration(
    recipe: &Recipe,
    params: BuildConfigurationParams,
) -> BuildConfiguration {
    BuildConfiguration {
        target_platform: params.host_platform.platform,
        host_platform: params.host_platform,
        build_platform: params.build_platform,
        hash: HashInfo::from_variant(&params.variant, &recipe.build.noarch),
        variant: params.variant,
        directories: params.directories,
        channels: params.channels,
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
        debug: Debug::new(false),
    }
}

#[cfg(test)]
mod tests {

    use std::{collections::BTreeMap, path::PathBuf};

    use indexmap::IndexMap;

    use pixi_build_type_conversions::to_project_model_v1;

    use pixi_build_types::ProjectModelV1;
    use pixi_manifest::Manifests;
    use rattler_build::{
        console_utils::LoggingOutputHandler,
        recipe::{Recipe, parser::find_outputs_from_src},
        source_code::Source,
    };
    use rattler_conda_types::{ChannelConfig, Platform};
    use recipe_stage0::recipe::IntermediateRecipe;
    use tempfile::{tempdir, tempfile};

    use crate::{
        build_script::BuildScriptContext, config::RustBackendConfig, rust::RustBuildBackend,
    };

    fn project_model_v1(
        manifest_source: &str,
        _config: RustBackendConfig,
    ) -> (ProjectModelV1, PathBuf) {
        let tmp_dir = tempdir().unwrap();
        let tmp_manifest = tmp_dir.path().join("pixi.toml");
        std::fs::write(&tmp_manifest, manifest_source).unwrap();
        let manifest = Manifests::from_workspace_manifest_path(tmp_manifest.clone()).unwrap();
        let package = manifest.value.package.unwrap();
        let channel_config = ChannelConfig::default_with_root_dir(tmp_dir.path().to_path_buf());
        let project_model = to_project_model_v1(&package.value, &channel_config).unwrap();

        (project_model, tmp_manifest)
    }

    fn recipe(manifest_source: &str, config: RustBackendConfig) -> Recipe {
        let tmp_dir = tempdir().unwrap();
        let tmp_manifest = tmp_dir.path().join("pixi.toml");
        std::fs::write(&tmp_manifest, manifest_source).unwrap();
        let manifest = Manifests::from_workspace_manifest_path(tmp_manifest.clone()).unwrap();
        let package = manifest.value.package.unwrap();
        let channel_config = ChannelConfig::default_with_root_dir(tmp_dir.path().to_path_buf());
        let project_model = to_project_model_v1(&package.value, &channel_config).unwrap();

        let rust_backend = RustBuildBackend::new(
            tmp_manifest,
            project_model,
            config,
            LoggingOutputHandler::default(),
            None,
        )
        .unwrap();

        let (recipe, _) = rust_backend
            .recipe(Platform::current(), &BTreeMap::new())
            .unwrap();

        recipe
    }

    /// Im the next iterations this will be the public interface for the GenerateRecipe trait
    pub fn generate_recipe(
        model: ProjectModelV1,
        manifest_root: PathBuf,
        config: RustBackendConfig,
    ) -> IntermediateRecipe {
        let mut ir = IntermediateRecipe::from_model(model, manifest_root.clone());

        let build_script = BuildScriptContext {
            source_dir: manifest_root.display().to_string(),
            extra_args: config.extra_args.clone(),
            has_openssl: false,
            has_sccache: false,
            is_bash: !Platform::current().is_windows(),
        }
        .render();

        ir.build.script = build_script;

        ir
    }

    #[test]
    fn test_rust_is_in_build_requirements() {
        insta::assert_yaml_snapshot!(recipe(r#"
        [workspace]
        platforms = []
        channels = []
        preview = ["pixi-build"]

        [package]
        name = "foobar"
        version = "0.1.0"

        [package.build]
        backend = { name = "pixi-build-rust", version = "*" }
        "#, RustBackendConfig::default()), {
        ".source[0].path" => "[ ... path ... ]",
        ".build.script" => "[ ... script ... ]",
        ".requirements.build[0]" => insta::dynamic_redaction(|value, _path| {
            // assert that the value looks like a uuid here
            assert!(value.as_str().unwrap().contains("rust"));
        }),
        });
    }

    #[test]
    fn test_env_vars_are_set() {
        let manifest_source = r#"
        [workspace]
        platforms = []
        channels = []
        preview = ["pixi-build"]

        [package]
        name = "foobar"
        version = "0.1.0"

        [package.build]
        backend = { name = "pixi-build-rust", version = "*" }
        "#;

        let env = IndexMap::from([("foo".to_string(), "bar".to_string())]);

        let recipe = recipe(
            manifest_source,
            RustBackendConfig {
                env: env.clone(),
                ..Default::default()
            },
        );

        assert_eq!(recipe.build.script.env, env);
    }

    #[test]
    fn generate_rust_recipe_from_project_model() {
        // take the pixi-build-examples rust package as an example
        // and generate intermediate recipe from it.
        // by converting it to recipe.yaml we can validate the structure
        let manifest_source = r#"
        [workspace]
        authors = ["Wolf Vollprecht <wolfv@prefix.dev>"]
        channels = ["https://prefix.dev/conda-forge"]
        description = "Showcases how to build a Rust project with pixi"
        name = "build-rust"
        platforms = ["win-64", "linux-64", "osx-64", "osx-arm64"]
        preview = ["pixi-build"]
        version = "0.1.0"

        [package]
        name = "build-rust"
        version = "0.1.0"

        [package.build-dependencies]
        boltons = "*"

        [package.target.unix.build-dependencies]
        rich = "*"

        [dependencies]
        build-rust = { path = "." }

        [package.build]
        backend = { name = "pixi-build-rust", version = "*" }
        channels = ["https://prefix.dev/pixi-build-backends", "https://prefix.dev/conda-forge"]
        "#;

        let (project_model, manifest_path) =
            project_model_v1(manifest_source, RustBackendConfig::default());

        let ir = generate_recipe(
            project_model,
            manifest_path.parent().unwrap().to_path_buf(),
            RustBackendConfig::default(),
        );

        insta::assert_yaml_snapshot!(ir, {
            ".source[0].path" => "[ ... path ... ]",
            ".build.script" => "[ ... script ... ]",
        }
        );
    }
}
