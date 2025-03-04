// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use deno_ast::ModuleSpecifier;
use deno_core::anyhow::bail;
use deno_core::anyhow::Context;
use deno_core::error::custom_error;
use deno_core::error::AnyError;
use deno_core::parking_lot::Mutex;
use deno_core::url::Url;
use deno_npm::registry::NpmPackageVersionDistInfo;
use deno_npm::NpmPackageCacheFolderId;
use deno_semver::npm::NpmPackageNv;
use deno_semver::Version;
use once_cell::sync::Lazy;

use crate::args::CacheSetting;
use crate::http_util::HttpClient;
use crate::util::fs::canonicalize_path;
use crate::util::fs::hard_link_dir_recursive;
use crate::util::path::root_url_to_safe_local_dirname;
use crate::util::progress_bar::ProgressBar;

use super::tarball::verify_and_extract_tarball;

static SHOULD_SYNC_DOWNLOAD: Lazy<bool> =
  Lazy::new(|| std::env::var("DENO_UNSTABLE_NPM_SYNC_DOWNLOAD").is_ok());

/// For some of the tests, we want downloading of packages
/// to be deterministic so that the output is always the same
pub fn should_sync_download() -> bool {
  // this gets called a lot when doing npm resolution and was taking
  // a significant amount of time, so cache it in a lazy
  *SHOULD_SYNC_DOWNLOAD
}

const NPM_PACKAGE_SYNC_LOCK_FILENAME: &str = ".deno_sync_lock";

pub fn with_folder_sync_lock(
  package: &NpmPackageNv,
  output_folder: &Path,
  action: impl FnOnce() -> Result<(), AnyError>,
) -> Result<(), AnyError> {
  fn inner(
    output_folder: &Path,
    action: impl FnOnce() -> Result<(), AnyError>,
  ) -> Result<(), AnyError> {
    fs::create_dir_all(output_folder).with_context(|| {
      format!("Error creating '{}'.", output_folder.display())
    })?;

    // This sync lock file is a way to ensure that partially created
    // npm package directories aren't considered valid. This could maybe
    // be a bit smarter in the future to not bother extracting here
    // if another process has taken the lock in the past X seconds and
    // wait for the other process to finish (it could try to create the
    // file with `create_new(true)` then if it exists, check the metadata
    // then wait until the other process finishes with a timeout), but
    // for now this is good enough.
    let sync_lock_path = output_folder.join(NPM_PACKAGE_SYNC_LOCK_FILENAME);
    match fs::OpenOptions::new()
      .write(true)
      .create(true)
      .open(&sync_lock_path)
    {
      Ok(_) => {
        action()?;
        // extraction succeeded, so only now delete this file
        let _ignore = std::fs::remove_file(&sync_lock_path);
        Ok(())
      }
      Err(err) => {
        bail!(
          concat!(
            "Error creating package sync lock file at '{}'. ",
            "Maybe try manually deleting this folder.\n\n{:#}",
          ),
          output_folder.display(),
          err
        );
      }
    }
  }

  match inner(output_folder, action) {
    Ok(()) => Ok(()),
    Err(err) => {
      if let Err(remove_err) = fs::remove_dir_all(output_folder) {
        if remove_err.kind() != std::io::ErrorKind::NotFound {
          bail!(
            concat!(
              "Failed setting up package cache directory for {}, then ",
              "failed cleaning it up.\n\nOriginal error:\n\n{}\n\n",
              "Remove error:\n\n{}\n\nPlease manually ",
              "delete this folder or you will run into issues using this ",
              "package in the future:\n\n{}"
            ),
            package,
            err,
            remove_err,
            output_folder.display(),
          );
        }
      }
      Err(err)
    }
  }
}

#[derive(Clone, Debug)]
pub struct ReadonlyNpmCache {
  root_dir: PathBuf,
  // cached url representation of the root directory
  root_dir_url: Url,
}

impl ReadonlyNpmCache {
  pub fn new(root_dir: PathBuf) -> Self {
    fn try_get_canonicalized_root_dir(
      root_dir: &Path,
    ) -> Result<PathBuf, AnyError> {
      if !root_dir.exists() {
        std::fs::create_dir_all(root_dir)
          .with_context(|| format!("Error creating {}", root_dir.display()))?;
      }
      Ok(canonicalize_path(root_dir)?)
    }

    // this may fail on readonly file systems, so just ignore if so
    let root_dir =
      try_get_canonicalized_root_dir(&root_dir).unwrap_or(root_dir);
    let root_dir_url = Url::from_directory_path(&root_dir).unwrap();
    Self {
      root_dir,
      root_dir_url,
    }
  }

  pub fn root_dir_url(&self) -> &Url {
    &self.root_dir_url
  }

  pub fn package_folder_for_id(
    &self,
    folder_id: &NpmPackageCacheFolderId,
    registry_url: &Url,
  ) -> PathBuf {
    if folder_id.copy_index == 0 {
      self.package_folder_for_name_and_version(&folder_id.nv, registry_url)
    } else {
      self
        .package_name_folder(&folder_id.nv.name, registry_url)
        .join(format!("{}_{}", folder_id.nv.version, folder_id.copy_index))
    }
  }

  pub fn package_folder_for_name_and_version(
    &self,
    package: &NpmPackageNv,
    registry_url: &Url,
  ) -> PathBuf {
    self
      .package_name_folder(&package.name, registry_url)
      .join(package.version.to_string())
  }

  pub fn package_name_folder(&self, name: &str, registry_url: &Url) -> PathBuf {
    let mut dir = self.registry_folder(registry_url);
    if name.to_lowercase() != name {
      let encoded_name = mixed_case_package_name_encode(name);
      // Using the encoded directory may have a collision with an actual package name
      // so prefix it with an underscore since npm packages can't start with that
      dir.join(format!("_{encoded_name}"))
    } else {
      // ensure backslashes are used on windows
      for part in name.split('/') {
        dir = dir.join(part);
      }
      dir
    }
  }

  pub fn registry_folder(&self, registry_url: &Url) -> PathBuf {
    self
      .root_dir
      .join(root_url_to_safe_local_dirname(registry_url))
  }

  pub fn resolve_package_folder_id_from_specifier(
    &self,
    specifier: &ModuleSpecifier,
    registry_url: &Url,
  ) -> Result<NpmPackageCacheFolderId, AnyError> {
    match self
      .maybe_resolve_package_folder_id_from_specifier(specifier, registry_url)
    {
      Some(id) => Ok(id),
      None => bail!("could not find npm package for '{}'", specifier),
    }
  }

  fn maybe_resolve_package_folder_id_from_specifier(
    &self,
    specifier: &ModuleSpecifier,
    registry_url: &Url,
  ) -> Option<NpmPackageCacheFolderId> {
    let registry_root_dir = self
      .root_dir_url
      .join(&format!(
        "{}/",
        root_url_to_safe_local_dirname(registry_url)
          .to_string_lossy()
          .replace('\\', "/")
      ))
      // this not succeeding indicates a fatal issue, so unwrap
      .unwrap();
    let mut relative_url = registry_root_dir.make_relative(specifier)?;
    if relative_url.starts_with("../") {
      return None;
    }

    // base32 decode the url if it starts with an underscore
    // * Ex. _{base32(package_name)}/
    if let Some(end_url) = relative_url.strip_prefix('_') {
      let mut parts = end_url
        .split('/')
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
      match mixed_case_package_name_decode(&parts[0]) {
        Some(part) => {
          parts[0] = part;
        }
        None => return None,
      }
      relative_url = parts.join("/");
    }

    // examples:
    // * chalk/5.0.1/
    // * @types/chalk/5.0.1/
    // * some-package/5.0.1_1/ -- where the `_1` (/_\d+/) is a copy of the folder for peer deps
    let is_scoped_package = relative_url.starts_with('@');
    let mut parts = relative_url
      .split('/')
      .enumerate()
      .take(if is_scoped_package { 3 } else { 2 })
      .map(|(_, part)| part)
      .collect::<Vec<_>>();
    if parts.len() < 2 {
      return None;
    }
    let version_part = parts.pop().unwrap();
    let name = parts.join("/");
    let (version, copy_index) =
      if let Some((version, copy_count)) = version_part.split_once('_') {
        (version, copy_count.parse::<u8>().ok()?)
      } else {
        (version_part, 0)
      };
    Some(NpmPackageCacheFolderId {
      nv: NpmPackageNv {
        name,
        version: Version::parse_from_npm(version).ok()?,
      },
      copy_index,
    })
  }

  pub fn get_cache_location(&self) -> PathBuf {
    self.root_dir.clone()
  }
}

/// Stores a single copy of npm packages in a cache.
#[derive(Debug)]
pub struct NpmCache {
  readonly: ReadonlyNpmCache,
  cache_setting: CacheSetting,
  http_client: Arc<HttpClient>,
  progress_bar: ProgressBar,
  /// ensures a package is only downloaded once per run
  previously_reloaded_packages: Mutex<HashSet<NpmPackageNv>>,
}

impl NpmCache {
  pub fn new(
    cache_dir_path: PathBuf,
    cache_setting: CacheSetting,
    http_client: Arc<HttpClient>,
    progress_bar: ProgressBar,
  ) -> Self {
    Self {
      readonly: ReadonlyNpmCache::new(cache_dir_path),
      cache_setting,
      http_client,
      progress_bar,
      previously_reloaded_packages: Default::default(),
    }
  }

  pub fn as_readonly(&self) -> ReadonlyNpmCache {
    self.readonly.clone()
  }

  pub fn cache_setting(&self) -> &CacheSetting {
    &self.cache_setting
  }

  pub fn root_dir_url(&self) -> &Url {
    self.readonly.root_dir_url()
  }

  /// Checks if the cache should be used for the provided name and version.
  /// NOTE: Subsequent calls for the same package will always return `true`
  /// to ensure a package is only downloaded once per run of the CLI. This
  /// prevents downloads from re-occurring when someone has `--reload` and
  /// and imports a dynamic import that imports the same package again for example.
  fn should_use_global_cache_for_package(
    &self,
    package: &NpmPackageNv,
  ) -> bool {
    self.cache_setting.should_use_for_npm_package(&package.name)
      || !self
        .previously_reloaded_packages
        .lock()
        .insert(package.clone())
  }

  pub async fn ensure_package(
    &self,
    package: &NpmPackageNv,
    dist: &NpmPackageVersionDistInfo,
    registry_url: &Url,
  ) -> Result<(), AnyError> {
    self
      .ensure_package_inner(package, dist, registry_url)
      .await
      .with_context(|| format!("Failed caching npm package '{package}'."))
  }

  async fn ensure_package_inner(
    &self,
    package: &NpmPackageNv,
    dist: &NpmPackageVersionDistInfo,
    registry_url: &Url,
  ) -> Result<(), AnyError> {
    let package_folder = self
      .readonly
      .package_folder_for_name_and_version(package, registry_url);
    if self.should_use_global_cache_for_package(package)
      && package_folder.exists()
      // if this file exists, then the package didn't successfully extract
      // the first time, or another process is currently extracting the zip file
      && !package_folder.join(NPM_PACKAGE_SYNC_LOCK_FILENAME).exists()
    {
      return Ok(());
    } else if self.cache_setting == CacheSetting::Only {
      return Err(custom_error(
        "NotCached",
        format!(
          "An npm specifier not found in cache: \"{}\", --cached-only is specified.",
          &package.name
        )
      )
      );
    }

    let guard = self.progress_bar.update(&dist.tarball);
    let maybe_bytes = self
      .http_client
      .download_with_progress(&dist.tarball, &guard)
      .await?;
    match maybe_bytes {
      Some(bytes) => {
        verify_and_extract_tarball(package, &bytes, dist, &package_folder)
      }
      None => {
        bail!("Could not find npm package tarball at: {}", dist.tarball);
      }
    }
  }

  /// Ensures a copy of the package exists in the global cache.
  ///
  /// This assumes that the original package folder being hard linked
  /// from exists before this is called.
  pub fn ensure_copy_package(
    &self,
    folder_id: &NpmPackageCacheFolderId,
    registry_url: &Url,
  ) -> Result<(), AnyError> {
    assert_ne!(folder_id.copy_index, 0);
    let package_folder =
      self.readonly.package_folder_for_id(folder_id, registry_url);

    if package_folder.exists()
      // if this file exists, then the package didn't successfully extract
      // the first time, or another process is currently extracting the zip file
      && !package_folder.join(NPM_PACKAGE_SYNC_LOCK_FILENAME).exists()
      && self.cache_setting.should_use_for_npm_package(&folder_id.nv.name)
    {
      return Ok(());
    }

    let original_package_folder = self
      .readonly
      .package_folder_for_name_and_version(&folder_id.nv, registry_url);
    with_folder_sync_lock(&folder_id.nv, &package_folder, || {
      hard_link_dir_recursive(&original_package_folder, &package_folder)
    })?;
    Ok(())
  }

  pub fn package_folder_for_id(
    &self,
    id: &NpmPackageCacheFolderId,
    registry_url: &Url,
  ) -> PathBuf {
    self.readonly.package_folder_for_id(id, registry_url)
  }

  pub fn package_folder_for_name_and_version(
    &self,
    package: &NpmPackageNv,
    registry_url: &Url,
  ) -> PathBuf {
    self
      .readonly
      .package_folder_for_name_and_version(package, registry_url)
  }

  pub fn package_name_folder(&self, name: &str, registry_url: &Url) -> PathBuf {
    self.readonly.package_name_folder(name, registry_url)
  }

  pub fn registry_folder(&self, registry_url: &Url) -> PathBuf {
    self.readonly.registry_folder(registry_url)
  }

  pub fn resolve_package_folder_id_from_specifier(
    &self,
    specifier: &ModuleSpecifier,
    registry_url: &Url,
  ) -> Result<NpmPackageCacheFolderId, AnyError> {
    self
      .readonly
      .resolve_package_folder_id_from_specifier(specifier, registry_url)
  }
}

pub fn mixed_case_package_name_encode(name: &str) -> String {
  // use base32 encoding because it's reversable and the character set
  // only includes the characters within 0-9 and A-Z so it can be lower cased
  base32::encode(
    base32::Alphabet::RFC4648 { padding: false },
    name.as_bytes(),
  )
  .to_lowercase()
}

pub fn mixed_case_package_name_decode(name: &str) -> Option<String> {
  base32::decode(base32::Alphabet::RFC4648 { padding: false }, name)
    .and_then(|b| String::from_utf8(b).ok())
}

#[cfg(test)]
mod test {
  use deno_core::url::Url;
  use deno_semver::npm::NpmPackageNv;
  use deno_semver::Version;

  use super::ReadonlyNpmCache;
  use crate::npm::cache::NpmPackageCacheFolderId;

  #[test]
  fn should_get_package_folder() {
    let deno_dir = crate::cache::DenoDir::new(None).unwrap();
    let root_dir = deno_dir.npm_folder_path();
    let cache = ReadonlyNpmCache::new(root_dir.clone());
    let registry_url = Url::parse("https://registry.npmjs.org/").unwrap();

    assert_eq!(
      cache.package_folder_for_id(
        &NpmPackageCacheFolderId {
          nv: NpmPackageNv {
            name: "json".to_string(),
            version: Version::parse_from_npm("1.2.5").unwrap(),
          },
          copy_index: 0,
        },
        &registry_url,
      ),
      root_dir
        .join("registry.npmjs.org")
        .join("json")
        .join("1.2.5"),
    );

    assert_eq!(
      cache.package_folder_for_id(
        &NpmPackageCacheFolderId {
          nv: NpmPackageNv {
            name: "json".to_string(),
            version: Version::parse_from_npm("1.2.5").unwrap(),
          },
          copy_index: 1,
        },
        &registry_url,
      ),
      root_dir
        .join("registry.npmjs.org")
        .join("json")
        .join("1.2.5_1"),
    );

    assert_eq!(
      cache.package_folder_for_id(
        &NpmPackageCacheFolderId {
          nv: NpmPackageNv {
            name: "JSON".to_string(),
            version: Version::parse_from_npm("2.1.5").unwrap(),
          },
          copy_index: 0,
        },
        &registry_url,
      ),
      root_dir
        .join("registry.npmjs.org")
        .join("_jjju6tq")
        .join("2.1.5"),
    );

    assert_eq!(
      cache.package_folder_for_id(
        &NpmPackageCacheFolderId {
          nv: NpmPackageNv {
            name: "@types/JSON".to_string(),
            version: Version::parse_from_npm("2.1.5").unwrap(),
          },
          copy_index: 0,
        },
        &registry_url,
      ),
      root_dir
        .join("registry.npmjs.org")
        .join("_ib2hs4dfomxuuu2pjy")
        .join("2.1.5"),
    );
  }
}
