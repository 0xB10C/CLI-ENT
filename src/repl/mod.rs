//! The REPL (PLAN.md §8): a dedicated blocking thread running rustyline, printing
//! events above the prompt via the external printer, feeding the command channel.

pub mod commands;
pub mod completer;
pub mod printer;
