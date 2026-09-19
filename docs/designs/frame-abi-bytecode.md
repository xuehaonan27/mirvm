# Frame Layout, Calling Convention, and Bytecode Format — M4 Design Baseline (Model A)

> Status: Implemented · Scope: Model A native frame kinds, interpreted-frame layout and guest local storage, frame descriptors, calling convention and the four interp↔compiled transitions, and the register-based bytecode format.
>
> M4 historical baseline; the main body is implemented. Tree-walking Model A, the slaved ByteRegion, frozen metadata, real threads and M4 unwind have landed; method-level Cranelift and the i2c/c2i product adapters were delivered by M5.3–M5.5; `.mirvm` mode B distribution is implemented. Interpreted locals formally keep the slaved ByteRegion. Current rollout state lives in [`../current-status.md`](../current-status.md); the A/B and local-storage two-axis evolution lives in git history. **2026-08-12 unwind erratum:** earlier text wrote "across FFI" as an unconditional abort, which is too broad. The current rule is that an ordinary C boundary terminates while a `C-unwind` boundary lets the original exception pass through and run cleanup; outbound `ffi_call`, callback/P1 wrapper, interpreter and JIT are all implemented to this rule. Normative contract: [`c-unwind-contract.md`](c-unwind-contract.md).

## 1. Contract
1. **One activation, one native frame.** Every guest function call activation maps to exactly one native stack frame, so interp↔compiled calls are native calls and unwind walks a single stack. This is the Model A property the JIT depends on.
2. **Guest locals are orthogonal to JIT interop.** JIT interop requires only that call activity (control flow + unwind) sit on the native stack. It never requires guest local data to be inlined there: compiled code never reads an interpreted frame's locals, and cross-function traffic uses only the calling convention.
3. **Interpreted-frame locals live in the slaved operand region.** Each thread owns one operand region, slaved to `interp_frame` recursion (LIFO, in step with entry and exit). It must never be suspended independently — mirvm has no coroutines (§async). Control flow stays on the native stack.
4. **Region SP discipline.** Entering `interp_frame` bumps the region SP by the frame's `total_frame_size` to reserve that frame's register slots; leaving it, and unwind, restore the region SP.
5. **Register slot = MIR local.** `r0..rn` correspond to MIR `_0.._n`; each slot is sized and aligned by its type's frozen layout. Slots are real addresses, so `&local` handed to C is natural.
6. **Frame storage must not be coupled to the safety mode.** Frame local storage and the safety mode (`fast`/`checked`, axis S) are two orthogonal axes and must stay uncoupled in the implementation. They meet at exactly one abstraction, `GuestMemory::contains(addr)->bool`, the "is this legal guest memory" predicate: `fast` never calls it, `checked` calls it before any raw dereference, and `FrameStorage` provides it. The current slaved region can answer it with a cheap range compare; any future storage reopened on performance evidence must not change `checked`'s semantic contract.
7. **No `alloca` for interpreted locals (2026-08-12 final ruling, replacing the 2026-07-05 migration promise).** Hot functions already use native frames and SSA under Cranelift; `alloca` would change only the cold interpreter while additionally carrying stack probing, zeroing, unwind and checked address tracking. It reopens only when real interpreter load proves end-to-end benefit.
8. **Freeze metadata at lowering; never touch tcx at runtime (C8).** Layout, call targets, vtable slots, drop glue and constants resolve into concrete numbers during lowering. The product is an immutable `BytecodeBody`, read-only and thread-shared after publication, with lock-free reads. Lowering runs on the compilation service thread or under a lazy lock (≈ HotSpot resolved constant pool / class loading).
9. **Adapters only move arguments.** i2c/c2i move guest arguments between operand-region slots and convention registers; compiled→compiled is a direct native call with zero adaptation.
10. **The JIT calling convention must be unwind-capable.** Plain `"C"` is an abort shim and is kept only as a fallback for ordinary C abort.
11. **Exception partition.** Across an ordinary C boundary a guest exception ends (abort); across a `C-unwind` boundary the original exception passes through and cleanup runs. The normative contract is [`c-unwind-contract.md`](c-unwind-contract.md).
12. **The engine is `Sync` with no GIL (VM tier), one guest thread per OS thread.** `BytecodeBody` and frozen metadata are read-only shared after publication; the operand region is per-thread private; Rust heap allocation goes through a per-thread arena (C8); each guest thread's interpreted frames use that OS thread's native stack.
13. **JIT backend and distribution are fixed.** Cranelift sits behind the `JITBackend` trait and the VM core talks only to the trait. Distribution is a multi-target fat `.mirvm` artifact; a single artifact that runs on any target is rejected (§3), and a missing triple must be reported as "unsupported platform".

## 2. Model
### 2.1 Frame kinds
A call activation is on the native stack even when its locals are not:

```
 one OS thread's native stack (grows downward):
   interp_frame(main)   [interpreted]  Rust frame: dispatch state + main's operand region
   <compiled> foo       [compiled]     Cranelift frame: foo's locals/spills, pure native
   interp_frame(bar)    [interpreted]  ← current
 main(interp) → foo(compiled) → bar(interp): one native stack, each crossing an i2c/c2i adapter (§2.4); unwind walks this stack (§2.8).
```

- Interpreted frame = one Rust call to `interp_frame`; a guest call becomes host recursion.
- Compiled frame = a Cranelift-generated native frame; a guest call is a direct native call.
- Both kinds share one native stack, joined by adapters. That is Model A.
- Why tree-walking (host recursion) rather than a HotSpot-style assembly template interpreter: the latter manipulates the native SP explicitly to push and pop interpreted frames at assembly level, which is too heavy; tree-walking maps guest calls to native calls in safe Rust and reaches the same Model A JIT interop. Its cost is one host `interp_frame` overhead per interpreted frame, acceptable because the interpreter is the cold tier (hot code runs in compiled frames). The assembly/`alloca` route survives only as an evidence-triggered performance candidate (§5, git history).

### 2.2 In-frame layout and guest local storage
**Compiled frame.** Cranelift manages it (locals, spill slots, callee-saved registers). This contract defines only its calling convention (§2.4) and unwind info (§2.8); mirvm places no layout for it.

**Interpreted frame.** A guest frame's size is dynamic (it depends on the function's local count and types) while Rust locals are fixed-size, so guest locals cannot be inlined directly into the native stack. Scheme: one interpreter operand region per thread, slaved to `interp_frame` recursion.

```
 per-thread operand region (contiguous buffer; SP slaved to interp_frame recursion):
   [main's register slots r0..rn][bar's slots r0..rm]...   ← region SP
   enter interp_frame: bump region SP per the frame descriptor to reserve this frame's slots; exit/unwind: restore region SP.
```

Compiled frames never use this region; they use their own Cranelift frame.

### 2.3 Frame descriptors
One per function, computed at lowering and frozen, consumed by `interp_frame` to build a frame:

```
FrameDescriptor {
    reg_count,                                        // number of registers (= MIR locals)
    reg_slots: [{offset, size, align, ty_layout_id}], // each register's slot in the operand region
    total_frame_size,                                 // how much is reserved in the region
    drops: [{reg, drop_glue_instance, cond}],         // registers needing Drop + drop glue (frozen)
    cleanup_edges,                                    // unwind targets (catch/cleanup), frozen from MIR
}
```

### 2.4 Calling convention and the four transitions
Goal: compiled↔compiled is a pure native call, and interp↔compiled is cheap to adapt. Cranelift-compiled guest functions use one defined mirvm calling convention (based on the platform C ABI or a custom Cranelift calling convention). Scalars and pointers pass in registers (under real addresses a pointer is a real address); aggregates follow the rustc ABI, reusing rustc layout and staying consistent with native. Function identity is the monomorphized `Instance` (lazy monomorphization) → one code ptr after compilation.

```
compiled → compiled : direct native call (Cranelift per the convention). Zero adaptation.
interp   → compiled : i2c adapter — interp_frame moves guest arguments from operand-region slots into convention registers, then native-calls the code ptr.
compiled → interp   : c2i adapter — compiled code calls a stub c2i(instance, args...); the stub moves the arguments into the new interpreted frame's operand-region slots and calls interp_frame(instance).
interp   → interp   : interp_frame recursively calls interp_frame(callee) (host recursion).
```

- Adapters only move arguments (slots ↔ registers); they are cheap because both sides are on the same native stack. Structurally identical to HotSpot i2c/c2i.
- An uncompiled hot `Instance`: when interp calls it, it either keeps interpreting or requests compilation (compilation service thread, C8). This design fixes no tiering policy (M5); it guarantees only that a compiled callee plugs in seamlessly.

### 2.5 Bytecode format and MIR correspondence
The bytecode is register-based (not stack-based) with registers ≈ MIR locals. Rationale: MIR is already register/place-based (`_0.._n` + projections), so register form lowers almost mechanically from MIR and uses fewer, faster instructions (Lua and Dalvik made the same choice). The bytecode is "MIR after metadata resolution and flattening".

| MIR | mirvm bytecode |
|---|---|
| local `_i` | register `ri` (one slot in the operand region) |
| place projection `_3.2`, `(*_4)[i]` | resolved to concrete offset arithmetic (layout frozen → offset computed at compile time) |
| rvalue (BinaryOp/Ref/Cast/Aggregate…) | corresponding bytecode instruction, writing the destination register |
| `SwitchInt` (match/discriminant/**async state machine**) | `switch ri -> [value:target]` |
| `Call(f, args, dest, unwind)` | `call <InstanceId/code ptr>, [arg regs], dest reg, unwind blk` |
| `Drop(place, unwind)` | `drop ri` (using the frozen drop glue instance) |
| `Return` | `ret r0` |
| foreign call | `call_foreign <os::handler id>` or through the §os boundary |

Instruction-set sketch (illustrative, not the full set):

```
# arithmetic/logic: bin <op> rd, ra, rb / un <op> rd, ra   # add/sub/mul/... with overflow semantics (frozen overflow-checks)
# memory (real addresses; no AllocId metadata, raw access)
load rd, [rbase + off] / store [rbase + off], rs / ref rd, rplace   # off computed at compile time
# aggregates/projections, with off already resolved
field rd, rbase, off / index rd, rbase, ridx, elem_sz
discr rd, rbase, disc_enc / setdiscr rbase, variant, disc_enc      # frozen encoding/niche
# control flow
jump blk
switch ri -> [v0:blk0, v1:blk1, ...]     # async state machine dispatch is exactly this
call <target>, [args], rd, unwind=blk    # target: direct InstanceId / dyn: vtable slot
ret ri
# builtins: engine-implemented or forwarded to os::
intrinsic <id>, [args], rd
```

Projections and discriminants are all frozen offsets, so the runtime never queries tcx (C8). `switch` directly serves async (§async): the state machine is discr + switch and needs no special support.

### 2.6 Metadata freezing
Lowering MIR to bytecode for one monomorphized `Instance` resolves and freezes:

- **layout** — per-type size/align/field offsets/discriminant & niche encoding → concrete numbers.
- **call targets** — direct call → the monomorphized `Instance`'s `BytecodeBodyId` / code ptr; dyn call → vtable slot number.
- **vtable** — dyn type vtable layout (method slots).
- **drop glue** — every position needing Drop → a concrete drop instance.
- **constants** — interned into this function's constant pool.
- **intrinsic** — marked as a builtin op or resolved.

### 2.7 Execution: `interp_frame` and transitions
```rust
// one call = one interpreted frame (on the native stack)
fn interp_frame(body: &BytecodeBody, args: Args, region: &mut OperandRegion) -> Value {
    let base = region.reserve(body.desc.total_frame_size); // slaved bump
    load_args_into_slots(region, base, args, &body.desc);
    let mut blk = 0; let mut ip = 0;
    loop {
        match body.code[blk][ip] {
            Bin(op, rd, ra, rb) => { /* read/write region[base+slot] */ }
            Field(rd, rb, off)  => { /* already-frozen offset */ }
            Switch(ri, targets) => { blk = targets[read(ri)]; ip = 0; continue; }
            Call(target, aregs, rd, unwind) => {
                let a = gather(region, base, aregs);
                let r = match target {
                    Interp(callee) => interp_frame(callee, a, region),   // host recursion (Model A)
                    Compiled(ptr)  => i2c_call(ptr, a),                  // native call + adaptation
                    Foreign(h)     => os::dispatch(h, a),               // §os boundary
                };
                write(region, base, rd, r);
            }
            Drop(ri) => run_drop(body.desc.drop_of(ri), region, base),
            Ret(ri)  => { let v = read(region, base, ri); region.restore(base); return v; }
            ...
        }
        ip += 1;
    }
}
```
Pure Rust, tree-walking: `region.reserve`/`region.restore` implement the slaved operand region, a compiled callee goes through a native call, an interpreted callee through recursion, and a foreign callee through the `os::` boundary.

### 2.8 Unwind
Model A's price: guest frames sit on the native stack, so unwind must walk a native stack mixing interpreted and compiled frames and run each frame's guest Drop. That is harder than tier-0 (Model B pops its own `Vec<Frame>`), and it is the bill for Model A's seamless JIT. Requirement: a guest panic unwinds the native stack frame by frame, runs guest Drops in guest order, and is either caught by a `catch_unwind` frame or leaves `main` (exit 101).

- **Candidate A — reuse the platform unwinder (libunwind + personality).** Compiled frames: Cranelift emits landing pads (supported by 2025) and runs guest Drop, the same way natively compiled Rust does. Interpreted frames: `interp_frame` is a Rust function and participates through landing pads / catch, captures the unwind, runs this frame's guest Drop, then rethrows. Upside: naturally consistent with Cranelift, and it gives ordinary-C abort semantics for free (crossing C = abort). Difficulty: guest unwind and host Rust unwind must share one personality, which requires designing the guest exception object and personality routine.
- **Candidate B — a self-built stack walker (HotSpot style).** Walk the native stack from frame metadata and run Drop ourselves. Upside: full control. Difficulty: it must recognize and step over Cranelift frames by reading their unwind info, which is a large amount of work.

**Candidate A chosen (2026-07-05, because the JIT was fixed on Cranelift, §2.9):** reuse Cranelift's existing landing-pad machinery instead of building one; cg_clif has already blazed MIR→Cranelift+unwinding for all of Rust, which lowers the risk. A spike is still required for guest exception propagation on a mixed interpreted+compiled stack, Drop order, and `catch_unwind`. This is the number-one M4 pre-spike, and candidate B is retained as a fallback.

**Spike 3 passed (2026-07-07):** the host panic mechanism (the same platform unwinder + Rust personality, the concrete form of candidate A) matched native bit-for-bit on a mixed stack for propagation, Drop order (inner first), `catch_unwind`, and the ordinary-C cross-boundary abort covered by the probe, including re-entry into mixed execution from inside a landing pad (a cleanup chain calling a compiled helper). **Candidate A is seated; candidate B is retired to a paper fallback.** The frame-ABI unwind dimension took shape: interpreted frame = `CleanupGuard` + dynamic `unwind_edge` (dynamic LSDA) + region restore; compiled frame = static LSDA + landing pad; a single native stack means the unwinder is naturally inner-first per frame with zero VM-side coordination. Residuals: real Cranelift LSDA emission is left for an M4 re-check (the same checkpoint as the vmctx internal convention), and the JIT calling convention must be unwind-capable (plain `"C"` = abort shim, the fallback for ordinary C abort).

**Spike 5 narrowed the residuals (2026-07-07):** CFI propagation was verified with real Cranelift — `create_unwind_info` → gimli `.eh_frame` → self-registration via `__register_frame`, after which a guest panic correctly passes through a real JIT frame (running bare gives the expected SIGABRT because cranelift-jit does not register system `.eh_frame`; its wasmtime-unwinder exception path does not interoperate with the host unwinder and is formally not adopted). The only M4 residual is landing pad/LSDA, i.e. running drop glue and catch inside a JIT frame, with the cg_clif personality/exception-table precedent. i2c/c2i/cc→cc direct calls were also confirmed with real Cranelift, so the §2.4 adapter model is empirical rather than a stand-in.

### 2.9 JIT backend and bytecode distribution (decided 2026-07-05)
**Backend = Cranelift, hidden behind `JITBackend`.** Cranelift is built for JIT (≈10× faster compilation than LLVM; code quality ≈2% slower than V8 and ~14% slower than LLVM), which fits the target of JIT ≈ debug build where compile latency matters and peak quality does not. cg_clif already maps MIR→Cranelift for all Rust (Rust ABI, layout, 2025 unwinding), so its knowledge is reused; Wasmtime/Wasmer/SpiderMonkey baseline are production validation.

- **`JITBackend` trait:** `compile(BytecodeInstance) -> (code ptr, unwind info, ...)`; the VM core talks only to the trait and Cranelift is the first impl (the same discipline as os::, P7). copy-and-patch (CPython 3.13 style; no runtime backend dependency and more portable, but worse code and memory bloat) is recorded as an alternative to evaluate if less runtime dependency is wanted later.
- **Coupling is controllable:** only (1) the calling convention and (2) the unwind model cannot be abstracted away, and neither is Cranelift-specific — (1) we already use the Rust ABI (tier-0 `fn_abi`) and cg_clif does too, so the shared convention is "Rust ABI" and co-design cost is low; (2) landing-pad unwind is Rust's native way, so binding it ≈ binding "how Rust unwinds", which is unavoidable. Nothing is sacrificed to Cranelift in the memory, thread, metadata or real-address model.

**Bytecode stays close to MIR, in a two-level structure.** Keeping the bytecode near MIR (not sinking to CLIF level) lets the interpreter and the Cranelift JIT share one MIR-level source of truth and reuse cg_clif's MIR→Cranelift. Distribution and execution are two levels:

```
mirvmc:  rustc front end (all checks) → Stable MIR (rustc_public + serde) → serialize into a .mirvm artifact (versioned, ≈ .class/.jar; built on Stable MIR rather than raw internal MIR, so there is a stability story)
mirvm runtime "class loading":  .mirvm → freeze layout per target (C8) → lower to
                    ├─ the interpreter's resolved register bytecode (offsets/calls/vtable fully resolved, §2.5/§2.6)
                    └─ the JIT input fed to Cranelift (reuse cg_clif MIR→CLIF) (once per platform, cached; ≈ Java classfile→verify→interpret/JIT, CPython .pyc→specialize→JIT)
```

- **Base = Stable MIR / `rustc_public`:** it runs the full rustc analysis (all checks) and serde-serializes monomorphized bodies plus layout-bearing type metadata plus symbol names into a self-contained file; the consumer does not link rustc. It specifically solves MIR version binding (a SemVer conversion layer).
- **Version binding is honest:** mirvm bytecode is like a classfile with a version number — the runtime must match or convert (manageable; not "any mirvm runs any bytecode forever").
- **Distribution format = multi-target packaging (fat artifact), decided 2026-07-05 with user confirmation.** A single artifact that runs on any target is rejected for full Rust (§3). mirvmc instead runs the front end once per selected triple (rustc cross-compilation) and packages N target-specific Stable-MIR sections; the runtime picks the matching section to load. This gives the consumer zero toolchain and one file covering common platforms (the real value of .jar), reuses the front end, skips codegen/linking (fast), and can still JIT at runtime. The cost is N cross-compilations by mirvmc and an artifact ×N (metadata only, compressible and downloadable per section). os:: is selected per platform at build time (Linux/macOS impl) and is independent of the distribution format.
- **Section format:** `.mirvm` is one container with a target index header (triple → section offset); each section is that triple's Stable-MIR serialization plus frozen metadata. The runtime looks up its own triple; on a hit it loads, on a miss it reports "unsupported platform".
- **On demand:** sections can be compressed individually and the format can support downloading only the matching section (network distribution).
- **Default triple set:** common combinations of x86_64/aarch64 × linux(gnu/musl)/darwin/windows; mirvmc is configurable.

### 2.10 Real OS threads (C8)
Each guest thread is one OS thread: its interpreted frames use that OS thread's native stack, its slaved operand region is a private block on that thread, and compiled frames likewise run on each OS thread's native stack. **Thunk re-entry (native→interp, C8):** a pthread thread_start or C callback thunk is a native stub equivalent to c2i: on the current OS thread it calls `interp_frame(thread_start_instance, args, thread_local_region)`. Because the operand region and the native stack are both per-thread, this is naturally concurrency-safe. **Sync requirements:** `BytecodeBody` / frozen metadata are read-only shared after publication (C8); the operand region is per-thread private; Rust heap allocation goes through a per-thread arena (C8). Therefore the engine is `Sync` and there is no GIL (VM tier).

### 2.11 Deep recursion and stack overflow
- Guest deep recursion becomes host `interp_frame` deep recursion plus operand-region growth, bounded by the native stack limit (≈ native Rust, faithful, §3.5); compiled frames are smaller than interpreted frames, so hot recursion goes deeper once compiled. Graceful capture: `interp_frame` may check the remaining native stack at entry (guard page / stack pointer threshold) and raise a guest stack overflow at the boundary (≈ native abort).

## 3. Boundaries
- **Model B is not supported.** Its guest calls do not recurse the native stack (flat loop + `Vec<Frame>`), so a compiled frame's interpreted caller is not on the native stack and unwind cannot pass through. Adopting a flat-frame model reopens the JIT interop contract.
- **Independent suspension of interpreted frames is rejected.** The slaved operand region is LIFO; mirvm has no coroutines (§async).
- **`alloca` for interpreted locals is rejected unless evidence reopens it.** Hot functions already use native frames and SSA; `alloca` changes only the cold interpreter and adds stack probing, zeroing, unwind and checked address tracking. Reopen trigger: real interpreter load proves an end-to-end benefit (2026-08-12 ruling replacing the 2026-07-05 migration promise).
- **A HotSpot-style assembly template interpreter is rejected.** It manipulates the native SP explicitly at assembly level, which is too heavy; tree-walking in safe Rust achieves the same Model A interop. It survives only as an evidence-triggered performance candidate.
- **Unwind candidate B (self-built stack walker) is retired to a paper fallback.** Candidate A reuses the platform unwinder, and cg_clif already blazed MIR→Cranelift+unwinding. Reopen trigger: candidate A fails on a mixed stack.
- **Bare `cranelift-jit` execution and the wasmtime-unwinder exception path are rejected.** Bare cranelift-jit does not register system `.eh_frame`, so a guest panic crosses a JIT frame with no handler (bare run = expected SIGABRT), and mirvm therefore self-registers the `.eh_frame` via `__register_frame`; the wasmtime-unwinder path does not interoperate with the host unwinder and is not adopted.
- **Plain `"C"` as the JIT calling convention is rejected** because it is not unwind-capable. It is kept only as the abort shim for the ordinary-C boundary.
- **Ordinary C boundary:** a guest exception terminates (abort); a `C-unwind` boundary propagates and runs cleanup (see [`c-unwind-contract.md`](c-unwind-contract.md)).
- **A single-artifact "run any target" distribution is rejected** as theoretically impossible for full Rust. The root obstacle is `cfg`: rustc prunes cfg per `--target`, so different targets are literally different programs and rustc has no target-independent MIR output (compounded by usize, observable layout and const-eval). Java can do it because it has no compile-time target cfg, layout is fixed at JVM load time, and primitives are fixed-size; Rust violates all three, which is an inherent property of the language.
- **A missing triple must be refused, not guessed.** When the fat artifact carries no matching section, the runtime reports "unsupported platform".
- **Tiering policy is not fixed here.** When to compile, whether to OSR (not first: compile whole methods at call boundaries), and deoptimization are M5 decisions; this design guarantees only seamless plug-in after compilation.

## 4. Verification
Runtime behavior is judged against fixed native output: the interpreter is the reference and the JIT must agree byte-for-byte.

| Evidence | Proves |
|---|---|
| `./tests/run.sh suite runtime.semantics` — unwind section, 13 items: 9 existing unwind/recovery semantics, plus an uncaught guest payload dropping exactly once for interpreter and JIT, cleanup then continuing to call and a second panic, plus interpreter and JIT distinguishing a real `lang_start` main panic from normal `Termination` 101 | mixed interpreted+compiled unwind, Drop order, `catch_unwind`, exit 101 |
| `./tests/run.sh suite runtime.c-unwind` — 13 items: interpreter and forced-sync JIT preserve exception identity, Drop, and the ordinary-C termination boundary; a C++ typed exception passes through the whole Engine; a C++ exception terminates at a guest catch; rejection of non-C/System ABIs | the ordinary-C / `C-unwind` partition |
| `./tests/run.sh suite runtime.tsan` — TSan exit code 0, no data-race warnings | per-thread region/arena with shared read-only bytecode (C8) |
| `./tests/run.sh suite runtime.jit-stats` — `demo/jit_unwind_probe.rs` | JIT publication path (`JITBackend`, i2c/c2i) |
| Three-dimension byte-equality: mirvm default / native `cargo run` / `MIRVM_JIT_THRESHOLD=1` | the four transitions agree with native |
| TSan single case: `cd tsan && MIRVM_BUILD_ID=0000000000000000 RUSTFLAGS="-Zsanitizer=thread" cargo +nightly-2026-07-02 run -Zbuild-std --target x86_64-unknown-linux-gnu --release -- <case-id>` | the same, outside the bundled suite |

Historical evidence: Spike 3 (2026-07-07, passed) and Spike 5 (2026-07-07) are recorded in §2.8. Foundation spikes: (1) minimal skeleton — `interp_frame` tree-walking + slaved operand region + register bytecode, run on pure computation (fib) and differentially compared against the tier-0 `InterpCx` oracle; (2) interp↔compiled adapter spike with one or two functions hand-written or minimally Cranelift-compiled; (3) the unwind spike (top item); (4) concurrency spike — N host threads each running `interp_frame` over shared read-only bytecode and per-thread region/arena, passing TSan.

## 5. Open items
1. **Unwind mechanism:** candidate A vs B is decided (A seated, B retired), but real Cranelift landing-pad/LSDA emission remains an M4 re-check, at the same checkpoint as the vmctx internal convention. Reopen trigger: a mixed-stack failure.
2. **Frame local storage (decided):** the interpreter formally uses the slaved ByteRegion; `alloca` is not presumed faster and reopens only when real interpreter load proves an end-to-end benefit.
3. **Calling-convention details:** whether to base the convention on the platform C ABI or a custom Cranelift calling convention, and the concrete treatment of aggregate passing aligned with the rustc ABI.
4. **JIT tiering policy (M5):** when to compile, whether to OSR (not first: compile whole methods at call boundaries), and deoptimization. This design guarantees only seamless plug-in after compilation.
5. **thunk/closure generation:** libffi closures or self-generated small stubs, merged with the c2i adapter.
6. **Bytecode verification/lowering pipeline:** the MIR→bytecode pass and the caching plus content addressing of frozen metadata (echoing the sysroot cache).
7. **vmctx passing mechanism** (→ [`vmctx-passing.md`](vmctx-passing.md), 2026-07-07): the boundary is already forced — FFI escape pointers, callbacks and signal entry points must look up the execution state for the current thread through TLS plus lazy attach (the `AttachCurrentThread` analogue, owned by `os::thread`). Three constraints force it: a plain-C escape cannot carry a hidden parameter in its signature, the ctx is one per thread so a capturing thunk is wrong across threads, and signals run on arbitrary threads. The internal convention is still to be fixed in M4: an explicit vmctx first parameter versus a Cranelift pinned register (`r15`), paired with multiple entries (`f_boundary` reads TLS → tail-calls `f_fast(ctx,…)`, the HotSpot verified/adapter entry analogue). Bonus: thunks narrow back to their own job (only interpreted-state escape needs one). Spikes currently use an explicit parameter.
