//! Fenced-code colours via gpui-component's Tree-sitter highlighter.
//!
//! gpui-base paints Markdown fences as plain mono until a
//! [`gpui_base::TextView::code_block_highlighter`] is installed.
//! gpui-component already owns that stack (`SyntaxHighlighter` +
//! `HighlightTheme`); this module only adapts it for cibergit's bodies.

use gpui::HighlightStyle;
use gpui_base::text::CodeBlock;
use gpui_component::highlighter::{HighlightTheme, LanguageRegistry, SyntaxHighlighter};
use ropey::Rope;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

/// Highlight one fenced block for the current appearance.
pub(super) fn highlight_code_block(
    block: &CodeBlock,
    dark: bool,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let Some(lang) = block.lang() else {
        return Vec::new();
    };
    let lang = lang.trim();
    if lang.is_empty() {
        return Vec::new();
    }
    // Touch the registry so feature-gated grammars are present before the
    // first fence paints (LazyLock loads Language::all once).
    let _ = LanguageRegistry::singleton().languages();

    let code = block.code();
    if code.is_empty() {
        return Vec::new();
    }

    thread_local! {
        static HIGHLIGHTERS: RefCell<HashMap<String, SyntaxHighlighter>> =
            RefCell::new(HashMap::new());
    }

    let theme: Arc<HighlightTheme> = if dark {
        HighlightTheme::default_dark()
    } else {
        HighlightTheme::default_light()
    };

    HIGHLIGHTERS.with(|cache| {
        let mut cache = cache.borrow_mut();
        let key = lang.to_ascii_lowercase();
        let highlighter = cache
            .entry(key.clone())
            .or_insert_with(|| SyntaxHighlighter::new(&key));
        if let Some(config) = LanguageRegistry::singleton().language(&key)
            && highlighter.language() != &config.name
        {
            *highlighter = SyntaxHighlighter::new(&key);
        }

        let rope = Rope::from_str(code.as_ref());
        highlighter.update(None, &rope, None);
        highlighter.styles(&(0..code.len()), theme.as_ref())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_base::text::CodeBlock;

    #[test]
    fn rust_fence_produces_styles() {
        let block = CodeBlock::from_code("fn main() { let x = 1; }\n", Some("rust"));
        let styles = highlight_code_block(&block, false);
        assert!(
            !styles.is_empty(),
            "tree-sitter rust should emit at least one style run"
        );
    }
}
