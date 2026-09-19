#!/usr/bin/env mirvm
---
[dependencies]
miden-assembly = "=0.25.8"
miden-processor = "=0.25.8"
miden-prover = "=0.25.8"
miden-verifier = "=0.25.8"
---
// miden-vm 0.25.8 proof surface (prove -> verify -> tamper counter-anchor), three-way
// differential. Like c_miden_exec it uses the same MASM fib program on the same
// assembler/processor base, but drives the full STARK proof: miden-prover produces the
// proof, miden-verifier verifies it, and flipping a proof byte must make verification fail.
//
// Version pins:
//   * miden-assembly / miden-processor / miden-prover / miden-verifier are all pinned to
//     =0.25.8. 0.25.5 was yanked from crates.io on 2026-08-10, so a fresh resolve correctly
//     rejects it; 0.25.8 is a non-yanked patch on the same 0.25 release line, and all four
//     crates share that version.
//   * miden-prover 0.25.8's STARK backend is the Plonky3-family miden-lifted-stark 0.28
//     (re-exported through miden-crypto 0.28's stark module). ProvingOptions only picks the
//     hash function (Blake3_256 by default); the FRI and security parameters are hardcoded to
//     96-bit in miden-air::config, so there is no tunable proving-parameter surface and the
//     defaults are fixed values.
//   * No Git [patch] is needed: the 0.25.8 prover and verifier no longer depend on the old
//     wincode, so the dependency graph resolves normally from crates.io.
//
// Determinism (the evidence chain for reproducible proof bytes):
//   * The prover's "randomness" (aux-trace random challenges and FRI challenges) all comes
//     from the Fiat-Shamir channel (channel.sample_algebra_element, seeded with the protocol
//     parameters, the public values and the main commitment); it never touches an OS random
//     source and there is no ZK blinding band.
//     (miden-lifted-stark 0.28 prover calls channel.sample_algebra_element::<EF>()).
//     The proof is therefore a pure function of the program, inputs and protocol parameters.
//   * Features stay at the default (std) with `concurrent` off, so p3_maybe_rayon degrades to
//     sequential iteration and proof construction is single-threaded and byte-reproducible.
//   * tracing::instrument events have no subscriber, so they are silently dropped and stderr
//     stays empty.
//
// Coverage:
//   ① A fixed MASM fib(20) program (a repeat-count loop; final stack top [10946, 6765]) runs
//      through miden_prover::prove_sync and produces a proof: the stack top, proof byte
//      length, FNV-1a/64 fingerprint and security_level are printed.
//   ② Self-verification: miden_verifier::verify(ProgramInfo, StackInputs, StackOutputs,
//      proof) must be Ok; verify=true and the returned security level are printed.
//   ③ Tamper counter-anchor: XOR one byte in the middle of the STARK proof body (inside the
//      FRI/commitment data, where any change yields a deterministic verification failure) and
//      verify must be Err; verify_tampered=false is printed.
//
// Three-way rerun:
//   A: target/release/mirvm run corpus/c_miden_prove.rs
//   B: d=$(grep -l 'name = "c_miden_prove"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname) && cd "$d" && RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_miden_prove.rs
//
// The native oracle prints exactly these four lines (stderr empty, exit 0):
//   fib stack_top=[10946, 6765, 0, 0]; proof len=37599 fnv=449fe979d1395d47
//   security=96; verify=true security=96; verify_tampered=false
// The proof bytes are bit-identical across implementations (mirvm interpreter/JIT vs native)
// and across repeated runs of the same implementation, which is the Fiat-Shamir determinism
// claimed above made concrete.

use std::sync::Arc;

use miden_assembly::Assembler;
use miden_assembly::debuginfo::{DefaultSourceManager, SourceManager};
use miden_processor::{DefaultHost, ExecutionOptions, Program, StackInputs};
use miden_prover::{AdviceInputs, ExecutionProof, ProvingOptions, prove_sync};
use miden_verifier::{ProgramInfo, verify};

/// fib(20): a repeat-count loop. The final stack is [fib(21), fib(20), 0x14] = [10946, 6765, ...].
/// The same program as c_miden_exec; the loop leaves the stack balanced.
const FIB_SRC: &str = r"
begin
    push.1
    repeat.20
        swap dup.1 add
    end
    movup.15 drop
end
";

/// FNV-1a 64 fingerprint of the proof bytes (no external dependency, bit-exact).
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn assemble(name: &str, source: &str) -> Program {
    let sm: Arc<dyn SourceManager> = Arc::new(DefaultSourceManager::default());
    Assembler::new(sm)
        .assemble_program(name, source)
        .unwrap_or_else(|e| panic!("assemble {name}: {e}"))
        .unwrap_program()
}

fn main() {
    let program = assemble("fib", FIB_SRC);
    let program_info = ProgramInfo::from(program.clone());
    let stack_inputs = StackInputs::default();

    // ① Execute and prove (default ProvingOptions = Blake3_256, 96-bit parameters hardcoded).
    let mut host = DefaultHost::default();
    let (stack_outputs, proof) = prove_sync(
        &program,
        stack_inputs.clone(),
        AdviceInputs::default(),
        &mut host,
        ExecutionOptions::default(),
        ProvingOptions::default(),
    )
    .unwrap_or_else(|e| panic!("prove fib: {e}"));

    let outs: Vec<u64> = stack_outputs.iter().map(|f| f.as_canonical_u64()).collect();
    assert_eq!(&outs[..2], &[10946, 6765], "fib stack top mismatch");
    let proof_bytes = proof.to_bytes();
    println!("fib stack_top={:?}", &outs[..4]);
    println!(
        "proof len={} fnv={:016x} security={}",
        proof_bytes.len(),
        fnv1a64(&proof_bytes),
        proof.security_level()
    );

    // ② Positive self-verification anchor.
    let security = verify(
        program_info.clone(),
        stack_inputs.clone(),
        stack_outputs.clone(),
        proof.clone(),
    )
    .unwrap_or_else(|e| panic!("verify good proof: {e}"));
    println!("verify=true security={security}");

    // ③ Tamper counter-anchor: XOR the middle byte of the proof body with 0x01; verify must be Err.
    let mut tampered: ExecutionProof = proof;
    let mid = tampered.proof.len() / 2;
    tampered.proof[mid] ^= 0x01;
    let ok = verify(program_info, stack_inputs, stack_outputs, tampered).is_ok();
    assert!(!ok, "tampered proof must not verify");
    println!("verify_tampered={ok}");
}
