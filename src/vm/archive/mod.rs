//! Process-independent execution artifacts and the ownership boundary of verified bytecode.

pub(crate) mod bytes;
pub(crate) mod functions;
pub(crate) mod meta;
#[cfg(test)]
mod tests;
mod validator;

use std::sync::Arc;

use crate::error::Error;
use crate::vm::instance::Instance;
use crate::vm::ir::Module;

/// Verified immutable code and the exact metadata it was checked against. Only this owner can
/// bind that evidence to a fresh instance; callers cannot substitute module tables or bytecode.
pub(crate) struct Image {
    meta: meta::ModuleMeta,
    functions: Arc<functions::Functions>,
    addresses: crate::vm::verify::AddressUses,
}

impl Image {
    pub(crate) fn load(
        meta: meta::ModuleMeta,
        functions: functions::Functions,
    ) -> Result<Self, Error> {
        let (module, instance) = meta.instantiate()?;
        let addresses = std::cell::RefCell::new(crate::vm::verify::AddressUses::default());
        crate::vm::verify::archived::module(&module, &instance, &functions, Some(&addresses))?;
        Ok(Self {
            meta,
            functions: Arc::new(functions),
            addresses: addresses.into_inner(),
        })
    }

    /// A disposable cache hit transfers its metadata directly into the one instance that uses it.
    pub(crate) fn load_once(
        meta: meta::ModuleMeta,
        functions: functions::Functions,
    ) -> Result<VerifiedArtifact, Error> {
        let start = crate::options::timing().then(std::time::Instant::now);
        let (mut module, instance) = meta.into_instance()?;
        let restored = start.map(|_| std::time::Instant::now());
        // This instance consumes the proof directly; it has no later address rebinding.
        crate::vm::verify::archived::module(&module, &instance, &functions, None)?;
        let verified = start.map(|_| std::time::Instant::now());
        module.funcs = crate::vm::ir::FuncTable::from_archive(Arc::new(functions));
        if let Some(((start, restored), verified)) = start.zip(restored).zip(verified) {
            crate::diag::instrument(format_args!(
                "mirvm-image-instance: restore={:.3}ms verification={:.3}ms functions={:.3}ms",
                (restored - start).as_secs_f64() * 1e3,
                (verified - restored).as_secs_f64() * 1e3,
                verified.elapsed().as_secs_f64() * 1e3,
            ));
        }
        Ok(VerifiedArtifact { module, instance })
    }

    pub(crate) fn native_paths(&self) -> &[Box<str>] {
        self.meta.native_paths()
    }

    pub(crate) fn instantiate(&self) -> Result<VerifiedArtifact, Error> {
        let (mut module, instance) = self.meta.instantiate()?;
        self.addresses
            .check(&module, &instance, self.functions.len())?;
        module.funcs = crate::vm::ir::FuncTable::from_archive(self.functions.clone());
        Ok(VerifiedArtifact { module, instance })
    }
}

/// Validation evidence owns the values it describes. Consuming this type transfers those values
/// together; exposing arbitrary mutable IR would invalidate the evidence and is not supported.
pub(crate) struct VerifiedArtifact {
    module: Module,
    instance: Instance,
}

impl VerifiedArtifact {
    #[cfg(feature = "compiler")]
    pub(crate) fn verify(module: Module, instance: Instance) -> Result<Self, Error> {
        crate::vm::verify::module(&module, &instance)?;
        Ok(Self { module, instance })
    }

    pub(crate) fn module(&self) -> &Module {
        &self.module
    }

    /// Native products replace only the loader's transport paths and process-local images. They do
    /// not change the verified instructions, function ids or frozen address domains.
    pub(crate) fn set_native_images(
        &mut self,
        paths: Vec<Box<str>>,
        hashes: Vec<u128>,
        images: Vec<crate::vm::mcload::McImage>,
    ) {
        self.module.required_native_libs = paths;
        self.module.required_native_hashes = hashes;
        self.instance.mc_images = images;
    }

    pub(crate) fn set_asm_addrs(&mut self, addresses: Vec<u64>) -> Result<(), Error> {
        if addresses.len() != self.module.asm_sites.len() {
            return Err(crate::fail!(
                Engine,
                "materialized asm table has the wrong length"
            ));
        }
        self.instance.asm_stub_addrs = addresses;
        Ok(())
    }

    pub(crate) fn finalize_argv(&mut self, argv: &[String]) -> Result<(), Error> {
        self.instance.finalize_entry_argv(&mut self.module, argv)
    }

    pub(crate) fn into_parts(self) -> (Module, Instance) {
        (self.module, self.instance)
    }
}
