//! Unit tests for the transcript reader, one file per submodule.

mod assembler;
mod claude;
mod codex;
mod grok;
mod locate;
mod reader;
mod wire;

use super::*;

use std::io::Write;
