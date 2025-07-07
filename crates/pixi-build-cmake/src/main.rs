mod build_script;
mod config;

use std::path::Path;

use build_script::{BuildPlatform, BuildScriptContext};
use config::CMakeBackendConfig;
use miette::IntoDiagnostic;
use pixi_build_backend::{
    compilers::{Language, compiler_requirement, default_compiler},
    generated_recipe::{GenerateRecipe, GeneratedRecipe},
    intermediate_backend::IntermediateBackendInstantiator,
};
use rattler_conda_types::{MatchSpec, PackageName, ParseStrictness, Platform};
use recipe_stage0::{matchspec::PackageDependency, recipe::Script};

#[derive(Default, Clone)]
pub struct CMakeGenerator {}

impl GenerateRecipe for CMakeGenerator {
    type Config = CMakeBackendConfig;

    fn generate_recipe(
        &self,
        model: &pixi_build_types::ProjectModelV1,
        config: &Self::Config,
        manifest_root: std::path::PathBuf,
        host_platform: rattler_conda_types::Platform,
    ) -> miette::Result<GeneratedRecipe> {
        let mut generated_recipe =
            GeneratedRecipe::from_model(model.clone(), manifest_root.clone());

        // we need to add compilers

        let requirements = &mut generated_recipe.recipe.requirements;

        let resolved_requirements = requirements.resolve(Some(&host_platform));

        // Ensure the compiler function is added to the build requirements
        // only if a specific compiler is not already present.
        // TODO: Correctly, we should ask cmake to give us the language used in the project
        // instead of assuming C++.
        let language_compiler = default_compiler(&host_platform, &Language::Cxx.to_string());

        let build_platform = Platform::current();

        if !resolved_requirements
            .build
            .contains_key(&PackageName::new_unchecked(language_compiler))
        {
            if build_platform.is_windows() {
                // on windows, we use newer vs2019 compiler

                if !resolved_requirements
                    .build
                    .contains_key(&PackageName::new_unchecked("vs2017"))
                    && !resolved_requirements
                        .build
                        .contains_key(&PackageName::new_unchecked("vs2019"))
                {
                    let vss = MatchSpec::from_str("vs2019", ParseStrictness::Strict)
                        .expect("Failed to parse vs2019 match spec");
                    requirements
                        .build
                        .push(PackageDependency::Binary(vss).into());
                }
            } else {
                // otherwise we default to cmpi
                requirements
                    .build
                    .push(compiler_requirement(&Language::Cxx));
            }
        }

        // add necessary build tools
        for tool in ["cmake", "ninja"] {
            let tool_name = PackageName::new_unchecked(tool);
            if !resolved_requirements.build.contains_key(&tool_name) {
                requirements.build.push(tool.parse().into_diagnostic()?);
            }
        }

        // Check if the host platform has a host python dependency
        // This is used to determine if we need to the cmake argument for the python executable
        let has_host_python = resolved_requirements.contains(&PackageName::new_unchecked("python"));

        let build_script = BuildScriptContext {
            build_platform: if build_platform.is_windows() {
                BuildPlatform::Windows
            } else {
                BuildPlatform::Unix
            },
            source_dir: manifest_root.display().to_string(),
            extra_args: config.extra_args.clone(),
            has_host_python,
        }
        .render();

        generated_recipe.recipe.build.script = Script {
            content: build_script,
            env: config.env.clone(),
            ..Default::default()
        };

        Ok(generated_recipe)
    }

    fn build_input_globs(config: &Self::Config, _workdir: impl AsRef<Path>) -> Vec<String> {
        [
            // Source files
            "**/*.{c,cc,cxx,cpp,h,hpp,hxx}",
            // CMake files
            "**/*.{cmake,cmake.in}",
            "**/CMakeFiles.txt",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain(config.extra_input_globs.clone())
        .collect()
    }
}

#[tokio::main]
pub async fn main() {
    if let Err(err) =
        pixi_build_backend::cli::main(IntermediateBackendInstantiator::<CMakeGenerator>::new).await
    {
        eprintln!("{err:?}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    use indexmap::IndexMap;
    use pixi_build_types::ProjectModelV1;

    #[test]
    fn test_input_globs_includes_extra_globs() {
        let config = CMakeBackendConfig {
            extra_input_globs: vec!["custom/*.c".to_string()],
            ..Default::default()
        };

        let result = CMakeGenerator::build_input_globs(&config, PathBuf::new());

        insta::assert_debug_snapshot!(result);
    }

    #[macro_export]
    macro_rules! project_fixture {
        ($($json:tt)+) => {
            serde_json::from_value::<ProjectModelV1>(
                serde_json::json!($($json)+)
            ).expect("Failed to create TestProjectModel from JSON fixture.")
        };
    }

    #[test]
    fn test_cxx_is_in_build_requirements() {
        let project_model = project_fixture!({
            "name": "foobar",
            "version": "0.1.0",
            "targets": {
                "defaultTarget": {
                    "runDependencies": {
                        "boltons": {
                            "binary": {
                                "version": "*"
                            }
                        }
                    }
                },
            }
        });

        let generated_recipe = CMakeGenerator::default()
            .generate_recipe(
                &project_model,
                &CMakeBackendConfig::default(),
                PathBuf::from("."),
                Platform::Linux64,
            )
            .expect("Failed to generate recipe");

        insta::assert_yaml_snapshot!(generated_recipe.recipe, {
        ".source[0].path" => "[ ... path ... ]",
        ".build.script" => "[ ... script ... ]",
        });
    }

    #[test]
    fn test_env_vars_are_set() {
        let project_model = project_fixture!({
            "name": "foobar",
            "version": "0.1.0",
            "targets": {
                "defaultTarget": {
                    "runDependencies": {
                        "boltons": {
                            "binary": {
                                "version": "*"
                            }
                        }
                    }
                },
            }
        });

        let env = IndexMap::from([("foo".to_string(), "bar".to_string())]);

        let generated_recipe = CMakeGenerator::default()
            .generate_recipe(
                &project_model,
                &CMakeBackendConfig {
                    env: env.clone(),
                    ..Default::default()
                },
                PathBuf::from("."),
                Platform::Linux64,
            )
            .expect("Failed to generate recipe");

        insta::assert_yaml_snapshot!(generated_recipe.recipe.build.script,
        {
            ".content" => "[ ... script ... ]",
        });
    }

    #[test]
    fn test_has_python_is_set_in_build_script() {
        let project_model = project_fixture!({
            "name": "foobar",
            "version": "0.1.0",
            "targets": {
                "defaultTarget": {
                    "runDependencies": {
                        "boltons": {
                            "binary": {
                                "version": "*"
                            }
                        }
                    },
                    "hostDependencies": {
                        "python": {
                            "binary": {
                                "version": "*"
                            }
                        }
                    }
                },
            }
        });

        let generated_recipe = CMakeGenerator::default()
            .generate_recipe(
                &project_model,
                &CMakeBackendConfig::default(),
                PathBuf::from("."),
                Platform::Linux64,
            )
            .expect("Failed to generate recipe");

        // we want to check that
        // -DPython_EXECUTABLE=$PYTHON is set in the build script
        insta::assert_yaml_snapshot!(generated_recipe.recipe.build,

            {
            ".script.content" => insta::dynamic_redaction(|value, _path| {
                dbg!(&value);
                // assert that the value looks like a uuid here
                assert!(value
                    .as_slice()
                    .unwrap()
                    .iter()
                    .any(|c| c.as_str().unwrap().contains("-DPython_EXECUTABLE"))
                );
                "[content]"
            })
        });
    }

    #[test]
    fn test_cxx_is_not_added_if_gcc_is_already_present() {
        let project_model = project_fixture!({
            "name": "foobar",
            "version": "0.1.0",
            "targets": {
                "defaultTarget": {
                    "runDependencies": {
                        "boltons": {
                            "binary": {
                                "version": "*"
                            }
                        }
                    },
                    "buildDependencies": {
                        "gxx": {
                            "binary": {
                                "version": "*"
                            }
                        }
                    }
                },
            }
        });

        let generated_recipe = CMakeGenerator::default()
            .generate_recipe(
                &project_model,
                &CMakeBackendConfig::default(),
                PathBuf::from("."),
                Platform::Linux64,
            )
            .expect("Failed to generate recipe");

        insta::assert_yaml_snapshot!(generated_recipe.recipe, {
        ".source[0].path" => "[ ... path ... ]",
        ".build.script" => "[ ... script ... ]",
        });
    }
}
