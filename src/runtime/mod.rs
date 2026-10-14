//! Execute validated products without a compiler session.

pub(crate) mod entry;

/// Run a prepared image, returning `None` when its inputs or stored products require preparation.
///
/// The arguments must be the complete compiler argument vector used to prepare the image, including
/// argv[0]. Guest arguments are applied only after validation and are not part of the image key.
///
/// # Safety
///
/// The caller must trust embedded native code and foreign ABI declarations, as for
/// [`crate::pack::Package::instantiate`].
pub unsafe fn run_image(
    compiler_args: &[String],
    guest_args: &[String],
) -> Result<Option<i32>, crate::error::Error> {
    crate::sysroot::pin_args(compiler_args)?;
    let Some(artifact) = crate::image::execution::lookup(compiler_args) else {
        return Ok(None);
    };
    run(
        artifact,
        guest_args,
        crate::image::program::heat(compiler_args),
    )
    .map(Some)
}

/// Run a standalone package with the supplied guest argv, including argv[0].
///
/// # Safety
///
/// The caller must trust embedded native code and foreign ABI declarations, as for
/// [`crate::pack::Package::instantiate`].
pub unsafe fn run_package(
    path: &std::path::Path,
    guest_args: &[String],
) -> Result<i32, crate::error::Error> {
    let package = crate::pack::load_package(path)?;
    let mut artifact = package.instantiate()?;
    let addresses =
        crate::native::artifact::assembly::try_materialize(&artifact.module().asm_sites)
            .map_err(crate::pack::Error::from)?;
    artifact
        .set_asm_addrs(addresses)
        .map_err(|error| crate::fail!(Run, error.to_string()))?;
    run(artifact, guest_args, crate::vm::jit::Heat::default())
}

fn run(
    mut artifact: crate::vm::archive::VerifiedArtifact,
    guest_args: &[String],
    heat: crate::vm::jit::Heat,
) -> Result<i32, crate::error::Error> {
    artifact
        .finalize_argv(guest_args)
        .map_err(|error| crate::fail!(Run, error.to_string()))?;
    let (module, instance) = artifact.into_parts();
    Ok(entry::run_loaded(module, instance, None, heat))
}
