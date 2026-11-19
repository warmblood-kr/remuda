//! Bounded, inert syntax tokens for the GUI document reader.
//!
//! Tokens are byte ranges into one already-projected display line. Nothing here
//! runs code, loads a resource or changes the document text: the GUI maps a
//! token kind to a colour and renders the same characters. Markdown/HTML read
//! through the native projection without this tokenizer.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Keyword,
    String,
    Comment,
    Number,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
}

/// Longer display lines are not tokenized; the pane is bounded well below this.
const MAX_LINE_BYTES: usize = 4096;

pub fn tokens(line: &str, language: &str) -> Vec<Token> {
    if line.len() > MAX_LINE_BYTES {
        return Vec::new();
    }
    let keywords = keywords(language);
    if keywords.is_empty() {
        return Vec::new();
    }
    let comment = match language {
        "python" | "shell" | "toml" | "yaml" => Some("#"),
        "rust" | "javascript" => Some("//"),
        _ => None,
    };
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (start, c) = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if let Some(prefix) = comment {
            if line[start..].starts_with(prefix) {
                out.push(Token {
                    start,
                    end: line.len(),
                    kind: Kind::Comment,
                });
                break;
            }
        }
        if c == '"' || c == '\'' || (language == "javascript" && c == '`') {
            let quote = c;
            let mut j = i + 1;
            let mut escaped = false;
            while j < chars.len() {
                let current = chars[j].1;
                if escaped {
                    escaped = false;
                } else if current == '\\' {
                    escaped = true;
                } else if current == quote {
                    j += 1;
                    break;
                }
                j += 1;
            }
            out.push(Token {
                start,
                end: end_of(&chars, j, line),
                kind: Kind::String,
            });
            i = j;
            continue;
        }
        if c.is_ascii_digit() {
            let mut j = i + 1;
            while j < chars.len()
                && (chars[j].1.is_ascii_alphanumeric()
                    || matches!(chars[j].1, '_' | '.' | 'x' | 'X'))
            {
                j += 1;
            }
            out.push(Token {
                start,
                end: end_of(&chars, j, line),
                kind: Kind::Number,
            });
            i = j;
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].1.is_ascii_alphanumeric() || chars[j].1 == '_') {
                j += 1;
            }
            let word = &line[start..end_of(&chars, j, line)];
            if keywords.contains(&word) {
                out.push(Token {
                    start,
                    end: end_of(&chars, j, line),
                    kind: Kind::Keyword,
                });
            }
            i = j;
            continue;
        }
        i += 1;
    }
    out
}

fn end_of(chars: &[(usize, char)], index: usize, line: &str) -> usize {
    chars.get(index).map_or(line.len(), |(offset, _)| *offset)
}

fn keywords(language: &str) -> &'static [&'static str] {
    match language {
        "python" => &[
            "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
            "elif", "else", "except", "False", "finally", "for", "from", "global", "if", "import",
            "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return",
            "True", "try", "while", "with", "yield",
        ],
        "rust" => &[
            "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
            "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
            "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
            "trait", "true", "type", "unsafe", "use", "where", "while",
        ],
        "javascript" => &[
            "async",
            "await",
            "break",
            "case",
            "catch",
            "class",
            "const",
            "continue",
            "default",
            "delete",
            "do",
            "else",
            "export",
            "extends",
            "false",
            "finally",
            "for",
            "function",
            "if",
            "import",
            "in",
            "instanceof",
            "let",
            "new",
            "null",
            "return",
            "static",
            "super",
            "switch",
            "this",
            "throw",
            "true",
            "try",
            "typeof",
            "var",
            "while",
        ],
        "shell" => &[
            "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function", "if",
            "in", "local", "readonly", "return", "then", "until", "while",
        ],
        "json" => &["false", "null", "true"],
        "toml" | "yaml" => &["false", "null", "true"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice<'a>(line: &'a str, token: &Token) -> &'a str {
        &line[token.start..token.end]
    }

    #[test]
    fn python_keywords_strings_comments_and_numbers() {
        let line = "def f(x): return \"s\"  # note 3";
        let found = tokens(line, "python");
        assert!(found
            .iter()
            .any(|t| t.kind == Kind::Keyword && slice(line, t) == "def"));
        assert!(found
            .iter()
            .any(|t| t.kind == Kind::Keyword && slice(line, t) == "return"));
        assert!(found
            .iter()
            .any(|t| t.kind == Kind::String && slice(line, t) == "\"s\""));
        assert!(found
            .iter()
            .any(|t| t.kind == Kind::Comment && slice(line, t) == "# note 3"));
    }

    #[test]
    fn rust_and_javascript_line_comments_stop_at_the_end() {
        let line = "let x = 42; // note";
        let found = tokens(line, "rust");
        assert!(found
            .iter()
            .any(|t| t.kind == Kind::Keyword && slice(line, t) == "let"));
        assert!(found
            .iter()
            .any(|t| t.kind == Kind::Number && slice(line, t) == "42"));
        assert!(found
            .iter()
            .any(|t| t.kind == Kind::Comment && slice(line, t) == "// note"));
        let js = "const s = 'x'; // y";
        assert!(tokens(js, "javascript")
            .iter()
            .any(|t| t.kind == Kind::String && slice(js, t) == "'x'"));
    }

    #[test]
    fn json_literals_and_multibyte_ranges_stay_on_boundaries() {
        let line = "{\"한글\": true, \"n\": 1}";
        let found = tokens(line, "json");
        assert!(found
            .iter()
            .any(|t| t.kind == Kind::String && slice(line, t) == "\"한글\""));
        assert!(found
            .iter()
            .any(|t| t.kind == Kind::Keyword && slice(line, t) == "true"));
        assert!(found
            .iter()
            .any(|t| t.kind == Kind::Number && slice(line, t) == "1"));
        for token in found {
            assert!(line.is_char_boundary(token.start));
            assert!(line.is_char_boundary(token.end));
        }
    }

    #[test]
    fn markdown_and_unknown_languages_are_left_alone() {
        assert!(tokens("# heading", "markdown").is_empty());
        assert!(tokens("plain prose", "text").is_empty());
        assert!(tokens("x".repeat(MAX_LINE_BYTES + 1).as_str(), "python").is_empty());
    }
}
