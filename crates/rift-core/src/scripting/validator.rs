//! Script validator trait and unified error types.
//!
//! Provides a common interface for validating scripts across different engines
//! (Rhai, Lua, JavaScript), enabling consistent validation behavior and error
//! handling throughout the codebase.

use std::error::Error;
use std::fmt::{self, Display};

/// Trait for script validators that check script syntax and structure.
///
/// Each script engine implements this trait to provide consistent validation
/// behavior while allowing engine-specific error types.
pub trait ScriptValidator {
    /// The error type returned when validation fails.
    type Error: Error + Display + Clone;

    /// Validates a script for syntax and basic structural requirements.
    ///
    /// # Arguments
    /// * `script` - The script source code to validate
    ///
    /// # Returns
    /// * `Ok(())` if the script is valid
    /// * `Err(Self::Error)` if validation fails
    fn validate(&self, script: &str) -> Result<(), Self::Error>;

    /// Validates multiple scripts and returns all errors encountered.
    ///
    /// # Arguments
    /// * `scripts` - Slice of (id, script) tuples to validate
    ///
    /// # Returns
    /// Vector of (id, Result) tuples for each script
    fn validate_batch<'a>(
        &self,
        scripts: &[(&'a str, &str)],
    ) -> Vec<(&'a str, Result<(), Self::Error>)> {
        scripts
            .iter()
            .map(|(id, script)| (*id, self.validate(script)))
            .collect()
    }
}

/// Unified script validation error that can represent errors from any engine.
///
/// This enum provides a common error type for use in contexts where the
/// specific engine type is not known at compile time.
#[derive(Debug, Clone)]
pub enum ScriptValidationError {
    /// Syntax error in the script
    SyntaxError { engine: String, message: String },
    /// Required function is missing
    MissingFunction { engine: String, function: String },
    /// Script failed to compile
    CompilationError { engine: String, message: String },
    /// Script failed to load
    LoadError { engine: String, message: String },
    /// Unknown or unsupported engine type
    UnsupportedEngine { engine: String },
}

impl Display for ScriptValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScriptValidationError::SyntaxError { engine, message } => {
                write!(f, "[{engine}] Syntax error: {message}")
            }
            ScriptValidationError::MissingFunction { engine, function } => {
                write!(f, "[{engine}] Missing required function: {function}")
            }
            ScriptValidationError::CompilationError { engine, message } => {
                write!(f, "[{engine}] Compilation error: {message}")
            }
            ScriptValidationError::LoadError { engine, message } => {
                write!(f, "[{engine}] Load error: {message}")
            }
            ScriptValidationError::UnsupportedEngine { engine } => {
                write!(f, "Unsupported script engine: {engine}")
            }
        }
    }
}

impl Error for ScriptValidationError {}

impl ScriptValidationError {
    /// Returns the engine name associated with this error.
    pub fn engine(&self) -> &str {
        match self {
            ScriptValidationError::SyntaxError { engine, .. }
            | ScriptValidationError::MissingFunction { engine, .. }
            | ScriptValidationError::CompilationError { engine, .. }
            | ScriptValidationError::LoadError { engine, .. }
            | ScriptValidationError::UnsupportedEngine { engine } => engine,
        }
    }

    /// Returns the error message without the engine prefix.
    pub fn message(&self) -> String {
        match self {
            ScriptValidationError::SyntaxError { message, .. } => message.clone(),
            ScriptValidationError::MissingFunction { function, .. } => {
                format!("Missing required function: {function}")
            }
            ScriptValidationError::CompilationError { message, .. } => message.clone(),
            ScriptValidationError::LoadError { message, .. } => message.clone(),
            ScriptValidationError::UnsupportedEngine { engine } => {
                format!("Unsupported script engine: {engine}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_script_validation_error_display() {
        let err = ScriptValidationError::SyntaxError {
            engine: "rhai".to_string(),
            message: "unexpected token".to_string(),
        };
        assert_eq!(err.to_string(), "[rhai] Syntax error: unexpected token");
    }

    #[test]
    fn test_script_validation_error_engine() {
        let err = ScriptValidationError::MissingFunction {
            engine: "javascript".to_string(),
            function: "should_inject".to_string(),
        };
        assert_eq!(err.engine(), "javascript");
    }

    #[test]
    fn test_script_validation_error_message() {
        let err = ScriptValidationError::CompilationError {
            engine: "lua".to_string(),
            message: "failed to parse".to_string(),
        };
        assert_eq!(err.message(), "failed to parse");
    }
}
