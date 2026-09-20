# Recorded `Cargo.lock` shapes

Cargo output recorded once, so the lockfile byte-identity test can cover shapes the fixture
projects do not produce: a git-source package row, and an unused patch with and without a source.
The `file://` URLs are from the recording session; only the shape matters, nothing here is resolved.
