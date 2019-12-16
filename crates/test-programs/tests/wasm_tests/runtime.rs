use anyhow::{bail, Context};
use std::fs::File;
use std::path::Path;
use wasmtime::{Config, Engine, HostRef, Instance, Module, Store};
use wasmtime_environ::settings::{self, Configurable};

pub fn instantiate(data: &[u8], bin_name: &str, workspace: Option<&Path>) -> anyhow::Result<()> {
    // Prepare runtime
    let mut flag_builder = settings::builder();

    // Enable proper trap for division
    flag_builder
        .enable("avoid_div_traps")
        .context("error while enabling proper division trap")?;

    let mut config = Config::new();
    config.flags(settings::Flags::new(flag_builder));
    let engine = HostRef::new(Engine::new(&config));
    let store = HostRef::new(Store::new(&engine));

    let global_exports = store.borrow().global_exports().clone();
    let get_preopens = |workspace: Option<&Path>| -> anyhow::Result<Vec<_>> {
        if let Some(workspace) = workspace {
            let preopen_dir = wasi_common::preopen_dir(workspace)
                .context(format!("error while preopening {:?}", workspace))?;

            Ok(vec![(".".to_owned(), preopen_dir)])
        } else {
            Ok(vec![])
        }
    };

    // Create our wasi context with pretty standard arguments/inheritance/etc.
    // Additionally register andy preopened directories if we have them.
    let mut builder = wasi_common::WasiCtxBuilder::new()
        .arg(bin_name)
        .arg(".")
        .inherit_stdio();
    for (dir, file) in get_preopens(workspace)? {
        builder = builder.preopened_dir(file, dir);
    }

    // The nonstandard thing we do with `WasiCtxBuilder` is to ensure that
    // `stdin` is always an unreadable pipe. This is expected in the test suite
    // where `stdin` is never ready to be read. In some CI systems, however,
    // stdin is closed which causes tests to fail.
    let (reader, _writer) = os_pipe::pipe()?;
    builder = builder.stdin(reader_to_file(reader));
    let snapshot1 = Instance::from_handle(
        &store,
        wasmtime_wasi::instantiate_wasi_with_context(
            global_exports.clone(),
            builder.build().context("failed to build wasi context")?,
        )
        .context("failed to instantiate wasi")?,
    );

    // ... and then do the same as above but for the old snapshot of wasi, since
    // a few tests still test that
    let mut builder = wasi_common::old::snapshot_0::WasiCtxBuilder::new()
        .arg(bin_name)
        .arg(".")
        .inherit_stdio();
    for (dir, file) in get_preopens(workspace)? {
        builder = builder.preopened_dir(file, dir);
    }
    let (reader, _writer) = os_pipe::pipe()?;
    builder = builder.stdin(reader_to_file(reader));
    let snapshot0 = Instance::from_handle(
        &store,
        wasmtime_wasi::old::snapshot_0::instantiate_wasi_with_context(
            global_exports.clone(),
            builder.build().context("failed to build wasi context")?,
        )
        .context("failed to instantiate wasi")?,
    );

    let module = HostRef::new(Module::new(&store, &data).context("failed to create wasm module")?);
    let imports = module
        .borrow()
        .imports()
        .iter()
        .map(|i| {
            let instance = if i.module() == "wasi_unstable" {
                &snapshot0
            } else if i.module() == "wasi_snapshot_preview1" {
                &snapshot1
            } else {
                bail!("import module {} was not found", i.module())
            };
            let field_name = i.name();
            if let Some(export) = instance.find_export_by_name(field_name) {
                Ok(export.clone())
            } else {
                bail!(
                    "import {} was not found in module {}",
                    field_name,
                    i.module(),
                )
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    let instance = HostRef::new(Instance::new(&store, &module, &imports).context(format!(
        "error while instantiating Wasm module '{}'",
        bin_name,
    ))?);

    let export = instance
        .borrow()
        .find_export_by_name("_start")
        .context("expected a _start export")?
        .clone();

    if let Err(trap) = export
        .func()
        .context("expected export to be a func")?
        .borrow()
        .call(&[])
    {
        bail!("trapped: {:?}", trap.borrow());
    }

    Ok(())
}

#[cfg(unix)]
fn reader_to_file(reader: os_pipe::PipeReader) -> File {
    use std::os::unix::prelude::*;
    unsafe { File::from_raw_fd(reader.into_raw_fd()) }
}

#[cfg(windows)]
fn reader_to_file(reader: os_pipe::PipeReader) -> File {
    use std::os::windows::prelude::*;
    unsafe { File::from_raw_handle(reader.into_raw_handle()) }
}