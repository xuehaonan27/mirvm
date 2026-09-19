# Frame Layout, Calling Convention, and Bytecode Format

> Status: Implemented · Scope: Model A native frame kinds, the interpreted frame and where guest
> locals live, frame descriptors, the calling convention and the four interp↔compiled transitions,
> and the register-based bytecode. Unwind semantics are normative in
> [c-unwind-contract.md](c-unwind-contract.md); concurrency in
> [concurrency-arch.md](concurrency-arch.md).

## 1. Contract

1. **One activation, one native frame.** Every guest call activation maps to exactly one native stack
   frame, so interp↔compiled calls are native calls and unwind walks a single stack. This is the
   Model A property the JIT depends on.
2. **Guest locals are orthogonal to JIT interop.** Interop requires only that call activity — control
   flow and unwind — sit on the native stack. Compiled code never reads an interpreted frame's
   locals, and cross-function traffic uses only the calling convention.
3. **Interpreted-frame locals live in the slaved operand region.** Each thread owns one operand
   region, slaved to `interp_frame` recursion (LIFO, in step with entry and exit); it is never
   suspended independently, because mirvm has no coroutines.
4. **Region SP discipline.** Entering `interp_frame` bumps the region SP by the frame's
   `total_frame_size`; leaving it, and unwind, restore the region SP.
5. **Register slot = MIR local.** `r0..rn` correspond to MIR `_0.._n`, each sized and aligned by its
   type's frozen layout. Slots are real addresses, so `&local` handed to C is natural.
6. **Frame storage stays uncoupled from the safety mode.** Fast and checked mode are orthogonal axes
   meeting at exactly one predicate, `GuestMemory::contains(addr) -> bool`: fast never calls it,
   checked calls it before any raw dereference, and `FrameStorage` provides it. The slaved region
   answers it with a range compare; any future storage must keep that contract.
7. **No `alloca` for interpreted locals.** Hot functions already use native frames and SSA under
   Cranelift, and `alloca` would change only the cold interpreter while adding stack probing,
   zeroing, unwind and checked address tracking. It reopens only on real interpreter load proving an
   end-to-end benefit.
8. **Freeze metadata at lowering; never touch `tcx` at run time.** Layout, call targets, vtable
   slots, drop glue and constants resolve into concrete numbers during lowering. The product is an
   immutable `BytecodeBody`, read-only and thread-shared after publication with lock-free reads.
9. **Adapters only move arguments.** i2c/c2i move guest arguments between operand-region slots and
   convention registers; compiled→compiled is a direct native call with zero adaptation.
10. **The JIT calling convention must be unwind-capable.** Plain `"C"` is kept only as the abort shim
    for an ordinary C boundary.
11. **Exception partition.** Across an ordinary C boundary a guest exception ends (abort); across a
    `C-unwind` boundary the original exception passes through and cleanup runs.
12. **`Sync` engine, no GIL, one guest thread per OS thread.** `BytecodeBody` and frozen metadata are
    read-only shared; the operand region is per-thread private; Rust heap allocation goes through a
    per-thread arena.
13. **JIT backend and distribution are fixed.** Cranelift sits behind the `JITBackend` trait and the
    VM core talks only to the trait. Distribution is a multi-target fat `.mirvm` artifact; a single
    artifact that runs on any target is rejected, and a missing triple is reported as
    "unsupported platform".

## 2. Model

### 2.1 Frame kinds

A call activation is on the native stack even when its locals are not:

```text
one OS thread's native stack (grows downward):
  interp_frame(main)   [interpreted]  Rust frame: dispatch state + main's operand region
  <compiled> foo       [compiled]     Cranelift frame: foo's locals and spills, pure native
  interp_frame(bar)    [interpreted]  <- current
```

`main(interp) -> foo(compiled) -> bar(interp)` is one native stack, each crossing an i2c/c2i adapter,
and unwind walks that stack. An interpreted frame is one Rust call to `interp_frame`, so a guest call
becomes host recursion; a compiled frame is a Cranelift-generated native frame, so a guest call is a
direct native call.

Tree-walking is chosen over a HotSpot-style assembly template interpreter: the latter manipulates the
native SP explicitly to push and pop interpreted frames, which is too heavy, while tree-walking maps
guest calls to native calls in safe Rust and reaches the same interop. Its cost is one host
`interp_frame` overhead per interpreted frame, which is acceptable because the interpreter is the
cold tier.

### 2.2 In-frame layout and guest local storage

A compiled frame is Cranelift's business — this contract defines only its calling convention and
unwind info.

An interpreted frame's size is dynamic (it depends on the function's local count and types) while
Rust locals are fixed-size, so guest locals cannot be inlined into the native stack. Instead each
thread owns one contiguous operand region, slaved to `interp_frame` recursion:

```text
per-thread operand region (SP slaved to interp_frame recursion):
  [main's register slots r0..rn][bar's slots r0..rm]...   <- region SP
  enter: bump region SP by the frame descriptor to reserve this frame's slots
  exit / unwind: restore region SP
```

### 2.3 Frame descriptors

One per function, computed at lowering, frozen, and consumed by `interp_frame` to build a frame:

```text
FrameDescriptor {
    reg_count,                                        // registers = MIR locals
    reg_slots: [{offset, size, align, ty_layout_id}], // each register's slot in the operand region
    total_frame_size,                                 // how much is reserved in the region
    drops: [{reg, drop_glue_instance, cond}],         // registers needing Drop + frozen drop glue
    cleanup_edges,                                    // unwind targets (catch/cleanup) from MIR
}
```

### 2.4 Calling convention and the four transitions

Cranelift-compiled guest functions use one defined mirvm calling convention based on the platform C
ABI. Scalars and pointers pass in registers (under real addresses a pointer is a real address);
aggregates follow the rustc ABI and reuse rustc layout. Function identity is the monomorphized
`Instance`, which has one code pointer after compilation.

```text
compiled -> compiled : direct native call, zero adaptation
interp   -> compiled : i2c adapter moves arguments from operand-region slots into convention
                       registers, then native-calls the code pointer
compiled -> interp   : c2i adapter: compiled code calls c2i(instance, args...), which moves the
                       arguments into the new interpreted frame's slots and calls interp_frame
interp   -> interp   : interp_frame recursion (host recursion)
```

Adapters only move arguments, and are cheap because both sides are on the same native stack. An
uncompiled hot `Instance` either keeps interpreting or requests compilation from the service thread;
this design fixes no tiering policy, only that a compiled callee plugs in seamlessly.

### 2.5 Bytecode format and MIR correspondence

The bytecode is register-based, with registers corresponding to MIR locals. MIR is already
register/place-based (`_0.._n` plus projections), so register form lowers almost mechanically and
uses fewer, faster instructions — the same choice Lua and Dalvik made. The bytecode is "MIR after
metadata resolution and flattening".

| MIR | mirvm bytecode |
|---|---|
| local `_i` | register `ri` (one slot in the operand region) |
| place projection `_3.2`, `(*_4)[i]` | resolved offset arithmetic (layout frozen, offset computed at lowering) |
| rvalue (BinaryOp/Ref/Cast/Aggregate…) | corresponding instruction writing the destination register |
| `SwitchInt` (match, discriminant, async state machine) | `switch ri -> [value:target]` |
| `Call(f, args, dest, unwind)` | `call <InstanceId/code ptr>, [arg regs], dest reg, unwind blk` |
| `Drop(place, unwind)` | `drop ri` using the frozen drop glue instance |
| `Return` | `ret r0` |

Illustrative instruction sketch, not the full set:

```text
bin <op> rd, ra, rb / un <op> rd, ra      # arithmetic with frozen overflow semantics
load rd, [rbase + off] / store [rbase+off], rs / ref rd, rplace
field rd, rbase, off / index rd, rbase, ridx, elem_sz
discr rd, rbase, disc_enc / setdiscr rbase, variant, disc_enc    # frozen encoding and niche
jump blk / switch ri -> [v0:blk0, v1:blk1, ...] / ret ri
call <target>, [args], rd, unwind=blk     # direct InstanceId, or a vtable slot for dyn
intrinsic <id>, [args], rd                # engine-implemented or forwarded to os::
```

Every projection and discriminant is a frozen offset, so the runtime never queries `tcx`, and
`switch` serves the async state machine with no special support.

### 2.6 Metadata freezing

Lowering one monomorphized `Instance` resolves and freezes: layout (per-type size, align, field
offsets, discriminant and niche encoding), call targets (direct call to the monomorphized instance's
body id or code pointer; dyn call to a vtable slot number), the vtable layout, drop glue for every
position that needs Drop, interned constants, and intrinsics marked as builtins or resolved.

### 2.7 Execution

```rust
// one call = one interpreted frame (on the native stack)
fn interp_frame(body: &BytecodeBody, args: Args, region: &mut OperandRegion) -> Value {
    let base = region.reserve(body.desc.total_frame_size);   // slaved bump
    load_args_into_slots(region, base, args, &body.desc);
    loop {
        match next_instruction(body) {
            Call(target, aregs, rd, unwind) => {
                let a = gather(region, base, aregs);
                let r = match target {
                    Interp(callee) => interp_frame(callee, a, region),  // host recursion
                    Compiled(ptr)  => i2c_call(ptr, a),                // native call + adaptation
                    Foreign(h)     => os::dispatch(h, a),              // os:: boundary
                };
                write(region, base, rd, r);
            }
            Ret(ri) => { let v = read(region, base, ri); region.restore(base); return v; }
            ...
        }
    }
}
```

Pure Rust and tree-walking: `reserve`/`restore` implement the slaved region, a compiled callee goes
through a native call, an interpreted callee through recursion, and a foreign callee through the
`os::` boundary.

### 2.8 Unwind

Model A's price: because guest frames sit on the native stack, unwind must walk a stack mixing
interpreted and compiled frames and run each frame's guest Drop. That is the bill for seamless JIT
interop, and it replaces the flat frame stack an earlier model used.

The mechanism is the platform unwinder plus Rust personality, the same way natively compiled Rust
unwinds:

- **Compiled frames**: Cranelift emits landing pads and runs guest Drop.
- **Interpreted frames**: `interp_frame` is a Rust function and participates through landing pads and
  catch — it captures the unwind, runs this frame's guest Drop, then rethrows.
- The interpreted frame is a `CleanupGuard` with a dynamic `unwind_edge` (dynamic LSDA) plus region
  restore; the compiled frame is a static LSDA plus landing pad. With one native stack the unwinder
  is naturally inner-first per frame, so no VM-side coordination is needed.
- Cranelift's `.eh_frame` is self-registered through `__register_frame`. Bare `cranelift-jit` does
  not register system `.eh_frame` (a guest panic would cross a JIT frame with no handler), and the
  wasmtime-unwinder exception path does not interoperate with the host unwinder, so neither is used.
- A self-built HotSpot-style stack walker was the alternative. It is retained only as a paper
  fallback: it would have to recognize and step over Cranelift frames by reading their unwind info,
  which is a large amount of work for no gain while the platform unwinder works.

A guest panic therefore unwinds frame by frame running guest Drops in guest order, and is either
caught by a `catch_unwind` frame or leaves `main` with exit 101.

### 2.9 JIT backend and bytecode distribution

**Backend: Cranelift behind `JITBackend`.** Cranelift is built for JIT — roughly 10× faster
compilation than LLVM with code quality about 2% behind V8 and 14% behind LLVM — which fits a target
of "JIT about as good as a debug build", where compile latency matters and peak quality does not.
cg_clif already maps MIR to Cranelift for all of Rust, so that knowledge is reused. The trait is
`compile(BytecodeInstance) -> (code pointer, unwind info, ...)`, and the VM core talks only to the
trait; copy-and-patch is recorded as an alternative if less runtime dependency is wanted later. Only
two things cannot be abstracted away — the calling convention and the unwind model — and neither is
Cranelift-specific, since both are "how Rust does it".

**Bytecode stays close to MIR, in two levels.** Keeping it near MIR rather than sinking to CLIF lets
the interpreter and the JIT share one source of truth and reuse cg_clif. `mirvmc` runs the rustc
front end, extracts Stable MIR (`rustc_public` + serde) and serializes a versioned `.mirvm`; the
runtime freezes layout per target and lowers either to the interpreter's resolved register bytecode
or to Cranelift's input, cached once per platform. Building on Stable MIR rather than internal MIR is
what gives the stability story, and version binding is explicit: the runtime must match or convert,
not "any mirvm runs any bytecode forever".

**Distribution is a multi-target fat artifact.** A single artifact that runs on any target is
impossible for full Rust (see §3), so `mirvmc` runs the front end once per selected triple and
packages N target-specific Stable-MIR sections; the runtime picks the matching section. The consumer
needs no toolchain, and one file covers the common platforms. The cost is N cross-compilations and an
artifact N times larger in metadata, which is compressible and can be downloaded per section. The
container is a target index header mapping triple to section offset; a miss reports
"unsupported platform". Default triples are the common x86_64/aarch64 × linux(gnu/musl)/darwin/windows
combinations, and `mirvmc` is configurable.

### 2.10 Real OS threads

Each guest thread is one OS thread: its interpreted frames use that OS thread's native stack, its
slaved operand region is a private block on that thread, and compiled frames run on the same native
stack. A `pthread` thread start or C callback thunk is a native stub equivalent to c2i: on the current
OS thread it calls `interp_frame(thread_start_instance, args, thread_local_region)`. Because both the
operand region and the native stack are per-thread, this is naturally concurrency-safe.

### 2.11 Deep recursion and stack overflow

Guest deep recursion becomes host `interp_frame` recursion plus operand-region growth, bounded by the
native stack limit, so it behaves like native Rust. Compiled frames are smaller than interpreted
frames, so hot recursion goes deeper once compiled. `interp_frame` may check the remaining native
stack at entry through a guard page or stack-pointer threshold and raise a guest stack overflow at
the boundary rather than faulting.

## 3. Boundaries

- **A flat-frame model is not supported.** Its guest calls do not recurse the native stack, so a
  compiled frame's interpreted caller is not on the stack and unwind cannot pass through. Adopting
  one reopens the JIT interop contract.
- **Independent suspension of interpreted frames is rejected.** The slaved operand region is LIFO and
  mirvm has no coroutines.
- **`alloca` for interpreted locals is rejected** unless real interpreter load proves an end-to-end
  benefit; see §1.7.
- **A HotSpot-style assembly template interpreter is rejected** as too heavy; it survives only as an
  evidence-triggered performance candidate.
- **A self-built stack walker is retired** to a paper fallback, reopening only if the platform
  unwinder fails on a mixed stack.
- **Bare `cranelift-jit` execution and the wasmtime-unwinder exception path are rejected** — the first
  does not register system `.eh_frame`, the second does not interoperate with the host unwinder.
- **Plain `"C"` as the JIT calling convention is rejected** because it is not unwind-capable; it is
  kept only as the abort shim.
- **A single-artifact "run any target" distribution is rejected** as impossible for full Rust. `cfg`
  is pruned per `--target`, so different targets are literally different programs, and rustc has no
  target-independent MIR output; this is compounded by `usize`, observable layout and const-eval.
  Java can do it only because it has no compile-time target cfg, fixes layout at load time and has
  fixed-size primitives — Rust violates all three.
- **A missing triple is refused, not guessed**, as "unsupported platform".
- **Tiering policy is not fixed here.** When to compile, whether to OSR (not first: compile whole
  methods at call boundaries) and deoptimization are separate decisions; this design guarantees only
  seamless plug-in after compilation.

## 4. Verification

Runtime behavior is judged against fixed native output: the interpreter is the reference and the JIT
must agree byte-for-byte.

- `make mode M=vmcall  # the exported-entry cases`, unwind section (13 items): mixed interpreted and compiled unwind,
  Drop order, `catch_unwind` and exit 101, including an uncaught guest payload dropping exactly once
  in both tiers and a real `lang_start` main panic being distinguished from a normal
  `Termination` 101.
- `make case C=c-unwind` (13 items): exception identity, Drop and the ordinary-C termination
  boundary across interpreter and forced-sync JIT, a C++ typed exception passing through the whole
  Engine, a C++ exception terminating at a guest catch, and rejection of non-C/System ABIs.
- `make case C=tsan`: TSan exit code 0 with no data-race warnings, proving the per-thread
  region and arena with shared read-only bytecode.
- `make case C=jit-stats` (`data/programs/jit_unwind_probe.rs`): the JIT publication path
  through `JITBackend` and the i2c/c2i adapters.
- Three-dimension byte-equality (mirvm default, native `cargo run`, `MIRVM_JIT_THRESHOLD=1`): the
  four transitions agree with native.

## 5. Open items

1. **Landing-pad/LSDA emission in real Cranelift** remains to be re-checked, at the same checkpoint
   as the vmctx internal convention. Reopen trigger: a mixed-stack failure.
2. **Calling-convention details**: whether to base the convention on the platform C ABI or a custom
   Cranelift calling convention, and the concrete treatment of aggregate passing aligned with the
   rustc ABI.
3. **Tiering policy**: when to compile, whether to OSR and deoptimization.
4. **Thunk and closure generation**: libffi closures or self-generated small stubs, merged with the
   c2i adapter.
5. **Bytecode verification and lowering pipeline**: the MIR→bytecode pass, plus caching and content
   addressing of frozen metadata.
6. **vmctx passing mechanism** — the boundary is forced: FFI escape pointers, callbacks and signal
   entry points must look up the current thread's execution state through TLS plus lazy attach,
   because a plain-C escape cannot carry a hidden parameter, the context is one per thread so a
   capturing thunk is wrong across threads, and signals run on arbitrary threads. The internal
   convention is still open: an explicit vmctx first parameter versus a Cranelift pinned register,
   paired with multiple entries. See [vmctx-passing.md](vmctx-passing.md).
