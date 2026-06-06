// Helpers shared by the daemon's CDP handlers: one-shot JS evaluation
// (`eval::evaluate_value`), navigation + readyState polling (`page`), and the
// shared text-extraction JS body (`get_text::build_text_body`).

pub mod eval;
pub mod get_text;
pub mod page;
