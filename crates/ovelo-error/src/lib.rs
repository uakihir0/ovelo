//! Shared error taxonomy for the ovelo runtime.
//!
//! The CLI, GUI and diagnostics all speak the same stable error categories so
//! that users and tooling can key off them (design doc chapter 14).

use std::fmt;

/// Stable, user-visible error category (design doc §14, "Error contract").
///
/// Each category names the first subsystem that refused the operation. The
/// category is stable across releases; `cause` and `phase` carry the detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCategory {
    /// APK signature verification failed (v2/v3 scheme, signer mismatch).
    PkgSignature,
    /// DEX could not be verified or executed (bad header, unknown opcode,
    /// unimplemented semantics).
    DexUnsupported,
    /// Native ABI problem: unknown ELF relocation, missing symbol, wrong TLS.
    AbiSymbol,
    /// A requested GPU feature, format or extension is unavailable.
    GpuFeature,
    /// An Android service the app calls is not implemented.
    ServiceMissing,
    /// The permission broker denied an operation.
    PolicyDenied,
    /// The host process itself faulted.
    HostCrash,
}

impl ErrorCategory {
    /// The stable string form used in CLI output and ledger records.
    pub fn code(self) -> &'static str {
        match self {
            Self::PkgSignature => "PKG_SIGNATURE",
            Self::DexUnsupported => "DEX_UNSUPPORTED",
            Self::AbiSymbol => "ABI_SYMBOL",
            Self::GpuFeature => "GPU_FEATURE",
            Self::ServiceMissing => "SERVICE_MISSING",
            Self::PolicyDenied => "POLICY_DENIED",
            Self::HostCrash => "HOST_CRASH",
        }
    }
}

impl fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// The pipeline phase where the error surfaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// APK inspection / install commit.
    Install,
    /// DEX verification or loading.
    DexLoad,
    /// DEX interpretation or AOT execution.
    DexExec,
    /// ELF loading, relocation, Bionic, or JNI.
    Native,
    /// Resource / manifest resolution.
    Resources,
    /// Measure, layout, draw, text.
    Ui,
    /// GPU / surface composition.
    Gpu,
    /// Brokered host service call.
    Service,
    /// macOS window, input, or IME.
    Host,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Install => "install",
            Self::DexLoad => "dex_load",
            Self::DexExec => "dex_exec",
            Self::Native => "native",
            Self::Resources => "resources",
            Self::Ui => "ui",
            Self::Gpu => "gpu",
            Self::Service => "service",
            Self::Host => "host",
        };
        f.write_str(s)
    }
}

/// An error carrying the stable category, phase and a concrete cause name.
#[derive(Debug)]
pub struct OveloError {
    category: ErrorCategory,
    phase: Phase,
    /// Concrete cause name, e.g. the missing API signature or ELF symbol.
    cause: String,
    /// Optional reproduction hint shown to developers.
    detail: Option<String>,
}

impl OveloError {
    /// Create an error in `category`/`phase` with a machine-readable `cause`.
    pub fn new(
        category: ErrorCategory,
        phase: Phase,
        cause: impl Into<String>,
    ) -> Self {
        Self {
            category,
            phase,
            cause: cause.into(),
            detail: None,
        }
    }

    /// Attach a human-readable detail string.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Stable category.
    pub fn category(&self) -> ErrorCategory {
        self.category
    }

    /// Phase where the error surfaced.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Concrete cause name.
    pub fn cause(&self) -> &str {
        &self.cause
    }
}

impl fmt::Display for OveloError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} [{}] {}", self.category, self.phase, self.cause)?;
        if let Some(d) = &self.detail {
            write!(f, ": {d}")?;
        }
        Ok(())
    }
}

impl std::error::Error for OveloError {}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, OveloError>;
