use std::{path::Path, sync::Arc};

use miette::IntoDiagnostic;
use rattler_conda_types::{Channel, MatchSpec, NamelessMatchSpec, PackageName};

use pixi_build_types::{self as pbt};

use crate::dependencies::resolve_path;

/// Get the * version for the version type, that is currently being used
pub trait AnyVersion {
    fn any() -> Self;
}

/// Convert a binary spec to a nameless match spec
pub trait BinarySpecExt {
    fn to_nameless(&self) -> NamelessMatchSpec;
}

/// A trait that define the package spec interface
pub trait PackageSpec {
    /// Returns true if the specified [`PackageSpec`] is a valid variant spec.
    fn can_be_used_as_variant(&self) -> bool;

    /// Converts the package spec to a match spec.
    fn to_match_spec(
        &self,
        name: PackageName,
        root_dir: &Path,
        ignore_self: bool,
    ) -> miette::Result<MatchSpec>;
}

impl PackageSpec for pbt::PackageSpecV1 {
    fn can_be_used_as_variant(&self) -> bool {
        match self {
            pbt::PackageSpecV1::Binary(boxed_spec) => {
                let pbt::BinaryPackageSpecV1 {
                    version,
                    build,
                    build_number,
                    file_name,
                    channel,
                    subdir,
                    md5,
                    sha256,
                } = &**boxed_spec;

                version == &Some(rattler_conda_types::VersionSpec::Any)
                    && build.is_none()
                    && build_number.is_none()
                    && file_name.is_none()
                    && channel.is_none()
                    && subdir.is_none()
                    && md5.is_none()
                    && sha256.is_none()
            }
            _ => false,
        }
    }

    fn to_match_spec(
        &self,
        name: PackageName,
        root_dir: &Path,
        ignore_self: bool,
    ) -> miette::Result<MatchSpec> {
        match self {
            pbt::PackageSpecV1::Binary(binary_spec) => {
                if binary_spec.version == Some("*".parse().unwrap()) {
                    // Skip dependencies with wildcard versions.
                    name.as_normalized()
                        .to_string()
                        .parse::<MatchSpec>()
                        .into_diagnostic()
                } else {
                    Ok(MatchSpec::from_nameless(
                        binary_spec.to_nameless(),
                        Some(name),
                    ))
                }
            }
            pbt::PackageSpecV1::Source(source_spec) => match source_spec {
                pbt::SourcePackageSpecV1::Path(path) => {
                    let path = resolve_path(Path::new(&path.path), root_dir).ok_or_else(|| {
                        miette::miette!("failed to resolve home dir for: {}", path.path)
                    })?;

                    if ignore_self && path.as_path() == root_dir {
                        // Skip source dependencies that point to the root directory.
                        Err(miette::miette!("Skipping self-referencing dependency"))
                    } else {
                        // All other source dependencies are not yet supported.
                        Err(miette::miette!(
                            "recursive source dependencies are not yet supported"
                        ))
                    }
                }
                _ => Err(miette::miette!(
                    "recursive source dependencies are not yet supported"
                )),
            },
        }
    }
}

impl AnyVersion for pbt::PackageSpecV1 {
    fn any() -> Self {
        pbt::PackageSpecV1::Binary(Box::new(rattler_conda_types::VersionSpec::Any.into()))
    }
}

impl BinarySpecExt for pbt::BinaryPackageSpecV1 {
    fn to_nameless(&self) -> NamelessMatchSpec {
        NamelessMatchSpec {
            version: self.version.clone(),
            build: self.build.clone(),
            build_number: self.build_number.clone(),
            file_name: self.file_name.clone(),
            channel: self
                .channel
                .as_ref()
                .map(|url| Arc::new(Channel::from_url(url.clone()))),
            subdir: self.subdir.clone(),
            md5: self.md5.as_ref().map(|m| m.0),
            sha256: self.sha256.as_ref().map(|s| s.0),
            namespace: None,
            url: None,
            extras: None,
        }
    }
}
