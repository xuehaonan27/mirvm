# Startup cache

> Status: Implemented. Owns managed compiler-input identity, prepared launch, base construction/loading and bound execution images.
> Shared fragments and collection remain owned by [dep-sharing-design.md](dep-sharing-design.md);
> the package container remains owned by [modeb-mirvmar-design.md](modeb-mirvmar-design.md).

## Managed inputs

A build recipe decides which inputs a request needs; a generation identifies the actual published
outputs. A generation is the BLAKE3 of a versioned canonical manifest containing target, recipe and
sorted relative member paths, lengths and content hashes. Local paths, inodes and timestamps are
excluded, including the producer's local stamp file. The selector records producer freshness
separately: compiler stat and the rust-src top-level sentinel can trigger rebuilding, but cannot
change the identity of equal outputs under equal build flags. Hand-editing a rust-src leaf is
outside that sentinel's protected surface; removing the sysroot family forces reconstruction.
A recipe match alone never proves output identity.

The initial producer is mirvm's own sysroot. Source files, includes, ordinary extern artifacts and
external sysroots retain full content stamping. A file becomes managed only through the sysroot
owner's resolved generation, never because its path happens to be inside the home. Compiler arguments
name that generation's concrete payload directory. A child may reopen a generation registered in
the same store; an arbitrary directory with a copied manifest is still external.

Publication stages complete regular files, hashes the final bytes, writes the manifest, publishes an
immutable generation, then atomically replaces the small recipe selector. Generations are never
overwritten while readers hold them. They share a store lease with collection: readers and publishers
hold it shared, deletion holds it exclusive. A separate writer lock serializes generation publication.
The lock identities live outside removable families. Existing directory replacement is unsuitable
for a live generation because it removes the previous reader's paths. Old generations remain until
an explicit data-family purge; cache-only collection cannot remove compiler inputs.

Copy and content-validation consistency compare complete sorted namespaces, directory device/inode
identities and every file's full revision before and after copying or hashing. Directory revision
timestamps alone do not invalidate those operations: filesystems may report a delayed timestamp
update after child creation. Changed names, replaced directories and changed file identities still
reject the snapshot. The receipt fast path continues to compare complete metadata; a changed
directory revision requires full content validation before a receipt refresh.

A discardable local receipt records member and directory identities. Normal selection checks the
small manifest and these metadata observations. A missing or changed receipt requires a complete
content check; equal bytes refresh the receipt, unequal bytes reject the generation. A receipt is
never a content identity. Unsupported external in-place mutation and silent corruption invisible to
metadata are outside the managed fast path's guarantee. External inputs retain full hashing, and
bytecode, frozen data and machine-code integrity checks remain mandatory.

Input manifests record managed member references separately from external file stamps. A hit compares
the recorded generation with the current request's pinned generation: an old generation remaining on
disk is insufficient. Compilation records the handle it actually used, not a path resolved again
after compilation. Managed provenance expands into ordinary stamps when writing a package.

## Std base loading

The base is a pre-lowered std runtime closure, not the MIR-rich compiler sysroot and not every
possible std generic instance. The current producer and loader below use a disposable local cache.
The installed-std lifetime and release delivery contract belong to
[distribution-design.md](distribution-design.md#27-installed-execution-products).

A base miss is constructed in the user's `after_analysis` callback before program lowering. Its
roots are `std::rt::lang_start::<()>`, the guest panic cleanup function and its payload drop glue.
The worklist closes over these roots without the user's mono collector, main address, native
archives/global assembly or allocator shims. Link simulation accepts only definitions from the
selected sysroot; every enqueued instance must remain sysroot-owned. Program lowering still
materializes the complete native link inputs and supplies the main entry and custom allocator.
Base allocation operations use Engine builtins, which dispatch through that program's allocator.

Base function identities use the defining crate as the instantiating crate in rustc's mangling
routine. This prevents a user's crate name from entering shared generic identities; native symbol
resolution retains rustc's actual names. The isolated preparation command uses the same roots.
Equal producer settings yield equal base bytes across programs, including different main return
types and allocators.

The lookup key includes build identity, sysroot stamp and the semantic compiler profile. Check
flags are normalized to their effective defaults so scripts and Cargo's explicit dev defaults
share a base. Target/layout flags, cfg and remaps affecting imported sysroot source locations remain
in the profile. Program identity, native inputs, debug output, codegen partitioning, thread count
and MIR optimization level do not partition it. MIR optimization is a producer choice with the same
runtime semantics; the layer key also includes the whole-file content digest, so a rebuilt base
with different bodies or ids invalidates every consumer of the old absolute layout.

Postcard metadata excludes bodies and stores export and entry-link indexes in sorted order. An
aligned function archive follows; a BLAKE3 checksum covers both. Loading checks integrity,
generation, stamp, full profile and fixed domain before mapping. Structural and semantic checks
inspect archive views without decoding owned functions. The session also checks the lowering
fingerprint. A fresh base retains its mapping, replaces its owned bodies with the just-encoded
archive and publishes best effort, without another compiler or a file reload. Publication failure
still permits execution and a self-contained bound image. An unpublishable fixed domain falls back
to ordinary lowering. A frontend error never starts base construction.

Layer merging moves function-table segments and their immutable owners, preserving absolute ids,
entry identity and initialized decode cells. Verification walks owned or archived bodies against the
merged metadata. Bound-image publication copies existing archive chunks and encodes only owned
segments; it does not force base decoding. A function is decoded on demand for interpretation or JIT.
Fork preparation and the capture-domain rewrite deliberately materialize bodies when their existing
lifecycle rules require it. The shared archive wire layout is specified by the package
[FUNCS contract](modeb-mirvmar-design.md#22-container).

The driver ledger's `base-load` is nested inside `frontend` or `cache-load`. On a miss, `base-build`
runs after analysis and measures std lowering, encoding, validation and publication separately from
program `lower`. `layer-load` sums manifest loading before analysis and after a newly built base.
Compiler finalization still completes before Engine entry.

## Prepared launch

The default own dependency track publishes a disposable launch recipe for runnable projects and
frontmatter scripts. Installation still visits the preparation pipeline so missing dependency
templates can be repaired. An unchanged `run` validates the recipe before opening the registry or
constructing the schedule, then enters the bound-image loader. The Cargo compatibility track retains
Cargo's freshness decisions. Verbose and build/resolver diagnostic modes retain the ordinary path
and its cached diagnostics.

The key includes the input path, caller directory, selected binary, rust-version policy, relevant
Cargo/rustflags environment, sysroot selection and compiler options. The record pins the MIRVM build,
root and mutable dependency source trees, manifests, lockfile, config discovery candidates and
includes (including absent optional files), dependency source presence, and build-script records.
Mutable tree fingerprints include file contents as well as paths, sizes and timestamps; additions,
deletions and equal-size rewrites with restored timestamps invalidate them. Registry and locked Git
sources retain the resolver's immutable source contract. Existing build-script rerun rules are
rechecked, including declared file and environment inputs. The sysroot selector must still select the
recorded generation; the image loader independently validates its managed/external compiler inputs,
compile-time environment and native files.

Only compiler-injected environment values and `argv[0]` are replayed. Guest arguments, runtime
environment and working directory belong to the current invocation. No guest is started until the
execution image passes its existing checks. A missing, corrupt or stale record or image falls back to
ordinary preparation; an image miss restores the caller's environment before resolving. Record
publication is atomic and best effort, and the checksum covers the whole recipe. A record without a
successfully published image cannot bypass preparation. `cache purge --ir` removes launch records
along with program and bound images.

`MIRVM_TIMING=1` reports `mirvm-launch: hit validation=...ms` only when both recipe and image succeed.
Validation precedes and is separate from the driver's cache-load interval. Large mutable source
trees still require content reads; this shortcut does not promise constant-time validation of
arbitrary external inputs.

## Bound execution images

Sharing manifests remain the canonical incremental products. A separate disposable execution image
contains the already-bound module, its complete frozen-region snapshots, and process-independent
materialization recipes. Its input header pins compiler arguments, managed/external inputs, build
compatibility and lowering configuration. A hit can load before constructing the shared image stack;
a miss reconstructs from existing layers and publishes a new bound image before guest mutation.

The execution image owns its data rather than referring to mutable global pack locations. It retains
logical addresses, not host pointers. Every instantiation allocates independent frozen memory and
rebuilds native, GOT, entry and asm state. Its removal loses only an optimization. It cannot keep a
partially restored image alive after a missing member or validation failure. A cache hit consumes its
metadata into the instance used for verification and execution, so it need not instantiate twice.
It validates every address directly without collecting obligations for a later instance. Reusable
packages retain their metadata and address obligations and check them on each fresh instance.

Frozen snapshot payloads decode through serde's byte-buffer channel. Their postcard bytes remain
identical to the existing byte-sequence encoding; the change does not weaken domain, length or
relocation checks. Formats providing byte sequences, including JSON, retain the sequence visitor.

`MIRVM_TIMING=1` adds a `mirvm-image-load` ledger for a successful bound-image hit. Its sequential
intervals separate payload integrity, header/input validation, metadata decoding, function archive
structure/index construction, instance restoration plus semantic verification, and assembly
materialization. It starts after the file read and excludes command routing, dependency resolution,
Engine startup and guest execution. The outer phase ledger and whole-process timings remain
necessary when attributing cold-process latency; a cache miss does not emit a successful-load ledger.
The nested `mirvm-image-instance` ledger separates instance restoration, semantic verification and
lazy function-table initialization within `instantiate-verify`. Its intervals must not be added to
the enclosing load ledger. A successful instance check can precede a later materialization failure.

During publication, the same option emits `mirvm-image-roots` and `mirvm-image-store`: distinct
conservative roots, total/reachable function slots, metadata bytes and function-archive bytes. The
census roots the ordinary entry, all address-taken functions and native callbacks, allocator shims
and panic cleanup, then follows direct calls and shared bodies' concrete call adapters. It borrows
archive views without filling body decode cells. An artifact without an ordinary entry retains every
function in this census. These diagnostics do not prune images or alter the exported-function table;
static reachability and actual execution frequency are separate measurements.

## Immutable verification

The loader owns one immutable byte snapshot. Archive bounds, discriminants, lengths and semantic
references are checked against that snapshot before native materialization or execution. Function
views permit verification without constructing temporary owned bodies; demand decoding restores a
body from precisely the verified representation. The function section begins at a 16-byte boundary
and borrows its immutable file allocation. The loader checks the allocation's actual alignment and
copies only when necessary; it never maps a mutable inode. Archive-only relative pointers keep large
address expressions and foreign signatures from enlarging every operand, instruction and block.
Runtime IR keeps its existing layout.

An archived module uses one immutable verification context for its module counts and address owner.
Each function still checks its frame, operands, references and control flow; main panic boundary and
catcher counts are accumulated during that same walk. Owned and archived bodies compile the same
verification rules. No serialized flag, checksum or previously loaded instance substitutes for these
checks.

Verification evidence is an ownership-bearing internal type, never a disk boolean. It binds the
module tables, function bytes and instantiated address context. Only validated transformations may
carry it into Engine construction; arbitrary public modules still require full verification. A
process rebinding must check the new context, not reuse proofs about another Engine's addresses.

## Acceptance

Focused checks cover changed outputs under one recipe; current-generation mismatch with the old one
retained; interrupted publication; concurrent selection and collection; lost receipts; same-size
rewrites with restored timestamps; relocation; missing members; corrupt archive bounds and semantic
references; independent Engine instances; and package mutation after loading. Existing guest output,
unwind, signal and JIT cache oracles remain authoritative.

Unprofiled release measurements distinguish empty derived caches, small cache hits and populated
stores, with alternating binaries and identical inputs. Managed steady-state hits must stop reading
the sysroot payload for freshness. Execution-image hits must avoid shared pack discovery and binding.
Base loading, layer merging, verification and bound-image publication must preserve demand decoding.
Archive verification must avoid temporary owned bodies and redundant instantiation.
CPU profiles and syscall counts establish which work disappeared; elapsed time alone is insufficient.
