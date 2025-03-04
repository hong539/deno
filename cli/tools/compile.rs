// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use crate::args::CompileFlags;
use crate::args::Flags;
use crate::factory::CliFactory;
use crate::graph_util::error_for_any_npm_specifier;
use crate::standalone::is_standalone_binary;
use crate::util::path::path_has_trailing_slash;
use deno_core::anyhow::bail;
use deno_core::anyhow::Context;
use deno_core::error::generic_error;
use deno_core::error::AnyError;
use deno_core::resolve_url_or_path;
use deno_runtime::colors;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use super::installer::infer_name_from_url;

pub async fn compile(
  flags: Flags,
  compile_flags: CompileFlags,
) -> Result<(), AnyError> {
  let factory = CliFactory::from_flags(flags).await?;
  let cli_options = factory.cli_options();
  let module_graph_builder = factory.module_graph_builder().await?;
  let parsed_source_cache = factory.parsed_source_cache()?;
  let binary_writer = factory.create_compile_binary_writer().await?;
  let module_specifier = cli_options.resolve_main_module()?;
  let module_roots = {
    let mut vec = Vec::with_capacity(compile_flags.include.len() + 1);
    vec.push(module_specifier.clone());
    for side_module in &compile_flags.include {
      vec.push(resolve_url_or_path(side_module, cli_options.initial_cwd())?);
    }
    vec
  };

  let output_path = resolve_compile_executable_output_path(
    &compile_flags,
    cli_options.initial_cwd(),
  )
  .await?;

  let graph = Arc::try_unwrap(
    module_graph_builder
      .create_graph_and_maybe_check(module_roots)
      .await?,
  )
  .unwrap();

  if !cli_options.unstable() {
    error_for_any_npm_specifier(&graph).context(
      "Using npm specifiers with deno compile requires the --unstable flag.",
    )?;
  }

  let parser = parsed_source_cache.as_capturing_parser();
  let eszip = eszip::EszipV2::from_graph(graph, &parser, Default::default())?;

  log::info!(
    "{} {} to {}",
    colors::green("Compile"),
    module_specifier.to_string(),
    output_path.display(),
  );
  validate_output_path(&output_path)?;

  let mut file = std::fs::File::create(&output_path)?;
  binary_writer
    .write_bin(
      &mut file,
      eszip,
      &module_specifier,
      &compile_flags,
      cli_options,
    )
    .await
    .with_context(|| format!("Writing {}", output_path.display()))?;
  drop(file);

  // set it as executable
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o777);
    std::fs::set_permissions(output_path, perms)?;
  }

  Ok(())
}

/// This function writes out a final binary to specified path. If output path
/// is not already standalone binary it will return error instead.
fn validate_output_path(output_path: &Path) -> Result<(), AnyError> {
  if output_path.exists() {
    // If the output is a directory, throw error
    if output_path.is_dir() {
      bail!(
        concat!(
          "Could not compile to file '{}' because a directory exists with ",
          "the same name. You can use the `--output <file-path>` flag to ",
          "provide an alternative name."
        ),
        output_path.display()
      );
    }

    // Make sure we don't overwrite any file not created by Deno compiler because
    // this filename is chosen automatically in some cases.
    if !is_standalone_binary(output_path) {
      bail!(
        concat!(
          "Could not compile to file '{}' because the file already exists ",
          "and cannot be overwritten. Please delete the existing file or ",
          "use the `--output <file-path>` flag to provide an alternative name."
        ),
        output_path.display()
      );
    }

    // Remove file if it was indeed a deno compiled binary, to avoid corruption
    // (see https://github.com/denoland/deno/issues/10310)
    std::fs::remove_file(output_path)?;
  } else {
    let output_base = &output_path.parent().unwrap();
    if output_base.exists() && output_base.is_file() {
      bail!(
          concat!(
            "Could not compile to file '{}' because its parent directory ",
            "is an existing file. You can use the `--output <file-path>` flag to ",
            "provide an alternative name.",
          ),
          output_base.display(),
        );
    }
    std::fs::create_dir_all(output_base)?;
  }

  Ok(())
}

async fn resolve_compile_executable_output_path(
  compile_flags: &CompileFlags,
  current_dir: &Path,
) -> Result<PathBuf, AnyError> {
  let module_specifier =
    resolve_url_or_path(&compile_flags.source_file, current_dir)?;

  let mut output = compile_flags.output.clone();

  if let Some(out) = output.as_ref() {
    if path_has_trailing_slash(out) {
      if let Some(infer_file_name) = infer_name_from_url(&module_specifier)
        .await
        .map(PathBuf::from)
      {
        output = Some(out.join(infer_file_name));
      }
    } else {
      output = Some(out.to_path_buf());
    }
  }

  if output.is_none() {
    output = infer_name_from_url(&module_specifier)
      .await
      .map(PathBuf::from)
  }

  output.ok_or_else(|| generic_error(
    "An executable name was not provided. One could not be inferred from the URL. Aborting.",
  )).map(|output| {
    get_os_specific_filepath(output, &compile_flags.target)
  })
}

fn get_os_specific_filepath(
  output: PathBuf,
  target: &Option<String>,
) -> PathBuf {
  let is_windows = match target {
    Some(target) => target.contains("windows"),
    None => cfg!(windows),
  };
  if is_windows && output.extension().unwrap_or_default() != "exe" {
    if let Some(ext) = output.extension() {
      // keep version in my-exe-0.1.0 -> my-exe-0.1.0.exe
      output.with_extension(format!("{}.exe", ext.to_string_lossy()))
    } else {
      output.with_extension("exe")
    }
  } else {
    output
  }
}

#[cfg(test)]
mod test {
  pub use super::*;

  #[tokio::test]
  async fn resolve_compile_executable_output_path_target_linux() {
    let path = resolve_compile_executable_output_path(
      &CompileFlags {
        source_file: "mod.ts".to_string(),
        output: Some(PathBuf::from("./file")),
        args: Vec::new(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        include: vec![],
      },
      &std::env::current_dir().unwrap(),
    )
    .await
    .unwrap();

    // no extension, no matter what the operating system is
    // because the target was specified as linux
    // https://github.com/denoland/deno/issues/9667
    assert_eq!(path.file_name().unwrap(), "file");
  }

  #[tokio::test]
  async fn resolve_compile_executable_output_path_target_windows() {
    let path = resolve_compile_executable_output_path(
      &CompileFlags {
        source_file: "mod.ts".to_string(),
        output: Some(PathBuf::from("./file")),
        args: Vec::new(),
        target: Some("x86_64-pc-windows-msvc".to_string()),
        include: vec![],
      },
      &std::env::current_dir().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(path.file_name().unwrap(), "file.exe");
  }

  #[test]
  fn test_os_specific_file_path() {
    fn run_test(path: &str, target: Option<&str>, expected: &str) {
      assert_eq!(
        get_os_specific_filepath(
          PathBuf::from(path),
          &target.map(|s| s.to_string())
        ),
        PathBuf::from(expected)
      );
    }

    if cfg!(windows) {
      run_test("C:\\my-exe", None, "C:\\my-exe.exe");
      run_test("C:\\my-exe.exe", None, "C:\\my-exe.exe");
      run_test("C:\\my-exe-0.1.2", None, "C:\\my-exe-0.1.2.exe");
    } else {
      run_test("my-exe", Some("linux"), "my-exe");
      run_test("my-exe-0.1.2", Some("linux"), "my-exe-0.1.2");
    }

    run_test("C:\\my-exe", Some("windows"), "C:\\my-exe.exe");
    run_test("C:\\my-exe.exe", Some("windows"), "C:\\my-exe.exe");
    run_test("C:\\my-exe.0.1.2", Some("windows"), "C:\\my-exe.0.1.2.exe");
    run_test("my-exe-0.1.2", Some("linux"), "my-exe-0.1.2");
  }
}
