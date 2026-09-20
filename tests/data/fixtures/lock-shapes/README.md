# Recorded `Cargo.lock` shapes

Cargo output recorded once, so the lockfile byte-identity test can cover shapes the fixture
projects do not produce: a git-source package row, and unused patches with a path source, a git
source and a sparse-registry source. The `file://` and `127.0.0.1` URLs are from the recording
session; only the shape matters, nothing here is resolved.

No sample carries a checksum inside `[[patch.unused]]`, because cargo never writes one: an unused
patch is by definition absent from the resolve, so no checksum for it is ever recorded (see the
test's comment).
