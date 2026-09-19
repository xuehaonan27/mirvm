#!/usr/bin/env mirvm
---
[dependencies]
miden-assembly = "=0.25.8"
miden-processor = "=0.25.8"
---
// miden-vm 0.25.8 VM-in-VM execution differential. 0.25.5 was yanked upstream on
// 2026-08-10; 0.25.8 is a non-yanked patch of the same 0.25 release line.
//
// Shape and workaround (execute only, never prove):
//   * The miden-vm umbrella crate lists miden-prover as a non-optional dependency, so it
//     drags in the prover and a Fiat-Shamir random channel. Instead this driver uses
//     miden-assembly (MASM -> MAST) and miden-processor (synchronous FastProcessor)
//     directly; the umbrella, prover, verifier and winterfell crates stay out of the closure.
//   * All features are default (std); miden-processor's `concurrent` feature is off, so trace
//     building (trace::build_trace lays the execution trace out as a column matrix, not a
//     STARK proof) is single-threaded and deterministic. The closure is ~176 normal crates.
//     The FastProcessor is the synchronous interpreter used on both paths; no JIT is involved.
//   * The three programs are Rust string constants; the rec program substitutes the event id
//     into the sum_to body, and the assembler gets a DefaultSourceManager.
//
// Test surface (three small MASM programs, assembled by miden_assembly::Assembler into a
// Package -> unwrap_program, then executed by miden_processor on two paths):
//   ① fib(20): a repeat.20 counted loop (swap/dup.1/add); the final stack top is
//      [10946, 6765] and the oracle asserts fib(21)/fib(20).
//   ② A fixed-list multiply-add fold: acc=2 over [3,5,7,11,13] with acc = acc*x + x
//      (the swap dup.1 mul add quad), ending at 51207.
//   ③ Deeply recursive sum, sum(14)=105: MASM statically forbids recursion (the linker
//      rejects cycles in the call graph, including a self-referencing procref), so this
//      goes through the dyncall dynamic-call surface:
//        - dyncall reads the callee digest word from the caller context's memory by
//          address, and a new context gets isolated memory (values stored in the root
//          context are invisible to the callee).
//        - The digest is passed in two assembly rounds: round one uses procref.sum_to to
//          push the procedure digest onto the operand stack, which Rust reads back from
//          StackOutputs; round two declares that digest as an adv_map constant in the
//          program (the assembler writes it into the MastForest advice map, and at
//          execution time it is loaded into the single global advice provider, shared
//          across contexts and not switched with them).
//        - Each frame runs push.KEY adv.push_mapval adv_pushw (Pad4+AdvPopW pushes a word)
//          to fetch its own digest into mem[100], then dropw to clear and push.100 dyncall
//          to descend; n and the running sum stay on the operand stack and are inherited
//          by the callee.
//        - After each frame settles, dup.0 push.<EventId> emit emits S(k), and a Rust
//          closure registered through DefaultHost::register_handler (Arc<Mutex<Vec>>)
//          collects 15 emitted values in execution order:
//          [0,1,3,6,10,15,21,28,36,45,55,66,78,91,105].
//        - Along the way it exercises two VM invariants: InvalidStackDepthOnReturn (on
//          return from call/dyncall the callee stack depth must be exactly 16) and
//          "stack should have at most 16 elements".
//   Each program is compared on two paths: miden_processor::execute_sync (plain
//   FastProcessor) and FastProcessor::execute_trace_inputs_sync + trace::build_trace
//   (trace construction, not proving). The oracle asserts that the two StackOutputs are
//   identical and, on the rec program, that both emit sequences are identical; it prints
//   the stack top and the trace_len/core/range/hash/bitwise/memory segment lengths (miden
//   runs the same algorithm on native and mirvm, so these lengths are fully determined).
//
// Determinism discipline: every MASM source and assembly option is fixed; no time,
// randomness, env or TLS ordering; Felt values are always printed as as_canonical_u64;
// the emit event order equals the VM execution order; no tracing subscriber is registered
// (miden's tracing::instrument events are dropped silently); every Error or Report panics
// instead of printing. Output is about 12 lines.
//
// miden stack semantics that are easy to get wrong (this driver follows the correct ones):
// MLoadW / AdvPopW pop the address and then overwrite top-4 in place (they do not push);
// adv_pushw is the real push form, Pad four zeros + AdvPopW; swapdw swaps two words (8
// elements), not two double words; dyncall pops a single address element, and the callee
// inherits the caller's top-16 but gets its own memory; at a stack depth of 16 or more,
// drop-class instructions pull zeros from the overflow region back into the window.
//
// Output shape: fib and fold print a stack top and a trace_len line each; rec additionally
// prints its 15 emitted values.

use std::sync::{Arc, Mutex};

use miden_assembly::debuginfo::{DefaultSourceManager, SourceManager};
use miden_assembly::Assembler;
use miden_processor::advice::{AdviceInputs, AdviceMutation};
use miden_processor::event::{EventError, EventName};
use miden_processor::trace::build_trace;
use miden_processor::{
    DefaultHost, ExecutionOptions, ExecutionOutput, FastProcessor, Program, StackInputs,
};

/// ① fib(20): a counted repeat loop; final stack [fib(21), fib(20), 0×14] = [10946, 6765, ...].
/// The leading push.1 is a net +1 exactly balanced by the trailing movup.15 drop (depth ≤ 16).
const FIB_SRC: &str = r"
begin
    push.1
    repeat.20
        swap dup.1 add
    end
    movup.15 drop
end
";

/// ② Multiply-add fold: acc=2 over x ∈ [3,5,7,11,13], each step acc = acc*x + x.
/// 2->9->50->357->3938->51207. Six net pushes, balanced by six movup.15 drop pairs.
const FOLD_SRC: &str = r"
begin
    push.2
    push.3  swap dup.1 mul add
    push.5  swap dup.1 mul add
    push.7  swap dup.1 mul add
    push.11 swap dup.1 mul add
    push.13 swap dup.1 mul add
    movup.15 drop movup.15 drop movup.15 drop
    movup.15 drop movup.15 drop movup.15 drop
end
";

const EVT_NAME: &str = "mirvm::rec::sum";

/// MASM immediate string for the adv_map lookup key (push.7.0.0.0 -> a stack word, the key
/// copy_map_value_to_adv_stack reads at pos1..4).
const KEY_HEX: &str =
    "0x0000000000000000000000000000000000000000000000000700000000000000";

/// ③ sum_to procedure body: when n ≠ 0 decrement, fetch its own digest, descend, add back;
/// finally emit S(n). The frame load expands to (zero net stack effect):
///   push.7.0.0.0      key word onto the stack (its last element ends up on top)
///   adv.push_mapval   system event node: push the map value for the stack key to the advice stack (neutral)
///   dropw             drop the key word
///   adv_pushw        Pad4+AdvPopW: push the digest word onto the stack (+4)
///   mem_storew_le.100 store the digest into this frame's local memory 100 (original value kept)
///   dropw             drop the digest (-4)
const SUM_BODY: &str = r"
proc sum_to
    dup.0 push.0 neq
    if.true
        dup.0 push.1 sub
        push.7.0.0.0 adv.push_mapval dropw adv_pushw mem_storew_le.100 dropw
        push.100 dyncall
        add
    end
    dup.0 push.{evt} emit drop drop
end
";

fn sum_body(evt: u64) -> String {
    SUM_BODY.replace("{evt}", &evt.to_string())
}

/// Full rec source: the adv_map carries the real digest injected in round two.
fn rec_src(vals: &[String; 4], evt: u64) -> String {
    format!(
        r"adv_map KEY({KEY_HEX}) = [0x{}, 0x{}, 0x{}, 0x{}]

begin
    procref.sum_to mem_storew_le.100 dropw
    push.14
    push.100 dyncall
    movup.15 drop
end

{}
",
        vals[0], vals[1], vals[2], vals[3],
        sum_body(evt),
    )
}

fn assemble(name: &str, source: &str) -> Program {
    let sm: Arc<dyn SourceManager> = Arc::new(DefaultSourceManager::default());
    Assembler::new(sm)
        .assemble_program(name, source)
        .unwrap_or_else(|e| panic!("assemble {name}: {e}"))
        .unwrap_program()
}

/// Event sink: DefaultHost with a mirvm::rec::sum handler that collects emitted values in order.
fn run_with_host(
    program: &Program,
    tag: &str,
    sink: &Arc<Mutex<Vec<u64>>>,
) -> (Vec<u64>, Vec<u64>) {
    let mut host = DefaultHost::default();
    let cell = sink.clone();
    host.register_handler(
        EventName::new(EVT_NAME),
        Arc::new(move |state: &miden_processor::ProcessorState| {
            let v = state.get_stack_item(1).as_canonical_u64();
            cell.lock().unwrap().push(v);
            Ok(Vec::new()) as Result<Vec<AdviceMutation>, EventError>
        }),
    )
    .unwrap_or_else(|e| panic!("register handler {tag}: {e}"));
    let out = miden_processor::execute_sync(
        program,
        StackInputs::default(),
        AdviceInputs::default(),
        &mut host,
        ExecutionOptions::default(),
    )
    .unwrap_or_else(|e| panic!("exec {tag}: {e}"));
    let stack = out.stack.iter().map(|f| f.as_canonical_u64()).collect();
    (stack, sink.lock().unwrap().clone())
}

/// Trace path: execute_trace_inputs_sync + build_trace; returns (stack, per-segment trace lengths).
fn run_traced(
    program: &Program,
    tag: &str,
    sink: &Arc<Mutex<Vec<u64>>>,
) -> (Vec<u64>, Vec<u64>, (usize, usize, usize, usize, usize, usize)) {
    let mut host = DefaultHost::default();
    let cell = sink.clone();
    host.register_handler(
        EventName::new(EVT_NAME),
        Arc::new(move |state: &miden_processor::ProcessorState| {
            let v = state.get_stack_item(1).as_canonical_u64();
            cell.lock().unwrap().push(v);
            Ok(Vec::new()) as Result<Vec<AdviceMutation>, EventError>
        }),
    )
    .unwrap_or_else(|e| panic!("register handler {tag} traced: {e}"));
    let inputs = FastProcessor::new(StackInputs::default())
        .execute_trace_inputs_sync(program, &mut host)
        .unwrap_or_else(|e| panic!("trace-exec {tag}: {e}"));
    let trace = build_trace(inputs).unwrap_or_else(|e| panic!("build-trace {tag}: {e}"));
    let stack: Vec<u64> = trace.stack_outputs().iter().map(|f| f.as_canonical_u64()).collect();
    let sum = trace.trace_len_summary();
    let ch = sum.chiplets_trace_len();
    (
        stack,
        sink.lock().unwrap().clone(),
        (
            trace.length(),
            sum.core_trace_len(),
            sum.range_trace_len(),
            ch.hash_chiplet_len(),
            ch.bitwise_chiplet_len(),
            ch.memory_chiplet_len(),
        ),
    )
}

fn stack_str(s: &[u64]) -> String {
    format!("{:?}", &s[..8])
}

fn run_program(tag: &str, src: &str, want_stack: &[u64], want_emits: Option<&[u64]>) {
    let program = assemble(tag, src);

    let sink_plain = Arc::new(Mutex::new(Vec::<u64>::new()));
    let (s_plain, e_plain) = run_with_host(&program, tag, &sink_plain);

    let sink_traced = Arc::new(Mutex::new(Vec::<u64>::new()));
    let (s_traced, e_traced, lens) = run_traced(&program, tag, &sink_traced);

    assert_eq!(s_plain, s_traced, "{tag}: execute_sync and trace paths disagree on stack output");
    assert_eq!(&s_plain[..want_stack.len()], want_stack, "{tag}: stack top mismatches");
    if let Some(want) = want_emits {
        assert_eq!(e_plain, want, "{tag}: execute_sync emit sequence mismatches");
        assert_eq!(e_traced, want, "{tag}: trace-path emit sequence mismatches");
        println!("{tag} emits={e_plain:?}");
    }
    println!("{tag} stack_top={}", stack_str(&s_plain));
    println!(
        "{tag} trace_len={} core={} range={} hash={} bitwise={} memory={}",
        lens.0, lens.1, lens.2, lens.3, lens.4, lens.5
    );
}

fn main() {
    run_program("fib", FIB_SRC, &[10946, 6765], None);
    run_program("fold", FOLD_SRC, &[51207], None);

    // ③ rec, round one: a bare procref program fetches the sum_to digest (a procedure
    // digest depends only on its own body, not on this program's begin block).
    let evt = EventName::new(EVT_NAME).to_event_id().as_u64();
    let probe_src = format!(
        "begin procref.sum_to movup.15 drop movup.15 drop movup.15 drop movup.15 drop end\n\n{}\n",
        sum_body(evt)
    );
    let probe = assemble("rec_probe", &probe_src);
    let mut host0 = DefaultHost::default();
    let out0: ExecutionOutput = miden_processor::execute_sync(
        &probe,
        StackInputs::default(),
        AdviceInputs::default(),
        &mut host0,
        ExecutionOptions::default(),
    )
    .unwrap_or_else(|e| panic!("rec probe exec: {e}"));
    let vals: [String; 4] = out0
        .stack
        .iter()
        .take(4)
        .map(|f| format!("{:x}", f.as_canonical_u64()))
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();

    // Round two: inject the real digest into adv_map and run all 14 dyncall frames.
    let src = rec_src(&vals, evt);
    run_program(
        "rec",
        &src,
        &[105],
        Some(&[0, 1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 66, 78, 91, 105]),
    );
}
