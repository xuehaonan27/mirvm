use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(crate) mod capture;
pub(crate) mod decode;
pub(crate) mod format;
pub(crate) mod tool;

// This is an implementation default for the first measured slice, not a
// frozen product limit. The syscall workload benchmark decides the eventual
// automatic memory budget.
const INITIAL_PAGE_BUDGET_BYTES: usize = 64 << 20;

/// Options for one process-wide capture session.
///
/// The fork generation is settled here, not inside the writer: callers that
/// name the output after the generation (the CLI's
/// `events-<pid>-<generation>.mlog`) must use the same value the file header
/// records, so both come from one claim.
#[derive(Clone, Debug)]
pub struct CaptureOptions {
    output: PathBuf,
    process_generation: u64,
}

impl CaptureOptions {
    pub fn new(output: impl AsRef<Path>) -> Self {
        Self {
            output: output.as_ref().to_path_buf(),
            process_generation: capture::claim_process_generation(),
        }
    }

    /// Claim the generation, then hand it to the option that names the file, so
    /// the name and the header agree.
    pub fn with_process_generation(mut self, process_generation: u64) -> Self {
        self.process_generation = process_generation;
        self
    }

    /// Fork generation this session belongs to. A `fork` child takes a new
    /// generation, so a parent and its children never share one.
    pub fn process_generation(&self) -> u64 {
        self.process_generation
    }
}

/// Totals proven by a cleanly committed capture file.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CaptureSummary {
    pub encoded_records: u64,
    pub producer_drops: u64,
    pub sink_loss: u64,
    pub producers: u64,
}

/// Result of asking the writer to finish within a caller-selected deadline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureFinish {
    Finished(CaptureSummary),
    InProgress,
}

/// Owner of one process-wide MIRVM event capture.
///
/// Start the session before constructing the Engines that should be captured;
/// the first implementation selects its execution domain at Engine creation.
/// Existing plain Engines are not migrated while guest frames are running.
///
/// Dropping the handle requests a stop but does not block. Call [`Self::finish`]
/// when a clean `SessionEnd` and final rename are required.
pub struct CaptureSession {
    inner: capture::CaptureSession,
}

impl CaptureSession {
    pub fn start(options: CaptureOptions) -> io::Result<Self> {
        capture::CaptureSession::start(
            capture::StartOptions::new(options.output, INITIAL_PAGE_BUDGET_BYTES)
                .with_process_generation(options.process_generation),
        )
        .map(|inner| Self { inner })
    }

    pub fn request_stop(&self) {
        self.inner.request_stop();
    }

    pub fn finish(&mut self, timeout: Duration) -> io::Result<CaptureFinish> {
        match self.inner.finish(timeout)? {
            capture::FinishStatus::Finished(summary) => {
                Ok(CaptureFinish::Finished(CaptureSummary {
                    encoded_records: summary.encoded_records,
                    producer_drops: summary.producer_drops,
                    sink_loss: summary.sink_loss,
                    producers: summary.producers,
                }))
            }
            capture::FinishStatus::TimedOut => Ok(CaptureFinish::InProgress),
        }
    }
}
