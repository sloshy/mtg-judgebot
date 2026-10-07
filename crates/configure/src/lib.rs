//! `judge-config`: a page on localhost for editing `judge.toml` and the
//! non-secret settings in `.env`, with every draft checked by the same
//! loaders the binaries run (`check`). The form is generated from the
//! loader's types (`judge_bot::config::file_schema`) and the help text from
//! the files' own comments, so neither is restated here.

pub mod check;
pub mod env;
pub mod server;
pub mod toml_doc;
