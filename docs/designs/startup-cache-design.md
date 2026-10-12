# Startup cache

> Status: Implemented. Owns managed compiler-input identity and bound execution images.
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
Reusable packages retain their metadata and check address obligations on each fresh instance.

## Immutable verification

The loader owns one immutable byte snapshot. Archive bounds, discriminants, lengths and semantic
references are checked against that snapshot before native materialization or execution. Function
views permit verification without constructing temporary owned bodies; demand decoding restores a
body from precisely the verified representation. The function section begins at a 16-byte boundary
and borrows its immutable file allocation. The loader checks the allocation's actual alignment and
copies only when necessary; it never maps a mutable inode. Archive-only relative pointers keep large
address expressions and foreign signatures from enlarging every operand, instruction and block.
Runtime IR keeps its existing layout.

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
Archive verification must avoid eager function-body construction and repeated whole-module walks.
CPU profiles and syscall counts establish which work disappeared; elapsed time alone is insufficient.
