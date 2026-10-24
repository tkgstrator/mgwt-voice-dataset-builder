//! Stable error and process-exit classification.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitClass {
    Success,
    Processing,
    Validation,
}

impl ExitClass {
    pub const fn code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Processing => 1,
            Self::Validation => 2,
        }
    }
}

#[derive(Debug)]
pub enum AppError {
    Validation(String),
    Processing(String),
    Unity {
        kind: UnityErrorKind,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnityErrorKind {
    ApprovedInput,
    CandidateDecode,
    UnsupportedLayoutOrCodec,
    Io,
    Unexpected,
}

impl AppError {
    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation(message.into())
    }

    pub fn processing(message: impl Into<String>) -> Self {
        Self::Processing(message.into())
    }

    pub fn unity(kind: UnityErrorKind, message: impl Into<String>) -> Self {
        Self::Unity {
            kind,
            message: message.into(),
        }
    }

    pub const fn unity_kind(&self) -> Option<UnityErrorKind> {
        match self {
            Self::Unity { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    pub const fn exit_class(&self) -> ExitClass {
        match self {
            Self::Validation(_) => ExitClass::Validation,
            Self::Processing(_) => ExitClass::Processing,
            Self::Unity {
                kind: UnityErrorKind::ApprovedInput,
                ..
            } => ExitClass::Validation,
            Self::Unity { .. } => ExitClass::Processing,
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(message) => write!(f, "validation: {message}"),
            Self::Processing(message) => write!(f, "processing: {message}"),
            Self::Unity { kind, message } => write!(f, "Unity {kind:?}: {message}"),
        }
    }
}

impl std::error::Error for AppError {}
